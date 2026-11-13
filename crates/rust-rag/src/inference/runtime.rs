use anyhow::{Context, Result, ensure};
use ort::environment::Environment;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_LIBRARY_BYTES: u64 = 256 * 1024 * 1024;
static RUNTIME_LIBRARY: Mutex<Option<RuntimeIdentity>> = Mutex::new(None);

#[derive(Clone, PartialEq, Eq)]
struct RuntimeIdentity {
    path: PathBuf,
    sha256: String,
}

pub struct VerifiedRuntime {
    identity: RuntimeIdentity,
    // On Windows this read handle denies writes and deletion until loading finishes.
    _library_file: File,
}

impl VerifiedRuntime {
    pub fn open(path: &Path, expected_sha256: &str) -> Result<Self> {
        ensure!(
            expected_sha256.len() == 64
                && expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Supply the trusted ONNX Runtime library SHA-256 as 64 hexadecimal characters."
        );
        ensure!(
            path.is_absolute() && path.is_file(),
            "Choose an installed ONNX Runtime 1.24+ library with an absolute path."
        );
        let path = path.canonicalize()?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::{
                os::windows::fs::OpenOptionsExt,
                path::{Component, Prefix},
            };
            ensure!(
                matches!(path.components().next(), Some(Component::Prefix(prefix))
                    if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
                "Choose an ONNX Runtime library on a local drive."
            );
            const FILE_SHARE_READ: u32 = 0x1;
            options.share_mode(FILE_SHARE_READ);
        }
        let mut library = options
            .open(&path)
            .context("The selected ONNX Runtime library could not be read.")?;
        let mut digest = Sha256::new();
        let bytes = io::copy(&mut (&mut library).take(MAX_LIBRARY_BYTES + 1), &mut digest)?;
        ensure!(
            (1..=MAX_LIBRARY_BYTES).contains(&bytes),
            "The ONNX Runtime library has an unsupported size."
        );
        let sha256 = format!("{:x}", digest.finalize());
        ensure!(
            sha256.eq_ignore_ascii_case(expected_sha256),
            "The ONNX Runtime library failed its pinned SHA-256 check."
        );
        Ok(Self {
            identity: RuntimeIdentity { path, sha256 },
            _library_file: library,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.identity.sha256
    }

    pub fn environment(&self) -> Result<Environment> {
        let mut loaded = RUNTIME_LIBRARY
            .lock()
            .map_err(|_| anyhow::anyhow!("The ONNX Runtime loader is unavailable."))?;
        ensure!(
            loaded
                .as_ref()
                .is_none_or(|identity| identity == &self.identity),
            "A different ONNX Runtime library is already loaded in this process."
        );
        #[cfg(windows)]
        let _secure_preload = preload(&self.identity.path)?;
        // The preloaded Windows module is retained while ort acquires its own reference.
        let builder = ort::init_from(&self.identity.path)
            .context("Could not load the selected ONNX Runtime 1.24+ library.")?;
        *loaded = Some(self.identity.clone());
        builder
            .build()
            .map_err(|_| anyhow::anyhow!("Could not start the ONNX Runtime environment."))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn preload(path: &Path) -> Result<libloading::os::windows::Library> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
    };
    // SAFETY: The operator chose a trusted library whose hash has been verified. A read-only
    // handle denies modification or replacement until ort has retained the module. Resolve its
    // imports beside the selected library or in System32, excluding the current directory and PATH.
    unsafe {
        Library::load_with_flags(
            path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .context("Could not securely load the selected ONNX Runtime library.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_verifies_hash_before_any_loading() {
        use std::io::Write;
        let mut fixture = tempfile::NamedTempFile::new().unwrap();
        fixture.write_all(b"synthetic library fixture").unwrap();
        let path = fixture.path().to_owned();
        let temporary = fixture.into_temp_path();
        let expected = format!("{:x}", Sha256::digest(b"synthetic library fixture"));
        assert!(VerifiedRuntime::open(&path, "").is_err());
        assert!(VerifiedRuntime::open(&path, &"0".repeat(64)).is_err());
        assert!(VerifiedRuntime::open(Path::new("relative.dll"), &expected).is_err());
        assert!(VerifiedRuntime::open(&path, &expected).is_ok());
        drop(temporary);
    }
}
