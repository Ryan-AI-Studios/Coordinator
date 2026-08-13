//! Opt-in Hermes inbound webhook adapter (track 0015).
//!
//! POSTs existing [`NotifyEvent`] JSON to a **loopback** Hermes route, signed
//! with Hermes generic HMAC V2. Toast + Failure Artifact stay the default;
//! this adapter is extra and never required for orchestration.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::sync::{Arc, Mutex};

use crate::config::{HermesNotifyConfig, load_machine_config};
use crate::error::{CoordinatorError, Result};
use crate::notify::{NotifyAdapter, NotifyEvent};

/// Force-disable even if machine config is enabled.
pub const ENV_COORDINATOR_HERMES: &str = "COORDINATOR_HERMES";
/// Override webhook URL (wins over `config.json`).
pub const ENV_COORDINATOR_HERMES_URL: &str = "COORDINATOR_HERMES_URL";
/// HMAC secret. Env only — never persist.
pub const ENV_COORDINATOR_HERMES_SECRET: &str = "COORDINATOR_HERMES_SECRET";
/// Opt-in ignored live smoke (`1`).
pub const ENV_COORDINATOR_HERMES_LIVE: &str = "COORDINATOR_HERMES_LIVE";

const USER_AGENT: &str = "coordinator/0.1.0";
const HEADER_TIMESTAMP: &str = "X-Webhook-Timestamp";
const HEADER_SIGNATURE_V2: &str = "X-Webhook-Signature-V2";
const HEADER_REQUEST_ID: &str = "X-Request-ID";
const HEADER_EVENT: &str = "X-Coordinator-Event";
const EVENT_HARD_FAILURE: &str = "hard_failure";
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// Why Hermes did not POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    EnvOff,
    Disabled,
    MissingUrl,
    MissingSecret,
    InvalidUrl(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvOff => write!(f, "COORDINATOR_HERMES=off"),
            Self::Disabled => write!(
                f,
                "hermes disabled (set hermes.enabled or COORDINATOR_HERMES_URL)"
            ),
            Self::MissingUrl => write!(f, "missing webhook URL"),
            Self::MissingSecret => write!(f, "missing COORDINATOR_HERMES_SECRET"),
            Self::InvalidUrl(why) => write!(f, "invalid webhook URL: {why}"),
        }
    }
}

/// Resolved POST target, or a skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HermesResolve {
    Skip(SkipReason),
    Ready { url: String, secret: String },
}

/// Outcome of `coordinator notify hermes-test`.
#[derive(Debug)]
pub enum ProbeOutcome {
    Skipped(SkipReason),
    Delivered { status: u16 },
    Failed(CoordinatorError),
}

/// One signed request (tests + probe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Canned Scripted backend result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptedOutcome {
    Status(u16),
    Timeout,
    ConnectFail,
}

#[derive(Clone)]
enum HermesKind {
    NoOp,
    Http {
        url: String,
        secret: String,
    },
    #[cfg(test)]
    Test(TestInstall),
}

#[cfg(test)]
#[derive(Clone)]
struct TestInstall {
    url: String,
    secret: String,
    mode: TestMode,
    sink: Arc<Mutex<Vec<CapturedRequest>>>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum TestMode {
    Recording,
    Scripted(ScriptedOutcome),
}

/// Hermes slot on the notify composite.
pub struct HermesAdapter {
    kind: HermesKind,
}

impl HermesAdapter {
    pub fn noop() -> Self {
        Self {
            kind: HermesKind::NoOp,
        }
    }

    /// Production: machine config + env. Tests must use [`for_default_stack`].
    pub fn from_machine() -> Self {
        match resolve_from_machine() {
            HermesResolve::Skip(reason) => {
                if !matches!(reason, SkipReason::Disabled | SkipReason::EnvOff) {
                    eprintln!("coordinator: hermes skipped: {reason}");
                }
                Self::noop()
            }
            HermesResolve::Ready { url, secret } => Self {
                kind: HermesKind::Http { url, secret },
            },
        }
    }

    /// Composite wiring: tests never read machine-home Hermes settings.
    pub fn for_default_stack() -> Self {
        #[cfg(test)]
        {
            test_slot_adapter()
        }
        #[cfg(not(test))]
        {
            Self::from_machine()
        }
    }

    pub fn http(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            kind: HermesKind::Http {
                url: url.into(),
                secret: secret.into(),
            },
        }
    }

    /// Blocking POST (CLI probe + loopback tests). Production notify detaches.
    pub fn notify_blocking(&self, event: &NotifyEvent) -> Result<u16> {
        match &self.kind {
            HermesKind::NoOp => Ok(0),
            HermesKind::Http { url, secret } => post_http(url, secret, event),
            #[cfg(test)]
            HermesKind::Test(inst) => deliver_test(inst, event).map(|()| 200),
        }
    }
}

impl NotifyAdapter for HermesAdapter {
    fn notify(&self, event: &NotifyEvent) -> Result<()> {
        match &self.kind {
            HermesKind::NoOp => Ok(()),
            HermesKind::Http { url, secret } => {
                let url = url.clone();
                let secret = secret.clone();
                let event = event.clone();
                let _ = std::thread::Builder::new()
                    .name("coordinator-hermes".into())
                    .spawn(move || match post_http(&url, &secret, &event) {
                        Ok(status) => {
                            eprintln!("coordinator: hermes delivered HTTP {status}");
                        }
                        Err(e) => {
                            eprintln!("coordinator: hermes failed (non-fatal): {e}");
                        }
                    });
                Ok(())
            }
            #[cfg(test)]
            HermesKind::Test(inst) => deliver_test(inst, event),
        }
    }
}

/// HMAC-SHA256 hex of `{timestamp}.{body}` (Hermes generic V2; no `sha256=` prefix).
pub fn sign_v2(secret: &str, timestamp: u64, body: &[u8]) -> String {
    let mut msg = format!("{timestamp}.").into_bytes();
    msg.extend_from_slice(body);
    let mac = hmac_sha256::HMAC::mac(&msg, secret.as_bytes());
    hex_lower(&mac)
}

/// Literal-host loopback policy. No DNS.
pub fn validate_webhook_url(raw: &str) -> std::result::Result<(), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty url".into());
    }
    let Some(rest) = raw.strip_prefix("http://") else {
        if raw.len() >= 8 && raw[..8].eq_ignore_ascii_case("https://") {
            return Err("https is not allowed".into());
        }
        return Err("url must be http://".into());
    };

    let (host, path) = split_host_path(rest)?;
    if path.is_empty() {
        return Err("path must be non-empty".into());
    }
    if !is_allowed_host(&host) {
        return Err(format!("host is not loopback: {host}"));
    }
    Ok(())
}

/// Coordinator idempotency key (Hermes caches `X-Request-ID` for 1 hour).
pub fn request_id(event: &NotifyEvent) -> String {
    format!(
        "{}:{}:{}:{}",
        event.project_id,
        event.run_epoch,
        event.phase,
        event.failure_class.as_str()
    )
}

/// Resolve config + env. Does not POST.
pub fn resolve_from_machine() -> HermesResolve {
    if env_is_off(ENV_COORDINATOR_HERMES) {
        return HermesResolve::Skip(SkipReason::EnvOff);
    }
    let url_env = env_nonempty(ENV_COORDINATOR_HERMES_URL);
    let cfg = load_hermes_config();
    let enabled = cfg.enabled || url_env.is_some();
    if !enabled {
        return HermesResolve::Skip(SkipReason::Disabled);
    }
    let Some(url) = url_env.or(cfg.webhook_url.filter(|s| !s.trim().is_empty())) else {
        return HermesResolve::Skip(SkipReason::MissingUrl);
    };
    if let Err(why) = validate_webhook_url(&url) {
        return HermesResolve::Skip(SkipReason::InvalidUrl(why));
    }
    let Some(secret) = env_nonempty(ENV_COORDINATOR_HERMES_SECRET) else {
        return HermesResolve::Skip(SkipReason::MissingSecret);
    };
    HermesResolve::Ready { url, secret }
}

/// Synthetic probe: no artifact, no toast. Blocking POST when gated on.
pub fn probe(event: &NotifyEvent) -> ProbeOutcome {
    match resolve_from_machine() {
        HermesResolve::Skip(reason) => ProbeOutcome::Skipped(reason),
        HermesResolve::Ready { url, secret } => match post_http(&url, &secret, event) {
            Ok(status) => ProbeOutcome::Delivered { status },
            Err(e) => ProbeOutcome::Failed(e),
        },
    }
}

pub fn synthetic_event(project_id: impl Into<String>) -> NotifyEvent {
    NotifyEvent {
        project_id: project_id.into(),
        track_id: Some("hermes-test".into()),
        phase: "hermes-test".into(),
        failure_class: crate::outcome::FailureClass::Timeout,
        message: Some("synthetic probe; no FAILURE.md".into()),
        last_event: "hermes-test".into(),
        artifact_path: std::path::PathBuf::from("FAILURE.md"),
        written_at: chrono::Utc::now(),
        run_epoch: 0,
    }
}

fn load_hermes_config() -> HermesNotifyConfig {
    load_machine_config().map(|c| c.hermes).unwrap_or_default()
}

fn env_is_off(name: &str) -> bool {
    matches!(
        std::env::var(name),
        Ok(s) if s.eq_ignore_ascii_case("off")
    )
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

fn split_host_path(rest: &str) -> std::result::Result<(String, String), String> {
    let path_at = path_start_index(rest);
    let authority = &rest[..path_at];
    if authority.contains('@') {
        return Err("userinfo is not allowed".into());
    }
    let path = rest[path_at..].to_string();

    if let Some(after_br) = authority.strip_prefix('[') {
        let close = after_br
            .find(']')
            .ok_or_else(|| "invalid ipv6 host".to_string())?;
        let host = after_br[..close].to_string();
        let after = &after_br[close + 1..];
        if after.is_empty() {
            return Ok((host, path));
        }
        let Some(port) = after.strip_prefix(':') else {
            return Err("invalid url after ipv6 host".into());
        };
        if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return Err("invalid url after ipv6 host".into());
        }
        return Ok((host, path));
    }

    if authority.is_empty() {
        return Err("missing host".into());
    }
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            h.to_string()
        }
        _ => authority.to_string(),
    };
    Ok((host, path))
}

/// First `/` that starts the path (not inside an IPv6 `[…]` literal).
fn path_start_index(rest: &str) -> usize {
    if let Some(after_br) = rest.strip_prefix('[')
        && let Some(close) = after_br.find(']')
    {
        let after = close + 1;
        return rest[after..]
            .find('/')
            .map(|i| after + i)
            .unwrap_or(rest.len());
    }
    rest.find('/').unwrap_or(rest.len())
}

fn is_allowed_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "localhost" || h == "::1" {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
        return ip.octets()[0] == 127;
    }
    false
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct PreparedPost {
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl PreparedPost {
    fn new(url: &str, secret: &str, event: &NotifyEvent) -> Result<Self> {
        validate_webhook_url(url).map_err(CoordinatorError::Message)?;
        let body = serde_json::to_vec(event)?;
        let timestamp = unix_now();
        let sig = sign_v2(secret, timestamp, &body);
        let headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("User-Agent".into(), USER_AGENT.into()),
            (HEADER_TIMESTAMP.into(), timestamp.to_string()),
            (HEADER_SIGNATURE_V2.into(), sig),
            (HEADER_REQUEST_ID.into(), request_id(event)),
            (HEADER_EVENT.into(), EVENT_HARD_FAILURE.into()),
        ];
        Ok(Self {
            url: url.to_string(),
            headers,
            body,
        })
    }

    #[cfg(test)]
    fn captured(&self) -> CapturedRequest {
        CapturedRequest {
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
        }
    }
}

#[cfg(test)]
fn header<'a>(req: &'a CapturedRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn post_http(url: &str, secret: &str, event: &NotifyEvent) -> Result<u16> {
    let prepared = PreparedPost::new(url, secret, event)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(POST_TIMEOUT))
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let mut req = agent.post(&prepared.url);
    for (k, v) in &prepared.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.send(prepared.body.as_slice()) {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                Ok(status)
            } else {
                Err(CoordinatorError::Message(format!("hermes HTTP {status}")))
            }
        }
        Err(ureq::Error::StatusCode(code)) => {
            Err(CoordinatorError::Message(format!("hermes HTTP {code}")))
        }
        Err(e) => Err(CoordinatorError::Message(format!(
            "hermes POST failed: {e}"
        ))),
    }
}

#[cfg(test)]
fn deliver_test(inst: &TestInstall, event: &NotifyEvent) -> Result<()> {
    let prepared = PreparedPost::new(&inst.url, &inst.secret, event)?;
    inst.sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(prepared.captured());
    match inst.mode {
        TestMode::Recording => Ok(()),
        TestMode::Scripted(ScriptedOutcome::Status(code)) if (200..300).contains(&code) => Ok(()),
        TestMode::Scripted(ScriptedOutcome::Status(code)) => {
            Err(CoordinatorError::Message(format!("hermes HTTP {code}")))
        }
        TestMode::Scripted(ScriptedOutcome::Timeout) => {
            Err(CoordinatorError::Message("hermes timeout".into()))
        }
        TestMode::Scripted(ScriptedOutcome::ConnectFail) => {
            Err(CoordinatorError::Message("hermes connect refused".into()))
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_SLOT: std::cell::RefCell<Option<TestInstall>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_slot_adapter() -> HermesAdapter {
    TEST_SLOT.with(|s| match s.borrow().as_ref() {
        Some(inst) => HermesAdapter {
            kind: HermesKind::Test(inst.clone()),
        },
        None => HermesAdapter::noop(),
    })
}

/// Install a recording backend for `Composite::default_stack` on this thread.
#[cfg(test)]
pub fn install_recording(url: &str, secret: &str) -> TestHermesGuard {
    install_test(url, secret, TestMode::Recording)
}

/// Install a scripted backend (401 / timeout / refuse) for apply-path tests.
#[cfg(test)]
pub fn install_scripted(url: &str, secret: &str, outcome: ScriptedOutcome) -> TestHermesGuard {
    install_test(url, secret, TestMode::Scripted(outcome))
}

#[cfg(test)]
fn install_test(url: &str, secret: &str, mode: TestMode) -> TestHermesGuard {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let inst = TestInstall {
        url: url.to_string(),
        secret: secret.to_string(),
        mode,
        sink: sink.clone(),
    };
    TEST_SLOT.with(|s| {
        *s.borrow_mut() = Some(inst);
    });
    TestHermesGuard { sink }
}

/// RAII test backend. Dropping clears the thread-local slot.
#[cfg(test)]
pub struct TestHermesGuard {
    sink: Arc<Mutex<Vec<CapturedRequest>>>,
}

#[cfg(test)]
impl TestHermesGuard {
    pub fn take(&self) -> Vec<CapturedRequest> {
        self.sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }
}

#[cfg(test)]
impl Drop for TestHermesGuard {
    fn drop(&mut self) {
        TEST_SLOT.with(|s| {
            *s.borrow_mut() = None;
        });
    }
}

/// Loopback HTTP listener that records one request and replies 200.
#[cfg(test)]
pub struct LoopbackListener {
    pub url: String,
    rx: std::sync::mpsc::Receiver<CapturedRequest>,
}

#[cfg(test)]
impl LoopbackListener {
    pub fn bind() -> Self {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("http://{addr}/webhooks/test");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                if let Ok(captured) = read_http_request(&mut stream, &format!("http://{addr}")) {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    let _ = tx.send(captured);
                }
            }
        });
        Self { url, rx }
    }

    pub fn recv(self) -> CapturedRequest {
        self.rx
            .recv_timeout(Duration::from_secs(5))
            .expect("loopback received a POST")
    }

    pub fn try_recv_timeout(&self, timeout: Duration) -> Option<CapturedRequest> {
        self.rx.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
fn read_http_request(
    stream: &mut std::net::TcpStream,
    url_base: &str,
) -> std::io::Result<CapturedRequest> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_headers_end(&buf) {
            let head = &buf[..pos];
            let mut body = buf[pos + 4..].to_vec();
            let content_len = content_length(head).unwrap_or(0);
            while body.len() < content_len {
                let n = stream.read(&mut tmp)?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(content_len);
            let headers = parse_headers(head);
            let path = request_path(head).unwrap_or_else(|| "/".into());
            return Ok(CapturedRequest {
                url: format!("{url_base}{path}"),
                headers,
                body,
            });
        }
        if buf.len() > 1024 * 1024 {
            break;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "incomplete HTTP request",
    ))
}

#[cfg(test)]
fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
fn content_length(head: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            return rest.trim().parse().ok();
        }
        if let Some(rest) = line.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
fn request_path(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    let first = text.lines().next()?;
    let mut parts = first.split_whitespace();
    let _method = parts.next()?;
    parts.next().map(str::to_string)
}

#[cfg(test)]
fn parse_headers(head: &[u8]) -> Vec<(String, String)> {
    let Ok(text) = std::str::from_utf8(head) else {
        return Vec::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_COORDINATOR_HOME, HermesNotifyConfig, MACHINE_CONFIG_VERSION, MachineConfig,
        default_role_bindings, save_machine_config, test_env_lock,
    };
    use crate::outcome::FailureClass;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn event() -> NotifyEvent {
        NotifyEvent {
            project_id: "proj".into(),
            track_id: Some("0015".into()),
            phase: "implement".into(),
            failure_class: FailureClass::Timeout,
            message: Some("budget".into()),
            last_event: "x".into(),
            artifact_path: PathBuf::from("FAILURE.md"),
            written_at: Utc::now(),
            run_epoch: 9,
        }
    }

    fn isolated_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, dir.path());
            std::env::remove_var(ENV_COORDINATOR_HERMES);
            std::env::remove_var(ENV_COORDINATOR_HERMES_URL);
            std::env::remove_var(ENV_COORDINATOR_HERMES_SECRET);
        }
        dir
    }

    fn write_enabled_config(url: &str) {
        let cfg = MachineConfig {
            version: MACHINE_CONFIG_VERSION,
            scan_roots: Vec::new(),
            role_bindings: default_role_bindings(),
            phase_timeouts_secs: BTreeMap::new(),
            hermes: HermesNotifyConfig {
                enabled: true,
                webhook_url: Some(url.into()),
            },
        };
        save_machine_config(&cfg).unwrap();
    }

    #[test]
    fn sign_v2_known_vector() {
        let body = br#"{"hello":"world"}"#;
        let hex = sign_v2("test-secret", 1_700_000_000, body);
        assert_eq!(
            hex,
            "f85455a1b55ea86f7a15f5f9923d0abc4b888da84ec485f2ff358f427776beca"
        );
        assert!(!hex.starts_with("sha256="));
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn url_allow_table() {
        for url in [
            "http://127.0.0.1:8644/webhooks/coordinator-failure",
            "http://localhost:8644/webhooks/foo",
            "http://LOCALHOST:8644/webhooks/foo",
            "http://[::1]:8644/webhooks/foo",
            "http://[::1]/webhooks/foo",
            "http://127.0.0.2/x",
            "http://127.255.255.255:1/p",
        ] {
            validate_webhook_url(url).unwrap_or_else(|e| panic!("allow {url}: {e}"));
        }
    }

    #[test]
    fn url_reject_table() {
        for url in [
            "https://127.0.0.1:8644/x",
            "http://example.com/x",
            "http://0.0.0.0:8644/x",
            "http://127.0.0.1:8644",
            "http://user:pass@127.0.0.1:8644/x",
            "http://[::1]:8644@evil.com/x",
            "http://user@[::1]/x",
            "http://8.8.8.8/x",
            "ftp://127.0.0.1/x",
        ] {
            assert!(validate_webhook_url(url).is_err(), "should reject {url}");
        }
    }

    #[test]
    fn disabled_is_noop() {
        let _guard = test_env_lock();
        let _home = isolated_home();
        assert!(matches!(
            resolve_from_machine(),
            HermesResolve::Skip(SkipReason::Disabled)
        ));
        HermesAdapter::from_machine().notify(&event()).unwrap();
    }

    #[test]
    fn missing_secret_is_noop() {
        let _guard = test_env_lock();
        let _home = isolated_home();
        write_enabled_config("http://127.0.0.1:8644/webhooks/coordinator-failure");
        assert!(matches!(
            resolve_from_machine(),
            HermesResolve::Skip(SkipReason::MissingSecret)
        ));
    }

    #[test]
    fn env_off_wins() {
        let _guard = test_env_lock();
        let _home = isolated_home();
        write_enabled_config("http://127.0.0.1:8644/webhooks/coordinator-failure");
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HERMES_SECRET, "s");
            std::env::set_var(ENV_COORDINATOR_HERMES, "off");
        }
        assert!(matches!(
            resolve_from_machine(),
            HermesResolve::Skip(SkipReason::EnvOff)
        ));
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HERMES);
            std::env::remove_var(ENV_COORDINATOR_HERMES_SECRET);
        }
    }

    #[test]
    fn env_url_overrides_config() {
        let _guard = test_env_lock();
        let _home = isolated_home();
        write_enabled_config("http://127.0.0.1:8644/webhooks/old");
        unsafe {
            std::env::set_var(
                ENV_COORDINATOR_HERMES_URL,
                "http://127.0.0.1:8644/webhooks/new",
            );
            std::env::set_var(ENV_COORDINATOR_HERMES_SECRET, "s");
        }
        match resolve_from_machine() {
            HermesResolve::Ready { url, .. } => {
                assert_eq!(url, "http://127.0.0.1:8644/webhooks/new");
            }
            other => panic!("expected ready, got {other:?}"),
        }
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HERMES_URL);
            std::env::remove_var(ENV_COORDINATOR_HERMES_SECRET);
        }
    }

    #[test]
    fn recording_200_signs_once() {
        let ev = event();
        let sink = Arc::new(Mutex::new(Vec::new()));
        let adapter = HermesAdapter {
            kind: HermesKind::Test(TestInstall {
                url: "http://127.0.0.1:8644/webhooks/coordinator-failure".into(),
                secret: "s3cret".into(),
                mode: TestMode::Recording,
                sink: sink.clone(),
            }),
        };
        adapter.notify(&ev).unwrap();
        let captured = sink.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let req = &captured[0];
        let parsed: NotifyEvent = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(parsed, ev);
        let ts: u64 = header(req, HEADER_TIMESTAMP).unwrap().parse().unwrap();
        assert_eq!(
            header(req, HEADER_SIGNATURE_V2).unwrap(),
            sign_v2("s3cret", ts, &req.body)
        );
        assert_eq!(header(req, HEADER_REQUEST_ID).unwrap(), request_id(&ev));
        assert_eq!(header(req, HEADER_EVENT).unwrap(), EVENT_HARD_FAILURE);
        assert_eq!(header(req, "User-Agent").unwrap(), USER_AGENT);
        assert_eq!(request_id(&ev), "proj:9:implement:timeout");
    }

    #[test]
    fn scripted_401_is_err_and_records() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let adapter = HermesAdapter {
            kind: HermesKind::Test(TestInstall {
                url: "http://127.0.0.1:8644/webhooks/coordinator-failure".into(),
                secret: "s".into(),
                mode: TestMode::Scripted(ScriptedOutcome::Status(401)),
                sink: sink.clone(),
            }),
        };
        let err = adapter.notify(&event()).unwrap_err();
        assert!(err.to_string().contains("401"));
        assert_eq!(sink.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripted_connect_fail_is_err() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let adapter = HermesAdapter {
            kind: HermesKind::Test(TestInstall {
                url: "http://127.0.0.1:8644/webhooks/coordinator-failure".into(),
                secret: "s".into(),
                mode: TestMode::Scripted(ScriptedOutcome::ConnectFail),
                sink: sink.clone(),
            }),
        };
        let err = adapter.notify(&event()).unwrap_err();
        assert!(err.to_string().contains("connect"));
        assert_eq!(sink.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripted_timeout_isolated_in_composite() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Flag(Arc<AtomicBool>);
        impl NotifyAdapter for Flag {
            fn notify(&self, _event: &NotifyEvent) -> Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Vec::new()));
        let boom = HermesAdapter {
            kind: HermesKind::Test(TestInstall {
                url: "http://127.0.0.1:8644/webhooks/coordinator-failure".into(),
                secret: "s".into(),
                mode: TestMode::Scripted(ScriptedOutcome::Timeout),
                sink,
            }),
        };
        let seen = Arc::new(AtomicBool::new(false));
        let composite =
            crate::notify::Composite::new(vec![Box::new(boom), Box::new(Flag(seen.clone()))]);
        composite.notify(&event()).unwrap();
        assert!(seen.load(Ordering::SeqCst));
    }

    #[test]
    fn loopback_http_verifies_signature_over_exact_body() {
        let listener = LoopbackListener::bind();
        let ev = event();
        let status = HermesAdapter::http(&listener.url, "loop-secret")
            .notify_blocking(&ev)
            .unwrap();
        assert_eq!(status, 200);
        let req = listener.recv();
        let parsed: NotifyEvent = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(parsed.project_id, ev.project_id);
        assert_eq!(parsed.run_epoch, ev.run_epoch);
        let ts: u64 = header(&req, HEADER_TIMESTAMP).unwrap().parse().unwrap();
        assert_eq!(
            header(&req, HEADER_SIGNATURE_V2).unwrap(),
            sign_v2("loop-secret", ts, &req.body)
        );
        assert_eq!(header(&req, HEADER_REQUEST_ID).unwrap(), request_id(&ev));
    }

    #[test]
    fn default_stack_without_install_is_noop() {
        HermesAdapter::for_default_stack().notify(&event()).unwrap();
    }

    #[test]
    fn default_stack_ignores_machine_env_and_does_not_post() {
        let _guard = test_env_lock();
        let _home = isolated_home();
        let listener = LoopbackListener::bind();
        write_enabled_config(&listener.url);
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HERMES_URL, &listener.url);
            std::env::set_var(ENV_COORDINATOR_HERMES_SECRET, "live-secret");
        }
        HermesAdapter::for_default_stack().notify(&event()).unwrap();
        assert!(
            listener
                .try_recv_timeout(Duration::from_millis(400))
                .is_none(),
            "cfg(test) default_stack must not POST even if Hermes env/config are set"
        );
        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HERMES_URL);
            std::env::remove_var(ENV_COORDINATOR_HERMES_SECRET);
        }
    }

    #[test]
    #[ignore = "needs Hermes on 127.0.0.1:8644 + COORDINATOR_HERMES_LIVE=1"]
    fn hermes_live_probe() {
        if std::env::var(ENV_COORDINATOR_HERMES_LIVE).ok().as_deref() != Some("1") {
            eprintln!("skip: {ENV_COORDINATOR_HERMES_LIVE} != 1");
            return;
        }
        match probe(&synthetic_event("hermes-test")) {
            ProbeOutcome::Delivered { status } => {
                assert!((200..300).contains(&status), "status {status}");
            }
            ProbeOutcome::Skipped(r) => panic!("live skip: {r}"),
            ProbeOutcome::Failed(e) => panic!("live fail: {e}"),
        }
    }
}
