//! Hermetic fixtures for the trusted-evidence snapshot. No network, no `gh`, no nested cargo.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{Evidence, NoEvidenceReason, ProducerHook, WriteOutcome};

struct World {
    _dir: tempfile::TempDir,
    record: crate::registry::ProjectRecord,
    state: crate::state::RunState,
}

fn fixture() -> World {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    let execution = dir.path().join("repo");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&execution).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let record = crate::registry::ProjectRecord {
        id: "p-0123".into(),
        path: workspace,
        display_name: None,
        layout_profile: crate::layout::LayoutProfile::Nested,
        conductor_dir: None,
        execution_repo: Some(execution),
        execution_repos: std::collections::BTreeMap::new(),
        state_dir: Some(state_dir),
        auto_merge: true,
        phase_timeouts_secs: std::collections::BTreeMap::new(),
        notify_progress: false,
        worktree_isolation: false,
        ready_aliases: Vec::new(),
        auto_start: Default::default(),
        state_policies: Vec::new(),
        self_continuation: false,
        ci_fix_routing: false,
        review_comment_routing: false,
        skill_aliases: std::collections::BTreeMap::new(),
        created_at: chrono::Utc::now(),
    };
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.driver = crate::workflow::WorkflowDriver::Adapter;
    state.phase = "plan".into();
    state.track_id = Some("0123-Example".into());
    state.run_epoch = 7;
    World {
        _dir: dir,
        record,
        state,
    }
}

fn locked(world: &World, state: &crate::state::RunState) -> WriteOutcome {
    crate::state::with_run_state_lock(&world.record, || {
        Ok(super::write_for_epoch(
            &world.record,
            state,
            ProducerHook::RunBeginFreshRun,
        ))
    })
    .unwrap()
}

fn leaf(world: &World) -> PathBuf {
    world
        .record
        .state_dir
        .as_ref()
        .unwrap()
        .join("trusted-evidence")
        .join("current.json")
}

fn injection(
    world: &World,
    phase: &str,
    epoch: u64,
) -> crate::workflow::execution_repo::PreparedInjection {
    crate::workflow::execution_repo::PreparedInjection {
        project_id: world.record.id.clone(),
        run_epoch: epoch,
        phase: phase.into(),
        track_id: world.state.track_id.clone(),
        target: None,
    }
}

fn read_at(world: &World, phase: &str, epoch: u64) -> Evidence {
    let prepared = injection(world, phase, epoch);
    super::read(&world.record, &prepared)
}

fn reason(evidence: &Evidence) -> NoEvidenceReason {
    match evidence {
        Evidence::None(reason) => *reason,
        Evidence::Current(_) => panic!("expected NoEvidence, got {evidence:?}"),
    }
}

fn evidence_details(record: &crate::registry::ProjectRecord) -> Vec<String> {
    let text = std::fs::read_to_string(crate::progress_log::path(record)).unwrap_or_default();
    text.lines()
        .filter(|line| line.contains("  evidence  "))
        .map(|line| line.split("  evidence  ").nth(1).unwrap_or("").to_string())
        .collect()
}

fn load_snapshot(world: &World) -> Value {
    serde_json::from_slice(&std::fs::read(leaf(world)).unwrap()).unwrap()
}

fn store_snapshot(world: &World, value: &Value) {
    std::fs::write(leaf(world), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn resign(value: &mut Value) {
    let digest = super::digest_of(value).expect("digest");
    value
        .as_object_mut()
        .unwrap()
        .insert("digest".into(), Value::String(digest));
}

fn plant(
    record: &crate::registry::ProjectRecord,
    epoch: u64,
    name: &str,
    scope: &str,
    body: &str,
) -> PathBuf {
    let path = if scope == "workspace" {
        crate::workflow::reuse::chosen_workspace_skill(record, name).0
    } else {
        crate::workflow::reuse::execution_skill_path_for_epoch(record, epoch, name)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn skill_tree_snapshot(root: &Path) -> Vec<(String, u64, u64)> {
    fn walk(dir: &Path, root: &Path, rows: &mut Vec<(String, u64, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if meta.is_dir() {
                walk(&path, root, rows);
                continue;
            }
            rows.push((name, meta.len(), file_stamp(&meta)));
        }
    }
    let mut rows = Vec::new();
    walk(root, root, &mut rows);
    rows.sort();
    rows
}

fn file_stamp(meta: &std::fs::Metadata) -> u64 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.last_write_time()
    }
    #[cfg(not(windows))]
    {
        meta.modified()
            .ok()
            .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|stamp| stamp.as_secs())
            .unwrap_or(0)
    }
}

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(path).unwrap()
}

fn strip_cfg_test_items(source: &str) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < source.len() {
        if source[index..].starts_with("#[cfg(test)]") {
            let after = skip_ws(source, index + "#[cfg(test)]".len());
            if item_start(&source[after..]) {
                index = skip_item(source, after);
                continue;
            }
        }
        let next = source[index..].chars().next().unwrap();
        out.push(next);
        index += next.len_utf8();
    }
    out
}

fn skip_ws(source: &str, mut index: usize) -> usize {
    while source[index..].starts_with(|ch: char| ch.is_whitespace()) {
        index += source[index..].chars().next().unwrap().len_utf8();
    }
    index
}

fn item_start(rest: &str) -> bool {
    [
        "fn ",
        "pub ",
        "struct ",
        "enum ",
        "impl ",
        "mod ",
        "const ",
        "static ",
        "use ",
        "type ",
        "thread_local!",
    ]
    .iter()
    .any(|head| rest.starts_with(head))
}

fn skip_item(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut depth = 0i32;
    let mut seen = false;
    while index < bytes.len() {
        match bytes[index] {
            b';' if !seen => return index + 1,
            b'{' => {
                seen = true;
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    index
}

fn production(rel: &str) -> String {
    strip_cfg_test_items(&source(rel))
}

#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let out = std::process::Command::new("cmd")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn t_reparse_nonsurrogate() {
    assert!(super::is_reparse(0x400));
    assert!(super::is_reparse(0x400 | 0x10));
    assert!(!super::is_reparse(0));
    assert!(!super::is_reparse(0x2000_0000));
}

#[test]
fn canonical_digest_ignores_key_order_and_rejects_duplicate_keys() {
    let mut first =
        serde_json::json!({"b": 1, "a": {"z": true, "y": false}, "digest": "sha256:aa"});
    let mut second =
        serde_json::json!({"a": {"y": false, "z": true}, "digest": Value::Null, "b": 1});
    assert_eq!(super::digest_of(&first), super::digest_of(&second));
    resign(&mut first);
    resign(&mut second);
    assert_eq!(first["digest"], second["digest"]);
    let duplicate = br#"{"run_epoch":1,"run_epoch":2}"#;
    assert!(matches!(
        super::parse_strict(duplicate),
        Err(NoEvidenceReason::Malformed)
    ));
}

#[cfg(windows)]
#[test]
fn t_absent_and_archive_only() {
    let world = fixture();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Absent
    );
    assert!(evidence_details(&world.record).is_empty());
    locked(&world, &world.state);
    let bytes = std::fs::read(leaf(&world)).unwrap();
    std::fs::remove_file(leaf(&world)).unwrap();
    let archive = leaf(&world).parent().unwrap().join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let archived = archive.join("00000000000000000007.json");
    std::fs::write(&archived, &bytes).unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Absent
    );
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&archived)
        .unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Absent
    );
    drop(held);
}

#[cfg(windows)]
#[test]
fn t_schema_and_legacy_gates() {
    let world = fixture();
    locked(&world, &world.state);
    let valid = load_snapshot(&world);
    let mut legacy = serde_json::json!({"version": 1, "run_epoch": 7});
    store_snapshot(&world, &legacy);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    legacy["mystery"] = Value::Bool(true);
    store_snapshot(&world, &legacy);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    let mut versioned = valid.clone();
    versioned["schema_version"] = Value::from(0);
    resign(&mut versioned);
    store_snapshot(&world, &versioned);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    versioned["schema_version"] = Value::from(2);
    resign(&mut versioned);
    store_snapshot(&world, &versioned);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    versioned["schema_version"] = Value::from(1);
    versioned["schema"] = Value::String("other".into());
    resign(&mut versioned);
    store_snapshot(&world, &versioned);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("schema_version");
    store_snapshot(&world, &missing);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::UnsupportedVersion
    );
    let mut unknown = valid.clone();
    unknown["extra"] = Value::Bool(true);
    store_snapshot(&world, &unknown);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
}

#[cfg(windows)]
#[test]
fn t_identity_epoch_project_track() {
    let world = fixture();
    let mut state = world.state.clone();
    state.run_epoch = 6;
    locked(&world, &state);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::WrongEpoch
    );
    state.run_epoch = 8;
    locked(&world, &state);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::WrongEpoch
    );
    state.run_epoch = 7;
    locked(&world, &state);
    let mut other = world.record.clone();
    other.id = "p-other".into();
    let prepared = injection(&world, "implement", 7);
    assert_eq!(
        reason(&super::read(&other, &prepared)),
        NoEvidenceReason::WrongProject
    );
    let mut prepared = injection(&world, "implement", 7);
    prepared.track_id = Some("other-track".into());
    assert_eq!(
        reason(&super::read(&world.record, &prepared)),
        NoEvidenceReason::WrongTrack
    );
    prepared.track_id = None;
    assert_eq!(
        reason(&super::read(&world.record, &prepared)),
        NoEvidenceReason::WrongTrack
    );
}

#[cfg(windows)]
#[test]
fn t_digest_file_and_skill_and_fact_drift() {
    let world = fixture();
    let _path = plant(&world.record, 7, "implement", "execution", "alpha\n");
    locked(&world, &world.state);
    assert!(matches!(
        read_at(&world, "implement", 7),
        Evidence::Current(_)
    ));
    let mut flipped = load_snapshot(&world);
    let text = flipped["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == "implement" && skill["scope"] == "execution")
        .unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let mut chars: Vec<u8> = text.into_bytes();
    let index = chars.len() - 1;
    chars[index] = if chars[index] == b'a' { b'b' } else { b'a' };
    let replacement = String::from_utf8(chars).unwrap();
    for skill in flipped["skills"].as_array_mut().unwrap() {
        if skill["name"] == "implement" && skill["scope"] == "execution" {
            skill["digest"] = Value::String(replacement.clone());
        }
    }
    store_snapshot(&world, &flipped);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::SnapshotDigestMismatch
    );
    let edited = fixture();
    let path = plant(&edited.record, 7, "implement", "execution", "alpha\n");
    locked(&edited, &edited.state);
    std::fs::write(&path, "beta\n").unwrap();
    assert_eq!(
        reason(&read_at(&edited, "implement", 7)),
        NoEvidenceReason::FactDrifted
    );
    let deleted = fixture();
    let path = plant(&deleted.record, 7, "implement", "execution", "alpha\n");
    locked(&deleted, &deleted.state);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        reason(&read_at(&deleted, "implement", 7)),
        NoEvidenceReason::FactDrifted
    );
    let created = fixture();
    locked(&created, &created.state);
    plant(&created.record, 7, "onboarding", "execution", "now\n");
    assert_eq!(
        reason(&read_at(&created, "implement", 7)),
        NoEvidenceReason::FactDrifted
    );
}

#[cfg(windows)]
#[test]
fn t_oversize_skill_and_current_non_present() {
    let world = fixture();
    let implement = plant(&world.record, 7, "implement", "execution", "x");
    let onboarding = plant(&world.record, 7, "onboarding", "execution", "y");
    for path in [&implement, &onboarding] {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_len(super::SKILL_MAX_BYTES + 1).unwrap();
    }
    locked(&world, &world.state);
    match read_at(&world, "implement", 7) {
        Evidence::Current(current) => {
            assert_eq!(current.facts.len(), 2);
            assert!(current.facts.iter().all(|fact| fact.state == "oversize"));
        }
        other => panic!("expected Current, got {other:?}"),
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&implement)
        .unwrap();
    file.set_len(4).unwrap();
    drop(file);
    std::fs::write(&implement, "tiny").unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::FactDrifted
    );
}

#[cfg(windows)]
#[test]
fn t_reparse_and_directory_shapes() {
    let world = fixture();
    let state_dir = world.record.state_dir.clone().unwrap();
    let root = state_dir.join("trusted-evidence");
    junction(&root, &world._dir.path().join("evidence-target"));
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::ReparseAncestor
    );
    std::fs::remove_dir(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    junction(
        &root.join("current.json"),
        &world._dir.path().join("leaf-target"),
    );
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::ReparseLeaf
    );
    std::fs::remove_dir(root.join("current.json")).unwrap();
    std::fs::create_dir(root.join("current.json")).unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::NotRegularFile
    );
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, "not-a-dir").unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::NotADirectory
    );

    let upper = fixture();
    let real = upper._dir.path().join("real-parent");
    std::fs::create_dir_all(real.join("state")).unwrap();
    let link = upper._dir.path().join("link-parent");
    junction(&link, &real);
    let mut record = upper.record.clone();
    record.state_dir = Some(link.join("state"));
    let prepared = injection(&upper, "implement", 7);
    assert_eq!(
        reason(&super::read(&record, &prepared)),
        NoEvidenceReason::ReparseAncestor
    );

    let skill = fixture();
    let path =
        crate::workflow::reuse::execution_skill_path_for_epoch(&skill.record, 7, "implement");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    junction(&path, &skill._dir.path().join("skill-target"));
    locked(&skill, &skill.state);
    let evidence = read_at(&skill, "implement", 7);
    assert!(matches!(
        reason(&evidence),
        NoEvidenceReason::ReparseAncestor | NoEvidenceReason::PathNotAllowed
    ));
}

#[cfg(windows)]
#[test]
fn t_corrupt_bounds_and_consistency() {
    let world = fixture();
    locked(&world, &world.state);
    std::fs::write(leaf(&world), b"{").unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    value["producer"]["extra"] = Value::Bool(true);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let raw = std::fs::read_to_string(leaf(&world)).unwrap();
    let broken = raw.replacen("\"run_epoch\"", "\"run_epoch\": 7, \"run_epoch\"", 1);
    std::fs::write(leaf(&world), broken).unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    let skill = value["skills"][0].clone();
    let observation = value["observations"][0].clone();
    value["skills"].as_array_mut().unwrap().push(skill);
    value["observations"]
        .as_array_mut()
        .unwrap()
        .push(observation);
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    value["observations"][0]["outcome"] = Value::String("denied".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    for skill in value["skills"].as_array_mut().unwrap() {
        if skill["state"] == "absent" {
            skill["state"] = Value::String("present".into());
            break;
        }
    }
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    let mut skills = value["skills"].as_array().unwrap().clone();
    let mut observations = value["observations"].as_array().unwrap().clone();
    let phases = [
        "plan",
        "fold",
        "implement",
        "address-findings",
        "address-ci",
        "advance",
    ];
    let names = ["plan", "foldin", "implement", "onboarding"];
    let scopes = ["workspace", "execution"];
    'fill: for phase in phases {
        for name in names {
            for scope in scopes {
                if skills.len() >= 17 {
                    break 'fill;
                }
                let key = (phase, name, scope);
                let exists = skills.iter().any(|skill| {
                    skill["phase"] == phase && skill["name"] == name && skill["scope"] == scope
                });
                if exists {
                    continue;
                }
                let mut skill = skills[0].clone();
                skill["phase"] = Value::String(phase.into());
                skill["name"] = Value::String(name.into());
                skill["scope"] = Value::String(scope.into());
                skill["state"] = Value::String("absent".into());
                skill["path"] = Value::String(format!("C:\\unused\\{phase}-{name}-{scope}"));
                skill["digest"] = Value::Null;
                skill["size"] = Value::Null;
                skill["regular"] = Value::Bool(false);
                skill["reparse"] = Value::Bool(false);
                let mut observation = observations[0].clone();
                observation["phase"] = Value::String(phase.into());
                observation["name"] = Value::String(name.into());
                observation["outcome"] = Value::String("absent".into());
                observation["observed"] = Value::String("metadata".into());
                observation["reason"] = Value::String("not-found".into());
                skills.push(skill);
                observations.push(observation);
                let _ = key;
            }
        }
    }
    assert!(skills.len() >= 17);
    value["skills"] = Value::Array(skills);
    value["observations"] = Value::Array(observations);
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    let mut state = world.state.clone();
    let long = "t".repeat(4097);
    state.track_id = Some(long.clone());
    locked(&world, &state);
    let mut prepared = injection(&world, "implement", 7);
    prepared.track_id = Some(long);
    assert_eq!(
        reason(&super::read(&world.record, &prepared)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    value["captured_at"] = Value::String("not-rfc3339".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    value["captured_at"] = Value::String("2026-10-06T03:18:02+00:00".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::Malformed
    );
}

#[cfg(windows)]
#[test]
fn t_share_lock_oversize_race_and_leaf_change() {
    let world = fixture();
    locked(&world, &world.state);
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(leaf(&world))
        .unwrap();
    let evidence = read_at(&world, "implement", 7);
    assert!(matches!(
        reason(&evidence),
        NoEvidenceReason::ReadDenied | NoEvidenceReason::ReadError
    ));
    drop(held);
    let mut big = std::fs::read(leaf(&world)).unwrap();
    big.resize(300 * 1024, b' ');
    std::fs::write(leaf(&world), big).unwrap();
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::TooLarge
    );
    locked(&world, &world.state);
    let original = std::fs::read(leaf(&world)).unwrap();
    let _guard = super::BetweenReadsGuard::arm({
        let path = leaf(&world);
        move || {
            std::fs::write(&path, b"{\"replaced\":true}").unwrap();
        }
    });
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::CaptureRace
    );
    drop(_guard);
    std::fs::write(leaf(&world), &original).unwrap();
    let _leaf_guard = super::LeafHookGuard::arm({
        let path = leaf(&world);
        move || {
            std::fs::write(&path, b"short").unwrap();
        }
    });
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::LeafChanged
    );
}

#[cfg(windows)]
#[test]
fn t_root_and_skill_path_gates() {
    let world = fixture();
    let mut relative = world.record.clone();
    relative.state_dir = Some(PathBuf::from("relative-state"));
    let prepared = injection(&world, "implement", 7);
    assert_eq!(
        reason(&super::read(&relative, &prepared)),
        NoEvidenceReason::RelativeStateRoot
    );
    let mut remote = world.record.clone();
    remote.state_dir = Some(PathBuf::from(r"\\server\share\state"));
    assert_eq!(
        reason(&super::read(&remote, &prepared)),
        NoEvidenceReason::RemoteStateRoot
    );
    let mut dotted = world.record.clone();
    dotted.state_dir = Some(PathBuf::from(r"C:\foo\..\bar"));
    assert_eq!(
        reason(&super::read(&dotted, &prepared)),
        NoEvidenceReason::NonNormalComponent
    );
    let mut long = world.record.clone();
    let mut deep = world.record.state_dir.clone().unwrap();
    while super::utf16_len(&deep) < 250 {
        deep.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }
    long.state_dir = Some(deep);
    assert_eq!(
        reason(&super::read(&long, &prepared)),
        NoEvidenceReason::PathTooLong
    );
    locked(&world, &world.state);
    let mut value = load_snapshot(&world);
    value["skills"][0]["path"] = Value::String(r"relative\SKILL.md".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::PathNotAllowed
    );
    let mut value = load_snapshot(&world);
    value["skills"][0]["path"] = Value::String(r"\\server\share\SKILL.md".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::PathNotAllowed
    );
    let mut value = load_snapshot(&world);
    value["skills"][0]["path"] = Value::String(r"C:\Windows\notepad.exe".into());
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::PathNotAllowed
    );
    let mut value = load_snapshot(&world);
    let current = value["skills"][0]["path"].as_str().unwrap().to_string();
    value["skills"][0]["path"] = Value::String(format!("{current}:stream"));
    resign(&mut value);
    store_snapshot(&world, &value);
    assert_eq!(
        reason(&read_at(&world, "implement", 7)),
        NoEvidenceReason::PathNotAllowed
    );
}

#[test]
fn t_resolve_error() {
    let _guard = crate::config::test_env_lock();
    let world = fixture();
    let prepared = injection(&world, "implement", 7);
    unsafe {
        std::env::set_var(crate::config::ENV_COORDINATOR_STATE_DIR, "");
    }
    let evidence = super::read(&world.record, &prepared);
    unsafe {
        std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
    }
    assert_eq!(reason(&evidence), NoEvidenceReason::StateRootUnresolved);
}

#[cfg(windows)]
#[test]
fn t_case_and_readonly_snapshot() {
    let world = fixture();
    locked(&world, &world.state);
    let bytes = std::fs::read(leaf(&world)).unwrap();
    std::fs::remove_file(leaf(&world)).unwrap();
    let upper = leaf(&world).parent().unwrap().join("CURRENT.JSON");
    std::fs::write(&upper, &bytes).unwrap();
    assert!(matches!(
        read_at(&world, "implement", 7),
        Evidence::Current(_)
    ));
    let names: Vec<_> = std::fs::read_dir(leaf(&world).parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.eq_ignore_ascii_case("current.json"))
        .collect();
    assert_eq!(names.len(), 1);
    let mut perms = std::fs::metadata(leaf(&world)).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(leaf(&world), perms).unwrap();
    assert!(matches!(
        read_at(&world, "implement", 7),
        Evidence::Current(_)
    ));
}

#[cfg(windows)]
#[test]
fn w_lock_driver_and_sequence() {
    let world = fixture();
    let outside =
        super::write_for_epoch(&world.record, &world.state, ProducerHook::RunBeginFreshRun);
    assert_eq!(outside, WriteOutcome::NotLocked);
    assert!(!leaf(&world).exists());
    assert!(
        evidence_details(&world.record)
            .iter()
            .any(|line| line.contains("reason=not-locked"))
    );
    let record = world.record.clone();
    let state = world.state.clone();
    let other = std::thread::spawn(move || {
        super::write_for_epoch(&record, &state, ProducerHook::RunBeginFreshRun)
    })
    .join()
    .unwrap();
    assert_eq!(other, WriteOutcome::NotLocked);
    assert!(!leaf(&world).exists());
    let mut wrong = world.record.clone();
    wrong.id = "p-wrong".into();
    let outcome = crate::state::with_run_state_lock(&world.record, || {
        Ok(super::write_for_epoch(
            &wrong,
            &world.state,
            ProducerHook::RunBeginFreshRun,
        ))
    })
    .unwrap();
    assert_eq!(outcome, WriteOutcome::NotLocked);
    assert_eq!(locked(&world, &world.state), WriteOutcome::Written);
    let value = load_snapshot(&world);
    assert_eq!(value["run_epoch"], 7);
    assert_eq!(value["sequence"], 1);
    assert_eq!(value["producer"]["hook"], "run_begin_fresh_run");
    assert_eq!(value["producer"]["phase"], "plan");
    value["sequence"].as_u64().unwrap();
    let mut forced = value.clone();
    forced["sequence"] = Value::from(41);
    resign(&mut forced);
    store_snapshot(&world, &forced);
    locked(&world, &world.state);
    assert_eq!(load_snapshot(&world)["sequence"], 42);
    std::fs::write(leaf(&world), b"{not json").unwrap();
    locked(&world, &world.state);
    assert_eq!(load_snapshot(&world)["sequence"], 1);
    let mut stub = world.state.clone();
    stub.driver = crate::workflow::WorkflowDriver::Stub;
    let before = std::fs::read(leaf(&world)).unwrap();
    let skipped = crate::state::with_run_state_lock(&world.record, || {
        Ok(super::write_for_epoch(
            &world.record,
            &stub,
            ProducerHook::RunBeginFreshRun,
        ))
    })
    .unwrap();
    assert_eq!(skipped, WriteOutcome::Skipped);
    assert_eq!(std::fs::read(leaf(&world)).unwrap(), before);
}

#[cfg(windows)]
#[test]
fn w_autostart_alias_isolation_and_epoch_dir() {
    let world = fixture();
    let mut state = world.state.clone();
    state.run_epoch = 3;
    state.failure_class = None;
    crate::state::with_run_state_lock(&world.record, || {
        crate::workflow::auto_start(&world.record, &mut state, "0124-Next");
        Ok(())
    })
    .unwrap();
    let value = load_snapshot(&world);
    assert_eq!(value["run_epoch"], 4);
    assert_eq!(value["producer"]["hook"], "workflow_auto_start");
    assert_eq!(value["producer"]["phase"], "plan");
    assert_eq!(state.status, crate::state::RunStatus::Running);
    assert_eq!(state.phase, "plan");

    let mut aliased = fixture();
    aliased
        .record
        .skill_aliases
        .insert("implement".into(), "implement-track".into());
    aliased
        .record
        .skill_aliases
        .insert("plan".into(), "plan-track".into());
    aliased
        .record
        .skill_aliases
        .insert("foldin".into(), "fold-in".into());
    plant(&aliased.record, 7, "implement", "execution", "impl\n");
    plant(&aliased.record, 7, "onboarding", "execution", "onboard\n");
    plant(&aliased.record, 7, "plan", "workspace", "plan\n");
    plant(&aliased.record, 7, "foldin", "workspace", "fold\n");
    locked(&aliased, &aliased.state);
    let value = load_snapshot(&aliased);
    let implement = skill_path(&value, "implement", "execution");
    assert!(implement.contains("implement-track"));
    assert!(skill_path(&value, "plan", "workspace").contains("plan-track"));
    assert!(skill_path(&value, "foldin", "workspace").contains("fold-in"));
    let skills = value["skills"].as_array().unwrap();
    for pair in super::capture_pairs() {
        let skill = skills
            .iter()
            .find(|skill| {
                skill["phase"] == pair.phase
                    && skill["name"] == pair.name
                    && skill["scope"] == pair.scope
            })
            .unwrap_or_else(|| panic!("missing {} {} {}", pair.phase, pair.name, pair.scope));
        let path = skill["path"].as_str().unwrap();
        let dir = crate::workflow::reuse::skill_directory_name(&aliased.record, pair.name);
        assert!(
            path.contains(&dir),
            "{} {} {} path {path} missing {dir}",
            pair.phase,
            pair.name,
            pair.scope
        );
        assert_eq!(skill["state"], "present");
    }
    let expected =
        crate::workflow::reuse::execution_skill_path_for_epoch(&aliased.record, 7, "implement");
    assert_eq!(PathBuf::from(&implement), expected);

    let mut isolated = fixture();
    isolated.record.worktree_isolation = true;
    let state_dir = isolated.record.state_dir.clone().unwrap();
    std::fs::create_dir_all(state_dir.join("worktrees").join("5")).unwrap();
    std::fs::create_dir_all(state_dir.join("worktrees").join("4")).unwrap();
    let mut disk = isolated.state.clone();
    disk.run_epoch = 4;
    crate::state::save_run_state(&isolated.record, &disk).unwrap();
    isolated.state.run_epoch = 5;
    let path = plant(
        &isolated.record,
        5,
        "implement",
        "execution",
        "epoch-five\n",
    );
    assert!(path.to_string_lossy().contains("worktrees"));
    locked(&isolated, &isolated.state);
    let recorded = skill_path(&load_snapshot(&isolated), "implement", "execution");
    assert!(recorded.contains("worktrees\\5") || recorded.contains("worktrees/5"));
    assert!(!recorded.contains("worktrees\\4") && !recorded.contains("worktrees/4"));
}

#[cfg(windows)]
fn skill_path(value: &Value, name: &str, scope: &str) -> String {
    value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == name && skill["scope"] == scope)
        .unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string()
}

#[cfg(windows)]
#[test]
fn w_atomic_crash_disk_and_archive() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("blob.json");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let target = target.clone();
        let stop = stop.clone();
        let seen = seen.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(bytes) = std::fs::read(&target) {
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        seen.lock().unwrap().push(value);
                    } else {
                        seen.lock().unwrap().push(Value::Null);
                    }
                }
            }
        })
    };
    let mut handles = Vec::new();
    for index in 0..64 {
        let target = target.clone();
        handles.push(std::thread::spawn(move || {
            let bytes = serde_json::to_vec(&serde_json::json!({"n": index})).unwrap();
            let _ = crate::persist::atomic_write(&target, &bytes);
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|value| value != &Value::Null)
    );

    let world = fixture();
    locked(&world, &world.state);
    let old = load_snapshot(&world);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel::<()>();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let releaser = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let _ = started_rx.recv_timeout(Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(50));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = reader_done_rx.recv_timeout(Duration::from_secs(5));
            let _ = release_tx.send(());
        })
    };
    let reader = {
        let path = leaf(&world);
        let observations = observations.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(bytes) = std::fs::read(&path) {
                    let mark = match serde_json::from_slice::<Value>(&bytes) {
                        Ok(value) if super::check_digest(&value).is_ok() => {
                            value["digest"].as_str().unwrap_or("missing").to_string()
                        }
                        Ok(_) => "bad-digest".into(),
                        Err(_) => "torn".into(),
                    };
                    observations.lock().unwrap().push(mark);
                }
            }
            let _ = reader_done_tx.send(());
        })
    };
    let _guard = crate::persist::AtomicWriteHookGuard::arm("current.json", move |_| {
        let _ = started_tx.send(());
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        Ok(())
    });
    let mut next = world.state.clone();
    next.run_epoch = 8;
    assert_eq!(locked(&world, &next), WriteOutcome::Written);
    drop(_guard);
    releaser.join().unwrap();
    reader.join().unwrap();
    let new = load_snapshot(&world);
    let old_digest = old["digest"].as_str().unwrap().to_string();
    let new_digest = new["digest"].as_str().unwrap().to_string();
    assert_ne!(old_digest, new_digest);
    let sampled = observations.lock().unwrap().clone();
    assert!(!sampled.is_empty());
    for digest in sampled.iter() {
        assert!(digest == &old_digest || digest == &new_digest);
    }

    let live = std::fs::read(leaf(&world)).unwrap();
    let _guard = crate::persist::AtomicWriteHookGuard::arm("current.json", |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected",
        ))
    });
    let failed = locked(&world, &world.state);
    assert_eq!(failed, WriteOutcome::Failed);
    assert_eq!(std::fs::read(leaf(&world)).unwrap(), live);
    assert!(
        evidence_details(&world.record)
            .iter()
            .any(|line| line.contains("reason=io"))
    );
    let temps: Vec<_> = std::fs::read_dir(leaf(&world).parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".current.json.")
        })
        .collect();
    assert!(temps.is_empty());
    drop(_guard);

    let _guard = crate::persist::AtomicWriteHookGuard::arm("current.json", |_| {
        panic!("injected crash");
    });
    let path = leaf(&world);
    let crashed = crate::state::with_run_state_lock(&world.record, || {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::write_for_epoch(&world.record, &world.state, ProducerHook::RunBeginFreshRun)
        }));
        Ok(panicked.is_err())
    })
    .unwrap();
    assert!(crashed);
    drop(_guard);
    let orphan = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".current.json.") && name.ends_with(".tmp")
        })
        .expect("orphan temp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(orphan.path())
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60))
        .unwrap();
    drop(file);
    locked(&world, &world.state);
    let still = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with(".current.json.") && name.ends_with(".tmp")
        });
    assert!(!still);

    let names = Arc::new(Mutex::new(Vec::new()));
    let logged = names.clone();
    let _guard = crate::persist::AtomicWriteHookGuard::arm("", move |name| {
        logged.lock().unwrap().push(name.to_string());
        Ok(())
    });
    let mut fresh = fixture();
    locked(&fresh, &fresh.state);
    drop(_guard);
    let logged = names.lock().unwrap().clone();
    assert_eq!(logged.first().map(String::as_str), Some("current.json"));
    assert!(
        logged
            .iter()
            .any(|name| name.ends_with(".json") && name != "current.json")
    );

    for epoch in 1..=10 {
        fresh.state.run_epoch = epoch;
        locked(&fresh, &fresh.state);
    }
    let archive = fresh
        .record
        .state_dir
        .as_ref()
        .unwrap()
        .join("trusted-evidence")
        .join("archive");
    let numeric = numeric_archives(&archive);
    assert_eq!(numeric.len(), 8);
    assert!(!archive.join(format!("{:020}.json", 1)).exists());
    assert!(!archive.join(format!("{:020}.json", 2)).exists());
    assert!(archive.join(format!("{:020}.json", 10)).exists());
    std::fs::write(archive.join("notes.json"), b"keep").unwrap();
    std::fs::create_dir(archive.join("extra.json")).unwrap();
    fresh.state.run_epoch = 11;
    let archive_name = format!("{:020}.json", 11);
    let _guard = crate::persist::AtomicWriteHookGuard::arm(archive_name, |_| {
        Err(std::io::Error::other("archive down"))
    });
    let failed = locked(&fresh, &fresh.state);
    assert_eq!(failed, WriteOutcome::Failed);
    drop(_guard);
    assert!(archive.join("notes.json").is_file());
    assert!(archive.join("extra.json").is_dir());
    assert!(
        evidence_details(&fresh.record)
            .iter()
            .any(|line| line.contains("reason=archive"))
    );
    assert!(
        load_snapshot(&fresh)["run_epoch"] == 11
            || load_snapshot(&fresh)["sequence"].as_u64().unwrap() >= 1
    );
}

#[cfg(windows)]
fn numeric_archives(archive: &Path) -> Vec<String> {
    std::fs::read_dir(archive)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(".json")?;
            if stem.len() == 20
                && stem.bytes().all(|byte| byte.is_ascii_digit())
                && entry.path().is_file()
            {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(windows)]
#[test]
fn w_reparse_archive_junction_and_failed_save() {
    let world = fixture();
    let root = world
        .record
        .state_dir
        .as_ref()
        .unwrap()
        .join("trusted-evidence");
    let target = world._dir.path().join("reparse-target");
    junction(&root, &target);
    assert_eq!(locked(&world, &world.state), WriteOutcome::Failed);
    assert!(!target.join("current.json").exists());
    assert!(
        evidence_details(&world.record)
            .iter()
            .any(|line| line.contains("reason=reparse"))
    );
    std::fs::remove_dir(&root).unwrap();

    locked(&world, &world.state);
    let archive = root.join("archive");
    std::fs::remove_dir_all(&archive).unwrap();
    let marker_dir = world._dir.path().join("archive-target");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(marker_dir.join("marker.txt"), b"stay").unwrap();
    junction(&archive, &marker_dir);
    let mut next = world.state.clone();
    next.run_epoch = 9;
    assert_eq!(locked(&world, &next), WriteOutcome::Failed);
    assert!(leaf(&world).is_file());
    assert!(marker_dir.join("marker.txt").is_file());
    let planted = std::fs::read_dir(&marker_dir).unwrap().count();
    assert_eq!(planted, 1);
    assert!(
        evidence_details(&world.record)
            .iter()
            .any(|line| line.contains("reason=reparse"))
    );

    let reused = fixture();
    let path = plant(&reused.record, 7, "implement", "execution", "one\n");
    locked(&reused, &reused.state);
    std::fs::write(&path, "two\n").unwrap();
    assert_eq!(
        reason(&read_at(&reused, "implement", 7)),
        NoEvidenceReason::FactDrifted
    );
    let mut prepared = injection(&reused, "implement", 7);
    prepared.track_id = Some("changed".into());
    assert_ne!(
        reason(&super::read(&reused.record, &prepared)),
        NoEvidenceReason::FactDrifted
    );
    assert!(!matches!(
        super::read(&reused.record, &prepared),
        Evidence::Current(_)
    ));
}

#[cfg(windows)]
#[test]
fn w_failed_save_reuse() {
    let world = fixture();
    let skill = plant(&world.record, 7, "implement", "execution", "epoch-seven\n");
    crate::state::save_run_state(&world.record, &world.state).unwrap();
    let persisted = std::fs::read(crate::state::run_state_path(&world.record).unwrap()).unwrap();
    let mut bumped = world.state.clone();
    bumped.run_epoch = 8;
    bumped.track_id = Some("0123-Unsaved".into());
    assert_eq!(locked(&world, &bumped), WriteOutcome::Written);
    let _guard = crate::persist::AtomicWriteHookGuard::arm("run-state.json", |_| {
        Err(std::io::Error::other("save failed"))
    });
    let saved = crate::state::save_run_state(&world.record, &bumped);
    assert!(saved.is_err());
    drop(_guard);
    assert_eq!(
        std::fs::read(crate::state::run_state_path(&world.record).unwrap()).unwrap(),
        persisted
    );
    std::fs::write(&skill, "changed-after-failed-save\n").unwrap();
    let reused = read_at(&world, "implement", 7);
    assert!(!matches!(reused, Evidence::Current(_)));
    assert_eq!(reason(&reused), NoEvidenceReason::WrongEpoch);

    let world = fixture();
    let skill = plant(&world.record, 7, "implement", "execution", "same-epoch\n");
    crate::state::save_run_state(&world.record, &world.state).unwrap();
    assert_eq!(locked(&world, &world.state), WriteOutcome::Written);
    let persisted = std::fs::read(crate::state::run_state_path(&world.record).unwrap()).unwrap();
    let mut other = world.state.clone();
    other.track_id = Some("0123-Unsaved".into());
    let _guard = crate::persist::AtomicWriteHookGuard::arm("run-state.json", |_| {
        Err(std::io::Error::other("save failed"))
    });
    assert!(crate::state::save_run_state(&world.record, &other).is_err());
    drop(_guard);
    assert_eq!(
        std::fs::read(crate::state::run_state_path(&world.record).unwrap()).unwrap(),
        persisted
    );
    std::fs::write(&skill, "facts-changed\n").unwrap();
    let reused = read_at(&world, "implement", 7);
    assert!(!matches!(reused, Evidence::Current(_)));
    assert_eq!(reason(&reused), NoEvidenceReason::FactDrifted);
}

#[test]
fn w_inventory_and_structural_gates() {
    let run = production("src/run.rs");
    let workflow = production("src/workflow/mod.rs");
    assert_eq!(run.matches("write_for_epoch(").count(), 1);
    assert_eq!(workflow.matches("write_for_epoch(").count(), 1);
    assert_eq!(
        run.matches("state.run_epoch = state.run_epoch.saturating_add(1)")
            .count(),
        1
    );
    assert_eq!(
        workflow
            .matches("state.run_epoch = state.run_epoch.saturating_add(1)")
            .count(),
        1
    );
    let writer = production("src/workflow/trusted_evidence/writer.rs");
    let reader_src = production("src/workflow/trusted_evidence/reader.rs");
    let module_src = production("src/workflow/trusted_evidence/mod.rs");
    for token in [
        ".write(",
        ".append(",
        ".truncate(",
        ".create(",
        ".create_new(",
        "fs::write",
        "File::create",
        "create_new",
        "set_len",
        "set_permissions",
        "fs::copy",
        "OpenOptions",
    ] {
        assert!(!writer.contains(token), "{token} in writer");
        assert!(!reader_src.contains(token), "{token} in reader");
        assert!(!module_src.contains(token), "{token} in module");
    }
    assert!(!writer.contains("is_symlink"));
    let world = fixture();
    let path = plant(
        &world.record,
        7,
        "implement",
        "execution",
        "unchanged-bytes\n",
    );
    plant(&world.record, 7, "plan", "workspace", "workspace-bytes\n");
    let execution_root = world
        .record
        .execution_repo
        .as_ref()
        .unwrap()
        .join(".agents")
        .join("skills");
    let workspace_root = world.record.path.join(".agents").join("skills");
    let execution_before = skill_tree_snapshot(&execution_root);
    let workspace_before = skill_tree_snapshot(&workspace_root);
    assert!(
        execution_before
            .iter()
            .any(|(name, _, _)| name.contains("implement"))
    );
    assert!(
        workspace_before
            .iter()
            .any(|(name, _, _)| name.contains("plan"))
    );
    let before = std::fs::read(&path).unwrap();
    locked(&world, &world.state);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(skill_tree_snapshot(&execution_root), execution_before);
    assert_eq!(skill_tree_snapshot(&workspace_root), workspace_before);
    assert!(writer.contains("is_epoch_leaf"));
    let reader = production("src/workflow/trusted_evidence/reader.rs");
    let module = production("src/workflow/trusted_evidence/mod.rs");
    for banned in ["read_dir", "glob", "canonicalize", "read_link", "archive"] {
        assert!(!reader.contains(banned), "{banned} in reader");
        assert!(!module.contains(banned), "{banned} in mod");
    }
    let combined = format!("{module}\n{reader}\n{writer}");
    for banned in ["ureq", "std::net", "reqwest", "http"] {
        assert!(!combined.contains(banned), "{banned}");
    }
    for banned in ["evidence.md", "decision", "evidence_stamp"] {
        assert!(!reader.contains(banned), "{banned}");
    }
    assert!(!reader.contains("modified()"));
    assert!(reader.contains("last_write_time"));
    assert_eq!(reader.matches("current.json").count(), 1);
    assert!(!reader.contains("format!"));
    for banned in [
        "active_epoch_dir",
        "load_run_state",
        "missing_required_skill_paths",
        "execution_skill_path",
    ] {
        assert!(!reader.contains(banned), "{banned}");
    }
    assert!(!reader.contains("is_symlink"));
    let phases = crate::workflow::block::phases();
    assert_eq!(
        phases,
        [
            "plan",
            "fold",
            "implement",
            "address-findings",
            "address-ci",
            "address-review",
            "advance",
        ]
    );
    assert!(!super::SKILL_PHASES.contains(&"address-review"));
    let classes: Vec<_> = crate::outcome::FailureClass::ALL
        .into_iter()
        .map(|class| class.as_str())
        .collect();
    assert_eq!(
        classes,
        [
            "permission",
            "model_exhaustion",
            "difficulty",
            "harness_crash",
            "timeout",
            "ci_failed",
            "skill_missing",
            "review_never_settles",
            "gate",
            "blocked",
            "review_comments",
        ]
    );
    let cli = source("src/cli.rs");
    assert!(!cli.contains("trusted_evidence"));
    assert!(!source("Cargo.toml").contains("trusted-evidence"));
}

#[cfg(windows)]
#[test]
fn s9_path_allowed_precedes_skill_open() {
    let world = fixture();
    plant(&world.record, 7, "implement", "execution", "body\n");
    plant(&world.record, 7, "onboarding", "execution", "body\n");
    locked(&world, &world.state);
    let _ = super::take_path_counters();
    let counter = super::ReadCounterGuard::arm();
    let _after = super::AfterReadGuard::arm(|| {});
    assert!(matches!(
        read_at(&world, "implement", 7),
        Evidence::Current(_)
    ));
    assert_eq!(counter.count(), 1);
    let (allowed, opens) = super::take_path_counters();
    assert!(opens >= 1);
    assert!(allowed >= opens);
}

#[cfg(windows)]
#[test]
fn reader_panic_hook_is_unwind_safe() {
    let world = fixture();
    locked(&world, &world.state);
    let _guard = super::PanicOnceGuard::arm();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_at(&world, "implement", 7)
    }));
    assert!(panicked.is_err());
    drop(_guard);
    assert!(matches!(
        read_at(&world, "implement", 7),
        Evidence::Current(_)
    ));
}
