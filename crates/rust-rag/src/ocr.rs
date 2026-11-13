//! The optional OCR service receives document bytes over a private local socket.

use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(all(feature = "ocr", any(target_os = "linux", test)))]
mod wire;

pub(crate) const JOB_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Worker {
    fingerprint: String,
    #[cfg(all(feature = "ocr", target_os = "linux"))]
    socket: std::path::PathBuf,
}

pub(crate) struct Passage {
    pub page: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub provenance: Value,
    pub table_terms: String,
}

impl Worker {
    pub fn load(path: &Path) -> Result<Self> {
        #[cfg(all(feature = "ocr", target_os = "linux"))]
        {
            use anyhow::ensure;
            use std::{fs, io::Read};
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Profile {
                schema_version: u32,
                socket: std::path::PathBuf,
                fingerprint: String,
            }
            ensure!(
                fs::symlink_metadata(path)?.is_file(),
                "Expected a regular OCR profile."
            );
            let mut bytes = Vec::new();
            fs::File::open(path)?.take(4097).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 4096, "The OCR profile exceeds 4 KiB.");
            let profile: Profile = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("Invalid OCR profile."))?;
            ensure!(
                profile.schema_version == 1,
                "Unsupported OCR profile version."
            );
            ensure!(
                valid_fingerprint(&profile.fingerprint),
                "Invalid OCR fingerprint."
            );
            ensure!(
                profile.socket.is_absolute(),
                "Use an absolute OCR socket path."
            );
            let worker = Self {
                socket: profile.socket,
                fingerprint: profile.fingerprint,
            };
            worker.exchange(&[], Instant::now() + Duration::from_secs(2))?;
            return Ok(worker);
        }
        #[cfg(not(all(feature = "ocr", target_os = "linux")))]
        {
            let _ = path;
            anyhow::bail!("OCR ingestion requires a Linux build with the ocr feature.");
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn extract(&self, raw: &[u8], hash: &str, deadline: Instant) -> Result<Vec<Passage>> {
        let deadline = deadline.min(Instant::now() + JOB_TIMEOUT);
        #[cfg(all(feature = "ocr", target_os = "linux"))]
        {
            let response = self.exchange(raw, deadline)?;
            return wire::decode(&response)?.passages(hash, self.fingerprint());
        }
        #[cfg(not(all(feature = "ocr", target_os = "linux")))]
        {
            let _ = (raw, hash, deadline);
            anyhow::bail!("OCR ingestion requires a Linux build with the ocr feature.");
        }
    }

    #[cfg(all(feature = "ocr", target_os = "linux"))]
    fn exchange(&self, raw: &[u8], deadline: Instant) -> Result<Vec<u8>> {
        use anyhow::{Context, ensure};
        use socket2::{Domain, SockAddr, Socket, Type};
        use std::{
            fs,
            io::{Read, Write},
            os::{
                fd::OwnedFd,
                unix::{
                    fs::{FileTypeExt, MetadataExt},
                    net::UnixStream,
                },
            },
        };

        fn left(deadline: Instant) -> Result<Duration> {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|time| !time.is_zero())
                .context("The OCR worker reached its deadline.")
        }
        fn read(stream: &mut UnixStream, mut target: &mut [u8], deadline: Instant) -> Result<()> {
            while !target.is_empty() {
                stream.set_read_timeout(Some(left(deadline)?))?;
                let read = stream
                    .read(target)
                    .context("The OCR worker stopped responding.")?;
                ensure!(read > 0, "The OCR worker did not complete the request.");
                target = &mut target[read..];
            }
            Ok(())
        }
        fn write(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
            while !bytes.is_empty() {
                stream.set_write_timeout(Some(left(deadline)?))?;
                let sent = stream
                    .write(bytes)
                    .context("The OCR worker stopped accepting input.")?;
                ensure!(sent > 0, "The OCR worker stopped accepting input.");
                bytes = &bytes[sent..];
            }
            Ok(())
        }

        ensure!(
            raw.len() <= crate::files::MAX_BYTES,
            "The PDF exceeds the 16 MiB input limit."
        );
        let parent = self.socket.parent().context("Invalid OCR socket path.")?;
        let directory = fs::symlink_metadata(parent)?;
        let target = fs::symlink_metadata(&self.socket)?;
        ensure!(
            directory.is_dir()
                && parent.canonicalize()? == parent
                && directory.mode() & 0o077 == 0
                && directory.uid() == target.uid()
                && target.file_type().is_socket()
                && target.mode() & 0o077 == 0,
            "Use a canonical, owner-only OCR socket directory and socket."
        );
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket
            .connect_timeout(&SockAddr::unix(&self.socket)?, left(deadline)?)
            .context("Cannot connect to the OCR worker.")?;
        let descriptor: OwnedFd = socket.into();
        let mut stream = UnixStream::from(descriptor);
        let peer = rustix::net::sockopt::socket_peercred(&stream)?;
        ensure!(
            peer.uid == rustix::process::geteuid() && peer.uid.as_raw() == target.uid(),
            "The OCR worker must run under the same mapped user ID."
        );
        let mut hello = [0; 72];
        hello[..8].copy_from_slice(b"RRAGOCR1");
        hello[8..].copy_from_slice(self.fingerprint.as_bytes());
        write(&mut stream, &hello, deadline)?;
        write(&mut stream, &(raw.len() as u32).to_be_bytes(), deadline)?;
        let mut peer = [0; 72];
        read(&mut stream, &mut peer, deadline)?;
        ensure!(
            peer == hello,
            "The OCR worker fingerprint does not match the profile."
        );
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        write(&mut stream, raw, deadline)?;
        let mut size = [0; 4];
        read(&mut stream, &mut size, deadline)?;
        let size = u32::from_be_bytes(size) as usize;
        ensure!(
            (1..=wire::MAX_RESPONSE_BYTES).contains(&size),
            "Invalid OCR response length."
        );
        let mut bytes = vec![0; size];
        read(&mut stream, &mut bytes, deadline)?;
        Ok(bytes)
    }
}

pub(crate) fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(all(test, feature = "ocr", target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{
            fs::{DirBuilderExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::PathBuf,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct SocketFixture(PathBuf);

    impl SocketFixture {
        fn start(
            handler: impl FnOnce(UnixStream, [u8; 72]) + Send + 'static,
        ) -> (Self, Worker, thread::JoinHandle<()>) {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("rr-ocr-{}-{stamp}", std::process::id()));
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            let socket = dir.join("worker.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
            let thread = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut hello = [0; 72];
                stream.read_exact(&mut hello).unwrap();
                let mut size = [0; 4];
                stream.read_exact(&mut size).unwrap();
                assert_eq!(u32::from_be_bytes(size), 4);
                handler(stream, hello);
            });
            (
                Self(dir),
                Worker {
                    socket,
                    fingerprint: "a".repeat(64),
                },
                thread,
            )
        }
    }

    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join("worker.sock"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn fingerprint_mismatch_sends_no_document() {
        let (_fixture, worker, peer) = SocketFixture::start(|mut stream, mut hello| {
            hello[8] = b'b';
            stream.write_all(&hello).unwrap();
            assert_eq!(stream.read(&mut [0; 4]).unwrap(), 0);
        });
        let error = worker
            .exchange(b"%PDF", Instant::now() + Duration::from_secs(2))
            .unwrap_err();
        assert!(error.to_string().contains("fingerprint"));
        peer.join().unwrap();
    }

    #[test]
    fn rejects_oversized_and_truncated_frames() {
        for (length, bytes) in [
            ((wire::MAX_RESPONSE_BYTES + 1) as u32, &b""[..]),
            (8, &b"one"[..]),
        ] {
            let (_fixture, worker, peer) = SocketFixture::start(move |mut stream, hello| {
                stream.write_all(&hello).unwrap();
                let mut document = [0; 4];
                stream.read_exact(&mut document).unwrap();
                assert_eq!(&document, b"%PDF");
                stream.write_all(&length.to_be_bytes()).unwrap();
                stream.write_all(bytes).unwrap();
            });
            assert!(
                worker
                    .exchange(b"%PDF", Instant::now() + Duration::from_secs(2))
                    .is_err()
            );
            peer.join().unwrap();
        }
    }

    #[test]
    fn stalled_response_obeys_the_shared_deadline() {
        let (_fixture, worker, peer) = SocketFixture::start(|mut stream, hello| {
            stream.write_all(&hello).unwrap();
            stream.read_exact(&mut [0; 4]).unwrap();
            stream.write_all(&64_u32.to_be_bytes()).unwrap();
            stream.write_all(b"partial").unwrap();
            assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
        });
        let start = Instant::now();
        assert!(
            worker
                .exchange(b"%PDF", start + Duration::from_millis(250))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        peer.join().unwrap();
    }
}
