//! Engine tests for the 0110 swap (isolated `COORDINATOR_HOME`; never the live config).

use super::*;
use crate::config::{self, MachineConfig, RoleBinding};
use crate::harness::preflight::{ScriptedProbe, install_test_probe};
use crate::harness::roles::ROLE_FOLD;
use crate::workflow::graph::{PHASE_IMPLEMENT, PHASE_PLAN, PHASE_PLAN_REVIEW};
use chrono::Utc;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::MutexGuard;
use tempfile::TempDir;

const FEATURE_ENV: &str = "COORDINATOR_FEATURE_MODEL_SWAP";

struct SwapHome {
    _lock: MutexGuard<'static, ()>,
    home: TempDir,
    prev: Vec<(&'static str, Option<OsString>)>,
}

impl SwapHome {
    fn enter() -> Self {
        let lock = config::test_env_lock();
        let keys: Vec<&'static str> = vec![
            config::ENV_COORDINATOR_HOME,
            config::ENV_COORDINATOR_STATE_DIR,
            FEATURE_ENV,
            crate::harness::grok::ENV_GROK_BIN,
            crate::harness::grok::ENV_CURSOR_BIN,
            config::ENV_COORDINATOR_CODEX_BIN,
            config::ENV_COORDINATOR_CLAUDE_BIN,
            config::ENV_COORDINATOR_OPENCODE_BIN,
            config::ENV_COORDINATOR_AGY_BIN,
        ];
        let prev = keys
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(config::ENV_COORDINATOR_HOME, home.path());
            std::env::remove_var(config::ENV_COORDINATOR_STATE_DIR);
            std::env::remove_var(FEATURE_ENV);
            for key in &keys[3..] {
                std::env::remove_var(key);
            }
        }
        Self {
            _lock: lock,
            home,
            prev,
        }
    }

    fn dummy(&self, name: &str) -> std::path::PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(&path, b"stub").unwrap();
        path
    }

    fn write_bindings(&self, mutate: impl FnOnce(&mut BTreeMap<String, RoleBinding>)) {
        let mut cfg = MachineConfig::default();
        mutate(&mut cfg.role_bindings);
        config::save_machine_config(&cfg).unwrap();
    }

    fn config_bytes(&self) -> Vec<u8> {
        std::fs::read(config::machine_config_path().unwrap()).unwrap_or_default()
    }

    fn binding(&self, role: &str) -> RoleBinding {
        config::load_machine_config().unwrap().role_bindings[role].clone()
    }
}

impl Drop for SwapHome {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in &self.prev {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// A codex adapter role with a dummy ACP shim and a companion `codex` pin.
fn codex_planner(home: &SwapHome, model: Option<&str>) -> (std::path::PathBuf, std::path::PathBuf) {
    let shim = home.dummy("codex-acp.exe");
    let companion = home.dummy("codex.exe");
    let shim_s = shim.to_string_lossy().to_string();
    let model = model.map(str::to_string);
    home.write_bindings(|b| {
        let planner = b.get_mut(ROLE_PLANNER).unwrap();
        planner.harness = "codex".into();
        planner.command = shim_s;
        planner.model = model;
    });
    unsafe {
        std::env::set_var(config::ENV_COORDINATOR_CODEX_BIN, &companion);
    }
    (shim, companion)
}

fn ready_probe() -> ScriptedProbe {
    ScriptedProbe::ready()
}

#[test]
fn swap_applies_field_exact_and_rollback_restores_the_previous_object() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("gpt-6.1-sol[medium]"));
    let _probe = install_test_probe(ready_probe());
    let before = home.config_bytes();
    let other_before = home.binding("implementor");

    let outcome = swap(
        "planner",
        PartialBinding {
            model: Some("gpt-6-luna".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(outcome.applied && !outcome.deferred, "{outcome:?}");
    let applied = home.binding("planner");
    assert_eq!(applied.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(applied.harness, "codex");
    assert_eq!(home.binding("implementor"), other_before);
    let snapshot = outcome.snapshot.clone().expect("snapshot");
    let snap_cfg = config::load_machine_config_at(&snapshot).unwrap();
    assert_eq!(
        snap_cfg.role_bindings["planner"].model.as_deref(),
        Some("gpt-6.1-sol[medium]")
    );
    let ledger = load_ledger().unwrap();
    assert_eq!(
        ledger.last["planner"].snapshot.as_deref(),
        Some(snapshot.as_path())
    );
    assert!(!ledger.errors.contains_key("planner"));

    let rolled = rollback("planner", None).unwrap();
    assert!(rolled.applied);
    assert_eq!(
        home.binding("planner").model.as_deref(),
        Some("gpt-6.1-sol[medium]")
    );
    assert_eq!(home.binding("implementor"), other_before);
    let ledger = load_ledger().unwrap();
    assert_eq!(ledger.last["planner"].action, SwapAction::Rollback);
    assert_ne!(
        ledger.last["planner"].snapshot.as_ref(),
        Some(&snapshot),
        "rollback takes its own pre-rollback snapshot"
    );
    let _ = before;
}

#[test]
fn two_operations_in_the_same_second_get_distinct_snapshots() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());

    let first = swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let second = swap(
        "planner",
        PartialBinding {
            model: Some("model-c".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let s1 = first.snapshot.unwrap();
    let s2 = second.snapshot.unwrap();
    assert_ne!(s1, s2, "same-second snapshots must not collide");
    assert!(s1.is_file() && s2.is_file());
    assert_eq!(
        config::load_machine_config_at(&s1).unwrap().role_bindings["planner"]
            .model
            .as_deref(),
        Some("model-a")
    );
    assert_eq!(
        config::load_machine_config_at(&s2).unwrap().role_bindings["planner"]
            .model
            .as_deref(),
        Some("model-b")
    );
    // Rollback restores this operation's predecessor (model-b), not model-a.
    rollback("planner", None).unwrap();
    assert_eq!(home.binding("planner").model.as_deref(), Some("model-b"));
}

#[test]
fn refusal_leaves_the_config_bytes_and_ledger_untouched() {
    let home = SwapHome::enter();
    let missing = home.home.path().join("missing-codex-acp.exe");
    let companion = home.dummy("codex.exe");
    let missing_s = missing.to_string_lossy().to_string();
    home.write_bindings(|b| {
        let planner = b.get_mut(ROLE_PLANNER).unwrap();
        planner.harness = "codex".into();
        planner.command = missing_s;
    });
    unsafe {
        std::env::set_var(config::ENV_COORDINATOR_CODEX_BIN, &companion);
    }
    let _probe = install_test_probe(ready_probe());
    let before = home.config_bytes();
    let err = swap(
        "planner",
        PartialBinding {
            model: Some("gpt-6-luna".into()),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("missing-codex-acp") || err.contains("not found"),
        "err={err}"
    );
    assert_eq!(home.config_bytes(), before);
    assert!(!load_ledger().unwrap().last.contains_key("planner"));
}

#[test]
fn opencode_variant_probe_matches_the_base_and_fails_when_absent() {
    let home = SwapHome::enter();
    let opencode = home.dummy("opencode.exe");
    let opencode_s = opencode.to_string_lossy().to_string();
    home.write_bindings(|b| {
        let slot = b.get_mut(ROLE_REVIEWER_OPENCODE).unwrap();
        slot.harness = "opencode".into();
        slot.command = opencode_s;
        slot.model = Some("fixture-provider/fixture-model#high".into());
    });
    let probe = ScriptedProbe::ready().with_models(
        "opencode",
        0,
        "fixture-provider/fixture-model\nanother/model\n",
        false,
    );
    let _probe = install_test_probe(probe);
    let outcome = swap(
        "plan_reviewer_opencode",
        PartialBinding {
            model: Some("fixture-provider/fixture-model#high".into()),
            ..Default::default()
        },
    );
    // model is unchanged -> no-op refusal; use a different but listed base is impossible,
    // so assert the probe path directly instead.
    assert!(outcome.is_err());
    let receipt = crate::harness::preflight::probe_target(
        "plan_reviewer_opencode",
        &home.binding("plan_reviewer_opencode"),
    )
    .expect("base listed");
    match receipt.model_check {
        ModelCheck::Pass { ref detail } => {
            assert!(detail.contains("catalog-defined"), "{detail}")
        }
        other => panic!("expected Pass, got {other:?}"),
    }

    // Base absent -> Fail -> refusal.
    home.write_bindings(|b| {
        b.get_mut(ROLE_REVIEWER_OPENCODE).unwrap().model =
            Some("fixture-provider/absent#high".into());
    });
    let receipt = crate::harness::preflight::probe_target(
        "plan_reviewer_opencode",
        &home.binding("plan_reviewer_opencode"),
    );
    let err = receipt.unwrap_err().message();
    assert!(err.contains("not listed"), "err={err}");
}

#[test]
fn model_swap_gate_off_refuses_write_but_status_still_works() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    unsafe {
        std::env::set_var(FEATURE_ENV, "off");
    }
    let before = home.config_bytes();
    let err = swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("model_swap is disabled"), "err={err}");
    assert!(err.contains("Env"), "err names the source: {err}");
    assert_eq!(home.config_bytes(), before);
    assert!(status().is_ok());
    unsafe {
        std::env::remove_var(FEATURE_ENV);
    }
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn rollback_and_prove_and_status_are_not_gated() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    unsafe {
        std::env::set_var(FEATURE_ENV, "off");
    }
    let rolled = rollback("planner", None).unwrap();
    assert!(rolled.applied);
    assert!(status().is_ok());
    unsafe {
        std::env::remove_var(FEATURE_ENV);
    }
}

#[test]
fn crashed_lock_blocks_mutation_and_unlock_recovers_it() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let lock = lock_path().unwrap();
    std::fs::write(
        &lock,
        serde_json::json!({"pid": 999_999_u32, "started_at": Utc::now()}).to_string(),
    )
    .unwrap();
    let err = swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("--unlock"), "err={err}");
    let info = unlock().unwrap();
    assert_eq!(info.pid, Some(999_999));
    assert!(!lock.exists());
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn deferred_swap_applies_at_the_phase_boundary_in_process() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_IMPLEMENT.into();
    state.track_id = Some("0110".into());
    state.run_epoch = 3;
    crate::state::save_run_state(&record, &state).unwrap();
    // implementor is the phase role; use it for the deferred request.
    let impl_shim = home.dummy("codex-acp.exe");
    let impl_shim_s = impl_shim.to_string_lossy().to_string();
    home.write_bindings(|b| {
        let implementor = b.get_mut(ROLE_IMPLEMENTOR).unwrap();
        implementor.harness = "codex".into();
        implementor.command = impl_shim_s;
        implementor.model = Some("model-a".into());
    });
    let before = home.config_bytes();
    let deferred = swap(
        "implementor",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(deferred.deferred && !deferred.applied);
    assert!(
        deferred.message.contains("project="),
        "{}",
        deferred.message
    );
    assert!(deferred.message.contains("run_epoch=3"));
    assert!(deferred.message.contains("no serve restart"));
    assert_eq!(home.config_bytes(), before);
    assert!(load_ledger().unwrap().pending.contains_key("implementor"));

    let outcomes = apply_pending_for_phase("implement");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(outcomes[0].applied);
    assert_eq!(
        home.binding("implementor").model.as_deref(),
        Some("model-b")
    );
    let ledger = load_ledger().unwrap();
    assert!(!ledger.pending.contains_key("implementor"));
    assert!(!ledger.errors.contains_key("implementor"));
}

#[test]
fn a_second_deferred_request_for_the_same_role_is_refused_until_cancel() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN.into();
    crate::state::save_run_state(&record, &state).unwrap();
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let err = swap(
        "planner",
        PartialBinding {
            model: Some("model-c".into()),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("--cancel"), "err={err}");
    cancel("planner").unwrap();
    assert!(!load_ledger().unwrap().pending.contains_key("planner"));
}

#[test]
fn boundary_probe_failure_keeps_the_previous_binding_and_records_probe_failed() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN.into();
    crate::state::save_run_state(&record, &state).unwrap();
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // Break the runtime before the boundary applies.
    std::fs::remove_file(home.home.path().join("codex-acp.exe")).unwrap();
    let before = home.config_bytes();
    let outcomes = apply_pending_for_phase("plan");
    assert!(outcomes.is_empty());
    assert_eq!(home.config_bytes(), before);
    let ledger = load_ledger().unwrap();
    assert!(!ledger.pending.contains_key("planner"));
    assert_eq!(ledger.errors["planner"].kind, SwapErrorKind::ProbeFailed);
}

#[test]
fn previous_changed_since_request_is_not_clobbered() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN.into();
    crate::state::save_run_state(&record, &state).unwrap();
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // A raw writer (out of scope for the lock) changes the target.
    let mut cfg = config::load_machine_config().unwrap();
    cfg.role_bindings.get_mut("planner").unwrap().model = Some("raw-writer".into());
    config::save_machine_config(&cfg).unwrap();
    let outcomes = apply_pending_for_phase("plan");
    assert!(outcomes.is_empty());
    assert_eq!(home.binding("planner").model.as_deref(), Some("raw-writer"));
    let ledger = load_ledger().unwrap();
    assert_eq!(
        ledger.errors["planner"].kind,
        SwapErrorKind::PreviousChanged
    );
    assert!(!ledger.pending.contains_key("planner"));
}

#[test]
fn gate_off_at_the_boundary_keeps_the_pending_for_a_later_tick() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN.into();
    crate::state::save_run_state(&record, &state).unwrap();
    swap(
        "planner",
        PartialBinding {
            model: Some("model-b".into()),
            ..Default::default()
        },
    )
    .unwrap();
    unsafe {
        std::env::set_var(FEATURE_ENV, "off");
    }
    assert!(apply_pending_for_phase("plan").is_empty());
    assert!(load_ledger().unwrap().pending.contains_key("planner"));
    unsafe {
        std::env::remove_var(FEATURE_ENV);
    }
    assert_eq!(apply_pending_for_phase("plan").len(), 1);
}

#[test]
fn corrupt_ledger_fails_open_and_keeps_the_old_binding() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    std::fs::write(ledger_path().unwrap(), b"{not json").unwrap();
    let before = home.config_bytes();
    assert!(apply_pending_for_phase("plan").is_empty());
    assert_eq!(home.config_bytes(), before);
}

#[test]
fn plan_review_pending_applies_before_its_binding_is_read() {
    let home = SwapHome::enter();
    let opencode = home.dummy("opencode.exe");
    let opencode_s = opencode.to_string_lossy().to_string();
    home.write_bindings(|b| {
        let slot = b.get_mut(ROLE_REVIEWER_OPENCODE).unwrap();
        slot.harness = "opencode".into();
        slot.command = opencode_s;
        slot.model = Some("fixture-provider/fixture-model".into());
    });
    let probe = ScriptedProbe::ready().with_models(
        "opencode",
        0,
        "fixture-provider/fixture-model\nfixture-provider/fixture-model:2\n",
        false,
    );
    let _probe = install_test_probe(probe);
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN_REVIEW.into();
    crate::state::save_run_state(&record, &state).unwrap();
    swap(
        "plan_reviewer_opencode",
        PartialBinding {
            model: Some("fixture-provider/fixture-model:2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let outcomes = apply_pending_for_roles(&[ROLE_REVIEWER_OPENCODE]);
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(
        home.binding("plan_reviewer_opencode").model.as_deref(),
        Some("fixture-provider/fixture-model:2")
    );
}

#[test]
fn prove_is_read_only_and_does_not_write_last_used_json() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let last_used = home.home.path().join("last-used.json");
    let before = std::fs::read(&last_used).ok();
    let view = prove("planner", Some(&record.id)).unwrap();
    assert!(!view.proven);
    assert!(
        view.reason
            .as_deref()
            .unwrap_or_default()
            .contains("no live holder"),
        "{view:?}"
    );
    assert_eq!(std::fs::read(&last_used).ok(), before);
}

#[test]
fn fixture_provider_is_data_and_run_argv_carries_the_selector_byte_exact() {
    let home = SwapHome::enter();
    let record_path = home.home.path().join("run-argv.txt");
    let opencode = home.home.path().join("opencode.cmd");
    let script = format!(
        "@echo off\r\n\
         if \"%1\"==\"--version\" (echo opencode v2.0.32& exit /b 0)\r\n\
         if \"%1\"==\"auth\" (echo OpenCode\\tAPI key\\tstored& exit /b 0)\r\n\
         if \"%1\"==\"models\" (echo fixture-provider/fixture-model& exit /b 0)\r\n\
         if \"%1\"==\"run\" (echo %*>\"{}\"& echo {{}}& exit /b 0)\r\n\
         exit /b 0\r\n",
        record_path.display()
    );
    std::fs::write(&opencode, script).unwrap();
    let opencode_s = opencode.to_string_lossy().to_string();
    home.write_bindings(|b| {
        let slot = b.get_mut(ROLE_REVIEWER_OPENCODE).unwrap();
        slot.harness = "opencode".into();
        slot.command = opencode_s;
        slot.model = Some("fixture-provider/fixture-model#high".into());
    });

    let receipt = crate::harness::preflight::probe_target(
        ROLE_REVIEWER_OPENCODE,
        &home.binding(ROLE_REVIEWER_OPENCODE),
    )
    .expect("base is listed");
    match &receipt.model_check {
        ModelCheck::Pass { detail } => {
            assert!(detail.contains("catalog-defined"), "{detail}");
            assert!(detail.contains("#high"), "{detail}");
        }
        other => panic!("expected Pass, got {other:?}"),
    }

    // Byte-exact selector through the real runtime argv and a real child process.
    let argv = crate::workflow::plan_review::opencode_argv(
        Some("fixture-provider/fixture-model#high"),
        Some("plan-review 0110"),
    );
    let out = crate::review::spawn::run_process(
        &opencode,
        &argv,
        home.home.path(),
        crate::review::spawn::ProcessWait {
            timeout: std::time::Duration::from_secs(10),
            stall: None,
            watch_paths: &[],
        },
        &[],
        None,
    )
    .unwrap();
    assert_eq!(out.exit, 0, "stderr={}", out.stderr);
    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    assert!(
        recorded.contains("fixture-provider/fixture-model#high"),
        "recorded argv={recorded}"
    );

    // Negative: base absent -> Fail (refusal), no provider code involved.
    let probe = ScriptedProbe::ready().with_models("opencode", 0, "other/model\n", false);
    let _probe = install_test_probe(probe);
    let err = crate::harness::preflight::probe_target(
        ROLE_REVIEWER_OPENCODE,
        &RoleBinding {
            harness: "opencode".into(),
            command: opencode.to_string_lossy().to_string(),
            model: Some("fixture-provider/fixture-model#high".into()),
        },
    )
    .expect_err("base absent must refuse");
    assert!(err.message().contains("not listed"), "{}", err.message());
}

#[test]
fn phase_mapping_applies_only_the_phases_single_role_key() {
    let home = SwapHome::enter();
    let (_shim, _companion) = codex_planner(&home, Some("model-a"));
    let _probe = install_test_probe(ready_probe());
    let project_dir = tempfile::tempdir().unwrap();
    let record = crate::api::project_add(
        project_dir.path(),
        crate::registry::ProjectAddOptions::default(),
    )
    .unwrap();
    let mut state = crate::state::RunState::idle(&record.id);
    state.status = crate::state::RunStatus::Running;
    state.phase = PHASE_PLAN.into();
    crate::state::save_run_state(&record, &state).unwrap();

    // A pending `fold` role must not apply for the `plan` phase.
    let pending = PendingSwap {
        role: ROLE_FOLD.into(),
        previous: RoleBinding {
            harness: "codex".into(),
            command: "codex-acp".into(),
            model: Some("model-a".into()),
        },
        proposed: RoleBinding {
            harness: "codex".into(),
            command: "codex-acp".into(),
            model: Some("model-b".into()),
        },
        receipt: ProbeReceipt {
            role: ROLE_FOLD.into(),
            class: RoleClass::Adapter,
            harness: "codex".into(),
            command: "codex-acp".into(),
            model: Some("model-b".into()),
            runtime_path: None,
            runtime_version: None,
            auth_path: None,
            auth_argv: Vec::new(),
            auth_detail: None,
            model_check: ModelCheck::NotRequested,
            probed_at: Utc::now(),
            duplicate_harness_preexisting: false,
        },
        requested_at: Utc::now(),
    };
    let mut ledger = new_ledger();
    ledger.pending.insert(ROLE_FOLD.into(), pending);
    save_ledger(&ledger).unwrap();
    assert!(apply_pending_for_phase("plan").is_empty());
    assert!(load_ledger().unwrap().pending.contains_key(ROLE_FOLD));
}
