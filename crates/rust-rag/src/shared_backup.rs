use super::auth::Principal;
use super::{
    ApiError, ApiResult, MAX_POLICY, Service, bounded_file, hex_id, new_private, strict_json,
    unavailable,
};
use crate::store;
use anyhow::{Result, ensure};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_SNAPSHOT: u64 = 2 * 1024 * 1024 * 1024;

pub(super) enum BackupStore {
    File(PathBuf),
    #[cfg(feature = "aws")]
    S3(crate::cloud::S3Backups),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    binding: String,
    backup_id: String,
    collections: BTreeMap<String, String>,
}

fn backup_id() -> Result<String> {
    let mut random = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("Could not generate a backup identifier."))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn file_hash(path: &Path) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        ensure!(
            fs::symlink_metadata(path)?.file_attributes() & 0x0044_1400 == 0,
            "Unsafe snapshot file."
        );
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SNAPSHOT,
        "Invalid snapshot size."
    );
    let mut hasher = Sha256::new();
    let bytes = std::io::copy(&mut file.take(MAX_SNAPSHOT + 1), &mut hasher)?;
    ensure!(bytes == metadata.len(), "Snapshot size changed.");
    Ok(format!("{:x}", hasher.finalize()))
}

fn snapshot(source: &Connection, destination: &mut Connection) -> Result<()> {
    let backup = Backup::new(source, destination)?;
    ensure!(
        matches!(backup.step(-1)?, StepResult::Done),
        "The snapshot database was busy."
    );
    Ok(())
}

fn validate_snapshot(path: &Path, expected: &str, live: &Connection) -> Result<Connection> {
    ensure!(
        hex_id(expected, 64) && file_hash(path)? == expected,
        "Snapshot checksum mismatch."
    );
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY; PRAGMA query_only=ON;")?;
    for pragma in ["application_id", "user_version"] {
        let actual: i64 = db.pragma_query_value(None, pragma, |row| row.get(0))?;
        let expected: i64 = live.pragma_query_value(None, pragma, |row| row.get(0))?;
        ensure!(actual == expected, "Incompatible backup database.");
    }
    ensure!(
        store::ocr_fingerprint(&db)? == store::ocr_fingerprint(live)?,
        "Incompatible backup OCR binding."
    );
    let quick: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(quick == "ok", "Snapshot integrity check failed.");
    Ok(db)
}

fn copy_snapshot(from: &Path, to: &Path) -> Result<()> {
    let mut source = File::open(from)?;
    let size = source.metadata()?.len();
    ensure!(size <= MAX_SNAPSHOT, "Snapshot exceeds the backup limit.");
    let mut destination = new_private(to)?;
    ensure!(
        std::io::copy(&mut (&mut source).take(MAX_SNAPSHOT + 1), &mut destination)? == size,
        "Snapshot changed while copying."
    );
    destination.sync_all()?;
    Ok(())
}

impl BackupStore {
    fn save(
        &self,
        stage: &Path,
        manifest: &Manifest,
        before_publish: impl Fn() -> ApiResult<()>,
    ) -> ApiResult<()> {
        match self {
            Self::File(root) => {
                if crate::files::directory(root).map_err(unavailable)? != *root {
                    return Err(unavailable("backup root changed"));
                }
                let stage_copy = tempfile::Builder::new()
                    .prefix("snapshot-")
                    .tempdir_in(root)
                    .map_err(unavailable)?;
                for name in manifest.collections.keys() {
                    copy_snapshot(
                        &stage.join(format!("{name}.sqlite3")),
                        &stage_copy.path().join(format!("{name}.sqlite3")),
                    )
                    .map_err(unavailable)?;
                }
                copy_snapshot(
                    &stage.join("manifest.json"),
                    &stage_copy.path().join("manifest.json"),
                )
                .map_err(unavailable)?;
                before_publish()?;
                let destination = root.join(&manifest.backup_id);
                if destination.exists() {
                    return Err(unavailable("duplicate snapshot"));
                }
                #[cfg(unix)]
                File::open(stage_copy.path())
                    .and_then(|dir| dir.sync_all())
                    .map_err(unavailable)?;
                fs::rename(stage_copy.path(), destination).map_err(unavailable)?;
                #[cfg(unix)]
                File::open(root)
                    .and_then(|dir| dir.sync_all())
                    .map_err(unavailable)?;
            }
            #[cfg(feature = "aws")]
            Self::S3(store) => {
                for name in manifest.collections.keys() {
                    before_publish()?;
                    store
                        .save(
                            &format!("{}-{name}.sqlite3", manifest.backup_id),
                            &stage.join(format!("{name}.sqlite3")),
                        )
                        .map_err(unavailable)?;
                }
                before_publish()?;
                store
                    .save(
                        &format!("{}.json", manifest.backup_id),
                        &stage.join("manifest.json"),
                    )
                    .map_err(unavailable)?;
            }
        }
        Ok(())
    }

    fn load_manifest(&self, id: &str, stage: &Path) -> Result<Manifest> {
        let path = match self {
            Self::File(root) => root.join(id).join("manifest.json"),
            #[cfg(feature = "aws")]
            Self::S3(store) => {
                let path = stage.join("manifest.json");
                store.load(&format!("{id}.json"), &path)?;
                path
            }
        };
        let _ = stage;
        Ok(serde_json::from_value(strict_json(&bounded_file(
            &path, MAX_POLICY,
        )?)?)?)
    }

    fn load_file(&self, id: &str, collection: &str, stage: &Path) -> Result<PathBuf> {
        let path = stage.join(format!("{collection}.sqlite3"));
        match self {
            Self::File(root) => {
                let parent = crate::files::directory(&root.join(id))?;
                ensure!(
                    parent.starts_with(root),
                    "Snapshot directory escaped its configured root."
                );
                let source = parent.join(format!("{collection}.sqlite3"));
                // Verify source type before copying and verify the copied bytes
                // against the manifest before opening it with SQLite.
                file_hash(&source)?;
                copy_snapshot(&source, &path)?;
            }
            #[cfg(feature = "aws")]
            Self::S3(store) => store.load(&format!("{id}-{collection}.sqlite3"), &path)?,
        }
        Ok(path)
    }
}

impl Service {
    pub(super) fn backup(&self, principal: &Principal) -> ApiResult<serde_json::Value> {
        let _write = self.try_write()?;
        let (policy, digest) = self.policy()?;
        policy.admin(principal)?;
        self.ready()?;
        let store = self.backups.as_ref().ok_or(ApiError::new(
            400,
            "BACKUP_UNAVAILABLE",
            "Configure a private backup store first.",
        ))?;
        let id = backup_id().map_err(unavailable)?;
        let stage = tempfile::Builder::new()
            .prefix("backup-")
            .tempdir_in(&self.state)
            .map_err(unavailable)?;
        let mut manifest = Manifest {
            schema_version: 1,
            binding: self.binding.clone(),
            backup_id: id.clone(),
            collections: BTreeMap::new(),
        };
        for collection in self.sources.keys() {
            self.confirm_admin(principal, &digest)?;
            let live = self
                .open_collection(collection, false)
                .map_err(unavailable)?;
            let path = stage.path().join(format!("{collection}.sqlite3"));
            new_private(&path).map_err(unavailable)?;
            let mut target = Connection::open(&path).map_err(unavailable)?;
            target
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .map_err(unavailable)?;
            snapshot(&live, &mut target).map_err(unavailable)?;
            drop(target);
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.sync_all())
                .map_err(unavailable)?;
            manifest
                .collections
                .insert(collection.clone(), file_hash(&path).map_err(unavailable)?);
        }
        let raw = serde_json::to_vec(&manifest).map_err(unavailable)?;
        let mut file = new_private(&stage.path().join("manifest.json")).map_err(unavailable)?;
        file.write_all(&raw)
            .and_then(|_| file.sync_all())
            .map_err(unavailable)?;
        drop(file);
        store.save(stage.path(), &manifest, || {
            self.confirm_admin(principal, &digest).map(|_| ())
        })?;
        Ok(serde_json::json!({"schema_version":1,"backup_id":id,"status":"saved"}))
    }

    pub(super) fn restore(&self, principal: &Principal, id: &str) -> ApiResult<serde_json::Value> {
        let _write = self.try_write()?;
        let (policy, digest) = self.policy()?;
        policy.admin(principal)?;
        let store = self
            .backups
            .as_ref()
            .filter(|_| hex_id(id, 32))
            .ok_or(ApiError::new(
                400,
                "BACKUP_ID",
                "Supply an identifier from the configured private backup store.",
            ))?;
        let invalid = |_| {
            ApiError::new(
                503,
                "BACKUP_INVALID",
                "The backup could not be verified for this deployment.",
            )
        };
        let stage = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(&self.state)
            .map_err(unavailable)?;
        let manifest = store.load_manifest(id, stage.path()).map_err(invalid)?;
        if manifest.schema_version != 1
            || manifest.backup_id != id
            || manifest.binding != self.binding
            || manifest.collections.keys().ne(self.sources.keys())
        {
            return Err(invalid(anyhow::anyhow!("Snapshot binding mismatch.")));
        }
        let mut copies = Vec::new();
        for (collection, expected) in &manifest.collections {
            self.confirm_admin(principal, &digest)?;
            let path = store
                .load_file(id, collection, stage.path())
                .map_err(invalid)?;
            let live = self
                .open_collection(collection, false)
                .map_err(unavailable)?;
            let checked = validate_snapshot(&path, expected, &live).map_err(invalid)?;
            copies.push((checked, live));
        }
        self.confirm_admin(principal, &digest)?;
        // A durable incomplete marker precedes the first write. Interruption
        // after any individual index copy leaves serving and reopen locked.
        self.set_phase(true, None, true)?;
        for (saved, mut live) in copies {
            self.confirm_admin(principal, &digest)?;
            snapshot(&saved, &mut live).map_err(unavailable)?;
            // Rehash the current local files before reopening. A progress
            // marker copied from the snapshot cannot prove current content.
            crate::store::reset_local_ingest_progress(&live).map_err(unavailable)?;
        }
        self.confirm_admin(principal, &digest)?;
        self.set_phase(true, None, false)?;
        Ok(serde_json::json!({"schema_version":1,"status":"restored","maintenance":true}))
    }
}
