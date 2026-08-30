//! Harness preflight (`doctor`) and adapter `run` gate (track **0028**).
//!
//! Probes bound harnesses plus `gh`. Never stores or prints secrets. Never
//! starts a login TUI. Default `cargo test` uses [`ScriptedProbe`] only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{
    ENV_COORDINATOR_AGY_BIN, ENV_COORDINATOR_CLAUDE_BIN, ENV_COORDINATOR_CODEX_BIN,
    ENV_COORDINATOR_GH_BIN, ENV_COORDINATOR_OPENCODE_BIN, RoleBinding,
};
use crate::error::Result;
use crate::harness::grok::{ENV_GROK_BIN, reject_or_replace_ps1, resolve_command};
use crate::harness::roles::{ROLE_FOLD, ROLE_NEXT, load_role_bindings};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const REQUIRED_ROLES: &[&str] = &[
    "planner",
    "implementor",
    "plan_reviewer_agy",
    "plan_reviewer_opencode",
    "cross_model_primary",
    "ci",
];

/// JSON report from `coordinator doctor` / HTTP `GET /v1/doctor`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorReport {
    pub ok: bool,
    pub rows: Vec<DoctorRow>,
}

impl DoctorReport {
    /// True when no **required** row is `missing` or `auth`. `unknown` does not flip this.
    pub fn ok_required(&self) -> bool {
        !self
            .rows
            .iter()
            .any(|r| r.required && matches!(r.status, RowStatus::Missing | RowStatus::Auth))
    }

    /// One-line operator message: unique login commands of required failing rows.
    pub fn preflight_message(&self) -> String {
        let mut logins = Vec::new();
        for row in &self.rows {
            if row.required
                && matches!(row.status, RowStatus::Missing | RowStatus::Auth)
                && let Some(login) = &row.login
                && !logins.iter().any(|s| s == login)
            {
                logins.push(login.clone());
            }
        }
        if logins.is_empty() {
            "preflight failed".into()
        } else {
            format!("preflight failed; login: {}", logins.join("; "))
        }
    }
}

/// One Role Binding (or synthetic `ci`/`gh`) row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorRow {
    pub role: String,
    pub harness: String,
    pub command: String,
    pub path: Option<PathBuf>,
    pub status: RowStatus,
    pub required: bool,
    pub login: Option<String>,
    pub detail: Option<String>,
}

/// Probe classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStatus {
    Ready,
    Missing,
    Auth,
    Unknown,
}

#[derive(Debug, Clone)]
struct RawOut {
    exit: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

trait Probe: Send + Sync {
    fn run(&self, bin: &Path, args: &[String]) -> RawOut;
}

struct DefaultProbe;

impl Probe for DefaultProbe {
    fn run(&self, bin: &Path, args: &[String]) -> RawOut {
        match crate::review::spawn::run_process(
            bin,
            args,
            &std::env::temp_dir(),
            PROBE_TIMEOUT,
            &[],
            None,
        ) {
            Ok(out) => RawOut {
                timed_out: out.exit == 124,
                exit: out.exit,
                stdout: out.stdout,
                stderr: out.stderr,
            },
            Err(_) => RawOut {
                exit: 127,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            },
        }
    }
}

/// Recorded spawn from a [`ScriptedProbe`] (tests).
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub bin: PathBuf,
    pub args: Vec<String>,
}

/// Scripted spawn stand-in. Default `cargo test` must not talk to a live CLI.
#[cfg(test)]
#[derive(Clone)]
pub struct ScriptedProbe {
    inner: std::sync::Arc<ScriptedInner>,
}

#[cfg(test)]
struct ScriptedInner {
    calls: std::sync::Mutex<Vec<RecordedCall>>,
    version_by_stem: HashMap<String, RawOut>,
    auth_by_stem: HashMap<String, RawOut>,
    default_version: RawOut,
    default_auth: RawOut,
}

#[cfg(test)]
impl ScriptedProbe {
    /// All `--version` and auth probes succeed (ready-shaped stdout).
    pub fn ready() -> Self {
        Self {
            inner: std::sync::Arc::new(ScriptedInner {
                calls: std::sync::Mutex::new(Vec::new()),
                version_by_stem: HashMap::new(),
                auth_by_stem: HashMap::new(),
                default_version: RawOut {
                    exit: 0,
                    stdout: "1.0.0\n".into(),
                    stderr: String::new(),
                    timed_out: false,
                },
                default_auth: RawOut {
                    exit: 0,
                    stdout: "{\"loggedIn\":true}\nxai credential\n".into(),
                    stderr: String::new(),
                    timed_out: false,
                },
            }),
        }
    }

    pub fn with_version(mut self, stem: &str, exit: i32, timed_out: bool) -> Self {
        self.ensure_unique();
        std::sync::Arc::get_mut(&mut self.inner)
            .expect("scripted probe unique")
            .version_by_stem
            .insert(
                stem.to_ascii_lowercase(),
                RawOut {
                    exit,
                    stdout: String::new(),
                    stderr: if timed_out {
                        "probe timed out".into()
                    } else {
                        String::new()
                    },
                    timed_out,
                },
            );
        self
    }

    pub fn with_auth(
        mut self,
        stem: &str,
        exit: i32,
        stdout: &str,
        stderr: &str,
        timed_out: bool,
    ) -> Self {
        self.ensure_unique();
        std::sync::Arc::get_mut(&mut self.inner)
            .expect("scripted probe unique")
            .auth_by_stem
            .insert(
                stem.to_ascii_lowercase(),
                RawOut {
                    exit,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                    timed_out,
                },
            );
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.inner.calls.lock().expect("scripted calls").clone()
    }

    fn ensure_unique(&mut self) {
        if std::sync::Arc::strong_count(&self.inner) > 1 {
            let inner = ScriptedInner {
                calls: std::sync::Mutex::new(self.calls()),
                version_by_stem: self.inner.version_by_stem.clone(),
                auth_by_stem: self.inner.auth_by_stem.clone(),
                default_version: self.inner.default_version.clone(),
                default_auth: self.inner.default_auth.clone(),
            };
            self.inner = std::sync::Arc::new(inner);
        }
    }
}

#[cfg(test)]
impl Probe for ScriptedProbe {
    fn run(&self, bin: &Path, args: &[String]) -> RawOut {
        self.inner
            .calls
            .lock()
            .expect("scripted calls")
            .push(RecordedCall {
                bin: bin.to_path_buf(),
                args: args.to_vec(),
            });
        let stem = bin
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let version = args.first().map(|s| s.as_str()) == Some("--version");
        if version {
            self.inner
                .version_by_stem
                .get(&stem)
                .cloned()
                .unwrap_or_else(|| self.inner.default_version.clone())
        } else {
            self.inner
                .auth_by_stem
                .get(&stem)
                .cloned()
                .unwrap_or_else(|| self.inner.default_auth.clone())
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PROBE: std::cell::RefCell<Option<std::sync::Arc<dyn Probe>>> =
        const { std::cell::RefCell::new(None) };
}

/// Clears the thread-local scripted probe on drop (`ci::TestBackendGuard` copy).
#[cfg(test)]
pub struct TestProbeGuard;

#[cfg(test)]
impl Drop for TestProbeGuard {
    fn drop(&mut self) {
        TEST_PROBE.with(|c| *c.borrow_mut() = None);
    }
}

/// Install a scripted probe for this thread. Drop the guard to clear.
#[cfg(test)]
pub fn install_test_probe(probe: ScriptedProbe) -> TestProbeGuard {
    TEST_PROBE.with(|c| *c.borrow_mut() = Some(std::sync::Arc::new(probe)));
    TestProbeGuard
}

fn active_probe() -> std::sync::Arc<dyn Probe> {
    #[cfg(test)]
    {
        if let Some(p) = TEST_PROBE.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    std::sync::Arc::new(DefaultProbe)
}

/// Probe machine Role Bindings plus synthetic `ci`/`gh`.
pub fn probe_machine() -> Result<DoctorReport> {
    let bindings = load_role_bindings()?;
    let probe = active_probe();
    let mut version_cache: HashMap<PathBuf, RawOut> = HashMap::new();
    let mut auth_cache: HashMap<(PathBuf, String), RawOut> = HashMap::new();
    let mut rows = Vec::new();

    for (role, binding) in &bindings {
        if (role == ROLE_FOLD || role == ROLE_NEXT) && binding.command.trim().is_empty() {
            continue;
        }
        rows.push(probe_row(
            role,
            binding,
            probe.as_ref(),
            &mut version_cache,
            &mut auth_cache,
        ));
    }

    let gh_command = if cfg!(windows) { "gh.exe" } else { "gh" };
    rows.push(probe_row(
        "ci",
        &RoleBinding {
            harness: "gh".into(),
            command: gh_command.into(),
            model: None,
        },
        probe.as_ref(),
        &mut version_cache,
        &mut auth_cache,
    ));

    let mut report = DoctorReport { ok: false, rows };
    report.ok = report.ok_required();
    Ok(report)
}

fn probe_row(
    role: &str,
    binding: &RoleBinding,
    probe: &dyn Probe,
    version_cache: &mut HashMap<PathBuf, RawOut>,
    auth_cache: &mut HashMap<(PathBuf, String), RawOut>,
) -> DoctorRow {
    let required = is_required(role);
    let login = login_for(&binding.harness).map(str::to_string);
    let command = binding.command.clone();
    let harness = binding.harness.clone();

    let path = match resolve_preflight_bin(&harness, &command) {
        Ok(p) => p,
        Err(_) => {
            return DoctorRow {
                role: role.to_string(),
                harness,
                command,
                path: None,
                status: RowStatus::Missing,
                required,
                login,
                detail: Some(redact_secrets("not on PATH")),
            };
        }
    };

    let version = version_cache
        .entry(path.clone())
        .or_insert_with(|| sanitize_out(probe.run(&path, &["--version".into()])))
        .clone();

    if version.timed_out {
        return DoctorRow {
            role: role.to_string(),
            harness,
            command,
            path: Some(path),
            status: RowStatus::Unknown,
            required,
            login,
            detail: Some("probe timed out".into()),
        };
    }
    if version.exit != 0 {
        return DoctorRow {
            role: role.to_string(),
            harness,
            command,
            path: Some(path),
            status: RowStatus::Unknown,
            required,
            login,
            detail: Some("version probe failed".into()),
        };
    }

    let (status, detail) = classify_auth(&harness, &path, probe, auth_cache);
    DoctorRow {
        role: role.to_string(),
        harness,
        command,
        path: Some(path),
        status,
        required,
        login,
        detail,
    }
}

fn classify_auth(
    harness: &str,
    path: &Path,
    probe: &dyn Probe,
    auth_cache: &mut HashMap<(PathBuf, String), RawOut>,
) -> (RowStatus, Option<String>) {
    let key = harness.to_ascii_lowercase();
    match key.as_str() {
        "grok" => {
            if grok_auth_ready() {
                (RowStatus::Ready, None)
            } else {
                (RowStatus::Auth, Some("logged out".into()))
            }
        }
        "antigravity" | "agy" => (RowStatus::Ready, None),
        other => {
            let Some(args) = auth_args(other) else {
                // Unknown harness: `--version` already succeeded.
                return (RowStatus::Ready, None);
            };
            let cache_key = (path.to_path_buf(), other.to_string());
            let out = auth_cache
                .entry(cache_key)
                .or_insert_with(|| sanitize_out(probe.run(path, &args)))
                .clone();
            classify_auth_out(other, &out)
        }
    }
}

fn classify_auth_out(harness: &str, out: &RawOut) -> (RowStatus, Option<String>) {
    if out.timed_out {
        return (RowStatus::Unknown, Some("probe timed out".into()));
    }
    match harness {
        "opencode" => {
            if out.exit != 0 {
                return (RowStatus::Auth, Some("logged out".into()));
            }
            let nonempty = out.stdout.lines().any(|l| !l.trim().is_empty());
            if nonempty {
                (RowStatus::Ready, None)
            } else {
                (RowStatus::Auth, Some("logged out".into()))
            }
        }
        "codex" | "gh" => {
            if out.exit == 0 {
                (RowStatus::Ready, None)
            } else {
                (RowStatus::Auth, Some("logged out".into()))
            }
        }
        "claude" => match serde_json::from_str::<serde_json::Value>(&out.stdout) {
            Ok(v) => match v.get("loggedIn").and_then(|x| x.as_bool()) {
                Some(true) => (RowStatus::Ready, None),
                Some(false) => (RowStatus::Auth, Some("logged out".into())),
                None => (RowStatus::Unknown, Some("auth probe failed".into())),
            },
            Err(_) => (RowStatus::Unknown, Some("auth probe failed".into())),
        },
        _ => {
            if out.exit == 0 {
                (RowStatus::Ready, None)
            } else {
                (RowStatus::Unknown, Some("auth probe failed".into()))
            }
        }
    }
}

fn auth_args(harness: &str) -> Option<Vec<String>> {
    match harness {
        "opencode" => Some(vec!["auth".into(), "list".into()]),
        "codex" => Some(vec!["login".into(), "status".into()]),
        "claude" => Some(vec!["auth".into(), "status".into(), "--json".into()]),
        "gh" => Some(vec![
            "auth".into(),
            "status".into(),
            "--active".into(),
            "--hostname".into(),
            "github.com".into(),
        ]),
        _ => None,
    }
}

fn is_required(role: &str) -> bool {
    REQUIRED_ROLES.contains(&role)
}

fn login_for(harness: &str) -> Option<&'static str> {
    match harness.to_ascii_lowercase().as_str() {
        "grok" => Some("grok login"),
        "antigravity" | "agy" => Some("agy"),
        "opencode" => Some("opencode auth login"),
        "codex" => Some("codex login"),
        "claude" => Some("claude auth login"),
        "gh" => Some("gh auth login"),
        _ => None,
    }
}

fn env_pin_for_harness(harness: &str) -> Option<&'static str> {
    match harness.to_ascii_lowercase().as_str() {
        "grok" => Some(ENV_GROK_BIN),
        "antigravity" | "agy" => Some(ENV_COORDINATOR_AGY_BIN),
        "opencode" => Some(ENV_COORDINATOR_OPENCODE_BIN),
        "claude" => Some(ENV_COORDINATOR_CLAUDE_BIN),
        "codex" => Some(ENV_COORDINATOR_CODEX_BIN),
        "gh" => Some(ENV_COORDINATOR_GH_BIN),
        _ => None,
    }
}

fn trim_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Env pin first (trim-nonempty), else binding command, then PATH + shim replace.
pub fn resolve_preflight_bin(harness: &str, command: &str) -> Result<PathBuf> {
    let raw = match env_pin_for_harness(harness).and_then(trim_env) {
        Some(pin) => pin,
        None => command.to_string(),
    };
    if raw.trim().is_empty() {
        return Err(crate::error::CoordinatorError::Message(
            "command must not be empty".into(),
        ));
    }
    resolve_command(&raw).and_then(reject_or_replace_ps1)
}

fn grok_auth_ready() -> bool {
    if trim_env("XAI_API_KEY").is_some() {
        return true;
    }
    grok_auth_json_path().is_file()
}

fn grok_auth_json_path() -> PathBuf {
    if let Some(home) = trim_env("GROK_HOME") {
        return PathBuf::from(home).join("auth.json");
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".grok").join("auth.json")
}

fn sanitize_out(out: RawOut) -> RawOut {
    RawOut {
        exit: out.exit,
        stdout: redact_secrets(&out.stdout),
        stderr: redact_secrets(&out.stderr),
        timed_out: out.timed_out,
    }
}

fn redact_secrets(s: &str) -> String {
    let mut out = s.to_string();
    for prefix in ["gho_", "ghs_", "sk-", "xai-", "ghp_"] {
        out = redact_prefix(&out, prefix);
    }
    out
}

fn redact_prefix(s: &str, prefix: &str) -> String {
    let mut result = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(prefix) {
        result.push_str(&rest[..i]);
        result.push_str(prefix);
        result.push_str("***");
        rest = &rest[i + prefix.len()..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::config::{
        ENV_COORDINATOR_HOME, ENV_COORDINATOR_STATE_DIR, MachineConfig, save_machine_config,
        test_env_lock,
    };
    use crate::error::CoordinatorError;
    use crate::registry::ProjectAddOptions;
    use crate::registry::ProjectRecord;
    use crate::state::RunStatus;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    struct IsolatedDoctor {
        prev_home: Option<OsString>,
        prev_state: Option<OsString>,
        prev_grok_home: Option<OsString>,
        prev_xai: Option<OsString>,
        prev_bins: Vec<(&'static str, Option<OsString>)>,
        _lock: MutexGuard<'static, ()>,
        home: TempDir,
        grok_home: TempDir,
    }

    impl IsolatedDoctor {
        fn enter() -> Self {
            let lock = test_env_lock();
            let home = tempfile::tempdir().unwrap();
            let grok_home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os(ENV_COORDINATOR_HOME);
            let prev_state = std::env::var_os(ENV_COORDINATOR_STATE_DIR);
            let prev_grok_home = std::env::var_os("GROK_HOME");
            let prev_xai = std::env::var_os("XAI_API_KEY");
            let bin_keys = [
                ENV_GROK_BIN,
                ENV_COORDINATOR_AGY_BIN,
                ENV_COORDINATOR_OPENCODE_BIN,
                ENV_COORDINATOR_CLAUDE_BIN,
                ENV_COORDINATOR_CODEX_BIN,
                ENV_COORDINATOR_GH_BIN,
            ];
            let prev_bins = bin_keys
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            unsafe {
                std::env::set_var(ENV_COORDINATOR_HOME, home.path());
                std::env::remove_var(ENV_COORDINATOR_STATE_DIR);
                std::env::set_var("GROK_HOME", grok_home.path());
                std::env::remove_var("XAI_API_KEY");
                for k in bin_keys {
                    std::env::remove_var(k);
                }
            }
            Self {
                prev_home,
                prev_state,
                prev_grok_home,
                prev_xai,
                prev_bins,
                _lock: lock,
                home,
                grok_home,
            }
        }

        fn write_bindings(&self, mutate: impl FnOnce(&mut BTreeMap<String, RoleBinding>)) {
            let mut cfg = MachineConfig::default();
            mutate(&mut cfg.role_bindings);
            save_machine_config(&cfg).unwrap();
        }

        fn dummy_bin(&self, name: &str) -> PathBuf {
            let p = self.home.path().join(name);
            std::fs::write(&p, b"").unwrap();
            p
        }

        fn point_all_to_dummies(&self) -> BTreeMap<String, PathBuf> {
            let grok = self.dummy_bin("grok.exe");
            let agy = self.dummy_bin("agy.exe");
            let opencode = self.dummy_bin("opencode.exe");
            let claude = self.dummy_bin("claude.exe");
            let codex = self.dummy_bin("codex.exe");
            let gh = self.dummy_bin("gh.exe");
            self.write_bindings(|b| {
                b.get_mut("planner").unwrap().command = grok.to_string_lossy().into();
                b.get_mut("implementor").unwrap().command = grok.to_string_lossy().into();
                b.get_mut("plan_reviewer_agy").unwrap().command = agy.to_string_lossy().into();
                b.get_mut("plan_reviewer_opencode").unwrap().command =
                    opencode.to_string_lossy().into();
                b.get_mut("cross_model_primary").unwrap().command = codex.to_string_lossy().into();
                b.get_mut("cross_model_secondary").unwrap().command =
                    claude.to_string_lossy().into();
                b.get_mut("cross_model_tertiary").unwrap().command =
                    opencode.to_string_lossy().into();
            });
            unsafe {
                std::env::set_var(ENV_COORDINATOR_GH_BIN, &gh);
            }
            let mut m = BTreeMap::new();
            m.insert("grok".into(), grok);
            m.insert("agy".into(), agy);
            m.insert("opencode".into(), opencode);
            m.insert("claude".into(), claude);
            m.insert("codex".into(), codex);
            m.insert("gh".into(), gh);
            m
        }

        fn grok_ready_via_key(&self) {
            unsafe {
                std::env::set_var("XAI_API_KEY", "test-not-a-real-key");
            }
        }

        fn add_project(&self) -> (TempDir, ProjectRecord) {
            let proj = tempfile::tempdir().unwrap();
            let rec = api::project_add(proj.path(), ProjectAddOptions::default()).unwrap();
            (proj, rec)
        }
    }

    impl Drop for IsolatedDoctor {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_HOME, v),
                    None => std::env::remove_var(ENV_COORDINATOR_HOME),
                }
                match &self.prev_state {
                    Some(v) => std::env::set_var(ENV_COORDINATOR_STATE_DIR, v),
                    None => std::env::remove_var(ENV_COORDINATOR_STATE_DIR),
                }
                match &self.prev_grok_home {
                    Some(v) => std::env::set_var("GROK_HOME", v),
                    None => std::env::remove_var("GROK_HOME"),
                }
                match &self.prev_xai {
                    Some(v) => std::env::set_var("XAI_API_KEY", v),
                    None => std::env::remove_var("XAI_API_KEY"),
                }
                for (k, v) in &self.prev_bins {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    fn row<'a>(report: &'a DoctorReport, role: &str) -> &'a DoctorRow {
        report
            .rows
            .iter()
            .find(|r| r.role == role)
            .unwrap_or_else(|| panic!("missing role {role}"))
    }

    #[test]
    fn doctor_ready_rows_and_ok() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let report = probe_machine().unwrap();
        assert!(report.ok, "report={report:?}");
        assert!(report.rows.iter().any(|r| r.role == "planner"));
        assert!(report.rows.iter().any(|r| r.role == "ci"));
        assert_eq!(row(&report, "planner").status, RowStatus::Ready);
        assert_eq!(row(&report, "ci").harness, "gh");
        assert_eq!(row(&report, "planner").login.as_deref(), Some("grok login"));
        assert_eq!(
            row(&report, "plan_reviewer_agy").login.as_deref(),
            Some("agy")
        );
        assert!(row(&report, "planner").required);
        assert!(!row(&report, "cross_model_secondary").required);
    }

    #[test]
    fn doctor_dedup_same_path_one_version_spawn() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let _ = probe_machine().unwrap();
        let grok_versions = probe
            .calls()
            .into_iter()
            .filter(|c| {
                c.bin.file_stem().and_then(|s| s.to_str()) == Some("grok")
                    && c.args.first().map(String::as_str) == Some("--version")
            })
            .count();
        assert_eq!(grok_versions, 1, "planner+implementor must share one spawn");
    }

    #[test]
    fn doctor_gh_and_grok_argv_pins() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let _ = probe_machine().unwrap();
        let calls = probe.calls();
        for c in &calls {
            assert!(!c.args.iter().any(|a| a == "--help"), "never --help: {c:?}");
            assert!(
                !c.args.iter().any(|a| a == "stdio" || a == "agent"),
                "never grok agent stdio: {c:?}"
            );
            assert!(
                !c.args.iter().any(|a| a == "--show-token"),
                "never --show-token: {c:?}"
            );
        }
        let gh_auth = calls.iter().any(|c| {
            c.args
                == [
                    "auth".to_string(),
                    "status".into(),
                    "--active".into(),
                    "--hostname".into(),
                    "github.com".into(),
                ]
        });
        assert!(gh_auth, "gh argv missing: {calls:?}");
    }

    #[test]
    fn doctor_scripted_auth_required_refuses_run() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        // no XAI_API_KEY, no auth.json → grok auth
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let (_proj, rec) = iso.add_project();
        let err =
            api::cmd_run(Some(&rec.id), Some("0028".into()), Some("adapter"), false).unwrap_err();
        let msg = err.to_string();
        match err {
            CoordinatorError::Preflight { report } => {
                assert!(!report.ok);
                assert_eq!(row(&report, "planner").status, RowStatus::Auth);
                assert_eq!(row(&report, "planner").login.as_deref(), Some("grok login"));
            }
            other => panic!("expected Preflight, got {other}"),
        }
        let path = crate::state::run_state_path(&rec).unwrap();
        assert!(!path.exists(), "run-state must not be written");
        let view = api::status(Some(&rec.id)).unwrap();
        assert_eq!(view.status, RunStatus::Idle);
        assert!(crate::notify::artifact::existing_path(&rec).is_none());
        assert!(msg.contains("grok login"), "Display must name login: {msg}");
    }

    #[test]
    fn doctor_scripted_missing_required_refuses_run() {
        let iso = IsolatedDoctor::enter();
        let bins = iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        iso.write_bindings(|b| {
            b.get_mut("planner").unwrap().command = iso
                .home
                .path()
                .join("missing-planner.exe")
                .to_string_lossy()
                .into();
            b.get_mut("implementor").unwrap().command = bins["grok"].to_string_lossy().into();
            b.get_mut("plan_reviewer_agy").unwrap().command = bins["agy"].to_string_lossy().into();
            b.get_mut("plan_reviewer_opencode").unwrap().command =
                bins["opencode"].to_string_lossy().into();
            b.get_mut("cross_model_primary").unwrap().command =
                bins["codex"].to_string_lossy().into();
            b.get_mut("cross_model_secondary").unwrap().command =
                bins["claude"].to_string_lossy().into();
            b.get_mut("cross_model_tertiary").unwrap().command =
                bins["opencode"].to_string_lossy().into();
        });
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let (_proj, rec) = iso.add_project();
        let err =
            api::cmd_run(Some(&rec.id), Some("0028".into()), Some("adapter"), false).unwrap_err();
        match &err {
            CoordinatorError::Preflight { report } => {
                assert_eq!(row(report, "planner").status, RowStatus::Missing);
                assert!(row(report, "planner").path.is_none());
            }
            other => panic!("expected Preflight, got {other}"),
        }
        assert!(!crate::state::run_state_path(&rec).unwrap().exists());
        assert_eq!(api::status(Some(&rec.id)).unwrap().status, RunStatus::Idle);
    }

    #[test]
    fn doctor_unknown_required_does_not_refuse() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready().with_version("codex", 124, true);
        let _g = install_test_probe(probe);
        let (_proj, rec) = iso.add_project();
        let result = api::cmd_run(Some(&rec.id), Some("0028".into()), Some("adapter"), false);
        match result {
            Err(CoordinatorError::Preflight { report }) => {
                panic!("unknown must not refuse: {report:?}")
            }
            Ok(view) => assert_eq!(view.status, RunStatus::Running),
            Err(e) => panic!("unexpected err {e}"),
        }
    }

    #[test]
    fn skip_preflight_proceeds_despite_missing() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.write_bindings(|b| {
            b.get_mut("planner").unwrap().command = iso
                .home
                .path()
                .join("no-planner.exe")
                .to_string_lossy()
                .into();
        });
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let (_proj, rec) = iso.add_project();
        let view = api::cmd_run(Some(&rec.id), Some("0028".into()), Some("adapter"), true).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert!(probe.calls().is_empty(), "skip must not probe");
    }

    #[test]
    fn stub_and_file_wait_zero_probe_calls() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let (_proj, rec) = iso.add_project();
        api::cmd_run(Some(&rec.id), Some("0028".into()), Some("stub"), false).unwrap();
        assert!(probe.calls().is_empty(), "stub must not probe");
        let (_proj2, rec2) = iso.add_project();
        api::cmd_run(
            Some(&rec2.id),
            Some("0028".into()),
            Some("file_wait"),
            false,
        )
        .unwrap();
        assert!(probe.calls().is_empty(), "file_wait must not probe");
    }

    #[test]
    fn doctor_omit_project_with_two_projects() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let _a = iso.add_project();
        let _b = iso.add_project();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let report = api::cmd_doctor(None).unwrap();
        assert!(report.ok);
        assert!(report.rows.iter().any(|r| r.role == "ci"));
    }

    #[test]
    fn doctor_project_typo_is_not_found_no_probes() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe.clone());
        let err = api::cmd_doctor(Some("no-such-project")).unwrap_err();
        assert!(matches!(err, CoordinatorError::ProjectNotFound(_)));
        assert!(probe.calls().is_empty());
    }

    #[test]
    fn env_pin_first_missing_binary() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let missing = iso.home.path().join("no-agy.exe");
        unsafe {
            std::env::set_var(ENV_COORDINATOR_AGY_BIN, &missing);
        }
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        assert_eq!(row(&report, "plan_reviewer_agy").status, RowStatus::Missing);
        assert!(!report.ok);
    }

    #[test]
    fn empty_env_pin_does_not_override() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_AGY_BIN, "   ");
        }
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        assert_eq!(row(&report, "plan_reviewer_agy").status, RowStatus::Ready);
    }

    #[test]
    fn empty_xai_api_key_is_not_grok_ready() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        unsafe {
            std::env::set_var("XAI_API_KEY", "  ");
        }
        assert!(!iso.grok_home.path().join("auth.json").exists());
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        assert_eq!(row(&report, "planner").status, RowStatus::Auth);
        assert!(!report.ok);
    }

    #[test]
    fn secrets_never_enter_report() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready().with_auth(
            "gh",
            1,
            "",
            "Token: gho_LIVESECRETVALUE extra",
            false,
        );
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("gho_LIVESECRETVALUE"), "{json}");
        assert!(!json.contains("gho_"), "{json}");
        assert!(!json.contains("test-not-a-real-key"), "{json}");
        assert!(!json.contains("XAI_API_KEY="), "{json}");
    }

    #[test]
    fn redact_prefix_strips_known_tokens() {
        let s = redact_secrets("token gho_abc123 and ghp_zzz sk-openai xai-key");
        assert!(!s.contains("abc123"));
        assert!(s.contains("gho_***"));
    }
}
