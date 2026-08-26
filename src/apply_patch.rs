use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rand::RngCore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArgs {
    #[schemars(description = "Mu/Codex-style patch envelope to apply")]
    pub patch: String,
    #[schemars(description = "Base directory for relative patch paths")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ApplyPatchOutput {
    pub output: String,
}

pub fn apply_patch(args: ApplyPatchArgs) -> Result<ApplyPatchOutput> {
    let cwd = match args.cwd {
        Some(cwd) => {
            if !cwd.is_dir() {
                bail!("working directory does not exist: {}", cwd.display());
            }
            cwd
        }
        None => std::env::current_dir().context("determining current directory")?,
    };
    let operations = parse_patch(&args.patch)?;
    let changes = preflight(&cwd, operations)?;
    commit(&changes)?;
    Ok(ApplyPatchOutput {
        output: format_summary(&changes, &cwd),
    })
}

#[derive(Debug)]
enum Operation {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Debug)]
struct Chunk {
    locator: Option<String>,
    lines: Vec<HunkLine>,
    end_of_file: bool,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug)]
enum PlannedChange {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
        regular: bool,
    },
    Update {
        path: PathBuf,
        reported_path: PathBuf,
        original: Vec<u8>,
        content: String,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
        original: Vec<u8>,
        content: String,
    },
    MoveSymlink {
        from: PathBuf,
        to: PathBuf,
        target_update: Option<(PathBuf, Vec<u8>, String)>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Repeatable {
    No,
    Update,
}

fn parse_patch(patch: &str) -> Result<Vec<Operation>> {
    let normalized = patch.replace("\r\n", "\n");
    let mut lines = normalized.lines().peekable();
    if lines.next() != Some("*** Begin Patch") {
        bail!("patch must begin with `*** Begin Patch`");
    }
    let mut operations = Vec::new();
    loop {
        let Some(line) = lines.next() else {
            bail!("patch is missing `*** End Patch`");
        };
        if line == "*** End Patch" {
            if lines.next().is_some() {
                bail!("unexpected content after `*** End Patch`");
            }
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = parse_path(path)?;
            let mut content = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let next = lines.next().expect("peeked line");
                let Some(added) = next.strip_prefix('+') else {
                    bail!("add-file content lines must begin with `+`");
                };
                content.push(added.to_string());
            }
            let content = if content.is_empty() {
                String::new()
            } else {
                format!("{}\n", content.join("\n"))
            };
            operations.push(Operation::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete {
                path: parse_path(path)?,
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = parse_path(path)?;
            let move_to = lines
                .peek()
                .and_then(|line| line.strip_prefix("*** Move to: "))
                .map(parse_path)
                .transpose()?;
            if move_to.is_some() {
                lines.next();
            }
            let mut chunks = Vec::new();
            while lines.peek().is_some_and(|line| line.starts_with("@@")) {
                let header = lines.next().expect("peeked hunk header");
                let locator = header
                    .strip_prefix("@@")
                    .expect("checked prefix")
                    .trim()
                    .to_string();
                let locator = (!locator.is_empty()).then_some(locator);
                let mut hunk_lines = Vec::new();
                let mut end_of_file = false;
                while let Some(next) = lines.peek() {
                    if next.starts_with("@@") || next.starts_with("*** ") {
                        if *next == "*** End of File" {
                            lines.next();
                            end_of_file = true;
                        }
                        break;
                    }
                    let next = lines.next().expect("peeked hunk line");
                    let (prefix, text) = next.split_at_checked(1).ok_or_else(|| {
                        anyhow::anyhow!("empty hunk line must be written as a single space")
                    })?;
                    let line = match prefix {
                        " " => HunkLine::Context(text.to_string()),
                        "-" => HunkLine::Remove(text.to_string()),
                        "+" => HunkLine::Add(text.to_string()),
                        _ => bail!("hunk lines must begin with space, `-`, or `+`"),
                    };
                    hunk_lines.push(line);
                }
                if hunk_lines.is_empty() {
                    bail!("update hunk must contain at least one line");
                }
                chunks.push(Chunk {
                    locator,
                    lines: hunk_lines,
                    end_of_file,
                });
            }
            if chunks.is_empty() && move_to.is_none() {
                bail!("update-file operation needs a hunk or move destination");
            }
            operations.push(Operation::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        bail!("unrecognized patch line `{line}`");
    }
    if operations.is_empty() {
        bail!("patch contains no file operations");
    }
    Ok(operations)
}

fn parse_path(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() {
        bail!("patch path cannot be empty");
    }
    Ok(path)
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn preflight(cwd: &Path, operations: Vec<Operation>) -> Result<Vec<PlannedChange>> {
    let mut claims = HashMap::new();
    let mut changes: Vec<PlannedChange> = Vec::new();
    let mut reported_entries = Vec::new();
    let mut repeatable = Vec::new();
    for operation in operations {
        match operation {
            Operation::Add { path, content } => {
                let full = resolve_path(cwd, &path);
                if let Some(&owner) = claims.get(&normalize_path(&full)) {
                    if reported_entries[owner] != path {
                        conflicting_operation(&path)?;
                    }
                    let replaceable =
                        matches!(changes[owner], PlannedChange::Delete { regular: true, .. });
                    if !replaceable {
                        return conflicting_operation(&path);
                    }
                    let metadata = fs::symlink_metadata(&full).with_context(|| {
                        format!("cannot replace missing file {}", path.display())
                    })?;
                    if !metadata.is_file() {
                        return conflicting_operation(&path);
                    }
                    let original = fs::read(&full)
                        .with_context(|| format!("reading file to replace {}", path.display()))?;
                    changes[owner] = PlannedChange::Update {
                        path: full.clone(),
                        reported_path: full,
                        original,
                        content,
                    };
                    repeatable[owner] = Repeatable::No;
                    continue;
                }
                if fs::symlink_metadata(&full).is_ok() {
                    bail!(
                        "add destination already exists: {}; inspect it first, then use run to move or remove the existing file before retrying apply_patch",
                        path.display()
                    );
                }
                let owner = changes.len();
                claim_path(&mut claims, &full, &path, owner)?;
                changes.push(PlannedChange::Add {
                    path: full,
                    content,
                });
                reported_entries.push(path);
                repeatable.push(Repeatable::No);
            }
            Operation::Delete { path } => {
                let full = resolve_path(cwd, &path);
                if claims.contains_key(&normalize_path(&full)) {
                    conflicting_operation(&path)?;
                }
                let metadata = file_or_symlink_metadata(&full, "delete")?;
                let owner = changes.len();
                claim_path(&mut claims, &full, &path, owner)?;
                changes.push(PlannedChange::Delete {
                    path: full,
                    regular: metadata.is_file(),
                });
                reported_entries.push(path);
                repeatable.push(Repeatable::No);
            }
            Operation::Update {
                path,
                move_to,
                chunks,
            } => {
                let full = resolve_path(cwd, &path);
                if let Some(&owner) = claims.get(&normalize_path(&full)) {
                    if reported_entries[owner] != path
                        || move_to.is_some()
                        || repeatable[owner] != Repeatable::Update
                    {
                        conflicting_operation(&path)?;
                    }
                    let PlannedChange::Update { content, .. } = &mut changes[owner] else {
                        return conflicting_operation(&path);
                    };
                    *content = apply_chunks(content, &path, &chunks)?;
                    continue;
                }
                let owner = changes.len();
                claim_path(&mut claims, &full, &path, owner)?;
                let destination_full = move_to
                    .as_ref()
                    .map(|destination| resolve_path(cwd, destination));
                if let (Some(destination), Some(destination_full)) = (&move_to, &destination_full) {
                    claim_path(&mut claims, destination_full, destination, owner)?;
                    if fs::symlink_metadata(destination_full).is_ok() {
                        bail!(
                            "move destination already exists: {}; inspect it first, then use run to move or remove the existing file before retrying apply_patch",
                            destination.display()
                        );
                    }
                }

                let entry_metadata = fs::symlink_metadata(&full)
                    .with_context(|| format!("cannot update missing file {}", path.display()))?;
                if entry_metadata.file_type().is_symlink() {
                    if chunks.is_empty() {
                        let destination = destination_full
                            .expect("an update without chunks must have a move destination");
                        changes.push(PlannedChange::MoveSymlink {
                            from: full,
                            to: destination,
                            target_update: None,
                        });
                        reported_entries.push(path);
                        repeatable.push(Repeatable::No);
                        continue;
                    }
                    let target = fs::canonicalize(&full).with_context(|| {
                        format!("resolving symlink to update {}", path.display())
                    })?;
                    claim_path(&mut claims, &target, &path, owner)?;
                    regular_file_metadata(&target, "update symlink target")?;
                    let original = fs::read_to_string(&target).with_context(|| {
                        format!("reading symlink target to update {}", path.display())
                    })?;
                    let content = apply_chunks(&original, &path, &chunks)?;
                    if let Some(destination) = destination_full {
                        changes.push(PlannedChange::MoveSymlink {
                            from: full,
                            to: destination,
                            target_update: Some((target, original.into_bytes(), content)),
                        });
                    } else {
                        changes.push(PlannedChange::Update {
                            path: target,
                            reported_path: full,
                            original: original.into_bytes(),
                            content,
                        });
                    }
                    reported_entries.push(path);
                    repeatable.push(if move_to.is_some() {
                        Repeatable::No
                    } else {
                        Repeatable::Update
                    });
                    continue;
                }
                if !entry_metadata.is_file() {
                    bail!("cannot update non-regular file {}", path.display());
                }
                let original = fs::read_to_string(&full)
                    .with_context(|| format!("reading file to update {}", path.display()))?;
                let content = if chunks.is_empty() {
                    original.clone()
                } else {
                    apply_chunks(&original, &path, &chunks)?
                };
                if let Some(destination_full) = destination_full {
                    changes.push(PlannedChange::Move {
                        from: full,
                        to: destination_full,
                        original: original.into_bytes(),
                        content,
                    });
                } else {
                    changes.push(PlannedChange::Update {
                        reported_path: full.clone(),
                        path: full,
                        original: original.into_bytes(),
                        content,
                    });
                }
                reported_entries.push(path);
                repeatable.push(if move_to.is_some() {
                    Repeatable::No
                } else {
                    Repeatable::Update
                });
            }
        }
    }
    Ok(changes)
}

fn claim_path(
    claims: &mut HashMap<PathBuf, usize>,
    resolved: &Path,
    reported: &Path,
    owner: usize,
) -> Result<()> {
    let normalized = normalize_path(resolved);
    if claims.contains_key(&normalized) {
        conflicting_operation(reported)?;
    }
    claims.insert(normalized, owner);
    Ok(())
}

fn conflicting_operation<T>(reported: &Path) -> Result<T> {
    bail!("multiple operations target {}", reported.display())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if normalized.file_name() == Some(std::ffi::OsStr::new("..")) {
                    normalized.push("..");
                } else {
                    let _ = normalized.pop();
                }
            }
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    normalized
}

fn regular_file_metadata(path: &Path, action: &str) -> Result<fs::Metadata> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot {action} missing file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("cannot {action} non-regular file {}", path.display());
    }
    Ok(metadata)
}

fn file_or_symlink_metadata(path: &Path, action: &str) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot {action} missing file {}", path.display()))?;
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        bail!("cannot {action} non-file {}", path.display());
    }
    Ok(metadata)
}

fn apply_chunks(original: &str, path: &Path, chunks: &[Chunk]) -> Result<String> {
    let mut lines = original.split('\n').map(str::to_string).collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut cursor = 0usize;
    for chunk in chunks {
        if let Some(locator) = &chunk.locator {
            let index = seek_sequence(&lines, std::slice::from_ref(locator), cursor, false)
                .with_context(|| {
                    format!("failed to find context `{locator}` in {}", path.display())
                })?;
            cursor = index + 1;
        }
        let old = chunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.clone()),
                HunkLine::Add(_) => None,
            })
            .collect::<Vec<_>>();
        let new = chunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Add(text) => Some(text.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect::<Vec<_>>();
        let index = if old.is_empty() {
            if chunk.end_of_file {
                lines.len()
            } else {
                cursor
            }
        } else {
            seek_sequence(&lines, &old, cursor, chunk.end_of_file).with_context(|| {
                format!(
                    "failed to find expected lines in {}:\n{}",
                    path.display(),
                    old.join("\n")
                )
            })?
        };
        lines.splice(index..index + old.len(), new.iter().cloned());
        cursor = index + new.len();
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let preferred = if eof {
        lines.len().saturating_sub(pattern.len())
    } else {
        start.min(lines.len())
    };
    for matcher in [
        exact_lines as fn(&[String], &[String]) -> bool,
        trim_end_lines,
        trim_lines,
        normalize_lines,
    ] {
        if eof && matcher(&lines[preferred..preferred + pattern.len()], pattern) {
            return Some(preferred);
        }
        for index in start.min(lines.len())..=lines.len() - pattern.len() {
            if matcher(&lines[index..index + pattern.len()], pattern) {
                return Some(index);
            }
        }
    }
    None
}

fn exact_lines(actual: &[String], expected: &[String]) -> bool {
    actual == expected
}

fn trim_end_lines(actual: &[String], expected: &[String]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| actual.trim_end() == expected.trim_end())
}

fn trim_lines(actual: &[String], expected: &[String]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| actual.trim() == expected.trim())
}

fn normalize_lines(actual: &[String], expected: &[String]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| normalize_line(actual) == normalize_line(expected))
}

fn normalize_line(line: &str) -> String {
    line.trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn commit(changes: &[PlannedChange]) -> Result<()> {
    let mut completed = Vec::new();
    for change in changes {
        let result = match change {
            PlannedChange::Add { path, content } => atomic_write(path, content, false, None),
            PlannedChange::Delete { path, .. } => {
                fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
            }
            PlannedChange::Update {
                path,
                reported_path: _,
                original,
                content,
            } => atomic_write(path, content, true, Some(original.as_slice())),
            PlannedChange::Move {
                from,
                to,
                original,
                content,
            } => {
                fs::rename(from, to)
                    .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
                if let Err(error) = atomic_write(to, content, true, Some(original.as_slice())) {
                    return match fs::rename(to, from) {
                        Ok(()) => Err(error.context(format!(
                            "updating moved file {}; move rolled back",
                            to.display()
                        ))),
                        Err(rollback_error) => Err(error.context(format!(
                            "updating moved file {}; also failed to roll back {} to {}: {rollback_error}",
                            to.display(),
                            to.display(),
                            from.display()
                        ))),
                    };
                }
                Ok(())
            }
            PlannedChange::MoveSymlink {
                from,
                to,
                target_update,
            } => (|| -> Result<()> {
                let parent = to.parent().unwrap_or_else(|| Path::new("."));
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent directory {}", parent.display()))?;
                fs::rename(from, to).with_context(|| {
                    format!("moving symlink {} to {}", from.display(), to.display())
                })?;
                if let Some((target, original, content)) = target_update
                    && let Err(error) =
                        atomic_write(target, content, true, Some(original.as_slice()))
                {
                    return match fs::rename(to, from) {
                        Ok(()) => Err(error.context(format!(
                            "updating symlink target after moving {}; move rolled back",
                            target.display()
                        ))),
                        Err(rollback_error) => Err(error.context(format!(
                            "updating symlink target after moving {}; also failed to roll back {} to {}: {rollback_error}",
                            target.display(),
                            to.display(),
                            from.display()
                        ))),
                    };
                }
                Ok(())
            })(),
        };
        if let Err(error) = result {
            let completed = if completed.is_empty() {
                "none".to_string()
            } else {
                completed.join(", ")
            };
            return Err(error.context(format!("completed changes before failure: {completed}")));
        }
        completed.push(change_label(change));
    }
    Ok(())
}

fn atomic_write(path: &Path, content: &str, replace: bool, expected: Option<&[u8]>) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory {}", parent.display()))?;
    if replace {
        return overwrite_existing(path, content.as_bytes(), expected);
    }

    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let (mut file, temporary) =
        create_temp_file(parent, &format!(".{filename}.connector-tmp-"), ".tmp")?;
    let result = (|| -> Result<()> {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&temporary, path)
            .with_context(|| format!("creating {} without overwriting", path.display()))?;
        fs::remove_file(&temporary)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn overwrite_existing(path: &Path, content: &[u8], expected: Option<&[u8]>) -> Result<()> {
    let mut target = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} for locked update", path.display()))?;
    target
        .try_lock()
        .map_err(|error| anyhow::anyhow!("file is busy or cannot be locked: {error}"))?;
    target.seek(SeekFrom::Start(0))?;
    let mut old = Vec::new();
    target.read_to_end(&mut old)?;
    if let Some(expected) = expected
        && old != expected
    {
        bail!(
            "file changed while the edit was being prepared; re-read {} and retry",
            path.display()
        );
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let (mut backup, backup_path) =
        create_temp_file(parent, &format!(".{filename}.connector-backup-"), ".tmp")?;
    if let Err(error) = backup.write_all(&old).and_then(|()| backup.sync_all()) {
        drop(backup);
        let _ = fs::remove_file(&backup_path);
        return Err(error)
            .with_context(|| format!("writing edit backup {}", backup_path.display()));
    }
    drop(backup);

    let result = (|| -> Result<()> {
        target.set_len(0)?;
        target.seek(SeekFrom::Start(0))?;
        target.write_all(content)?;
        target.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let restore = (|| -> Result<()> {
            target.set_len(0)?;
            target.seek(SeekFrom::Start(0))?;
            target.write_all(&old)?;
            target.sync_all()?;
            Ok(())
        })();
        if restore.is_ok() {
            let _ = fs::remove_file(&backup_path);
            return Err(error.context("edit failed; the original file was restored"));
        }
        return Err(error.context(format!(
            "edit failed; recovery backup remains at {}",
            backup_path.display()
        )));
    }
    fs::remove_file(&backup_path).with_context(|| {
        format!(
            "file was updated, but transient edit backup could not be removed: {}",
            backup_path.display()
        )
    })
}

fn create_temp_file(directory: &Path, prefix: &str, suffix: &str) -> Result<(File, PathBuf)> {
    fs::create_dir_all(directory)
        .with_context(|| format!("creating temporary directory {}", directory.display()))?;
    for _ in 0..32 {
        let mut bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut bytes);
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = directory.join(format!("{prefix}{token}{suffix}"));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true).mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary file {}", path.display()));
            }
        }
    }
    bail!(
        "could not choose a unique temporary filename in {}",
        directory.display()
    )
}

fn format_summary(changes: &[PlannedChange], cwd: &Path) -> String {
    let mut summary = String::from("Done!\n");
    for change in changes {
        summary.push_str(&change_label_relative(change, cwd));
        summary.push('\n');
    }
    summary
}

fn change_label_relative(change: &PlannedChange, cwd: &Path) -> String {
    let relative = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
    match change {
        PlannedChange::Add { path, .. } => format!("A {}", relative(path)),
        PlannedChange::Delete { path, .. } => format!("D {}", relative(path)),
        PlannedChange::Update { reported_path, .. } => format!("M {}", relative(reported_path)),
        PlannedChange::Move { from, to, .. } | PlannedChange::MoveSymlink { from, to, .. } => {
            format!("R {} -> {}", relative(from), relative(to))
        }
    }
}

fn change_label(change: &PlannedChange) -> String {
    match change {
        PlannedChange::Add { path, .. } => format!("A {}", path.display()),
        PlannedChange::Delete { path, .. } => format!("D {}", path.display()),
        PlannedChange::Update { reported_path, .. } => format!("M {}", reported_path.display()),
        PlannedChange::Move { from, to, .. } | PlannedChange::MoveSymlink { from, to, .. } => {
            format!("R {} -> {}", from.display(), to.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_add_update_move_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("old.txt"), "one\ntwo\n").unwrap();
        fs::write(dir.path().join("delete.txt"), "bye\n").unwrap();
        let output = apply_patch(ApplyPatchArgs {
            patch: "*** Begin Patch\n*** Add File: added.txt\n+hello\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n one\n-two\n+three\n*** Delete File: delete.txt\n*** End Patch\n".into(),
            cwd: Some(dir.path().into()),
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("added.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("moved.txt")).unwrap(),
            "one\nthree\n"
        );
        assert!(!dir.path().join("old.txt").exists());
        assert!(!dir.path().join("delete.txt").exists());
        assert_eq!(
            output.output,
            "Done!\nA added.txt\nR old.txt -> moved.txt\nD delete.txt\n"
        );
    }

    #[test]
    fn preflight_failure_does_not_publish_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("existing.txt"), "keep\n").unwrap();
        let error = apply_patch(ApplyPatchArgs {
            patch: "*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch\n".into(),
            cwd: Some(dir.path().into()),
        })
        .unwrap_err();

        assert!(error.to_string().contains("cannot update missing file"));
        assert!(!dir.path().join("new.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn repeated_updates_use_the_previous_virtual_result() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "alpha\nmiddle\nomega\n").unwrap();
        apply_patch(ApplyPatchArgs {
            patch: "*** Begin Patch\n*** Update File: file.txt\n@@\n-omega\n+OMEGA\n*** Update File: file.txt\n@@\n-alpha\n+ALPHA\n*** End Patch\n".into(),
            cwd: Some(dir.path().into()),
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "ALPHA\nmiddle\nOMEGA\n"
        );
    }
}
