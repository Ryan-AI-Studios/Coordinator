//! Journaled rename-aside swap for the installed binary (track 0108).
//!
//! The swap never rolls back internally. It records both sha256 values in the
//! receipt and leaves the journal at `verifying`; the orchestrator (`run_with`)
//! is the single rollback owner. A routine refusal that never touched the
//! install path removes its own journal and staged `.new`.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CoordinatorError, Result};

/// Journal schema version.
pub(crate) const JOURNAL_VERSION: u32 = 1;

/// Streamed sha256 (64 KiB chunks) plus the file size.
pub(crate) fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buf[..read]);
    }
    Ok((hex_lower(&hasher.finalize()), size))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// `<install>.self-update.journal`.
pub(crate) fn journal_path(install: &Path) -> PathBuf {
    with_suffix(install, ".self-update.journal")
}

/// `<install>.new` (never `with_extension`, which would rewrite `.exe`).
pub(crate) fn staged_path(install: &Path) -> PathBuf {
    with_suffix(install, ".new")
}

/// `<install>.old` (`coordinator.exe.old`).
pub(crate) fn old_path(install: &Path) -> PathBuf {
    with_suffix(install, ".old")
}

/// Crash journal. Written before each mutating step; kept from `renaming-old`
/// onward.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Journal {
    pub version: u32,
    pub phase: String,
    pub install: PathBuf,
    pub old: PathBuf,
    pub artifact: PathBuf,
    pub head_sha: String,
    pub previous_sha256: Option<String>,
    pub staged: PathBuf,
}

pub(crate) fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    crate::persist::atomic_write_json(path, journal)
}

/// `Ok(None)` = no journal. A corrupt/unreadable journal is an error (fail closed).
pub(crate) fn read_journal(path: &Path) -> Result<Option<Journal>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(CoordinatorError::Message(format!(
                "self-update journal path is not a regular file: {}; ask the owner",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let text = std::fs::read_to_string(path)?;
    let parsed: Journal = serde_json::from_str(&text).map_err(|e| {
        CoordinatorError::Message(format!(
            "self-update journal unreadable at {}: {e}",
            path.display()
        ))
    })?;
    if parsed.version != JOURNAL_VERSION {
        return Err(CoordinatorError::Message(format!(
            "self-update journal {} has unsupported version {}; ask the owner",
            path.display(),
            parsed.version
        )));
    }
    Ok(Some(parsed))
}

/// Idempotent journal removal (best-effort). Test helper only.
#[cfg(test)]
pub(crate) fn remove_journal_if_present(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// Rollback must not report success with an unreadable journal behind it.
fn remove_journal_strict(path: &Path) -> std::result::Result<(), SwapError> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| SwapError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

/// Phase-derived recovery guidance. No automatic recovery: the operator keeps
/// control, and a branch never names a file that may not exist.
pub(crate) fn recovery_message(journal: &Journal) -> String {
    let install = journal.install.display();
    let old = journal.old.display();
    let artifact = journal.artifact.display();
    let staged = journal.staged.display();
    let path = journal_path(&journal.install).display().to_string();
    match journal.phase.as_str() {
        "staging" | "clearing-old" => {
            format!("nothing was swapped; delete {path} (and {staged} if present), then rerun")
        }
        "renaming-old" => match journal.previous_sha256.as_deref() {
            Some(previous) => format!(
                "nothing was swapped — delete {path} and rerun. If {install} is absent, the \
                 rename-aside completed and {old} holds the previous bytes ({previous}). A rerun \
                 with {install} absent runs as a first install: it leaves {old} untouched and \
                 records previous_sha256: null, so if that rerun's post-swap smoke fails one \
                 further successful run recovers. Do not restore {old} by hand here: clearing-old \
                 may have removed or rotated it."
            ),
            None => format!(
                "nothing was swapped — delete {path} and rerun. This was a first install, so the \
                 rerun also runs as a first install and creates no {old}."
            ),
        },
        "installing" | "verifying" => match journal.previous_sha256.as_deref() {
            Some(previous) => format!(
                "compare sha256({install}) with sha256({artifact}); if equal, the swap completed — \
                 delete {path} (do not downgrade a correct install). If it equals {previous} or \
                 {install} is absent, copy {old} to {install}, verify sha256(install) == \
                 {previous}, then delete {path}. If neither matches, stop and ask the owner."
            ),
            None => format!(
                "compare sha256({install}) with sha256({artifact}); if equal, the swap completed — \
                 delete {path}. Otherwise there is no rollback source — delete {path} and rerun."
            ),
        },
        "rolling-back" => match journal.previous_sha256.as_deref() {
            Some(previous) => format!(
                "the run had already decided to restore previous_sha256: copy {old} to {install} \
                 and verify sha256(install) == {previous} unconditionally (do not treat {install} \
                 as complete even if it currently equals {artifact} — those are the bytes the run \
                 rejected), then delete {path}. If {old} is missing or the sha does not match, stop \
                 and ask the owner."
            ),
            None => format!(
                "first install: remove the rejected {install} and delete {path}; there is no \
                 previous binary to restore."
            ),
        },
        other => format!(
            "unknown journal phase '{other}'; stop and ask the owner — nothing was recovered \
             automatically"
        ),
    }
}

/// Swap request: install target, source bytes and the HEAD the bytes describe.
#[derive(Debug, Clone)]
pub(crate) struct SwapRequest {
    pub install: PathBuf,
    pub artifact: PathBuf,
    pub head_sha: String,
    /// Previous install sha captured **before** the pre-swap smoke, so a real
    /// OS pin (a handle without delete sharing, which may also deny a fresh
    /// read) cannot prevent the crash journal recording the rollback source.
    pub previous_sha256: Option<String>,
}

/// What the swap did. `installed_sha256` and `artifact_sha256` may differ: the
/// caller compares them and owns the rollback decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapReceipt {
    pub installed_sha256: String,
    pub artifact_sha256: String,
    pub previous_sha256: Option<String>,
    pub old_path: PathBuf,
    pub previous_path: Option<PathBuf>,
    pub rotated_old: Option<PathBuf>,
}

/// The five swap failure arms. Every arm maps to a defined refusal reason.
#[derive(Debug)]
pub(crate) enum SwapError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    RollbackMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    Pinned {
        path: PathBuf,
    },
    Interrupted {
        phase: String,
    },
    StagingMismatch {
        path: PathBuf,
    },
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for SwapError {}

impl SwapError {
    /// Always names the arm and its path.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Io { path, source } => {
                format!("io error at {}: {source}", path.display())
            }
            Self::RollbackMismatch {
                path,
                expected,
                actual,
            } => format!(
                "rollback-mismatch at {}: expected {expected}, got {actual}",
                path.display()
            ),
            Self::Pinned { path } => format!(
                "old-pinned: cannot clear or rotate {} (nothing was swapped)",
                path.display()
            ),
            Self::Interrupted { phase } => {
                format!("interrupted: previous attempt stopped at phase '{phase}'")
            }
            Self::StagingMismatch { path } => format!(
                "staging-mismatch at {}: staged bytes do not match the artifact (nothing was swapped)",
                path.display()
            ),
        }
    }
}

fn crate_to_swap(path: &Path, err: CoordinatorError) -> SwapError {
    match err {
        CoordinatorError::Io(source) => SwapError::Io {
            path: path.to_path_buf(),
            source,
        },
        other => SwapError::Io {
            path: path.to_path_buf(),
            source: io::Error::other(other.to_string()),
        },
    }
}

fn hash_or_swap(path: &Path) -> std::result::Result<(String, u64), SwapError> {
    sha256_file(path).map_err(|e| crate_to_swap(path, e))
}

fn write_journal_or_swap(path: &Path, journal: &Journal) -> std::result::Result<(), SwapError> {
    write_journal(path, journal).map_err(|e| crate_to_swap(path, e))
}

fn cleanup_no_swap(journal: &Path, staged: &Path) -> std::result::Result<(), SwapError> {
    for path in [staged, journal] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(SwapError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(())
}

/// Production wrapper: real copy/remove.
pub(crate) fn swap(req: &SwapRequest) -> std::result::Result<SwapReceipt, SwapError> {
    swap_with(
        req,
        |from, to| std::fs::copy(from, to),
        |path| std::fs::remove_file(path),
    )
}

/// Stage, clear/rotate `.old`, rename-aside, rename-in, record both shas.
///
/// The journal is left at `verifying`. No internal rollback, ever.
pub(crate) fn swap_with(
    req: &SwapRequest,
    copy: impl Fn(&Path, &Path) -> io::Result<u64>,
    remove_old: impl Fn(&Path) -> io::Result<()>,
) -> std::result::Result<SwapReceipt, SwapError> {
    let install = &req.install;
    let artifact = &req.artifact;
    let staged = staged_path(install);
    let old = old_path(install);
    let journal_path = journal_path(install);

    match std::fs::symlink_metadata(&journal_path) {
        Ok(metadata) if metadata.is_file() => {
            let phase = match read_journal(&journal_path) {
                Ok(Some(journal)) => journal.phase,
                Ok(None) => "unknown".to_string(),
                Err(e) => {
                    return Err(SwapError::Io {
                        path: journal_path,
                        source: io::Error::other(e.to_string()),
                    });
                }
            };
            return Err(SwapError::Interrupted { phase });
        }
        Ok(_) => {
            return Err(SwapError::Io {
                path: journal_path,
                source: io::Error::other("journal path is not a regular file"),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SwapError::Io {
                path: journal_path,
                source,
            });
        }
    }

    // Previous bytes were captured by the caller before the pre-swap smoke. If a
    // direct caller omitted it, fall back to hashing (still legal when readable).
    let previous_sha256 = match (&req.previous_sha256, install.exists()) {
        (Some(sha), _) => Some(sha.clone()),
        (None, true) => Some(hash_or_swap(install)?.0),
        (None, false) => None,
    };

    let mut journal = Journal {
        version: JOURNAL_VERSION,
        phase: "staging".to_string(),
        install: install.clone(),
        old: old.clone(),
        artifact: artifact.clone(),
        head_sha: req.head_sha.clone(),
        previous_sha256: previous_sha256.clone(),
        staged: staged.clone(),
    };
    if let Err(e) = write_journal_or_swap(&journal_path, &journal) {
        cleanup_no_swap(&journal_path, &staged)?;
        return Err(e);
    }

    if let Err(source) = stage_copy_with(artifact, &staged, copy) {
        cleanup_no_swap(&journal_path, &staged)?;
        return Err(SwapError::Io {
            path: staged,
            source,
        });
    }
    let (staged_sha, _) = match hash_or_swap(&staged) {
        Ok(v) => v,
        Err(e) => {
            cleanup_no_swap(&journal_path, &staged)?;
            return Err(e);
        }
    };
    let (artifact_sha, _) = match hash_or_swap(artifact) {
        Ok(v) => v,
        Err(e) => {
            cleanup_no_swap(&journal_path, &staged)?;
            return Err(e);
        }
    };
    if staged_sha != artifact_sha {
        cleanup_no_swap(&journal_path, &staged)?;
        return Err(SwapError::StagingMismatch { path: staged });
    }

    journal.phase = "clearing-old".to_string();
    if let Err(e) = write_journal_or_swap(&journal_path, &journal) {
        // Nothing touched the install path yet: a routine refusal must not leave
        // a false "previous attempt crashed" journal behind.
        cleanup_no_swap(&journal_path, &staged)?;
        return Err(e);
    }

    let mut rotated_old = None;
    if install.exists() {
        match clear_old_with(&old, &remove_old) {
            Ok(OldDisposition::Cleared) => {}
            Ok(OldDisposition::Rotated(path)) => rotated_old = Some(path),
            Err(e) => {
                cleanup_no_swap(&journal_path, &staged)?;
                return Err(e);
            }
        }
    }

    // From here on the journal is kept on every error path.
    journal.phase = "renaming-old".to_string();
    if let Err(e) = write_journal_or_swap(&journal_path, &journal) {
        // The install path is still untouched; `.old` may have been cleared but
        // no swap happened, so this is still a routine refusal.
        cleanup_no_swap(&journal_path, &staged)?;
        return Err(e);
    }
    if install.exists()
        && let Err(source) = std::fs::rename(install, &old)
    {
        return Err(SwapError::Io {
            path: install.clone(),
            source,
        });
    }

    journal.phase = "installing".to_string();
    write_journal_or_swap(&journal_path, &journal)?;
    if let Err(source) = std::fs::rename(&staged, install) {
        return Err(SwapError::Io {
            path: install.clone(),
            source,
        });
    }

    let (installed_sha256, _) = hash_or_swap(install)?;
    let (artifact_sha256, _) = hash_or_swap(artifact)?;

    journal.phase = "verifying".to_string();
    write_journal_or_swap(&journal_path, &journal)?;

    Ok(SwapReceipt {
        installed_sha256,
        artifact_sha256,
        previous_path: previous_sha256.as_ref().map(|_| old.clone()),
        previous_sha256,
        old_path: old,
        rotated_old,
    })
}

/// One call to the injected copy plus `sync_all` on the staged file.
pub(crate) fn stage_copy_with(
    artifact: &Path,
    staged: &Path,
    copy: impl Fn(&Path, &Path) -> io::Result<u64>,
) -> io::Result<()> {
    copy(artifact, staged)?;
    let file = std::fs::OpenOptions::new().write(true).open(staged)?;
    file.sync_all()?;
    Ok(())
}

/// Result of clearing the previous `.old`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OldDisposition {
    Cleared,
    Rotated(PathBuf),
}

/// Remove `.old`; on a sharing violation rotate it to `.old.<utc>`. If both
/// fail, refuse `Pinned` — never delete the only rollback copy.
pub(crate) fn clear_old_with(
    old: &Path,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> std::result::Result<OldDisposition, SwapError> {
    if !old.exists() {
        return Ok(OldDisposition::Cleared);
    }
    match remove(old) {
        Ok(()) => Ok(OldDisposition::Cleared),
        Err(_) => {
            if !old.exists() {
                return Ok(OldDisposition::Cleared);
            }
            let rotated = rotated_old_path(old, Utc::now());
            match std::fs::rename(old, &rotated) {
                Ok(()) => Ok(OldDisposition::Rotated(rotated)),
                Err(_) => Err(SwapError::Pinned {
                    path: old.to_path_buf(),
                }),
            }
        }
    }
}

fn rotated_old_path(old: &Path, now: DateTime<Utc>) -> PathBuf {
    with_suffix(old, &format!(".{}", now.format("%Y%m%dT%H%M%S%.3fZ")))
}

fn rejected_path(install: &Path, now: DateTime<Utc>) -> PathBuf {
    with_suffix(
        install,
        &format!(".rejected-{}", now.format("%Y%m%dT%H%M%S%.3fZ")),
    )
}

/// Restore the previous binary. Called **only** by `run_with`.
pub(crate) fn rollback(
    req: &SwapRequest,
    previous_sha256: Option<&str>,
) -> std::result::Result<(), SwapError> {
    let install = &req.install;
    let old = old_path(install);
    let journal_path = journal_path(install);
    let journal = Journal {
        version: JOURNAL_VERSION,
        phase: "rolling-back".to_string(),
        install: install.clone(),
        old: old.clone(),
        artifact: req.artifact.clone(),
        head_sha: req.head_sha.clone(),
        previous_sha256: previous_sha256.map(str::to_string),
        staged: staged_path(install),
    };
    write_journal_or_swap(&journal_path, &journal)?;

    match previous_sha256 {
        Some(previous) => {
            if install.exists() {
                let rejected = rejected_path(install, Utc::now());
                if let Err(source) = std::fs::rename(install, &rejected) {
                    return Err(SwapError::Io {
                        path: install.clone(),
                        source,
                    });
                }
                if let Err(source) = std::fs::copy(&old, install) {
                    return Err(SwapError::Io { path: old, source });
                }
                let (actual, _) = hash_or_swap(install)?;
                if actual != previous {
                    return Err(SwapError::RollbackMismatch {
                        path: install.clone(),
                        expected: previous.to_string(),
                        actual,
                    });
                }
                let _ = std::fs::remove_file(&rejected);
            } else {
                if let Err(source) = std::fs::copy(&old, install) {
                    return Err(SwapError::Io { path: old, source });
                }
                let (actual, _) = hash_or_swap(install)?;
                if actual != previous {
                    return Err(SwapError::RollbackMismatch {
                        path: install.clone(),
                        expected: previous.to_string(),
                        actual,
                    });
                }
            }
            remove_journal_strict(&journal_path)?;
            Ok(())
        }
        None => {
            // First install: no rollback source — remove the rejected install.
            if install.exists()
                && let Err(source) = std::fs::remove_file(install)
            {
                return Err(SwapError::Io {
                    path: install.clone(),
                    source,
                });
            }
            remove_journal_strict(&journal_path)?;
            Ok(())
        }
    }
}

/// Remove the journal only after the post-swap smoke **and** the ledger write.
pub(crate) fn finish(install: &Path) -> Result<()> {
    let path = journal_path(install);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
