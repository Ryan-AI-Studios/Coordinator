//! Role Binding load/defaults (machine `config.json`) and phase → role resolve (0019).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::save_machine_config;
pub use crate::config::{RoleBinding, default_role_bindings, load_machine_config};

use crate::error::{CoordinatorError, Result};
use crate::harness::grok::{ENV_CURSOR_BIN, ENV_GROK_BIN, reject_or_replace_ps1, resolve_command};
use crate::workflow::graph::{
    PHASE_ADDRESS_FINDINGS, PHASE_ADVANCE, PHASE_FOLD, PHASE_IMPLEMENT, PHASE_PLAN,
    ROLE_IMPLEMENTOR, ROLE_PLANNER,
};

/// Optional Role Binding key for `fold`. Not inserted by defaults / merge.
pub const ROLE_FOLD: &str = "fold";
/// Optional Role Binding key for `advance`. Not inserted by defaults / merge.
pub const ROLE_NEXT: &str = "next";

/// Load role bindings from machine config (defaults fill a missing field).
pub fn load_role_bindings() -> Result<BTreeMap<String, RoleBinding>> {
    Ok(load_machine_config()?.role_bindings)
}

/// Command to spawn for Grok: `implementor` binding, else `planner`, else `"grok"`.
///
/// CLI / HTTP `harness grok start` only — no phase context. Adapter ticks use
/// [`resolve_phase_binary`].
pub fn resolve_grok_command() -> Result<String> {
    let bindings = load_role_bindings()?;
    if let Some(b) = bindings.get(ROLE_IMPLEMENTOR)
        && !b.command.trim().is_empty()
    {
        return Ok(b.command.clone());
    }
    if let Some(b) = bindings.get(ROLE_PLANNER)
        && !b.command.trim().is_empty()
    {
        return Ok(b.command.clone());
    }
    Ok("grok".into())
}

/// Primary (fallback) role key for a long-lived adapter phase.
///
/// `fold` / `advance` return `planner` — optional keys are honored by
/// [`resolve_phase_role_key`].
pub fn phase_role_key(phase: &str) -> Option<&'static str> {
    match phase {
        PHASE_PLAN | PHASE_FOLD | PHASE_ADVANCE => Some(ROLE_PLANNER),
        PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS => Some(ROLE_IMPLEMENTOR),
        _ => None,
    }
}

fn command_nonempty(bindings: &BTreeMap<String, RoleBinding>, key: &str) -> bool {
    bindings
        .get(key)
        .is_some_and(|b| !b.command.trim().is_empty())
}

/// Phase → Role Binding key, honoring optional `fold` / `next` when present
/// with a non-empty command.
pub fn resolve_phase_role_key(
    phase: &str,
    bindings: &BTreeMap<String, RoleBinding>,
) -> Option<String> {
    match phase {
        PHASE_PLAN => Some(ROLE_PLANNER.to_string()),
        PHASE_IMPLEMENT | PHASE_ADDRESS_FINDINGS => Some(ROLE_IMPLEMENTOR.to_string()),
        PHASE_FOLD => {
            if command_nonempty(bindings, ROLE_FOLD) {
                Some(ROLE_FOLD.to_string())
            } else {
                Some(ROLE_PLANNER.to_string())
            }
        }
        PHASE_ADVANCE => {
            if command_nonempty(bindings, ROLE_NEXT) {
                Some(ROLE_NEXT.to_string())
            } else {
                Some(ROLE_PLANNER.to_string())
            }
        }
        _ => None,
    }
}

/// Role Binding for a long-lived adapter phase.
pub fn resolve_phase_binding(phase: &str) -> Result<RoleBinding> {
    let bindings = load_role_bindings()?;
    let key = resolve_phase_role_key(phase, &bindings)
        .ok_or_else(|| CoordinatorError::Message(format!("no role binding for phase {phase}")))?;
    bindings
        .get(&key)
        .cloned()
        .ok_or_else(|| CoordinatorError::Message(format!("role {key} not found")))
}

/// Resolve the phase command: grok env pin → binding command → shim replace.
pub fn resolve_phase_binary(phase: &str) -> Result<PathBuf> {
    let bindings = load_role_bindings()?;
    let key = resolve_phase_role_key(phase, &bindings)
        .ok_or_else(|| CoordinatorError::Message(format!("no role binding for phase {phase}")))?;
    let binding = bindings
        .get(&key)
        .ok_or_else(|| CoordinatorError::Message(format!("role {key} not found")))?;
    resolve_binding_binary(binding, &key)
}

fn resolve_binding_binary(binding: &RoleBinding, role: &str) -> Result<PathBuf> {
    let raw = if binding.harness.eq_ignore_ascii_case("grok") {
        match std::env::var(ENV_GROK_BIN) {
            Ok(over) if !over.trim().is_empty() => over,
            _ => binding.command.clone(),
        }
    } else if binding.harness.eq_ignore_ascii_case("cursor") {
        match std::env::var(ENV_CURSOR_BIN) {
            Ok(over) if !over.trim().is_empty() => over,
            _ => binding.command.clone(),
        }
    } else {
        binding.command.clone()
    };
    resolve_command(&raw)
        .and_then(reject_or_replace_ps1)
        .map_err(|e| CoordinatorError::Message(format!("role {role} command not resolvable: {e}")))
}

/// Pretty-JSON source for `coordinator roles show`.
pub fn roles_show() -> Result<BTreeMap<String, RoleBinding>> {
    load_role_bindings()
}

/// Rewrite planner + implementor (and present `fold` / `next`) to grok or cursor.
///
/// Clears `model`. Does not touch plan-reviewers or cross-model keys.
pub fn roles_use(target: &str) -> Result<BTreeMap<String, RoleBinding>> {
    let (harness, command) = match target.trim().to_ascii_lowercase().as_str() {
        "grok" => ("grok", "grok"),
        "cursor" => ("cursor", "cursor-agent"),
        other => {
            return Err(CoordinatorError::Message(format!(
                "unknown roles target {other} (expected grok or cursor)"
            )));
        }
    };
    let mut cfg = load_machine_config()?;
    let binding = RoleBinding {
        harness: harness.into(),
        command: command.into(),
        model: None,
    };
    cfg.role_bindings
        .insert(ROLE_PLANNER.to_string(), binding.clone());
    cfg.role_bindings
        .insert(ROLE_IMPLEMENTOR.to_string(), binding.clone());
    if cfg.role_bindings.contains_key(ROLE_FOLD) {
        cfg.role_bindings
            .insert(ROLE_FOLD.to_string(), binding.clone());
    }
    if cfg.role_bindings.contains_key(ROLE_NEXT) {
        cfg.role_bindings.insert(ROLE_NEXT.to_string(), binding);
    }
    save_machine_config(&cfg)?;
    Ok(cfg.role_bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, MachineConfig, save_machine_config, test_env_lock};
    use crate::harness::grok::{ENV_CURSOR_BIN, ENV_GROK_BIN};
    use crate::workflow::graph::{
        PHASE_CI_WAIT, PHASE_CROSS_MODEL, PHASE_PLAN_REVIEW, is_grok_bound,
    };
    use std::ffi::OsString;
    use tempfile::tempdir;

    struct IsolatedHome {
        prev_home: Option<OsString>,
        prev_bin: Option<OsString>,
        prev_cursor: Option<OsString>,
        prev_state: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
    }

    impl IsolatedHome {
        fn enter() -> Self {
            let lock = test_env_lock();
            let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
            let prev_bin = std::env::var_os(ENV_GROK_BIN);
            let prev_cursor = std::env::var_os(ENV_CURSOR_BIN);
            let prev_state = std::env::var_os(crate::config::ENV_COORDINATOR_STATE_DIR);
            let home = tempdir().unwrap();
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(ENV_GROK_BIN);
                std::env::remove_var(ENV_CURSOR_BIN);
                std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR);
            }
            Self {
                prev_home,
                prev_bin,
                prev_cursor,
                prev_state,
                _lock: lock,
                _home: home,
            }
        }

        fn write_bindings(&self, mutate: impl FnOnce(&mut BTreeMap<String, RoleBinding>)) {
            let mut cfg = MachineConfig::default();
            mutate(&mut cfg.role_bindings);
            save_machine_config(&cfg).unwrap();
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                    None => std::env::remove_var(ENV_COORDINATOR_HOME),
                }
                match &self.prev_bin {
                    Some(v) => std::env::set_var(ENV_GROK_BIN, v),
                    None => std::env::remove_var(ENV_GROK_BIN),
                }
                match &self.prev_cursor {
                    Some(v) => std::env::set_var(ENV_CURSOR_BIN, v),
                    None => std::env::remove_var(ENV_CURSOR_BIN),
                }
                match &self.prev_state {
                    Some(v) => std::env::set_var(crate::config::ENV_COORDINATOR_STATE_DIR, v),
                    None => std::env::remove_var(crate::config::ENV_COORDINATOR_STATE_DIR),
                }
            }
        }
    }

    fn dummy_bin(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    #[test]
    fn defaults_map_planner_and_implementor_to_grok() {
        let b = default_role_bindings();
        assert_eq!(b["planner"].harness, "grok");
        assert_eq!(b["implementor"].harness, "grok");
        assert_eq!(b["planner"].command, "grok");
        assert!(b["planner"].model.is_none());
        assert_eq!(b["plan_reviewer_agy"].harness, "antigravity");
        assert_eq!(b["plan_reviewer_agy"].command, "agy");
        assert_eq!(b["plan_reviewer_opencode"].harness, "opencode");
        assert_eq!(b["cross_model_primary"].harness, "codex");
        assert_eq!(b["cross_model_secondary"].harness, "claude");
        assert_eq!(b["cross_model_tertiary"].harness, "opencode");
        assert!(b["cross_model_primary"].model.is_none());
        assert!(!b.contains_key(ROLE_FOLD));
        assert!(!b.contains_key(ROLE_NEXT));
    }

    #[test]
    fn is_grok_bound_stays_static_five_phase() {
        assert!(is_grok_bound(PHASE_PLAN));
        assert!(is_grok_bound(PHASE_FOLD));
        assert!(is_grok_bound(PHASE_IMPLEMENT));
        assert!(is_grok_bound(PHASE_ADVANCE));
        assert!(is_grok_bound(PHASE_ADDRESS_FINDINGS));
        assert!(!is_grok_bound(PHASE_PLAN_REVIEW));
        assert!(!is_grok_bound(PHASE_CI_WAIT));
        assert!(!is_grok_bound(PHASE_CROSS_MODEL));
        assert_eq!(phase_role_key(PHASE_PLAN), Some(ROLE_PLANNER));
        assert_eq!(phase_role_key(PHASE_FOLD), Some(ROLE_PLANNER));
        assert_eq!(phase_role_key(PHASE_ADVANCE), Some(ROLE_PLANNER));
        assert_eq!(phase_role_key(PHASE_IMPLEMENT), Some(ROLE_IMPLEMENTOR));
        assert_eq!(
            phase_role_key(PHASE_ADDRESS_FINDINGS),
            Some(ROLE_IMPLEMENTOR)
        );
        assert!(phase_role_key(PHASE_PLAN_REVIEW).is_none());
    }

    #[test]
    fn default_bindings_plan_fold_advance_planner_implement_implementor() {
        let _home = IsolatedHome::enter();
        for phase in [PHASE_PLAN, PHASE_FOLD, PHASE_ADVANCE] {
            let b = resolve_phase_binding(phase).unwrap();
            assert_eq!(b.harness, "grok");
            assert_eq!(b.command, "grok");
            assert_eq!(
                resolve_phase_role_key(phase, &load_role_bindings().unwrap()).as_deref(),
                Some(ROLE_PLANNER)
            );
        }
        let impl_b = resolve_phase_binding(PHASE_IMPLEMENT).unwrap();
        assert_eq!(impl_b.harness, "grok");
        assert_eq!(impl_b.command, "grok");
        assert_eq!(
            resolve_phase_role_key(PHASE_IMPLEMENT, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_IMPLEMENTOR)
        );
        assert_eq!(
            phase_role_key(PHASE_ADDRESS_FINDINGS),
            phase_role_key(PHASE_IMPLEMENT)
        );
        assert_eq!(
            resolve_phase_role_key(PHASE_ADDRESS_FINDINGS, &load_role_bindings().unwrap())
                .as_deref(),
            Some(ROLE_IMPLEMENTOR)
        );
        let af_b = resolve_phase_binding(PHASE_ADDRESS_FINDINGS).unwrap();
        assert_eq!(af_b.harness, impl_b.harness);
        assert_eq!(af_b.command, impl_b.command);
        let impl_bin = resolve_phase_binary(PHASE_IMPLEMENT);
        let af_bin = resolve_phase_binary(PHASE_ADDRESS_FINDINGS);
        match (impl_bin, af_bin) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            other => panic!("implement vs address-findings binary class mismatch: {other:?}"),
        }
    }

    #[test]
    fn rebound_planner_command_is_plan_binary_not_implementor() {
        let home = IsolatedHome::enter();
        let planner = dummy_bin(home._home.path(), "planner-bin.exe");
        let implementor = dummy_bin(home._home.path(), "implementor-bin.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().command = implementor.to_string_lossy().into();
        });
        assert_eq!(resolve_phase_binary(PHASE_PLAN).unwrap(), planner);
        assert_eq!(resolve_phase_binary(PHASE_FOLD).unwrap(), planner);
        assert_eq!(resolve_phase_binary(PHASE_ADVANCE).unwrap(), planner);
        assert_eq!(resolve_phase_binary(PHASE_IMPLEMENT).unwrap(), implementor);
        assert_eq!(
            resolve_grok_command().unwrap(),
            implementor.to_string_lossy()
        );
    }

    #[test]
    fn resolve_grok_command_stays_implementor_first() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = "plan-only-bin".into();
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().command = "impl-only-bin".into();
        });
        assert_eq!(resolve_grok_command().unwrap(), "impl-only-bin");
    }

    #[test]
    fn optional_fold_and_next_present_vs_empty_vs_missing() {
        let home = IsolatedHome::enter();
        let planner = dummy_bin(home._home.path(), "planner.exe");
        let fold = dummy_bin(home._home.path(), "fold.exe");
        let next = dummy_bin(home._home.path(), "next.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.insert(
                ROLE_FOLD.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: fold.to_string_lossy().into(),
                    model: None,
                },
            );
            b.insert(
                ROLE_NEXT.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: next.to_string_lossy().into(),
                    model: None,
                },
            );
        });
        assert_eq!(resolve_phase_binary(PHASE_FOLD).unwrap(), fold);
        assert_eq!(resolve_phase_binary(PHASE_ADVANCE).unwrap(), next);
        assert_eq!(
            resolve_phase_role_key(PHASE_FOLD, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_FOLD)
        );
        assert_eq!(
            resolve_phase_role_key(PHASE_ADVANCE, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_NEXT)
        );

        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.insert(
                ROLE_FOLD.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: String::new(),
                    model: None,
                },
            );
            b.insert(
                ROLE_NEXT.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: String::new(),
                    model: None,
                },
            );
        });
        assert_eq!(resolve_phase_binary(PHASE_FOLD).unwrap(), planner);
        assert_eq!(resolve_phase_binary(PHASE_ADVANCE).unwrap(), planner);
        assert_eq!(
            resolve_phase_role_key(PHASE_FOLD, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_PLANNER)
        );

        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
            b.insert(
                ROLE_FOLD.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: "   ".into(),
                    model: None,
                },
            );
            b.insert(
                ROLE_NEXT.into(),
                RoleBinding {
                    harness: "grok".into(),
                    command: "\t".into(),
                    model: None,
                },
            );
        });
        assert_eq!(resolve_phase_binary(PHASE_FOLD).unwrap(), planner);
        assert_eq!(resolve_phase_binary(PHASE_ADVANCE).unwrap(), planner);
        assert_eq!(
            resolve_phase_role_key(PHASE_FOLD, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_PLANNER)
        );
        assert_eq!(
            resolve_phase_role_key(PHASE_ADVANCE, &load_role_bindings().unwrap()).as_deref(),
            Some(ROLE_PLANNER)
        );
    }

    #[test]
    fn grok_bin_env_wins_for_grok_harness() {
        let home = IsolatedHome::enter();
        let planner = dummy_bin(home._home.path(), "planner.exe");
        let pin = dummy_bin(home._home.path(), "pinned.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
        });
        unsafe {
            std::env::set_var(ENV_GROK_BIN, &pin);
        }
        assert_eq!(resolve_phase_binary(PHASE_PLAN).unwrap(), pin);
    }

    #[test]
    fn cursor_bin_env_wins_for_cursor_harness() {
        let home = IsolatedHome::enter();
        let planner = dummy_bin(home._home.path(), "planner.exe");
        let pin = dummy_bin(home._home.path(), "pinned-cursor.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().harness = "cursor".into();
            b.get_mut(ROLE_PLANNER).unwrap().command = planner.to_string_lossy().into();
        });
        unsafe {
            std::env::set_var(ENV_CURSOR_BIN, &pin);
        }
        assert_eq!(resolve_phase_binary(PHASE_PLAN).unwrap(), pin);
    }

    #[test]
    fn empty_command_is_error_no_spawn() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command.clear();
        });
        let err = resolve_phase_binary(PHASE_PLAN).unwrap_err().to_string();
        assert!(err.contains("planner"), "err={err}");
        assert!(
            err.contains("not resolvable") || err.contains("empty"),
            "err={err}"
        );
    }

    #[test]
    fn missing_binary_is_error() {
        let home = IsolatedHome::enter();
        let missing = home._home.path().join("missing-planner.exe");
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = missing.to_string_lossy().into();
        });
        let err = resolve_phase_binary(PHASE_PLAN).unwrap_err().to_string();
        assert!(err.contains("planner"), "err={err}");
        assert!(
            err.contains("missing-planner") || err.contains("not found"),
            "err={err}"
        );
    }

    #[test]
    fn shim_only_is_error() {
        let home = IsolatedHome::enter();
        let sh = home._home.path().join("planner-shim");
        std::fs::write(&sh, "#!/bin/sh\necho hi\n").unwrap();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().command = sh.to_string_lossy().into();
        });
        let err = resolve_phase_binary(PHASE_PLAN).unwrap_err().to_string();
        assert!(err.contains("planner"), "err={err}");
        assert!(err.contains("refusing to spawn shim"), "err={err}");
    }

    #[test]
    fn roles_use_cursor_then_grok_round_trips_planner_implementor() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.get_mut(ROLE_PLANNER).unwrap().model = Some("grok-build".into());
            b.get_mut(ROLE_IMPLEMENTOR).unwrap().model = Some("grok-build".into());
        });
        let after_cursor = roles_use("cursor").unwrap();
        assert_eq!(after_cursor[ROLE_PLANNER].harness, "cursor");
        assert_eq!(after_cursor[ROLE_PLANNER].command, "cursor-agent");
        assert!(after_cursor[ROLE_PLANNER].model.is_none());
        assert_eq!(after_cursor[ROLE_IMPLEMENTOR].harness, "cursor");
        assert_eq!(after_cursor[ROLE_IMPLEMENTOR].command, "cursor-agent");
        assert_eq!(after_cursor["plan_reviewer_agy"].harness, "antigravity");
        assert_eq!(after_cursor["cross_model_primary"].harness, "codex");
        let after_grok = roles_use("grok").unwrap();
        assert_eq!(after_grok[ROLE_PLANNER].harness, "grok");
        assert_eq!(after_grok[ROLE_PLANNER].command, "grok");
        assert!(after_grok[ROLE_PLANNER].model.is_none());
        assert_eq!(after_grok[ROLE_IMPLEMENTOR].harness, "grok");
        assert_eq!(after_grok[ROLE_IMPLEMENTOR].command, "grok");
        assert_eq!(roles_show().unwrap()[ROLE_PLANNER].harness, "grok");
        let _ = home;
    }

    #[test]
    fn roles_use_cursor_rewrites_present_fold_and_next() {
        let home = IsolatedHome::enter();
        home.write_bindings(|b| {
            b.insert(
                ROLE_FOLD.to_string(),
                RoleBinding {
                    harness: "grok".into(),
                    command: "grok".into(),
                    model: Some("leftover".into()),
                },
            );
            b.insert(
                ROLE_NEXT.to_string(),
                RoleBinding {
                    harness: "grok".into(),
                    command: "grok".into(),
                    model: None,
                },
            );
        });
        let after = roles_use("cursor").unwrap();
        assert_eq!(after[ROLE_FOLD].harness, "cursor");
        assert_eq!(after[ROLE_FOLD].command, "cursor-agent");
        assert!(after[ROLE_FOLD].model.is_none());
        assert_eq!(after[ROLE_NEXT].harness, "cursor");
        assert_eq!(after[ROLE_NEXT].command, "cursor-agent");
        let shown = roles_show().unwrap();
        assert_eq!(shown[ROLE_FOLD].harness, "cursor");
        let _ = home;
    }

    #[test]
    fn roles_use_unknown_target_errors() {
        let home = IsolatedHome::enter();
        let err = roles_use("opencode").unwrap_err().to_string();
        assert!(err.contains("opencode"), "err={err}");
        assert!(err.contains("grok") && err.contains("cursor"), "err={err}");
        let _ = home;
    }
}
