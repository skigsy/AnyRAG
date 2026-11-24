use crate::files::{self, MAX_BATCH_BYTES, MAX_BYTES, MAX_ENTRIES, digest};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    env,
    io::{BufRead, Cursor, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const COMMAND_DEADLINE: Duration = Duration::from_secs(45);
const MAX_PATH_BYTES: usize = 1024;
const MAX_TREE_BYTES: usize = (MAX_PATH_BYTES + 128) * MAX_ENTRIES;
const MAX_OBJECT_FRAME: usize = 96;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    kind: String,
    root: PathBuf,
    reference: String,
    prefix: Option<String>,
}

pub(crate) struct GitSource {
    root: PathBuf,
    reference: String,
    prefix: Option<String>,
    identity: String,
}

pub(crate) struct Snapshot {
    pub commit: String,
    pub entries: Vec<Entry>,
    pub scanned: usize,
    pub skipped_links: usize,
}

pub(crate) struct Entry {
    pub path: String,
    pub object: String,
    pub size: usize,
    pub format: Format,
}

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Text,
    Pdf,
    Docx,
    Xlsx,
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
        })
}

fn object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn format(path: &str) -> Option<Format> {
    match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "md" | "markdown" | "txt" => Some(Format::Text),
        "pdf" => Some(Format::Pdf),
        "docx" => Some(Format::Docx),
        "xlsx" => Some(Format::Xlsx),
        _ => None,
    }
}

impl GitSource {
    pub fn new(value: Value) -> Result<Self> {
        let spec: Spec = serde_json::from_value(value).context("Invalid Git source profile.")?;
        ensure!(
            spec.kind == "git" && spec.root.is_absolute(),
            "Use an absolute trusted bare Git mirror path."
        );
        ensure!(
            spec.reference.len() <= 255
                && spec.reference.bytes().all(|byte| byte.is_ascii_graphic())
                && (spec.reference.starts_with("refs/heads/")
                    || spec.reference.starts_with("refs/tags/")),
            "Use a fully qualified Git branch or tag ref."
        );
        ensure!(
            spec.prefix.as_deref().is_none_or(valid_path),
            "Use a repository-relative Git source prefix."
        );
        let root = files::directory(&spec.root)?;
        let identity = format!(
            "git:{}",
            digest(
                serde_json::to_string(&(
                    files::display_path(&root)?,
                    &spec.reference,
                    &spec.prefix,
                ))?
                .as_bytes()
            )
        );
        let source = Self {
            root,
            reference: spec.reference,
            prefix: spec.prefix,
            identity,
        };
        source.check_repository()?;
        source.run(&["check-ref-format", &source.reference], &[], 0)?;
        let _ = source.head()?;
        Ok(source)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn uri(&self, path: &str) -> String {
        format!("{}/{path}", self.identity)
    }

    pub fn relative_path<'a>(&self, uri: &'a str) -> Result<&'a str> {
        uri.strip_prefix(self.identity())
            .and_then(|suffix| suffix.strip_prefix('/'))
            .filter(|path| valid_path(path))
            .context("The Git source path does not match the configured mirror.")
    }

    fn check_repository(&self) -> Result<()> {
        ensure!(
            files::directory(&self.root)? == self.root,
            "The trusted Git source root changed."
        );
        ensure!(
            !self.root.join("objects/info/alternates").exists()
                && !self.root.join("objects/info/http-alternates").exists(),
            "Use a self-contained Git mirror without object alternates."
        );
        let bare = self.run(&["rev-parse", "--is-bare-repository"], &[], 16)?;
        ensure!(
            std::str::from_utf8(&bare)?.trim() == "true",
            "Use a bare Git repository as the source."
        );
        Ok(())
    }

    pub fn head(&self) -> Result<String> {
        let commit = self.run(
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", self.reference),
            ],
            &[],
            128,
        )?;
        let commit = std::str::from_utf8(&commit)?.trim();
        ensure!(object_id(commit), "Invalid Git commit identity.");
        Ok(commit.to_owned())
    }

    pub fn require_forward(&self, previous: &str, commit: &str) -> Result<()> {
        ensure!(
            object_id(previous) && object_id(commit),
            "Invalid Git commit identity."
        );
        if previous != commit {
            self.run(&["merge-base", "--is-ancestor", previous, commit], &[], 0)
                .context("The Git ref moved backward or its last indexed commit is missing.")?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.check_repository()?;
        let commit = self.head()?;
        let mut args = vec!["ls-tree", "-r", "-t", "-l", "-z", "--full-tree", &commit];
        if let Some(prefix) = &self.prefix {
            args.extend(["--", prefix]);
        }
        let listing = self.run(&args, &[], MAX_TREE_BYTES)?;
        ensure!(
            listing.is_empty() || listing.ends_with(b"\0"),
            "Incomplete Git inventory."
        );
        let mut entries = Vec::new();
        let mut skipped_links = 0;
        let mut scanned = 0;
        for row in listing
            .split(|byte| *byte == 0)
            .filter(|row| !row.is_empty())
        {
            scanned += 1;
            ensure!(
                scanned <= MAX_ENTRIES,
                "The Git tree exceeds the inventory limit."
            );
            let (metadata, path) = std::str::from_utf8(row)?
                .split_once('\t')
                .context("Incomplete Git inventory row.")?;
            ensure!(
                valid_path(path),
                "The Git tree contains an unsupported path."
            );
            let mut fields = metadata.split_ascii_whitespace();
            let mode = fields.next().context("Missing Git tree mode.")?;
            let kind = fields.next().context("Missing Git tree object type.")?;
            let object = fields.next().context("Missing Git tree object ID.")?;
            let size = fields.next().context("Missing Git tree object size.")?;
            ensure!(
                object_id(object) && fields.next().is_none(),
                "Malformed Git inventory row."
            );
            match (mode, kind) {
                ("040000", "tree") => continue,
                ("120000", "blob") | ("160000", "commit") => {
                    skipped_links += 1;
                    continue;
                }
                ("100644" | "100755", "blob") => {}
                _ => anyhow::bail!("The Git tree contains an unsupported object mode."),
            }
            if let Some(format) = format(path) {
                entries.push(Entry {
                    path: path.to_owned(),
                    object: object.to_owned(),
                    size: size.parse().context("Invalid Git blob size.")?,
                    format,
                });
            }
        }
        entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        ensure!(
            entries.windows(2).all(|pair| pair[0].path != pair[1].path),
            "Duplicate Git source path."
        );
        Ok(Snapshot {
            commit,
            entries,
            scanned,
            skipped_links,
        })
    }

    pub fn read_changed(&self, entries: &[&Entry]) -> Result<Vec<u8>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = String::new();
        let mut bytes = 0;
        for entry in entries {
            ensure!(
                entry.size <= MAX_BYTES,
                "An indexed Git blob exceeds the file size limit."
            );
            bytes += entry.size;
            ensure!(
                bytes <= MAX_BATCH_BYTES,
                "The changed Git data exceeds the sync byte limit."
            );
            input.push_str(&entry.object);
            input.push('\n');
        }
        self.run(
            &["cat-file", "--batch"],
            input.as_bytes(),
            bytes + entries.len() * MAX_OBJECT_FRAME,
        )
    }

    fn run(&self, args: &[&str], input: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        command.env_clear();
        for name in ["PATH", "SYSTEMROOT"] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        let trusted_path = format!("safe.directory={}", files::display_path(&self.root)?);
        let mut child = command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .args([
                "--no-replace-objects",
                "--no-optional-locks",
                "--literal-pathspecs",
            ])
            .args([
                "-c",
                "protocol.allow=never",
                "-c",
                &trusted_path,
                "--git-dir",
            ])
            .arg(files::display_path(&self.root)?)
            .args(args)
            .stdin(if input.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not start trusted Git.")?;
        let output = child.stdout.take().context("Git output was unavailable.")?;
        let pipe = child.stdin.take();
        thread::scope(|scope| -> Result<Vec<u8>> {
            let reader = scope.spawn(move || -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                output
                    .take((max_bytes + 1) as u64)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            });
            let writer = pipe.map(|mut pipe| scope.spawn(move || pipe.write_all(input)));
            let deadline = Instant::now() + COMMAND_DEADLINE;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    result => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break match result {
                            Err(error) => Err(error).context("Could not check Git process status."),
                            _ => Err(anyhow::anyhow!("Git source read timed out.")),
                        };
                    }
                }
            }?;
            let bytes = reader
                .join()
                .map_err(|_| anyhow::anyhow!("Git output failed."))??;
            if let Some(writer) = writer {
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Git input failed."))??;
            }
            ensure!(
                bytes.len() <= max_bytes,
                "Git source output exceeded its bounded size."
            );
            ensure!(
                status.success(),
                "Git could not read the complete requested data."
            );
            Ok(bytes)
        })
    }
}

pub(crate) fn next_blob<'a>(stream: &mut Cursor<&'a [u8]>, entry: &Entry) -> Result<&'a [u8]> {
    let mut header = Vec::new();
    let read = (&mut *stream)
        .take(MAX_OBJECT_FRAME as u64)
        .read_until(b'\n', &mut header)?;
    ensure!(
        read > 0 && header.ends_with(b"\n"),
        "Incomplete Git blob header."
    );
    let expected = format!("{} blob {}\n", entry.object, entry.size);
    ensure!(
        header == expected.as_bytes(),
        "Git blob identity or size changed."
    );
    let position = stream.position() as usize;
    let end = position + entry.size;
    let bytes = *stream.get_ref();
    ensure!(
        bytes.get(end) == Some(&b'\n'),
        "Incomplete Git blob payload."
    );
    stream.set_position((end + 1) as u64);
    let blob = &bytes[position..end];
    ensure!(
        !blob.starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
        "The Git file is an LFS pointer. Import its resolved contents through an approved source."
    );
    Ok(blob)
}
