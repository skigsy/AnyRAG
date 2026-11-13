//! Versioned S3 adapters for administrator-selected sources, policy, and backups.
//! The AWS SDK resolves workload identity and signs every request.

mod neptune;
pub use neptune::{NeptuneGraph, projection_scope};

use anyhow::{Result, anyhow, ensure};
use aws_config::{BehaviorVersion, retry::RetryConfig, timeout::TimeoutConfig};
use aws_sdk_s3::{Client, config::Region, primitives::ByteStream, types::ChecksumMode};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    future::Future,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, runtime::Runtime, sync::OnceCell};

const MAX_POLICY: u64 = 1024 * 1024;
const MAX_BACKUP: u64 = 4 * 1024 * 1024 * 1024;
const STREAM_IDLE: Duration = Duration::from_secs(30);

fn run<T: Send + 'static>(task: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    static WORKER: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    let runtime = WORKER
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("rust-rag-aws")
                .build()
        })
        .as_ref()
        .map_err(|_| anyhow!("Could not start the AWS worker."))?;
    let (result, receiver) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let _ = result.send(task.await);
    });
    receiver
        .recv()
        .map_err(|_| anyhow!("The AWS worker stopped."))?
}

fn region(value: Option<&Value>) -> Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    let Some(region) = value.as_str() else {
        anyhow::bail!("Configure a valid AWS region.")
    };
    let segments = region.split('-').collect::<Vec<_>>();
    ensure!(
        region.len() <= 40
            && segments.len() >= 3
            && segments[0].len() == 2
            && segments[0].bytes().all(|byte| byte.is_ascii_lowercase())
            && segments[segments.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
            && segments.iter().all(|segment| !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())),
        "Configure a valid AWS region."
    );
    Ok(Some(region.to_owned()))
}

async fn sdk_config(region: Option<String>) -> Result<aws_config::SdkConfig> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .retry_config(RetryConfig::standard().with_max_attempts(3))
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(5))
                .operation_timeout(Duration::from_secs(120))
                .build(),
        );
    if let Some(region) = region {
        loader = loader.region(Region::new(region));
    }
    let config = loader.load().await;
    let actual = config
        .region()
        .map(|region| Value::String(region.to_string()));
    ensure!(
        region_check(actual.as_ref()),
        "Select an AWS region in the profile or workload configuration."
    );
    Ok(config)
}

fn region_check(value: Option<&Value>) -> bool {
    value.is_some() && region(value).is_ok()
}

fn safe_key(key: &str) -> bool {
    key.len() <= 1024
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key.chars().any(char::is_control)
        && (key.is_empty()
            || key
                .trim_end_matches('/')
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")))
        && !key.ends_with("//")
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version != "null"
        && version.len() <= 1024
        && !version.chars().any(char::is_control)
}

#[derive(Clone)]
struct S3Location {
    bucket: String,
    path: String,
    region: Option<String>,
    client: Arc<OnceCell<Client>>,
}

impl S3Location {
    fn parse(value: &Value, field: &str) -> Result<Self> {
        let config = value
            .as_object()
            .ok_or_else(|| anyhow!("Invalid S3 settings."))?;
        let bucket = config
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = config
            .get(field)
            .map_or(Some(""), Value::as_str)
            .unwrap_or("/");
        let alphanumeric = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        ensure!(
            config
                .keys()
                .all(|key| ["kind", "bucket", field, "region"].contains(&key.as_str()))
                && value["kind"] == "s3"
                && (3..=63).contains(&bucket.len())
                && bucket.as_bytes().first().is_some_and(alphanumeric)
                && bucket.as_bytes().last().is_some_and(alphanumeric)
                && bucket
                    .bytes()
                    .all(|byte| alphanumeric(&byte) || matches!(byte, b'-' | b'.'))
                && !bucket.contains("..")
                && bucket.parse::<std::net::Ipv4Addr>().is_err()
                && safe_key(path)
                && if field == "key" {
                    !path.is_empty() && !path.ends_with('/')
                } else {
                    path.is_empty() || path.ends_with('/')
                },
            "Configure an S3 bucket and a relative object key or folder prefix."
        );
        Ok(Self {
            bucket: bucket.to_owned(),
            path: path.to_owned(),
            region: region(config.get("region"))?,
            client: Arc::default(),
        })
    }

    async fn client(&self) -> Result<&Client> {
        self.client
            .get_or_try_init(|| async {
                let shared = sdk_config(self.region.clone()).await?;
                let region = shared
                    .region()
                    .ok_or_else(|| anyhow!("Select an AWS region."))?
                    .as_ref();
                let suffix = if region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                ensure!(
                    !region.starts_with("us-iso"),
                    "Use a supported commercial, GovCloud, or China AWS region."
                );
                let config = aws_sdk_s3::config::Builder::from(&shared)
                    // Select the AWS regional endpoint explicitly to prevent ambient service endpoint overrides.
                    .endpoint_url(format!("https://s3.{region}.{suffix}"))
                    .force_path_style(true)
                    .build();
                Ok(Client::from_conf(config))
            })
            .await
    }

    fn uri(&self, key: &str) -> String {
        let mut uri = format!("s3://{}/", self.bucket);
        for byte in key.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
                uri.push(char::from(byte));
            } else {
                write!(uri, "%{byte:02X}").expect("writing to a string succeeds");
            }
        }
        uri
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct S3Entry {
    pub key: String,
    pub revision: String,
    pub size: u64,
    pub suffix: String,
}

pub struct S3Inventory {
    pub entries: Vec<S3Entry>,
    pub examined: usize,
}

#[derive(Clone)]
pub struct S3Source(S3Location);

impl S3Source {
    pub fn new(value: &Value) -> Result<Self> {
        Ok(Self(S3Location::parse(value, "prefix")?))
    }

    pub fn identity(&self) -> String {
        self.0.uri(&self.0.path)
    }

    pub fn uri(&self, entry: &S3Entry) -> String {
        self.0.uri(&entry.key)
    }

    pub fn inventory(&self, max_entries: usize, max_bytes: u64) -> Result<S3Inventory> {
        let location = self.0.clone();
        run(async move {
            ensure!(
                max_entries > 0 && max_bytes > 0,
                "S3 inventory requires positive limits."
            );
            let client = location.client().await?;
            let mut listing = Listing {
                prefix: &location.path,
                examined: 0,
                max_entries,
                max_bytes,
                latest: BTreeMap::new(),
            };
            let mut markers = BTreeSet::new();
            let mut marker: Option<(String, String)> = None;
            loop {
                let response = client
                    .list_object_versions()
                    .bucket(&location.bucket)
                    .prefix(&location.path)
                    .max_keys((max_entries - listing.examined).min(1000) as i32)
                    .set_key_marker(marker.as_ref().map(|pair| pair.0.clone()))
                    .set_version_id_marker(marker.as_ref().map(|pair| pair.1.clone()))
                    .send()
                    .await
                    .map_err(|_| incomplete())?;
                let before = listing.examined;
                for row in response.versions() {
                    listing.record(
                        row.key(),
                        row.is_latest(),
                        row.version_id(),
                        row.size(),
                        false,
                    )?;
                }
                for row in response.delete_markers() {
                    listing.record(row.key(), row.is_latest(), row.version_id(), None, true)?;
                }
                match response.is_truncated() {
                    Some(false) => {
                        return Ok(S3Inventory {
                            entries: listing.latest.into_values().flatten().collect(),
                            examined: listing.examined,
                        });
                    }
                    Some(true) => ensure!(
                        listing.examined < max_entries && listing.examined > before,
                        "The S3 inventory did not finish within its version-entry limit."
                    ),
                    None => return Err(incomplete()),
                }
                let key = response
                    .next_key_marker()
                    .filter(|key| key.starts_with(&location.path) && safe_key(key))
                    .ok_or_else(incomplete)?;
                let version = response
                    .next_version_id_marker()
                    .filter(|version| {
                        !version.is_empty()
                            && version.len() <= 1024
                            && !version.chars().any(char::is_control)
                    })
                    .ok_or_else(incomplete)?;
                let next = (key.to_owned(), version.to_owned());
                ensure!(
                    markers.insert(next.clone()),
                    "The S3 inventory repeated a pagination marker."
                );
                marker = Some(next);
            }
        })
    }

    pub fn read(&self, entry: &S3Entry, max_bytes: u64) -> Result<Vec<u8>> {
        ensure!(
            safe_key(&entry.key)
                && entry.key.starts_with(&self.0.path)
                && valid_version(&entry.revision)
                && entry.size <= max_bytes
                && max_bytes > 0,
            "The source entry is outside the configured S3 inventory or size limit."
        );
        let location = self.0.clone();
        let (key, version, size) = (entry.key.clone(), entry.revision.clone(), entry.size);
        run(async move {
            let client = location.client().await?;
            let object = get(client, &location.bucket, &key, Some(version), max_bytes).await?;
            ensure!(
                object.length == size,
                "The S3 object's version size changed."
            );
            object.bytes().await
        })
    }
}

fn incomplete() -> anyhow::Error {
    anyhow!("The S3 inventory was incomplete or ambiguous. No missing sources were reconciled.")
}

struct Listing<'a> {
    prefix: &'a str,
    examined: usize,
    max_entries: usize,
    max_bytes: u64,
    latest: BTreeMap<String, Option<S3Entry>>,
}

impl Listing<'_> {
    fn record(
        &mut self,
        key: Option<&str>,
        latest: Option<bool>,
        version: Option<&str>,
        size: Option<i64>,
        deleted: bool,
    ) -> Result<()> {
        self.examined += 1;
        ensure!(
            self.examined <= self.max_entries,
            "S3 inventory exceeded its version-entry limit."
        );
        let key = key
            .filter(|key| key.starts_with(self.prefix) && safe_key(key))
            .ok_or_else(incomplete)?;
        let suffix = Path::new(key)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !latest.ok_or_else(incomplete)?
            || key.ends_with('/')
            || !matches!(suffix.as_str(), "txt" | "md" | "pdf" | "docx")
        {
            return Ok(());
        }
        let version = version
            .filter(|version| valid_version(version))
            .ok_or_else(incomplete)?;
        let entry = if deleted {
            None
        } else {
            let size = size
                .and_then(|size| u64::try_from(size).ok())
                .ok_or_else(incomplete)?;
            ensure!(
                size <= self.max_bytes,
                "An S3 source exceeds the configured reader size limit."
            );
            Some(S3Entry {
                key: key.to_owned(),
                revision: version.to_owned(),
                size,
                suffix,
            })
        };
        ensure!(
            self.latest.insert(key.to_owned(), entry).is_none(),
            "The S3 inventory returned conflicting latest versions."
        );
        Ok(())
    }
}

struct Object {
    version: String,
    length: u64,
    encrypted: bool,
    checksum: Option<String>,
    body: ByteStream,
}

impl Object {
    async fn bytes(mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(part) = tokio::time::timeout(STREAM_IDLE, self.body.next())
            .await
            .map_err(|_| object_error())?
        {
            let part = part.map_err(|_| object_error())?;
            ensure!(
                part.len() as u64 <= self.length - bytes.len() as u64,
                "An S3 object exceeded its declared length."
            );
            bytes.extend_from_slice(&part);
        }
        ensure!(
            bytes.len() as u64 == self.length,
            "An S3 object ended before its declared length."
        );
        Ok(bytes)
    }
}

fn object_error() -> anyhow::Error {
    anyhow!("A bounded versioned S3 object could not be verified or read.")
}

async fn get(
    client: &Client,
    bucket: &str,
    key: &str,
    version: Option<String>,
    max_bytes: u64,
) -> Result<Object> {
    let response = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .set_version_id(version.clone())
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .map_err(|_| object_error())?;
    let actual = response
        .version_id()
        .filter(|value| {
            valid_version(value) && version.as_deref().is_none_or(|expected| expected == *value)
        })
        .ok_or_else(object_error)?;
    let length = response
        .content_length()
        .and_then(|length| u64::try_from(length).ok())
        .filter(|length| *length <= max_bytes)
        .ok_or_else(object_error)?;
    Ok(Object {
        version: actual.to_owned(),
        length,
        encrypted: response
            .server_side_encryption()
            .is_some_and(|value| value.as_str() == "AES256"),
        checksum: response.checksum_sha256().map(str::to_owned),
        body: response.body,
    })
}

#[derive(Clone)]
pub struct S3Policy(S3Location);

impl S3Policy {
    pub fn new(value: &Value) -> Result<Self> {
        Ok(Self(S3Location::parse(value, "key")?))
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        let location = self.0.clone();
        run(async move {
            let client = location.client().await?;
            let object = get(client, &location.bucket, &location.path, None, MAX_POLICY).await?;
            let version = object.version.clone();
            let current = object.bytes().await?;
            let pinned = get(
                client,
                &location.bucket,
                &location.path,
                Some(version.clone()),
                MAX_POLICY,
            )
            .await?;
            ensure!(
                current == pinned.bytes().await?,
                "The S3 policy version is inconsistent."
            );
            let latest = client
                .head_object()
                .bucket(&location.bucket)
                .key(&location.path)
                .send()
                .await
                .map_err(|_| object_error())?;
            ensure!(
                latest.version_id() == Some(version.as_str()),
                "The S3 policy changed during authorization."
            );
            Ok(current)
        })
    }
}

#[derive(Clone)]
pub struct S3Backups(S3Location);

impl S3Backups {
    pub fn new(value: &Value) -> Result<Self> {
        Ok(Self(S3Location::parse(value, "prefix")?))
    }

    fn key(&self, name: &str) -> Result<String> {
        ensure!(
            (1..=200).contains(&name.len())
                && name.as_bytes()[0].is_ascii_alphanumeric()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
            "Use a backup object name returned by this deployment."
        );
        let key = format!("{}{name}", self.0.path);
        ensure!(safe_key(&key), "The backup key is too long.");
        Ok(key)
    }

    pub fn save(&self, name: &str, snapshot: &Path) -> Result<()> {
        let key = self.key(name)?;
        let location = self.0.clone();
        let path = snapshot.to_owned();
        run(async move {
            let length = tokio::fs::metadata(&path).await?.len();
            ensure!(
                length > 0 && length <= MAX_BACKUP,
                "The backup is outside the allowed size."
            );
            let client = location.client().await?;
            let body = ByteStream::from_path(path)
                .await
                .map_err(|_| object_error())?;
            let result = client
                .put_object()
                .bucket(&location.bucket)
                .key(key)
                .body(body)
                .content_length(length as i64)
                .if_none_match("*")
                .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256)
                .send()
                .await
                .map_err(|_| anyhow!("The unique encrypted S3 backup could not be written."))?;
            ensure!(
                result.version_id().is_some_and(valid_version)
                    && result.checksum_sha256().is_some()
                    && result
                        .server_side_encryption()
                        .is_some_and(|value| value.as_str() == "AES256"),
                "A unique encrypted backup could not be confirmed in the versioned store."
            );
            Ok(())
        })
    }

    pub fn load(&self, name: &str, destination: &Path) -> Result<()> {
        let key = self.key(name)?;
        let location = self.0.clone();
        let destination = destination.to_owned();
        run(async move {
            let client = location.client().await?;
            let mut object = get(client, &location.bucket, &key, None, MAX_BACKUP).await?;
            ensure!(
                object.length > 0 && object.encrypted,
                "The S3 backup needs encryption."
            );
            let expected = object
                .checksum
                .ok_or_else(|| anyhow!("The S3 backup needs a SHA-256 checksum."))?;
            let mut options = tokio::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&destination).await?;
            let transfer = async {
                let mut remaining = object.length;
                let mut digest = Sha256::new();
                while let Some(part) = tokio::time::timeout(STREAM_IDLE, object.body.next())
                    .await
                    .map_err(|_| object_error())?
                {
                    let part = part.map_err(|_| object_error())?;
                    ensure!(
                        part.len() as u64 <= remaining,
                        "The S3 backup exceeded its declared length."
                    );
                    file.write_all(&part).await?;
                    digest.update(&part);
                    remaining -= part.len() as u64;
                }
                ensure!(
                    remaining == 0,
                    "The S3 backup ended before its declared length."
                );
                ensure!(
                    STANDARD.encode(digest.finalize()) == expected,
                    "The S3 backup checksum is invalid."
                );
                file.sync_all().await?;
                Ok(())
            }
            .await;
            drop(file);
            if transfer.is_err() {
                let _ = tokio::fs::remove_file(&destination).await;
            }
            transfer
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn s3_inventory_respects_deletion_revision_and_budget() {
        let mut listing = Listing {
            prefix: "documents/",
            examined: 0,
            max_entries: 4,
            max_bytes: 100,
            latest: BTreeMap::new(),
        };
        listing
            .record(
                Some("documents/a.md"),
                Some(false),
                Some("old"),
                Some(5),
                false,
            )
            .unwrap();
        listing
            .record(
                Some("documents/a.md"),
                Some(true),
                Some("delete"),
                None,
                true,
            )
            .unwrap();
        listing
            .record(
                Some("documents/b.PDF"),
                Some(true),
                Some("active"),
                Some(30),
                false,
            )
            .unwrap();
        assert!(listing.latest["documents/a.md"].is_none());
        assert_eq!(
            listing.latest["documents/b.PDF"].as_ref().unwrap().suffix,
            "pdf"
        );
        assert!(
            listing
                .record(
                    Some("documents/b.PDF"),
                    Some(true),
                    Some("conflict"),
                    Some(30),
                    false
                )
                .is_err()
        );
        assert!(
            listing
                .record(
                    Some("documents/c.txt"),
                    Some(true),
                    Some("active"),
                    Some(30),
                    false
                )
                .is_err()
        );
    }

    #[test]
    fn profile_and_object_keys_have_closed_boundaries() {
        assert!(
            S3Source::new(&json!({"kind":"s3","bucket":"example","prefix":"../other/"})).is_err()
        );
        assert!(
            S3Source::new(
                &json!({"kind":"s3","bucket":"example","endpoint":"https://example.com"})
            )
            .is_err()
        );
        let store =
            S3Backups::new(&json!({"kind":"s3","bucket":"example","prefix":"snapshots/"})).unwrap();
        assert_eq!(
            store.key("backup-1.json").unwrap(),
            "snapshots/backup-1.json"
        );
        assert!(store.key("../backup-1.json").is_err());
        assert!(!valid_version("null"));
        assert_eq!(
            store.0.uri("synthetic/with # and café.md"),
            "s3://example/synthetic/with%20%23%20and%20caf%C3%A9.md"
        );
    }

    #[test]
    fn versioned_connector_reuses_evidence_store_and_retires_a_delete_marker() {
        use tiny_http::{Header, Response, Server};

        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let source = S3Source::new(&json!({
            "kind":"s3","bucket":"fixture-bucket","prefix":"documents/",
        }))
        .unwrap();
        let client = Client::from_conf(
            aws_sdk_s3::config::Builder::new()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "synthetic-access",
                    "synthetic-secret",
                    None,
                    None,
                    "fixture",
                ))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .build(),
        );
        source.0.client.set(client).unwrap();

        const BODY: &str = "Cedar pump uses a yellow relay.";
        let version = format!(
            "<Version><Key>documents/note.md</Key><VersionId>version-1</VersionId>
             <IsLatest>true</IsLatest><Size>{}</Size></Version>",
            BODY.len()
        );
        let deleted = "<DeleteMarker><Key>documents/note.md</Key>
            <VersionId>delete-1</VersionId><IsLatest>true</IsLatest></DeleteMarker>";
        let listing = |record: &str| {
            format!(
                "<ListVersionsResult><IsTruncated>false</IsTruncated>{record}</ListVersionsResult>"
            )
        };
        let active = listing(&version);
        let retired = listing(deleted);
        let incomplete = "<ListVersionsResult><IsTruncated>true</IsTruncated></ListVersionsResult>";
        let worker = std::thread::spawn(move || {
            for (response, is_object) in [
                (active.as_str(), false),
                (BODY, true),
                (active.as_str(), false),
                (incomplete, false),
                (retired.as_str(), false),
                (retired.as_str(), false),
            ] {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("the signed request should reach the fixture");
                assert!(request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str().starts_with("AWS4-HMAC-SHA256 ")
                }));
                let reply = Response::from_string(response);
                if is_object {
                    assert!(
                        request
                            .url()
                            .starts_with("/fixture-bucket/documents/note.md?")
                    );
                    assert!(request.url().contains("versionId=version-1"));
                    request
                        .respond(reply.with_header(
                            Header::from_bytes("x-amz-version-id", "version-1").unwrap(),
                        ))
                        .unwrap();
                } else {
                    assert!(request.url().contains("versions"));
                    request.respond(reply).unwrap();
                }
            }
        });
        let state = tempfile::tempdir().unwrap();
        let mut db = crate::store::open(state.path(), true).unwrap();
        let report = crate::store::ingest_s3(&mut db, &source, false).unwrap();
        assert_eq!(report["updated_sources"], 1);
        let retrieve = |db: &mut rusqlite::Connection| {
            crate::store::query(
                db,
                "yellow relay",
                2,
                false,
                crate::store::Mode::Lexical,
                None,
            )
            .unwrap()["evidence"]
                .clone()
        };
        let evidence = retrieve(&mut db);
        assert_eq!(evidence[0]["path"], "s3://fixture-bucket/documents/note.md");
        assert_eq!(evidence[0]["revision"], "version-1");
        assert!(crate::store::ingest_s3(&mut db, &source, false).is_err());
        assert_eq!(retrieve(&mut db), evidence);
        let report = crate::store::ingest_s3(&mut db, &source, false).unwrap();
        assert_eq!(report["removed_sources"], 1);
        assert_eq!(retrieve(&mut db), json!([]));
        worker.join().unwrap();
    }
}
