"""Single-job Unix socket supervisor for the offline OCR container."""

import argparse
import ctypes
import errno
import hashlib
import os
from pathlib import Path
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent
MODEL = Path("/opt/rust-rag-ocr/model")
MANIFEST = ROOT / "artifacts.sha256"
MAGIC = b"RRAGOCR1"
MAX_PDF = 16 * 1024 * 1024
MAX_OUTPUT = 12 * 1024 * 1024
LIBC = ctypes.CDLL(None, use_errno=True)
LIBC.syscall.restype = ctypes.c_long


def checked(result):
    if result < 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    return result


def restrict_files(job):
    # Linux amd64 Landlock syscalls. ABI 3 is required to mediate truncate.
    create_ruleset, add_rule, restrict_self = 444, 445, 446
    if checked(LIBC.syscall(create_ruleset, 0, 0, 1)) < 3:
        raise OSError("Landlock ABI 3 or newer is required")
    read_file, read_dir, execute, write_file = 4, 8, 1, 2
    rights = (1 << 15) - 1
    rules = ctypes.c_uint64(rights)
    fd = checked(LIBC.syscall(create_ruleset, ctypes.byref(rules), ctypes.sizeof(rules), 0))
    try:
        paths = {
            "/usr": read_file | read_dir | execute,
            "/etc": read_file | read_dir,
            "/proc/self": read_file | read_dir,
            "/proc/cpuinfo": read_file,
            "/proc/meminfo": read_file,
            "/sys": read_file | read_dir,
            "/dev/null": read_file | write_file,
            "/dev/random": read_file,
            "/dev/urandom": read_file,
            sys.prefix: read_file | read_dir | execute,
            str(ROOT): read_file | read_dir,
            str(MODEL): read_file | read_dir,
            job: rights & ~execute,
        }
        for path, access in paths.items():
            target = os.open(path, os.O_PATH | os.O_CLOEXEC)
            try:
                rule = ctypes.create_string_buffer(struct.pack("=Qi", access, target))
                checked(LIBC.syscall(add_rule, fd, 1, rule, 0))
            finally:
                os.close(target)
        checked(LIBC.syscall(restrict_self, fd, 0))
    finally:
        os.close(fd)


def restrict_syscalls():
    # Landlock controls file content and topology. Seccomp closes metadata,
    # IPC and process-group escape paths that it does not cover at ABI 3.
    policy = """
        socket socketpair setsid setpgid unshare setns
        kill tkill tgkill rt_sigqueueinfo rt_tgsigqueueinfo pidfd_send_signal
        ptrace process_vm_readv process_vm_writev process_madvise process_mrelease pidfd_getfd
        io_uring_setup io_uring_enter io_uring_register
        chmod fchmod fchmodat fchmodat2 chown fchown fchownat lchown
        utime utimes utimensat futimesat setxattr fsetxattr lsetxattr
        removexattr fremovexattr lremovexattr
        msgget msgctl msgsnd msgrcv semget semctl semop semtimedop
        shmget shmat shmctl shmdt mq_open mq_unlink
        setpriority sched_setattr sched_setaffinity sched_setscheduler
        sched_setparam ioprio_set migrate_pages move_pages
    """.split()
    class Comparison(ctypes.Structure):
        _fields_ = [("arg", ctypes.c_uint), ("op", ctypes.c_int),
                    ("value", ctypes.c_uint64), ("mask_value", ctypes.c_uint64)]

    seccomp = ctypes.CDLL("libseccomp.so.2")
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]
    seccomp.seccomp_release.restype = None
    seccomp.seccomp_rule_add_array.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint,
        ctypes.POINTER(Comparison),
    ]
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_load.argtypes = [ctypes.c_void_p]
    context = seccomp.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
    if not context:
        raise OSError("Could not create the OCR seccomp policy")

    def deny(syscall, error=errno.EPERM, comparison=None):
        number = seccomp.seccomp_syscall_resolve_name(syscall.encode("ascii")) if isinstance(syscall, str) else syscall
        count = int(comparison is not None)
        args = ctypes.byref(comparison) if comparison is not None else None
        if number < 0 or seccomp.seccomp_rule_add_array(context, 0x00050000 | error, number, count, args):
            raise OSError(f"Could not block OCR syscall {syscall}")

    try:
        for name in policy:
            deny(name)
        # Linux amd64 setxattrat/removexattrat numbers may be unknown to
        # the libseccomp version in the pinned base.
        deny(463)
        deny(466)
        # clone3 arguments are passed by pointer. glibc falls back to clone
        # on ENOSYS, where seccomp can reject each namespace flag.
        deny("clone3", errno.ENOSYS)
        for flag in (0x20000, 0x2000000, 0x4000000, 0x8000000,
                     0x10000000, 0x20000000, 0x40000000):
            deny("clone", comparison=Comparison(0, 7, flag, flag))  # SCMP_CMP_MASKED_EQ
        deny("prlimit64", comparison=Comparison(0, 1, 0, 0))  # SCMP_CMP_NE
        for operation in (8, 10, 15):  # F_SETOWN, F_SETSIG, F_SETOWN_EX
            deny("fcntl", comparison=Comparison(1, 4, operation, 0))  # SCMP_CMP_EQ
        if seccomp.seccomp_load(context):
            raise OSError("Could not load the OCR seccomp policy")
    finally:
        seccomp.seccomp_release(context)


def isolate(job):
    checked(LIBC.prctl(38, 1, 0, 0, 0))  # PR_SET_NO_NEW_PRIVS
    restrict_files(job)
    restrict_syscalls()


def left(deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError
    return remaining


def read_exact(connection, count, deadline):
    result = bytearray(count)
    view = memoryview(result)
    while view:
        connection.settimeout(left(deadline))
        read = connection.recv_into(view)
        if not read:
            raise EOFError
        view = view[read:]
    return result


def clear_job(job):
    # An untrusted job can leave a tree deeper than Python's recursive
    # cleanup supports, or directories without owner search permission.
    deadline = time.monotonic() + 1
    remove = ["/usr/bin/rm", "-rf", "--one-file-system", "--", job]
    try:
        if subprocess.run(remove, timeout=left(deadline), stderr=subprocess.DEVNULL).returncode:
            subprocess.run(["/usr/bin/chmod", "-R", "u+rwX", "--", job],
                           timeout=left(deadline), stderr=subprocess.DEVNULL, check=True)
            subprocess.run(remove, timeout=left(deadline), stderr=subprocess.DEVNULL, check=True)
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError("OCR job scratch could not be cleared") from None
    try:
        os.lstat(job)
    except FileNotFoundError:
        return
    except OSError:
        raise RuntimeError("OCR job scratch could not be verified") from None
    raise RuntimeError("OCR job scratch survived cleanup")


def extract(pdf, deadline):
    # Files in the private tmpfs avoid pipe backpressure and let us observe
    # the leader's exit without reaping it before killing the process group.
    job = tempfile.mkdtemp(prefix="ocr-job-")
    reaped = True
    try:
        with tempfile.TemporaryFile(dir=job) as source, tempfile.TemporaryFile(dir=job) as result:
            source.write(pdf)
            source.seek(0)
            reaped = False
            try:
                child = subprocess.Popen(
                    [sys.executable, "-B", str(ROOT / "worker.py"), "--table-model", str(MODEL)],
                    stdin=source,
                    stdout=result,
                    stderr=subprocess.DEVNULL,
                    start_new_session=True,
                    cwd=job,
                    env={**os.environ, "HOME": job, "XDG_CACHE_HOME": job, "TMPDIR": job},
                    preexec_fn=lambda: isolate(job),
                )
            except (OSError, subprocess.SubprocessError):
                raise RuntimeError("OCR process could not start safely") from None
            try:
                while os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT | os.WNOHANG) is None:
                    time.sleep(min(0.02, left(deadline - 2)))
            finally:
                stop_group(child)
                reaped = True
            if child.returncode != 0 or not 0 < result.tell() <= MAX_OUTPUT:
                raise ValueError("OCR extraction failed")
            result.seek(0)
            return result.read(MAX_OUTPUT + 1)
    finally:
        # If stopping the child failed, exit rather than traverse its live tree.
        if reaped:
            clear_job(job)


def stop_group(child):
    # Keep the leader's PID reserved until we signal its group. Since jobs
    # cannot leave the group, the subreaper can collect every orphan here.
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError:
        raise RuntimeError("OCR process group could not be stopped") from None
    stop_by = time.monotonic() + 1
    try:
        child.wait(timeout=left(stop_by))
        while True:
            pid, _ = os.waitpid(-child.pid, os.WNOHANG)
            if pid == 0:
                time.sleep(min(0.02, left(stop_by)))
    except ChildProcessError:
        pass
    except (TimeoutError, subprocess.TimeoutExpired):
        raise RuntimeError("OCR process group did not stop") from None
    except OSError:
        raise RuntimeError("OCR process group could not be reaped") from None


def respond(connection, hello):
    credentials = struct.Struct("iII")
    _, uid, _ = credentials.unpack(connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, credentials.size))
    if uid != os.geteuid():
        return
    deadline = time.monotonic() + 58
    if read_exact(connection, len(hello), deadline) != hello:
        return
    size, = struct.unpack("!I", read_exact(connection, 4, deadline))
    if size > MAX_PDF:
        return
    connection.settimeout(left(deadline))
    connection.sendall(hello)
    if size == 0:
        return
    pdf = read_exact(connection, size, deadline)
    response = extract(pdf, deadline)
    connection.settimeout(left(deadline))
    connection.sendall(struct.pack("!I", len(response)))
    connection.settimeout(left(deadline))
    connection.sendall(response)


def stop(_signal, _frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--socket", type=Path)
    mode.add_argument("--fingerprint", action="store_true")
    parser.add_argument("--image-id", help="Content-addressed OCI image ID used by the operator")
    args = parser.parse_args()
    subprocess.run(["sha256sum", "--strict", "--check", str(MANIFEST)],
                   cwd=ROOT, stdout=subprocess.DEVNULL, check=True)
    manifest = MANIFEST.read_bytes()
    if args.image_id is not None:
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.image_id):
            parser.error("Use a complete content-addressed OCI image ID")
        manifest = args.image_id.encode("ascii") + b"\n" + manifest
    fingerprint = hashlib.sha256(manifest).hexdigest()
    if args.fingerprint:
        print(fingerprint)
        return

    os.umask(0o077)
    if os.uname().machine != "x86_64":
        raise SystemExit("The OCR process sandbox requires Linux amd64")
    try:
        checked(LIBC.prctl(36, 1, 0, 0, 0))  # PR_SET_CHILD_SUBREAPER
        checked(LIBC.prctl(4, 0, 0, 0, 0))  # PR_SET_DUMPABLE
        with tempfile.TemporaryDirectory(prefix="ocr-sandbox-check-") as job:
            subprocess.run([sys.executable, "-I", "-B", "-c", "pass"],
                           preexec_fn=lambda: isolate(job), check=True, timeout=2)
    except (OSError, subprocess.SubprocessError):
        raise SystemExit("Could not start the OCR process sandbox. Check Landlock and seccomp support.") from None
    signal.signal(signal.SIGTERM, stop)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(args.socket))
        bound = args.socket.lstat()
        try:
            listener.listen(1)
            while True:
                connection, _ = listener.accept()
                with connection:
                    try:
                        respond(connection, MAGIC + fingerprint.encode("ascii"))
                    except (OSError, EOFError, ValueError, subprocess.SubprocessError):
                        pass
        except KeyboardInterrupt:
            pass
        finally:
            try:
                current = args.socket.lstat()
                if (current.st_dev, current.st_ino) == (bound.st_dev, bound.st_ino):
                    args.socket.unlink()
            except FileNotFoundError:
                pass


if __name__ == "__main__":
    main()
