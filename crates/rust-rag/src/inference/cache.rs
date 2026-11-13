use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    time::Duration,
};
use ureq::{Agent, http::Uri};

const MANIFEST_JSON: &str = include_str!("model-manifest.json");
const MAX_REDIRECTS: usize = 6;

struct ModelFile<'a> {
    name: &'a str,
    url: &'a str,
    bytes: u64,
    sha256: &'a str,
}

pub struct VerifiedModel {
    pub onnx: Vec<u8>,
    pub tokenizer: Vec<u8>,
}

pub fn manifest() -> Value {
    let mut metadata: Value = serde_json::from_str(MANIFEST_JSON).expect("bundled BGE manifest");
    metadata["total_size_bytes"] = json!(files(&metadata).map(|file| file.bytes).sum::<u64>());
    metadata
}

fn files(metadata: &Value) -> impl Iterator<Item = ModelFile<'_>> {
    metadata["files"]
        .as_array()
        .expect("bundled BGE manifest files")
        .iter()
        .map(|item| ModelFile {
            name: item["path"].as_str().expect("bundled BGE file path"),
            url: item["url"].as_str().expect("bundled BGE file URL"),
            bytes: item["size_bytes"].as_u64().expect("bundled BGE file size"),
            sha256: item["sha256"].as_str().expect("bundled BGE file digest"),
        })
}

fn cache_name(metadata: &Value) -> String {
    format!(
        "bge-small-en-v1.5-{}",
        metadata["revision"].as_str().expect("bundled BGE revision")
    )
}

impl VerifiedModel {
    pub fn load(cache_dir: &Path) -> Result<Self> {
        let metadata = manifest();
        let root = crate::files::state_directory(cache_dir, false)?;
        let directory = crate::files::directory(&root.join(cache_name(&metadata)))
            .context("Install the pinned BGE model before indexing or querying.")?;
        let mut onnx = None;
        let mut tokenizer = None;
        for file in files(&metadata) {
            let data = file.read_local(&directory).with_context(|| {
                format!(
                    "BGE file {} is missing or changed. Reinstall the pinned model.",
                    file.name
                )
            })?;
            match file.name {
                "model_optimized.onnx" => onnx = Some(data),
                "tokenizer.json" => tokenizer = Some(data),
                _ => (),
            }
        }
        Ok(Self {
            onnx: onnx.context("Pinned BGE model is missing.")?,
            tokenizer: tokenizer.context("Pinned BGE tokenizer is missing.")?,
        })
    }
}

pub fn install(cache_dir: &Path, from: Option<&Path>) -> Result<Value> {
    let metadata = manifest();
    let source = from.map(crate::files::directory).transpose()?;
    let root = crate::files::state_directory(cache_dir, true)?;
    let directory = root.join(cache_name(&metadata));
    fs::create_dir_all(&directory)?;
    let directory = crate::files::directory(&directory)?;
    let mut installed = 0;
    let agent = Agent::new_with_config(
        Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(180)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .max_response_header_size(32 * 1024)
            .build(),
    );
    for file in files(&metadata) {
        let target = directory.join(file.name);
        if target.exists() {
            regular_file(&target)?;
            if file.read_local(&directory).is_ok() {
                continue;
            }
        }
        let bytes = if let Some(source) = &source {
            file.read_local(source)
                .with_context(|| format!("Imported BGE file {} failed verification.", file.name))?
        } else {
            file.download(&agent)
                .with_context(|| format!("Could not download verified BGE file {}.", file.name))?
        };
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&target).map_err(|error| error.error)?;
        installed += 1;
    }
    Ok(json!({
        "model": metadata["model"],
        "revision": metadata["revision"],
        "method": if from.is_some() { "import" } else { "download" },
        "files_installed": installed,
        "already_present": installed == 0,
    }))
}

fn regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.is_file(), "Model files must be regular files.");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject reparse points, offline placeholders and files recalled from sync storage.
        ensure!(
            metadata.file_attributes() & (0x400 | 0x1000 | 0x0004_0000 | 0x0040_0000) == 0,
            "Model files must be locally available regular files."
        );
    }
    Ok(())
}

impl ModelFile<'_> {
    fn read_local(&self, directory: &Path) -> Result<Vec<u8>> {
        read_local_file(&directory.join(self.name), self.bytes, self.sha256)
    }

    fn read_verified(&self, reader: impl Read) -> Result<Vec<u8>> {
        read_verified(reader, self.bytes, self.sha256)
    }

    fn download(&self, agent: &Agent) -> Result<Vec<u8>> {
        let mut uri = self.url.parse::<Uri>()?;
        for _ in 0..=MAX_REDIRECTS {
            ensure!(
                allowed_model_host(&uri),
                "The model download destination is outside Hugging Face."
            );
            let mut response = agent
                .get(uri.clone())
                .header("User-Agent", "Rust-RAG-model-acquisition/1")
                .call()
                .map_err(|_| anyhow::anyhow!("The public model download request failed."))?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get("Location")
                    .context("Model redirect has no location.")?
                    .to_str()?;
                uri = redirect_target(&uri, location)?;
                continue;
            }
            ensure!(
                response.status().is_success(),
                "The public model server returned HTTP {}.",
                response.status()
            );
            return self.read_verified(response.body_mut().as_reader());
        }
        bail!("The public model download exceeded the redirect limit.");
    }
}

pub(super) fn read_local_file(path: &Path, size: u64, sha256: &str) -> Result<Vec<u8>> {
    regular_file(path)?;
    read_verified(File::open(path)?, size, sha256)
}

fn read_verified(reader: impl Read, size: u64, sha256: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    reader.take(size + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size,
        "Model file size differs from the pinned manifest."
    );
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == sha256,
        "Model file SHA-256 differs from the pinned manifest."
    );
    Ok(bytes)
}

fn allowed_model_host(uri: &Uri) -> bool {
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    uri.scheme_str() == Some("https")
        && matches!(uri.port_u16(), None | Some(443))
        && uri
            .authority()
            .is_some_and(|authority| !authority.as_str().contains('@'))
        && (host == "huggingface.co"
            || host.ends_with(".huggingface.co")
            || host.ends_with(".hf.co"))
}

fn redirect_target(current: &Uri, location: &str) -> Result<Uri> {
    if location.starts_with('/') && !location.starts_with("//") {
        return Ok(Uri::builder()
            .scheme("https")
            .authority(
                current
                    .authority()
                    .context("Model host is missing.")?
                    .clone(),
            )
            .path_and_query(location)
            .build()?);
    }
    location
        .parse::<Uri>()
        .context("Invalid model redirect destination.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pins_every_expected_file_and_download_size() {
        let metadata = manifest();
        assert_eq!(metadata["total_size_bytes"], 67_410_671);
        assert_eq!(metadata["dimensions"], 384);
        for file in files(&metadata) {
            assert_eq!(Path::new(file.name).components().count(), 1);
            assert!(allowed_model_host(&file.url.parse().unwrap()));
            assert!(file.url.ends_with(file.name));
            assert_eq!(file.sha256.len(), 64);
            assert!(file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn model_file_requires_exact_length_and_hash() {
        let sha256 = format!("{:x}", Sha256::digest(b"test"));
        let file = ModelFile {
            name: "fixture",
            url: "",
            bytes: 4,
            sha256: &sha256,
        };
        assert_eq!(file.read_verified(&b"test"[..]).unwrap(), b"test");
        for invalid in [b"tes".as_slice(), b"tests", b"xxxx"] {
            assert!(file.read_verified(invalid).is_err());
        }
    }

    #[test]
    fn redirect_policy_only_allows_https_hugging_face_hosts() {
        let origin = "https://huggingface.co/org/model/resolve/rev/file"
            .parse()
            .unwrap();
        let relative = redirect_target(&origin, "/api/resolve-cache/org/model/rev/file").unwrap();
        assert!(allowed_model_host(&relative));
        for invalid in [
            "http://huggingface.co/file",
            "https://huggingface.co:8443/file",
            "https://huggingface.co.evil.test/file",
            "https://private.test/file",
            "//private.test/file",
        ] {
            assert!(!redirect_target(&origin, invalid).is_ok_and(|uri| allowed_model_host(&uri)));
        }
    }
}
