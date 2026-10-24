//! Snapshot reader. One leaf, then the injected phase's recorded pairs.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::registry::ProjectRecord;
use crate::workflow::execution_repo::PreparedInjection;

use super::{
    Evidence, NoEvidenceReason, PathShape, identity_fields, invariants, key_whitelist, none,
    parse_strict, path_shape, schema_gate,
};

#[cfg(windows)]
use super::{
    ParsedSkill, SKILL_MAX_BYTES, SNAPSHOT_MAX_BYTES, SkillFact, digest_bytes, metadata_is_reparse,
    path_allowed, utf16_len,
};

#[cfg(all(windows, test))]
use super::note_skill_open;

const LEAF_NAME: &str = "current.json";

pub(crate) fn read(record: &ProjectRecord, injected: &PreparedInjection) -> Evidence {
    #[cfg(test)]
    note_call();
    #[cfg(test)]
    if take_panic() {
        panic!("trusted evidence reader hook");
    }
    #[cfg(not(windows))]
    {
        let _ = (record, injected);
        return none(NoEvidenceReason::UnsupportedPlatform);
    }
    #[cfg(windows)]
    read_windows(record, injected)
}

#[cfg(windows)]
fn read_windows(record: &ProjectRecord, injected: &PreparedInjection) -> Evidence {
    let state_dir = match crate::state::resolve_state_dir(record) {
        Ok(dir) => dir,
        Err(_) => return none(NoEvidenceReason::StateRootUnresolved),
    };
    match path_shape(&state_dir) {
        PathShape::Ok => {}
        PathShape::Relative => return none(NoEvidenceReason::RelativeStateRoot),
        PathShape::Remote => return none(NoEvidenceReason::RemoteStateRoot),
        PathShape::NonNormal | PathShape::Colon => {
            return none(NoEvidenceReason::NonNormalComponent);
        }
        PathShape::TooLong => return none(NoEvidenceReason::PathTooLong),
    }
    let root = state_dir.join("trusted-evidence");
    let leaf = root.join(LEAF_NAME);
    if utf16_len(&leaf) > super::MAX_PATH_UTF16 {
        return none(NoEvidenceReason::PathTooLong);
    }
    if let Err(reason) = require_snapshot_dir(&root) {
        return none(reason);
    }
    let bytes = match read_leaf(&leaf, true) {
        Ok(bytes) => bytes,
        Err(reason) => return none(reason),
    };
    let value = match parse_strict(&bytes) {
        Ok(value) => value,
        Err(reason) => return none(reason),
    };
    if let Err(reason) = schema_gate(&value) {
        return none(reason);
    }
    if let Err(reason) = key_whitelist(&value) {
        return none(reason);
    }
    if let Err(reason) = super::check_digest(&value) {
        return none(reason);
    }
    let (project_id, track_id, run_epoch, sequence) = match identity_fields(&value) {
        Ok(fields) => fields,
        Err(reason) => return none(reason),
    };
    if project_id != record.id {
        return none(NoEvidenceReason::WrongProject);
    }
    if run_epoch != injected.run_epoch {
        return none(NoEvidenceReason::WrongEpoch);
    }
    if track_id != injected.track_id {
        return none(NoEvidenceReason::WrongTrack);
    }
    let parsed = match invariants(&value) {
        Ok(parsed) => parsed,
        Err(reason) => return none(reason),
    };
    let roots = allowed_roots(record, &state_dir, parsed.run_epoch);
    if let Err(reason) = validate_skill_paths(&parsed.skills, &roots) {
        return none(reason);
    }
    #[cfg(test)]
    fire_between_reads();
    let again = match read_leaf(&leaf, false) {
        Ok(bytes) => bytes,
        Err(_) => return none(NoEvidenceReason::CaptureRace),
    };
    let again_value = match parse_strict(&again) {
        Ok(value) => value,
        Err(_) => return none(NoEvidenceReason::CaptureRace),
    };
    if super::check_digest(&again_value).is_err() {
        return none(NoEvidenceReason::CaptureRace);
    }
    let again_fields = match identity_fields(&again_value) {
        Ok(fields) => fields,
        Err(_) => return none(NoEvidenceReason::CaptureRace),
    };
    if again_fields.3 != sequence || again_value.get("digest") != value.get("digest") {
        return none(NoEvidenceReason::CaptureRace);
    }
    let facts = match recheck_facts(injected.phase.as_str(), &parsed.skills, &roots) {
        Ok(facts) => facts,
        Err(reason) => return none(reason),
    };
    #[cfg(test)]
    fire_after_read();
    Evidence::Current(super::CurrentEvidence {
        project_id,
        track_id,
        run_epoch,
        phase: injected.phase.clone(),
        facts,
    })
}

#[cfg(windows)]
fn require_snapshot_dir(dir: &Path) -> Result<(), NoEvidenceReason> {
    let chain = super::ancestor_chain(dir).ok_or(NoEvidenceReason::ReparseAncestor)?;
    for ancestor in chain {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(meta) => {
                if metadata_is_reparse(&meta) {
                    return Err(NoEvidenceReason::ReparseAncestor);
                }
                if !meta.is_dir() {
                    return Err(NoEvidenceReason::NotADirectory);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(NoEvidenceReason::Absent);
            }
            Err(err) => return Err(super::map_io(&err)),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_leaf(leaf: &Path, arm_hook: bool) -> Result<Vec<u8>, NoEvidenceReason> {
    let pre = match std::fs::symlink_metadata(leaf) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(NoEvidenceReason::Absent);
        }
        Err(err) => return Err(super::map_io(&err)),
    };
    if metadata_is_reparse(&pre) {
        return Err(NoEvidenceReason::ReparseLeaf);
    }
    if !pre.is_file() {
        return Err(NoEvidenceReason::NotRegularFile);
    }
    if pre.len() > SNAPSHOT_MAX_BYTES {
        return Err(NoEvidenceReason::TooLarge);
    }
    if arm_hook {
        #[cfg(test)]
        fire_leaf_hook();
    }
    let file = match std::fs::File::open(leaf) {
        Ok(file) => file,
        Err(err) => return Err(super::map_io(&err)),
    };
    let handle = match file.metadata() {
        Ok(meta) => meta,
        Err(err) => return Err(super::map_io(&err)),
    };
    let mut limited = file.take(SNAPSHOT_MAX_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    if Read::read_to_end(&mut limited, &mut buf).is_err() {
        return Err(NoEvidenceReason::ReadError);
    }
    if buf.len() as u64 > SNAPSHOT_MAX_BYTES {
        return Err(NoEvidenceReason::TooLarge);
    }
    let re = match std::fs::symlink_metadata(leaf) {
        Ok(meta) => meta,
        Err(_) => return Err(NoEvidenceReason::LeafChanged),
    };
    // Step 7 identity witness: last_write_time and len must match the pre-stat and the handle.
    if metadata_is_reparse(&re) {
        return Err(NoEvidenceReason::ReparseLeaf);
    }
    if witness(&pre) != witness(&handle) || witness(&handle) != witness(&re) {
        return Err(NoEvidenceReason::LeafChanged);
    }
    if !handle.is_file() || handle.len() != buf.len() as u64 {
        return Err(NoEvidenceReason::LeafChanged);
    }
    Ok(buf)
}

#[cfg(windows)]
fn witness(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::windows::fs::MetadataExt;
    (meta.len(), meta.last_write_time())
}

#[cfg(windows)]
fn allowed_roots(record: &ProjectRecord, state_dir: &Path, epoch: u64) -> Vec<PathBuf> {
    let paths = crate::layout::resolve(record);
    let mut roots = vec![paths.workspace_root];
    if let Some(parent) = paths.conductor_dir.parent() {
        roots.push(parent.to_path_buf());
    }
    if let Some(repo) = paths.execution_repo {
        roots.push(repo);
    }
    if crate::worktree::isolation_enabled(record).ok() == Some(true) {
        roots.push(state_dir.join("worktrees").join(epoch.to_string()));
    }
    roots
}

#[cfg(windows)]
fn validate_skill_paths(skills: &[ParsedSkill], roots: &[PathBuf]) -> Result<(), NoEvidenceReason> {
    for skill in skills {
        let Some(path_text) = skill.path.as_deref() else {
            continue;
        };
        path_allowed(Path::new(path_text), roots)?;
    }
    Ok(())
}

#[cfg(windows)]
fn recheck_facts(
    phase: &str,
    skills: &[ParsedSkill],
    roots: &[PathBuf],
) -> Result<Vec<SkillFact>, NoEvidenceReason> {
    let mut facts = Vec::new();
    for pair in super::required_pairs(phase) {
        let Some(skill) = skills.iter().find(|skill| {
            skill.phase == pair.phase && skill.name == pair.name && skill.scope == pair.scope
        }) else {
            return Err(NoEvidenceReason::FactDrifted);
        };
        recheck_skill(skill, roots)?;
        facts.push(SkillFact {
            phase: skill.phase.clone(),
            name: skill.name.clone(),
            scope: skill.scope.clone(),
            state: skill.state.clone(),
        });
    }
    Ok(facts)
}

#[cfg(windows)]
fn recheck_skill(skill: &ParsedSkill, roots: &[PathBuf]) -> Result<(), NoEvidenceReason> {
    if skill.state == "non_utf8_path" || skill.state == "reparse" {
        return Err(NoEvidenceReason::FactDrifted);
    }
    let Some(path_text) = skill.path.as_deref() else {
        return Err(NoEvidenceReason::FactDrifted);
    };
    let path = Path::new(path_text);
    match skill.state.as_str() {
        "present" => rehash(path, roots, skill.digest.as_deref(), skill.size),
        "absent" => match std::fs::symlink_metadata(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(NoEvidenceReason::FactDrifted),
        },
        "unreadable" => retry_unreadable(path, roots),
        "oversize" => match std::fs::symlink_metadata(path) {
            Ok(meta)
                if meta.is_file()
                    && !metadata_is_reparse(&meta)
                    && meta.len() > SKILL_MAX_BYTES =>
            {
                Ok(())
            }
            _ => Err(NoEvidenceReason::FactDrifted),
        },
        "not_regular" => match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_file() => Ok(()),
            _ => Err(NoEvidenceReason::FactDrifted),
        },
        _ => Err(NoEvidenceReason::FactDrifted),
    }
}

#[cfg(windows)]
fn rehash(
    path: &Path,
    roots: &[PathBuf],
    expected_digest: Option<&str>,
    expected_size: Option<u64>,
) -> Result<(), NoEvidenceReason> {
    let Some(expected_digest) = expected_digest else {
        return Err(NoEvidenceReason::FactDrifted);
    };
    path_allowed(path, roots)?;
    let pre = std::fs::symlink_metadata(path).map_err(|_| NoEvidenceReason::FactDrifted)?;
    if metadata_is_reparse(&pre) || !pre.is_file() {
        return Err(NoEvidenceReason::FactDrifted);
    }
    #[cfg(test)]
    note_skill_open();
    let file = std::fs::File::open(path).map_err(|_| NoEvidenceReason::FactDrifted)?;
    let handle = file.metadata().map_err(|_| NoEvidenceReason::FactDrifted)?;
    let mut limited = file.take(SKILL_MAX_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    if Read::read_to_end(&mut limited, &mut buf).is_err() || buf.len() as u64 > SKILL_MAX_BYTES {
        return Err(NoEvidenceReason::FactDrifted);
    }
    let re = std::fs::symlink_metadata(path).map_err(|_| NoEvidenceReason::FactDrifted)?;
    if metadata_is_reparse(&re)
        || witness(&pre) != witness(&handle)
        || witness(&handle) != witness(&re)
    {
        return Err(NoEvidenceReason::FactDrifted);
    }
    if digest_bytes(&buf) != expected_digest || expected_size != Some(buf.len() as u64) {
        return Err(NoEvidenceReason::FactDrifted);
    }
    Ok(())
}

#[cfg(windows)]
fn retry_unreadable(path: &Path, roots: &[PathBuf]) -> Result<(), NoEvidenceReason> {
    if path_allowed(path, roots).is_err() {
        return Ok(());
    }
    #[cfg(test)]
    note_skill_open();
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(());
    };
    let mut limited = file.take(SKILL_MAX_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    if Read::read_to_end(&mut limited, &mut buf).is_ok() {
        Err(NoEvidenceReason::FactDrifted)
    } else {
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static PANIC_ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LEAF_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static BETWEEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn note_call() {
    if ARMED.with(std::cell::Cell::get) {
        CALLS.with(|cell| cell.set(cell.get().saturating_add(1)));
    }
}

#[cfg(test)]
fn take_panic() -> bool {
    PANIC_ONCE.with(|cell| cell.replace(false))
}

#[cfg(test)]
fn fire_leaf_hook() {
    if let Some(hook) = LEAF_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
fn fire_between_reads() {
    if let Some(hook) = BETWEEN_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
fn fire_after_read() {
    if let Some(hook) = AFTER_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
pub(crate) struct ReadCounterGuard;

#[cfg(test)]
impl ReadCounterGuard {
    pub(crate) fn arm() -> Self {
        ARMED.with(|cell| cell.set(true));
        CALLS.with(|cell| cell.set(0));
        Self
    }

    pub(crate) fn count(&self) -> u32 {
        CALLS.with(std::cell::Cell::get)
    }
}

#[cfg(test)]
impl Drop for ReadCounterGuard {
    fn drop(&mut self) {
        ARMED.with(|cell| cell.set(false));
        CALLS.with(|cell| cell.set(0));
    }
}

#[cfg(test)]
pub(crate) struct PanicOnceGuard;

#[cfg(test)]
impl PanicOnceGuard {
    pub(crate) fn arm() -> Self {
        PANIC_ONCE.with(|cell| cell.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for PanicOnceGuard {
    fn drop(&mut self) {
        PANIC_ONCE.with(|cell| cell.set(false));
    }
}

#[cfg(test)]
pub(crate) struct LeafHookGuard;

#[cfg(test)]
impl LeafHookGuard {
    pub(crate) fn arm(hook: impl FnOnce() + 'static) -> Self {
        LEAF_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}

#[cfg(test)]
impl Drop for LeafHookGuard {
    fn drop(&mut self) {
        LEAF_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) struct BetweenReadsGuard;

#[cfg(test)]
impl BetweenReadsGuard {
    pub(crate) fn arm(hook: impl FnOnce() + 'static) -> Self {
        BETWEEN_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}

#[cfg(test)]
impl Drop for BetweenReadsGuard {
    fn drop(&mut self) {
        BETWEEN_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) struct AfterReadGuard;

#[cfg(test)]
impl AfterReadGuard {
    pub(crate) fn arm(hook: impl FnOnce() + 'static) -> Self {
        AFTER_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}

#[cfg(test)]
impl Drop for AfterReadGuard {
    fn drop(&mut self) {
        AFTER_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}
