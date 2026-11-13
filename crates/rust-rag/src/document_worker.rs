//! Isolate untrusted document decoders behind bounded byte pipes.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const MAX_INPUT: usize = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MEMORY: usize = 512 * 1024 * 1024;
const LIMIT_ERROR: &str = "The isolated document reader could not set its resource limits.";
const WORKER_ERROR: &str = "The isolated document reader could not complete.";

struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn restrict_worker(child: &Child) -> Result<std::os::windows::io::OwnedHandle> {
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: Both optional pointers are null, so this creates an unnamed,
    // noninheritable job with default security attributes.
    let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error()).context(LIMIT_ERROR);
    }
    // SAFETY: CreateJobObjectW returned a new, valid handle. Ownership is
    // transferred exactly once, and OwnedHandle closes it on every exit.
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    // Windows specifies user-mode CPU time in 100-nanosecond units.
    limits.BasicLimitInformation.PerProcessUserTimeLimit = TIMEOUT.as_secs() as i64 * 10_000_000;
    limits.ProcessMemoryLimit = MAX_MEMORY;
    // SAFETY: The job handle is owned by this function, and the initialized
    // structure and size match the selected information class.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context(LIMIT_ERROR);
    }
    // SAFETY: The two borrowed handles stay valid for this call. The worker
    // is waiting for input, which the caller sends only after assignment.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error()).context(LIMIT_ERROR);
    }
    Ok(job)
}

#[cfg(unix)]
fn restrict_current_process() -> Result<()> {
    use rlimit::Resource;

    for (resource, limit) in [
        (Resource::AS, MAX_MEMORY as u64),
        (Resource::CPU, TIMEOUT.as_secs()),
        (Resource::CORE, 0),
    ] {
        let (soft, hard) = resource.get().context(LIMIT_ERROR)?;
        resource
            .set(soft.min(limit), hard.min(limit))
            .context(LIMIT_ERROR)?;
    }
    Ok(())
}

fn wait_for_worker(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context(WORKER_ERROR)? {
            return Ok(status);
        }
        ensure!(
            start.elapsed() < timeout,
            "Document parsing exceeded its time limit."
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn run(raw: &[u8], entry: &str, output_limit: usize) -> Result<Vec<u8>> {
    ensure!(
        raw.len() <= MAX_INPUT,
        "The document exceeds the input limit."
    );
    let executable = std::env::current_exe().context(WORKER_ERROR)?;
    let mut command = Command::new(&executable);
    command
        .arg(entry)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(directory) = executable.parent() {
        command.current_dir(directory);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    thread::scope(|scope| -> Result<_> {
        // On an early return, this guard kills the worker before the scope
        // joins its I/O threads. This also unblocks a full pipe.
        let mut worker = Worker(command.spawn().context(WORKER_ERROR)?);
        #[cfg(windows)]
        let _job = restrict_worker(&worker.0)?;
        let mut input = worker.0.stdin.take().context(WORKER_ERROR)?;
        let output = worker.0.stdout.take().context(WORKER_ERROR)?;
        let writer = thread::Builder::new()
            .spawn_scoped(scope, move || input.write_all(raw))
            .context(WORKER_ERROR)?;
        let reader = thread::Builder::new()
            .spawn_scoped(scope, move || read_bounded(output, output_limit))
            .context(WORKER_ERROR)?;
        let status = wait_for_worker(&mut worker.0, TIMEOUT)?;
        let response = reader
            .join()
            .map_err(|_| anyhow::anyhow!(WORKER_ERROR))?
            .context(WORKER_ERROR)?;
        ensure!(response.len() <= output_limit, WORKER_ERROR);
        ensure!(status.success(), "{WORKER_ERROR}");
        writer
            .join()
            .map_err(|_| anyhow::anyhow!(WORKER_ERROR))?
            .context(WORKER_ERROR)?;
        Ok(response)
    })
}

fn read_bounded(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn respond(
    parse: impl FnOnce(&[u8]) -> std::result::Result<Value, &'static str>,
) -> Result<()> {
    #[cfg(unix)]
    restrict_current_process()?;
    let raw = read_bounded(io::stdin().lock(), MAX_INPUT).context(WORKER_ERROR)?;
    let response = parse(&raw).unwrap_or_else(|error| json!({"schema_version": 1, "error": error}));
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &response).context(WORKER_ERROR)?;
    stdout.flush().context(WORKER_ERROR)?;
    Ok(())
}
