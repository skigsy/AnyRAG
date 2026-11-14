use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHARACTERS: usize = 2_000_000;
const CHUNK_SIZE: usize = 1_200;
const CHUNK_OVERLAP: usize = 150;
pub const MAX_ENTRIES: usize = 10_000;
pub const MAX_BATCH_BYTES: usize = 128 * 1024 * 1024;

pub(crate) fn link_or_unavailable(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
        const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        const BLOCKED: u32 = FILE_ATTRIBUTE_REPARSE_POINT
            | FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        metadata.file_attributes() & BLOCKED != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

fn checked_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "Use a path without '..' components."
    );
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        use std::path::Prefix;
        ensure!(
            matches!(path.components().next(), Some(Component::Prefix(p))
                if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
            "Choose a local drive path."
        );
    }
    let ancestors: Vec<_> = path.ancestors().collect();
    for ancestor in ancestors.into_iter().rev() {
        // A Windows drive prefix without its root is not a filesystem entry.
        if ancestor
            .components()
            .all(|part| matches!(part, Component::Prefix(_)))
        {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(info) => ensure!(
                !link_or_unavailable(&info),
                "Choose a local path without links, junctions or offline entries."
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

pub fn display_path(path: &Path) -> Result<&str> {
    let name = path
        .to_str()
        .context("Use a path representable as Unicode.")?;
    #[cfg(windows)]
    let name = name.strip_prefix(r"\\?\").unwrap_or(name);
    Ok(name)
}

pub fn directory(path: &Path) -> Result<PathBuf> {
    let path = checked_path(path)?;
    ensure!(path.is_dir(), "Choose an existing folder.");
    Ok(path.canonicalize()?)
}

pub fn default_state() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    Ok(base
        .context("Choose a private local folder with --state-dir.")?
        .join("RustRAG"))
}

fn sync_folder(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "onedrive" | "dropbox" | "googledrive" | "google drive" | "iclouddrive"
    ) || name.starts_with("onedrive -")
}

fn private_state_path(path: &Path) -> Result<PathBuf> {
    let path = checked_path(path)?;
    let existing = path
        .ancestors()
        .find_map(|ancestor| ancestor.canonicalize().ok());
    // Inspect the resolved ancestor before creating state. A Windows 8.3 alias
    // can otherwise hide the name of a sync directory.
    let mut ancestors = path
        .ancestors()
        .chain(existing.iter().flat_map(|ancestor| ancestor.ancestors()));
    ensure!(
        !ancestors.any(|ancestor| ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(sync_folder)),
        "Keep the index out of cloud sync folders."
    );
    for ancestor in path.ancestors() {
        ensure!(
            !ancestor.join(".git").exists() && !ancestor.join("public-files.json").is_file(),
            "Keep the index out of source checkouts."
        );
    }
    for variable in [
        "OneDrive",
        "OneDriveConsumer",
        "OneDriveCommercial",
        "DROPBOX",
    ] {
        if let Some(root) = env::var_os(variable).and_then(|root| fs::canonicalize(root).ok()) {
            ensure!(
                !existing
                    .as_ref()
                    .is_some_and(|ancestor| ancestor.starts_with(&root)),
                "Keep the index out of cloud sync folders."
            );
        }
    }
    Ok(path)
}

pub fn state_directory(path: &Path, create: bool) -> Result<PathBuf> {
    let path = private_state_path(path)?;
    if create && !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
    }
    validate_state_directory(&path)
}

#[cfg(feature = "dashboard")]
pub(crate) fn state_directory_if_present(path: &Path) -> Result<Option<PathBuf>> {
    let path = private_state_path(path)?;
    match path.symlink_metadata() {
        Ok(_) => validate_state_directory(&path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_state_directory(path: &Path) -> Result<PathBuf> {
    let path = directory(path).context("Run init with a private, unsynced state folder first.")?;
    ensure!(
        !path.join("config.json").exists() && !path.join("index.sqlite3").exists(),
        "Use a separate state folder from the Python edition."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            path.metadata()?.permissions().mode() & 0o077 == 0,
            "The state folder must be private to your account."
        );
    }
    for name in [
        "personal.sqlite3",
        "personal.sqlite3-journal",
        "personal.sqlite3-wal",
        "personal.sqlite3-shm",
    ] {
        let entry = path.join(name);
        checked_path(&entry)?;
        // SQLite can retire a sidecar while another connection opens the index.
        // Check presence and file type in the same metadata observation.
        match entry.symlink_metadata() {
            Ok(info) => ensure!(
                info.is_file() && !link_or_unavailable(&info),
                "A state entry is not a regular local file."
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub struct Inventory {
    pub files: Vec<PathBuf>,
    pub entries: usize,
    pub skipped_links: usize,
    pub skipped: Vec<(PathBuf, &'static str)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub bytes: u64,
    modified: SystemTime,
    #[cfg(windows)]
    created: u64,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileStamp {
    fn from_metadata(info: &fs::Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt;

        Ok(Self {
            bytes: info.len(),
            modified: info.modified()?,
            #[cfg(windows)]
            created: info.creation_time(),
            #[cfg(unix)]
            identity: (info.dev(), info.ino()),
            #[cfg(unix)]
            changed: (info.ctime(), info.ctime_nsec()),
        })
    }

    // This hint prioritizes content work. It is never used to declare that
    // bytes are unchanged or to authorize removal.
    pub(crate) fn hint(&self) -> String {
        format!("{self:?}")
    }
}

pub(crate) fn file_stamp(path: &Path) -> Result<FileStamp> {
    let path = checked_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !link_or_unavailable(&metadata),
        "The source is not a regular local file."
    );
    Ok(FileStamp::from_metadata(&metadata)?)
}

pub(crate) fn folder_stamp(path: &Path) -> Result<FileStamp> {
    let path = checked_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !link_or_unavailable(&metadata),
        "The source folder changed."
    );
    Ok(FileStamp::from_metadata(&metadata)?)
}

fn supported(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("txt")
            || ext.eq_ignore_ascii_case("md")
            || ext.eq_ignore_ascii_case("pdf")
            || ext.eq_ignore_ascii_case("docx")
            || ext.eq_ignore_ascii_case("xlsx")
    })
}

pub fn inventory(root: &Path) -> Result<Inventory> {
    inventory_with_review(root, false)
}

pub fn inventory_with_review(root: &Path, review: bool) -> Result<Inventory> {
    let mut inventory = Inventory {
        files: Vec::new(),
        entries: 0,
        skipped_links: 0,
        skipped: Vec::new(),
    };
    let mut pending = vec![root.to_owned()];
    let mut directories = Vec::new();
    while let Some(folder) = pending.pop() {
        let stamp = folder_stamp(&folder)?;
        for entry in fs::read_dir(checked_path(&folder)?)? {
            inventory.entries += 1;
            ensure!(
                inventory.entries <= MAX_ENTRIES,
                "The folder exceeds the {MAX_ENTRIES}-entry scan limit."
            );
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if matches!(name.as_str(), ".obsidian" | ".git" | ".trash") {
                continue;
            }
            let path = entry.path();
            // On Windows, enumeration can report recall-on-open before touching the entry.
            let info = entry.metadata()?;
            if link_or_unavailable(&info) {
                inventory.skipped_links += 1;
                if review {
                    inventory.skipped.push((
                        path,
                        "Links, junctions, and offline entries are skipped. Use a regular local file.",
                    ));
                }
            } else if info.is_dir() {
                pending.push(path);
            } else if info.is_file() && supported(&path) {
                display_path(&path)?;
                inventory.files.push(path);
            } else if review && info.is_file() {
                inventory.skipped.push((
                    path,
                    "Unsupported file format. Convert the source to Markdown or text before ingesting it.",
                ));
            }
        }
        directories.push((folder, stamp));
    }
    for (folder, stamp) in directories {
        ensure!(
            folder_stamp(&folder)? == stamp,
            "The folder inventory changed. Run ingest again."
        );
    }
    inventory.files.sort_unstable();
    Ok(inventory)
}

pub(crate) fn still_absent(root: &Path, indexed_path: &str) -> Result<bool> {
    let relative = Path::new(indexed_path)
        .strip_prefix(Path::new(display_path(root)?))
        .context("The indexed path is outside the selected folder.")?;
    ensure!(
        !relative.as_os_str().is_empty(),
        "The indexed path is not a source file."
    );
    let path = checked_path(&root.join(relative))?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub fn read(path: &Path, remaining: &mut usize) -> Result<Vec<u8>> {
    read_checked(path, remaining, None)
}

pub(crate) fn read_revision(
    path: &Path,
    remaining: &mut usize,
    expected: &FileStamp,
) -> Result<Vec<u8>> {
    read_checked(path, remaining, Some(expected))
}

fn read_checked(
    path: &Path,
    remaining: &mut usize,
    expected: Option<&FileStamp>,
) -> Result<Vec<u8>> {
    let path = checked_path(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && !link_or_unavailable(&before),
        "The source is not a regular local file."
    );
    ensure!(
        before.len() <= MAX_BYTES as u64,
        "The file exceeds the 16 MiB limit."
    );
    let cap = MAX_BYTES.min(*remaining);
    if let Some(expected) = expected {
        ensure!(
            &FileStamp::from_metadata(&before)? == expected,
            "The source changed before it was read. Run ingest again."
        );
        ensure!(
            *remaining > expected.bytes as usize,
            "The scan exceeds the byte limit."
        );
    }
    let read_limit = expected.map_or(cap as u64 + 1, |stamp| stamp.bytes + 1);
    let mut bytes = Vec::new();
    let read = file.by_ref().take(read_limit).read_to_end(&mut bytes);
    *remaining = remaining.saturating_sub(bytes.len());
    read?;
    let after = file.metadata()?;
    checked_path(&path)?;
    ensure!(
        bytes.len() <= cap,
        "The file or scan exceeds the byte limit."
    );
    ensure!(
        !link_or_unavailable(&after)
            && after.len() == before.len()
            && after.modified()? == before.modified()?
            && bytes.len() as u64 == after.len(),
        "The source changed while it was being read. Run ingest again."
    );
    if let Some(expected) = expected {
        ensure!(
            &FileStamp::from_metadata(&after)? == expected && &file_stamp(&path)? == expected,
            "The source changed while it was being read. Run ingest again."
        );
    }
    Ok(bytes)
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

pub fn is_xlsx(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xlsx"))
}

pub fn is_docx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("docx"))
}

pub fn rollback_copies(state: &Path) -> Result<Vec<PathBuf>> {
    let mut copies = Vec::new();
    for (index, entry) in fs::read_dir(state)?.enumerate() {
        ensure!(
            index < MAX_ENTRIES,
            "The rollback copy inventory is incomplete."
        );
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| {
            name.strip_prefix("personal-v1-")
                .or_else(|| name.strip_prefix("personal-v2-"))
                .and_then(|rest| {
                    rest.strip_suffix(".sqlite3.backup")
                        .or_else(|| rest.strip_suffix(".sqlite3.partial"))
                })
                .is_some_and(|time| {
                    !time.is_empty() && time.bytes().all(|byte| byte.is_ascii_digit())
                })
        }) {
            copies.push(entry.path());
        }
    }
    Ok(copies)
}

pub fn rollback_copy_count(state: &Path) -> Result<usize> {
    Ok(rollback_copies(state)?.len())
}

pub fn decode(raw: &[u8]) -> Result<String> {
    let text = match raw {
        [0xff, 0xfe, rest @ ..] | [0xfe, 0xff, rest @ ..] => {
            ensure!(rest.len() % 2 == 0, "Invalid UTF-16 text.");
            let decode = if raw[0] == 0xff {
                u16::from_le_bytes
            } else {
                u16::from_be_bytes
            };
            let units: Vec<_> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(decode)
                .collect();
            String::from_utf16(&units).context("Invalid UTF-16 text.")?
        }
        _ => std::str::from_utf8(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw))
            .context("Save the source as UTF-8 or BOM-marked UTF-16.")?
            .to_owned(),
    };
    ensure!(!text.contains('\0'), "Text contains a NUL character.");
    ensure!(
        text.chars().count() <= MAX_CHARACTERS,
        "The text exceeds the character limit."
    );
    Ok(text)
}

fn is_space(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

pub fn chunks(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    // Index decoded Unicode code points for citation offsets and slice bytes only at character boundaries.
    let positions: Vec<_> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain([text.len()])
        .collect();
    let length = positions.len() - 1;
    let mut start = 0;
    std::iter::from_fn(move || {
        while start < length {
            let mut end = (start + CHUNK_SIZE).min(length);
            if end < length {
                let window = &text[positions[start]..positions[end]];
                if let Some(byte) = window.rfind(['\n', ' ']) {
                    let boundary = start + window[..byte].chars().count();
                    if boundary > start + CHUNK_SIZE / 2 {
                        end = boundary;
                    }
                }
            }
            let section = &text[positions[start]..positions[end]];
            let left = section.trim_start_matches(is_space);
            let quote = left.trim_end_matches(is_space);
            let offset = start + section[..section.len() - left.len()].chars().count();
            start = if end == length {
                end
            } else {
                end - CHUNK_OVERLAP
            };
            if !quote.is_empty() {
                return Some((offset, offset + quote.chars().count(), quote));
            }
        }
        None
    })
}
