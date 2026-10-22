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
use crate::harness::grok::{ENV_CURSOR_BIN, ENV_GROK_BIN, reject_or_replace_ps1, resolve_command};
use crate::harness::roles::{ROLE_FOLD, ROLE_NEXT, load_role_bindings};
use crate::harness::roles_swap::{
    ModelCheck, ModelProbeRefusal, ProbeReceipt, ProbeRefusal, RoleClass, check_command_class,
    class_for_role, target_profile,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const CURSOR_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Model-list probes (`opencode models`, `grok models`, `agy models`).
const MODEL_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

fn probe_timeout(harness: &str) -> Duration {
    if harness.eq_ignore_ascii_case("cursor") {
        CURSOR_PROBE_TIMEOUT
    } else {
        PROBE_TIMEOUT
    }
}

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
    fn run(&self, bin: &Path, args: &[String], timeout: Duration) -> RawOut;
}

struct DefaultProbe;

impl Probe for DefaultProbe {
    fn run(&self, bin: &Path, args: &[String], timeout: Duration) -> RawOut {
        match crate::review::spawn::run_process(
            bin,
            args,
            &std::env::temp_dir(),
            crate::review::spawn::ProcessWait {
                timeout,
                stall: None,
                watch_paths: &[],
            },
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
    pub timeout: Duration,
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
    models_by_stem: HashMap<String, RawOut>,
    default_version: RawOut,
    default_auth: RawOut,
    default_models: RawOut,
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
                models_by_stem: HashMap::new(),
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
                default_models: RawOut {
                    exit: 0,
                    stdout: "You are logged in with grok.com.\n\nDefault model: grok-4.7\n\n\
                             Available models:\n  * grok-4.7 (default)\n"
                        .into(),
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

    pub fn with_models(mut self, stem: &str, exit: i32, stdout: &str, timed_out: bool) -> Self {
        self.ensure_unique();
        std::sync::Arc::get_mut(&mut self.inner)
            .expect("scripted probe unique")
            .models_by_stem
            .insert(
                stem.to_ascii_lowercase(),
                RawOut {
                    exit,
                    stdout: stdout.into(),
                    stderr: String::new(),
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
                models_by_stem: self.inner.models_by_stem.clone(),
                default_version: self.inner.default_version.clone(),
                default_auth: self.inner.default_auth.clone(),
                default_models: self.inner.default_models.clone(),
            };
            self.inner = std::sync::Arc::new(inner);
        }
    }
}

#[cfg(test)]
impl Probe for ScriptedProbe {
    fn run(&self, bin: &Path, args: &[String], timeout: Duration) -> RawOut {
        self.inner
            .calls
            .lock()
            .expect("scripted calls")
            .push(RecordedCall {
                bin: bin.to_path_buf(),
                args: args.to_vec(),
                timeout,
            });
        let stem = bin
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let first = args.first().map(|s| s.as_str());
        if first == Some("--version") {
            self.inner
                .version_by_stem
                .get(&stem)
                .cloned()
                .unwrap_or_else(|| self.inner.default_version.clone())
        } else if first == Some("models") {
            self.inner
                .models_by_stem
                .get(&stem)
                .cloned()
                .unwrap_or_else(|| self.inner.default_models.clone())
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

// ---------------------------------------------------------------------------
// 0110 targeted probe (one seam for the swap engine)
// ---------------------------------------------------------------------------

/// Probe one proposed binding with the runtime's own resolver for its role class.
///
/// Order: capability (`class_for_role` → `target_profile`) → per-class bin resolve →
/// command-class check → runtime `--version` → auth → model list. Every hard failure is
/// returned in [`ProbeRefusal::failures`] with the partial receipt; a refusal never
/// writes anything.
pub fn probe_target(
    role: &str,
    binding: &RoleBinding,
) -> std::result::Result<ProbeReceipt, ProbeRefusal> {
    let probe = active_probe();
    probe_target_with(probe.as_ref(), role, binding)
}

fn probe_target_with(
    probe: &dyn Probe,
    role: &str,
    binding: &RoleBinding,
) -> std::result::Result<ProbeReceipt, ProbeRefusal> {
    let class = class_for_role(role);
    let mut receipt = ProbeReceipt::empty(role, class, binding);
    let mut failures: Vec<String> = Vec::new();
    let mut cache: HashMap<(PathBuf, String), RawOut> = HashMap::new();

    if class == RoleClass::Other {
        failures.push(format!(
            "role {role} is not swappable (adapter, plan-review or cross-model role required)"
        ));
        return Err(ProbeRefusal {
            receipt: Box::new(receipt),
            failures,
        });
    }
    if let Err(e) = target_profile(class, &binding.harness) {
        failures.push(e.to_string());
        return Err(ProbeRefusal {
            receipt: Box::new(receipt),
            failures,
        });
    }

    let runtime = match resolve_runtime_bin(class, role, binding) {
        Ok(path) => path,
        Err(e) => {
            failures.push(format!(
                "runtime {} ({}) not resolvable for role class {}: {e}",
                binding.command,
                binding.harness,
                class.as_str()
            ));
            return Err(ProbeRefusal {
                receipt: Box::new(receipt),
                failures,
            });
        }
    };
    receipt.runtime_path = Some(runtime.clone());
    if let Err(e) = check_command_class(class, &binding.harness, &runtime) {
        failures.push(e.to_string());
    }

    let version = run_cached(
        probe,
        &mut cache,
        &runtime,
        &["--version".to_string()],
        probe_timeout(&binding.harness),
    );
    if version.timed_out {
        failures.push(format!(
            "runtime --version timed out: {}",
            runtime.display()
        ));
    } else if version.exit != 0 {
        failures.push(format!(
            "runtime --version failed (exit {}): {}",
            version.exit,
            runtime.display()
        ));
    } else {
        receipt.runtime_version =
            first_nonempty_line(&format!("{}\n{}", version.stdout, version.stderr));
        if receipt.runtime_version.is_none() {
            failures.push(format!(
                "runtime --version produced no output: {}",
                runtime.display()
            ));
        }
    }

    match auth_step_for(class, &binding.harness, &runtime) {
        Ok(step) => {
            receipt.auth_path = Some(step.path.clone());
            receipt.auth_argv = step.argv.clone();
            if step.note.is_some() {
                receipt.auth_detail = step.note.clone();
            }
            let out = run_cached(
                probe,
                &mut cache,
                &step.path,
                &step.argv,
                probe_timeout(&binding.harness),
            );
            let detail = classify_target_auth(&binding.harness, &out);
            receipt.auth_detail = Some(match (&receipt.auth_detail, &detail.detail) {
                (Some(note), Some(detail)) => format!("{note}; {detail}"),
                (Some(note), None) => note.clone(),
                (None, Some(detail)) => detail.clone(),
                (None, None) => String::new(),
            });
            if !detail.ready {
                failures.push(format!(
                    "auth {} {}: {}",
                    step.path.display(),
                    step.argv.join(" "),
                    detail
                        .detail
                        .clone()
                        .unwrap_or_else(|| "not authenticated".into())
                ));
            }
        }
        Err(e) => failures.push(format!("auth probe unavailable: {e}")),
    }

    match probe_model_with(
        probe,
        &mut cache,
        &binding.harness,
        &runtime,
        binding.model.as_deref(),
    ) {
        Ok(check) => {
            if check.is_fail() {
                failures.push(format!(
                    "model {}: {}",
                    binding.model.as_deref().unwrap_or("-"),
                    check.detail().unwrap_or("not available")
                ));
            }
            receipt.model_check = check;
        }
        Err(refusal) => {
            failures.push(format!(
                "model {}: {}",
                binding.model.as_deref().unwrap_or("-"),
                refusal.detail()
            ));
            receipt.model_check = ModelCheck::Fail {
                detail: refusal.detail().to_string(),
            };
        }
    }

    if failures.is_empty() {
        Ok(receipt)
    } else {
        Err(ProbeRefusal {
            receipt: Box::new(receipt),
            failures,
        })
    }
}

pub(crate) fn resolve_runtime_bin(
    class: RoleClass,
    role: &str,
    binding: &RoleBinding,
) -> crate::error::Result<PathBuf> {
    match class {
        RoleClass::Adapter => crate::harness::roles::resolve_binding_binary(binding, role),
        RoleClass::PlanReviewAgy => {
            if binding.harness.eq_ignore_ascii_case("codex") {
                crate::review::spawn::resolve_review_bin("codex", &binding.command)
            } else {
                crate::workflow::plan_review::resolve_agy_bin(&binding.command)
            }
        }
        RoleClass::PlanReviewOpenCode => {
            let harness = if binding.harness.eq_ignore_ascii_case("codex") {
                "codex"
            } else {
                "opencode"
            };
            crate::review::spawn::resolve_review_bin(harness, &binding.command)
        }
        RoleClass::CrossModel => {
            crate::review::spawn::resolve_review_bin(&binding.harness, &binding.command)
        }
        RoleClass::Other => Err(crate::error::CoordinatorError::Message(format!(
            "role {role} is not swappable"
        ))),
    }
}

struct AuthStep {
    path: PathBuf,
    argv: Vec<String>,
    note: Option<String>,
}

fn companion_step(cli: &str, env_name: &str, argv: Vec<String>) -> crate::error::Result<AuthStep> {
    let mut note = None;
    let path = match trim_env(env_name) {
        Some(pin) => {
            let pin_path = Path::new(&pin);
            if crate::harness::roles_swap::command_basename(pin_path).eq_ignore_ascii_case(cli) {
                resolve_command(&pin).and_then(reject_or_replace_ps1)?
            } else {
                note = Some("env pin points at an ACP shim; companion auth used PATH".into());
                resolve_command(cli).and_then(reject_or_replace_ps1)?
            }
        }
        None => resolve_command(cli).and_then(reject_or_replace_ps1)?,
    };
    Ok(AuthStep { path, argv, note })
}

fn auth_step_for(
    class: RoleClass,
    harness: &str,
    runtime: &Path,
) -> crate::error::Result<AuthStep> {
    match class {
        RoleClass::Adapter => match harness.to_ascii_lowercase().as_str() {
            "grok" => Ok(AuthStep {
                path: runtime.to_path_buf(),
                argv: vec!["models".into()],
                note: Some("grok models (login truth from output, not auth.json)".into()),
            }),
            "cursor" => Ok(AuthStep {
                path: runtime.to_path_buf(),
                argv: vec!["status".into(), "--format".into(), "json".into()],
                note: None,
            }),
            "claude" => companion_step(
                "claude",
                ENV_COORDINATOR_CLAUDE_BIN,
                vec!["auth".into(), "status".into(), "--json".into()],
            ),
            "codex" => companion_step(
                "codex",
                ENV_COORDINATOR_CODEX_BIN,
                vec!["login".into(), "status".into()],
            ),
            other => Err(crate::error::CoordinatorError::Message(format!(
                "no adapter auth probe for {other}"
            ))),
        },
        RoleClass::PlanReviewAgy => {
            if harness.eq_ignore_ascii_case("codex") {
                Ok(AuthStep {
                    path: runtime.to_path_buf(),
                    argv: vec!["login".into(), "status".into()],
                    note: None,
                })
            } else {
                Ok(AuthStep {
                    path: runtime.to_path_buf(),
                    argv: vec!["models".into()],
                    note: Some("agy has no auth-status CLI; `agy models` is the availability probe and auth stays unverified".into()),
                })
            }
        }
        RoleClass::PlanReviewOpenCode | RoleClass::CrossModel => {
            match harness.to_ascii_lowercase().as_str() {
                "codex" => Ok(AuthStep {
                    path: runtime.to_path_buf(),
                    argv: vec!["login".into(), "status".into()],
                    note: None,
                }),
                "claude" => Ok(AuthStep {
                    path: runtime.to_path_buf(),
                    argv: vec!["auth".into(), "status".into(), "--json".into()],
                    note: None,
                }),
                "opencode" => Ok(AuthStep {
                    path: runtime.to_path_buf(),
                    argv: vec!["auth".into(), "list".into()],
                    note: None,
                }),
                other => Err(crate::error::CoordinatorError::Message(format!(
                    "no auth probe for {other}"
                ))),
            }
        }
        RoleClass::Other => Err(crate::error::CoordinatorError::Message(
            "not swappable".into(),
        )),
    }
}

struct AuthVerdict {
    ready: bool,
    detail: Option<String>,
}

fn classify_target_auth(harness: &str, out: &RawOut) -> AuthVerdict {
    if out.timed_out {
        return AuthVerdict {
            ready: false,
            detail: Some("auth probe timed out".into()),
        };
    }
    match harness.to_ascii_lowercase().as_str() {
        "grok" => {
            let text = format!("{}\n{}", out.stdout, out.stderr);
            let lower = text.to_ascii_lowercase();
            if lower.contains("not authenticated") {
                return AuthVerdict {
                    ready: false,
                    detail: Some("`grok models` says not authenticated".into()),
                };
            }
            let (default, ids) = parse_grok_models(&text);
            if lower.contains("logged in") || default.is_some() || !ids.is_empty() {
                AuthVerdict {
                    ready: true,
                    detail: Some(format!(
                        "`grok models` logged in (default {})",
                        default.unwrap_or_else(|| "-".into())
                    )),
                }
            } else {
                AuthVerdict {
                    ready: false,
                    detail: Some("`grok models` output is ambiguous".into()),
                }
            }
        }
        "cursor" => match serde_json::from_str::<serde_json::Value>(&out.stdout) {
            Ok(v) => match v.get("isAuthenticated").and_then(|x| x.as_bool()) {
                Some(true) => AuthVerdict {
                    ready: true,
                    detail: Some("cursor-agent status authenticated".into()),
                },
                Some(false) => AuthVerdict {
                    ready: false,
                    detail: Some("cursor-agent not authenticated".into()),
                },
                None => AuthVerdict {
                    ready: false,
                    detail: Some("cursor-agent status unparseable".into()),
                },
            },
            Err(_) => AuthVerdict {
                ready: false,
                detail: Some("cursor-agent status unparseable".into()),
            },
        },
        "antigravity" | "agy" => {
            let ids = parse_agy_models(&out.stdout);
            if out.exit == 0 && !ids.is_empty() {
                AuthVerdict {
                    ready: true,
                    detail: Some(format!(
                        "`agy models` listed {} ids; auth unverified",
                        ids.len()
                    )),
                }
            } else {
                AuthVerdict {
                    ready: false,
                    detail: Some("`agy models` listed no ids".into()),
                }
            }
        }
        "opencode" => {
            if out.exit == 0 && out.stdout.lines().any(|l| !l.trim().is_empty()) {
                AuthVerdict {
                    ready: true,
                    detail: Some("`opencode auth list` has a stored credential".into()),
                }
            } else {
                AuthVerdict {
                    ready: false,
                    detail: Some("`opencode auth list` is empty".into()),
                }
            }
        }
        "codex" => {
            if out.exit == 0 {
                AuthVerdict {
                    ready: true,
                    detail: first_nonempty_line(&out.stdout),
                }
            } else {
                AuthVerdict {
                    ready: false,
                    detail: Some("`codex login status` failed".into()),
                }
            }
        }
        "claude" => match serde_json::from_str::<serde_json::Value>(&out.stdout) {
            Ok(v) => match v.get("loggedIn").and_then(|x| x.as_bool()) {
                Some(true) => AuthVerdict {
                    ready: true,
                    detail: Some("claude auth status loggedIn=true".into()),
                },
                Some(false) => AuthVerdict {
                    ready: false,
                    detail: Some("claude auth status loggedIn=false".into()),
                },
                None => AuthVerdict {
                    ready: false,
                    detail: Some("claude auth status unparseable".into()),
                },
            },
            Err(_) => AuthVerdict {
                ready: false,
                detail: Some("claude auth status unparseable".into()),
            },
        },
        other => {
            if out.exit == 0 {
                AuthVerdict {
                    ready: true,
                    detail: first_nonempty_line(&out.stdout),
                }
            } else {
                AuthVerdict {
                    ready: false,
                    detail: Some(format!("auth probe for {other} failed")),
                }
            }
        }
    }
}

fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn parse_grok_models(text: &str) -> (Option<String>, Vec<String>) {
    let mut default = None;
    let mut ids = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Default model:") {
            let value = rest.trim();
            if !value.is_empty() {
                default = Some(value.to_string());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("* ") {
            let id = rest.trim().trim_end_matches("(default)").trim();
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        }
    }
    (default, ids)
}

fn parse_agy_models(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let id = parts.next()?.trim();
            let display = parts.next()?;
            if id.is_empty() || display.trim().is_empty() {
                return None;
            }
            if id.to_ascii_lowercase().starts_with("fetching") {
                return None;
            }
            Some(id.to_string())
        })
        .collect()
}

fn run_cached(
    probe: &dyn Probe,
    cache: &mut HashMap<(PathBuf, String), RawOut>,
    bin: &Path,
    args: &[String],
    timeout: Duration,
) -> RawOut {
    let key = (bin.to_path_buf(), args.join(" "));
    cache
        .entry(key)
        .or_insert_with(|| sanitize_out(probe.run(bin, args, timeout)))
        .clone()
}

fn probe_model_with(
    probe: &dyn Probe,
    cache: &mut HashMap<(PathBuf, String), RawOut>,
    harness: &str,
    runtime: &Path,
    model: Option<&str>,
) -> std::result::Result<ModelCheck, ModelProbeRefusal> {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return Ok(ModelCheck::NotRequested);
    };
    match harness.to_ascii_lowercase().as_str() {
        "opencode" => {
            let out = run_cached(
                probe,
                cache,
                runtime,
                &["models".to_string()],
                MODEL_PROBE_TIMEOUT,
            );
            if out.timed_out {
                return Err(ModelProbeRefusal::Timeout {
                    detail: "`opencode models` timed out".into(),
                });
            }
            if out.exit != 0 {
                return Err(ModelProbeRefusal::Unavailable {
                    detail: format!("`opencode models` exited {}", out.exit),
                });
            }
            let lines: Vec<&str> = out
                .stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if lines.is_empty() {
                return Err(ModelProbeRefusal::Unavailable {
                    detail: "`opencode models` listed nothing".into(),
                });
            }
            // Variants are catalog-defined: the installed list has no `#` line. Match the
            // BASE selector exactly and carry the variant verbatim into the argv.
            let (base, variant) = match model.rfind('#') {
                Some(i) => (&model[..i], Some(&model[i + 1..])),
                None => (model, None),
            };
            if base.trim().is_empty() {
                return Err(ModelProbeRefusal::Unparseable {
                    detail: format!("model selector {model} has an empty base"),
                });
            }
            if lines.iter().any(|line| *line == base.trim()) {
                let detail = match variant {
                    Some(v) => format!(
                        "base {} listed; variant #{v} catalog-defined (not enumerated by `opencode models`)",
                        base.trim()
                    ),
                    None => format!("{} listed by `opencode models`", base.trim()),
                };
                Ok(ModelCheck::Pass { detail })
            } else {
                Ok(ModelCheck::Fail {
                    detail: format!(
                        "base {} is not listed by `opencode models` ({} lines)",
                        base.trim(),
                        lines.len()
                    ),
                })
            }
        }
        "grok" => {
            let out = run_cached(
                probe,
                cache,
                runtime,
                &["models".to_string()],
                MODEL_PROBE_TIMEOUT,
            );
            if out.timed_out {
                return Err(ModelProbeRefusal::Timeout {
                    detail: "`grok models` timed out".into(),
                });
            }
            let text = format!("{}\n{}", out.stdout, out.stderr);
            if text.to_ascii_lowercase().contains("not authenticated") {
                return Err(ModelProbeRefusal::Auth {
                    detail: "`grok models` says not authenticated".into(),
                });
            }
            let (default, ids) = parse_grok_models(&text);
            if default.is_none() && ids.is_empty() {
                return Err(ModelProbeRefusal::Ambiguous {
                    detail: "`grok models` output has no default and no bullet list".into(),
                });
            }
            if ids.iter().any(|id| id == model) || default.as_deref() == Some(model) {
                Ok(ModelCheck::Pass {
                    detail: format!("{model} listed by `grok models`"),
                })
            } else {
                Ok(ModelCheck::Fail {
                    detail: format!("{model} is not in `grok models`"),
                })
            }
        }
        "antigravity" | "agy" => {
            let out = run_cached(
                probe,
                cache,
                runtime,
                &["models".to_string()],
                MODEL_PROBE_TIMEOUT,
            );
            if out.timed_out {
                return Err(ModelProbeRefusal::Timeout {
                    detail: "`agy models` timed out".into(),
                });
            }
            let ids = parse_agy_models(&out.stdout);
            if ids.is_empty() {
                return Err(ModelProbeRefusal::Unparseable {
                    detail: "`agy models` produced a banner but no `id<TAB>display` rows".into(),
                });
            }
            if ids.iter().any(|id| id == model) {
                Ok(ModelCheck::Pass {
                    detail: format!("{model} listed by `agy models`"),
                })
            } else {
                Ok(ModelCheck::Fail {
                    detail: format!("{model} is not in `agy models`"),
                })
            }
        }
        "codex" | "claude" | "cursor" => Ok(ModelCheck::Unverified {
            detail: format!(
                "{} exposes no model list; auth checked only (OD-110-3: not availability proof)",
                harness
            ),
        }),
        other => Ok(ModelCheck::Unverified {
            detail: format!("{other} exposes no model list; auth checked only"),
        }),
    }
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
        .or_insert_with(|| {
            sanitize_out(probe.run(&path, &["--version".into()], probe_timeout(&harness)))
        })
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
    let detail = merge_shell_detail(&harness, detail);
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

fn merge_shell_detail(harness: &str, existing: Option<String>) -> Option<String> {
    if !harness.eq_ignore_ascii_case("grok") {
        return existing;
    }
    let Some(extra) = crate::harness::terminal::grok_terminal_shell_detail() else {
        return existing;
    };
    match existing {
        None => Some(extra),
        Some(old) => Some(format!("{old}; {extra}")),
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
            let cache_key = (path.to_path_buf(), "models".to_string());
            let out = auth_cache
                .entry(cache_key)
                .or_insert_with(|| {
                    sanitize_out(probe.run(path, &["models".to_string()], probe_timeout("grok")))
                })
                .clone();
            classify_auth_out("grok", &out)
        }
        "antigravity" | "agy" => (RowStatus::Ready, None),
        other => {
            let Some(args) = auth_args(other) else {
                // Unknown harness: `--version` already succeeded.
                return (RowStatus::Ready, None);
            };
            let cache_key = (path.to_path_buf(), other.to_string());
            let timeout = probe_timeout(other);
            let out = auth_cache
                .entry(cache_key)
                .or_insert_with(|| sanitize_out(probe.run(path, &args, timeout)))
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
        // 0110: read `grok models`, not the presence of `~/.grok/auth.json`.
        // Observed twice in one pass on 2026-10-21: the file existed while the command
        // said "You are not authenticated." The command output wins; the file/key is a
        // fallback only when the models probe itself is unavailable.
        "grok" => {
            if out.timed_out || out.exit == 127 {
                if grok_auth_ready() {
                    return (
                        RowStatus::Ready,
                        Some("models probe unavailable; XAI_API_KEY/auth.json present".into()),
                    );
                }
                return (
                    RowStatus::Auth,
                    Some("logged out (models probe unavailable)".into()),
                );
            }
            let text = format!("{}\n{}", out.stdout, out.stderr);
            let lower = text.to_ascii_lowercase();
            if lower.contains("not authenticated") {
                return (RowStatus::Auth, Some("logged out".into()));
            }
            let (default, ids) = parse_grok_models(&text);
            if lower.contains("logged in") || default.is_some() || !ids.is_empty() {
                return (RowStatus::Ready, first_nonempty_line(&text));
            }
            (
                RowStatus::Unknown,
                Some("grok models output ambiguous".into()),
            )
        }
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
        "cursor" => match serde_json::from_str::<serde_json::Value>(&out.stdout) {
            Ok(v) => match v.get("isAuthenticated").and_then(|x| x.as_bool()) {
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
        "cursor" => Some(vec!["status".into(), "--format".into(), "json".into()]),
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
        "cursor" => Some("cursor-agent login"),
        "gh" => Some("gh auth login"),
        _ => None,
    }
}

fn env_pin_for_harness(harness: &str) -> Option<&'static str> {
    match harness.to_ascii_lowercase().as_str() {
        "grok" => Some(ENV_GROK_BIN),
        "cursor" => Some(ENV_CURSOR_BIN),
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

pub(crate) fn redact_secrets(s: &str) -> String {
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
                ENV_CURSOR_BIN,
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
        assert!(
            report.ok,
            "missing pwsh must not refuse run: {:?}",
            row(&report, "planner").detail
        );
    }

    #[test]
    fn merge_shell_detail_skips_non_grok() {
        assert_eq!(
            merge_shell_detail("agy", Some("x".into())),
            Some("x".into())
        );
        assert_eq!(merge_shell_detail("gh", None), None);
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
    fn doctor_cursor_authenticated_ready_no_pii() {
        let iso = IsolatedDoctor::enter();
        let cursor = iso.dummy_bin("cursor-agent.exe");
        iso.point_all_to_dummies();
        let mut cfg = crate::config::load_machine_config().unwrap();
        cfg.role_bindings.get_mut("planner").unwrap().harness = "cursor".into();
        cfg.role_bindings.get_mut("planner").unwrap().command = cursor.to_string_lossy().into();
        cfg.role_bindings.get_mut("implementor").unwrap().harness = "cursor".into();
        cfg.role_bindings.get_mut("implementor").unwrap().command = cursor.to_string_lossy().into();
        save_machine_config(&cfg).unwrap();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready().with_auth(
            "cursor-agent",
            0,
            r#"{"status":"authenticated","isAuthenticated":true,"userInfo":{"email":"ryan@example.com","userId":1}}"#,
            "",
            false,
        );
        let _g = install_test_probe(probe.clone());
        let report = probe_machine().unwrap();
        let planner = row(&report, "planner");
        assert_eq!(planner.status, RowStatus::Ready);
        assert_eq!(planner.login.as_deref(), Some("cursor-agent login"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("ryan@example.com"), "{json}");
        assert!(!json.contains("userId"), "{json}");
        assert!(
            planner.detail.as_ref().is_none_or(|d| !d.contains("ryan@")),
            "detail={:?}",
            planner.detail
        );
        let cursor_calls: Vec<_> = probe
            .calls()
            .into_iter()
            .filter(|c| c.bin.file_stem().and_then(|s| s.to_str()) == Some("cursor-agent"))
            .collect();
        assert!(
            cursor_calls
                .iter()
                .any(|c| c.args == ["status".to_string(), "--format".into(), "json".into()]),
            "calls={cursor_calls:?}"
        );
        assert!(
            cursor_calls
                .iter()
                .all(|c| c.timeout == Duration::from_secs(8)),
            "cursor probe must be 8s: {cursor_calls:?}"
        );
    }

    #[test]
    fn doctor_cursor_logged_out_is_auth() {
        let iso = IsolatedDoctor::enter();
        let cursor = iso.dummy_bin("cursor-agent.exe");
        iso.point_all_to_dummies();
        let mut cfg = crate::config::load_machine_config().unwrap();
        cfg.role_bindings.get_mut("planner").unwrap().harness = "cursor".into();
        cfg.role_bindings.get_mut("planner").unwrap().command = cursor.to_string_lossy().into();
        save_machine_config(&cfg).unwrap();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready().with_auth(
            "cursor-agent",
            0,
            r#"{"isAuthenticated":false}"#,
            "",
            false,
        );
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        let planner = row(&report, "planner");
        assert_eq!(planner.status, RowStatus::Auth);
        assert_eq!(planner.detail.as_deref(), Some("logged out"));
        assert!(!report.ok);
    }

    #[test]
    fn doctor_cursor_bin_env_pin_wins() {
        let iso = IsolatedDoctor::enter();
        let command = iso.dummy_bin("command-cursor.exe");
        let pin = iso.dummy_bin("pinned-cursor.exe");
        iso.point_all_to_dummies();
        let mut cfg = crate::config::load_machine_config().unwrap();
        cfg.role_bindings.get_mut("planner").unwrap().harness = "cursor".into();
        cfg.role_bindings.get_mut("planner").unwrap().command = command.to_string_lossy().into();
        save_machine_config(&cfg).unwrap();
        iso.grok_ready_via_key();
        unsafe {
            std::env::set_var(ENV_CURSOR_BIN, &pin);
        }
        let probe = ScriptedProbe::ready().with_auth(
            "pinned-cursor",
            0,
            r#"{"isAuthenticated":true}"#,
            "",
            false,
        );
        let _g = install_test_probe(probe.clone());
        let report = probe_machine().unwrap();
        assert_eq!(row(&report, "planner").status, RowStatus::Ready);
        assert!(
            probe.calls().iter().any(|c| c.bin == pin),
            "calls={:?}",
            probe.calls()
        );
    }

    #[test]
    fn probe_timeout_cursor_is_eight_seconds() {
        assert_eq!(probe_timeout("cursor"), Duration::from_secs(8));
        assert_eq!(probe_timeout("CURSOR"), Duration::from_secs(8));
        assert_eq!(probe_timeout("grok"), Duration::from_secs(2));
        assert_eq!(probe_timeout("codex"), Duration::from_secs(2));
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
        // no XAI_API_KEY, no auth.json and the truthful `grok models` probe says logged out
        let probe =
            ScriptedProbe::ready().with_models("grok", 0, "You are not authenticated.", false);
        let _g = install_test_probe(probe);
        let (_proj, rec) = iso.add_project();
        let err = api::cmd_run(
            Some(&rec.id),
            Some("0028".into()),
            Some("adapter"),
            false,
            false,
        )
        .unwrap_err();
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
        let view = api::status(Some(&rec.id), false).unwrap();
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
        let err = api::cmd_run(
            Some(&rec.id),
            Some("0028".into()),
            Some("adapter"),
            false,
            false,
        )
        .unwrap_err();
        match &err {
            CoordinatorError::Preflight { report } => {
                assert_eq!(row(report, "planner").status, RowStatus::Missing);
                assert!(row(report, "planner").path.is_none());
            }
            other => panic!("expected Preflight, got {other}"),
        }
        assert!(!crate::state::run_state_path(&rec).unwrap().exists());
        assert_eq!(
            api::status(Some(&rec.id), false).unwrap().status,
            RunStatus::Idle
        );
    }

    #[test]
    fn doctor_unknown_required_does_not_refuse() {
        let iso = IsolatedDoctor::enter();
        iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        let probe = ScriptedProbe::ready().with_version("codex", 124, true);
        let _g = install_test_probe(probe);
        let (_proj, rec) = iso.add_project();
        let result = api::cmd_run(
            Some(&rec.id),
            Some("0028".into()),
            Some("adapter"),
            false,
            false,
        );
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
        let view = api::cmd_run(
            Some(&rec.id),
            Some("0028".into()),
            Some("adapter"),
            true,
            false,
        )
        .unwrap();
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
        api::cmd_run(
            Some(&rec.id),
            Some("0028".into()),
            Some("stub"),
            false,
            false,
        )
        .unwrap();
        assert!(probe.calls().is_empty(), "stub must not probe");
        let (_proj2, rec2) = iso.add_project();
        api::cmd_run(
            Some(&rec2.id),
            Some("0028".into()),
            Some("file_wait"),
            false,
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
    fn whitespace_grok_bin_is_unset_for_preflight_and_resolve() {
        let iso = IsolatedDoctor::enter();
        let bins = iso.point_all_to_dummies();
        iso.grok_ready_via_key();
        unsafe {
            std::env::set_var(ENV_GROK_BIN, "   ");
        }
        let probe = ScriptedProbe::ready();
        let _g = install_test_probe(probe);
        let report = probe_machine().unwrap();
        assert_eq!(row(&report, "planner").status, RowStatus::Ready);
        let resolved = crate::harness::resolve_grok_binary().unwrap();
        assert_eq!(resolved, bins["grok"]);
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
        let probe =
            ScriptedProbe::ready().with_models("grok", 0, "You are not authenticated.", false);
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
