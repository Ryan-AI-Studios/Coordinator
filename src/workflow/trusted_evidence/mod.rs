//! Epoch-scoped skill-material snapshot.
//!
//! The file proves materialization of one artifact and that the required
//! skill pairs were in the same observed state at the two bracket endpoints.
//! It does not prove write access, in-harness loading, or that the state held
//! between the endpoints.

mod reader;
mod writer;

pub(crate) use reader::read;
#[allow(unused_imports)]
pub(crate) use writer::{WriteOutcome, restore_current_leaf, write_for_epoch};

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

pub(crate) const SNAPSHOT_MAX_BYTES: u64 = 262_144;
pub(crate) const SKILL_MAX_BYTES: u64 = 1_048_576;
pub(crate) const MAX_SKILL_ENTRIES: usize = 16;
pub(crate) const MAX_OBSERVATION_ENTRIES: usize = 16;
pub(crate) const MAX_PATH_UTF16: usize = 259;
pub(crate) const MAX_STRING_BYTES: usize = 4_096;
pub(crate) const ARCHIVE_KEEP: usize = 8;
pub(crate) const MAX_ANCESTOR_COMPONENTS: usize = 64;
pub(crate) const SCHEMA: &str = "coordinator.trusted-evidence";
pub(crate) const WRITER_ID: &str = "coordinator.trusted-evidence.writer/1";

pub(crate) const SKILL_PHASES: &[&str] = &[
    "plan",
    "fold",
    "implement",
    "address-findings",
    "address-ci",
    "advance",
];

const TOP_KEYS: &[&str] = &[
    "captured_at",
    "digest",
    "observations",
    "producer",
    "project_id",
    "run_epoch",
    "schema",
    "schema_version",
    "sequence",
    "skills",
    "track_id",
];

const PRODUCER_KEYS: &[&str] = &[
    "command",
    "harness",
    "hook",
    "model",
    "phase",
    "pid",
    "product_version",
    "role_key",
    "writer",
];

const SKILL_KEYS: &[&str] = &[
    "digest", "name", "path", "phase", "regular", "reparse", "scope", "size", "state",
];

const OBSERVATION_KEYS: &[&str] = &["fact", "name", "observed", "outcome", "phase", "reason"];

/// `FILE_ATTRIBUTE_REPARSE_POINT`. Stable attribute bit, not a name-surrogate test.
pub(crate) fn is_reparse(attrs: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
pub(crate) fn metadata_is_reparse(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    is_reparse(meta.file_attributes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProducerHook {
    RunBeginFreshRun,
    WorkflowAutoStart,
}

impl ProducerHook {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RunBeginFreshRun => "run_begin_fresh_run",
            Self::WorkflowAutoStart => "workflow_auto_start",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    None(NoEvidenceReason),
    Current(CurrentEvidence),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentEvidence {
    pub project_id: String,
    pub track_id: Option<String>,
    pub run_epoch: u64,
    pub phase: String,
    pub facts: Vec<SkillFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillFact {
    pub phase: String,
    pub name: String,
    pub scope: String,
    pub state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoEvidenceReason {
    Absent,
    UnsupportedVersion,
    WrongProject,
    WrongEpoch,
    WrongTrack,
    SnapshotDigestMismatch,
    Malformed,
    TooLarge,
    PathTooLong,
    StateRootUnresolved,
    RelativeStateRoot,
    RemoteStateRoot,
    NonNormalComponent,
    ReparseAncestor,
    ReparseLeaf,
    NotADirectory,
    NotRegularFile,
    ReadDenied,
    ReadError,
    PathNotAllowed,
    LeafChanged,
    CaptureRace,
    FactDrifted,
    #[cfg_attr(windows, allow(dead_code))]
    UnsupportedPlatform,
}

impl NoEvidenceReason {
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::UnsupportedVersion => "unsupported-version",
            Self::WrongProject => "wrong-project",
            Self::WrongEpoch => "wrong-epoch",
            Self::WrongTrack => "wrong-track",
            Self::SnapshotDigestMismatch => "snapshot-digest-mismatch",
            Self::Malformed => "malformed",
            Self::TooLarge => "too-large",
            Self::PathTooLong => "path-too-long",
            Self::StateRootUnresolved => "state-root-unresolved",
            Self::RelativeStateRoot => "relative-state-root",
            Self::RemoteStateRoot => "remote-state-root",
            Self::NonNormalComponent => "non-normal-component",
            Self::ReparseAncestor => "reparse-ancestor",
            Self::ReparseLeaf => "reparse-leaf",
            Self::NotADirectory => "not-a-directory",
            Self::NotRegularFile => "not-regular-file",
            Self::ReadDenied => "read-denied",
            Self::ReadError => "read-error",
            Self::PathNotAllowed => "path-not-allowed",
            Self::LeafChanged => "leaf-changed",
            Self::CaptureRace => "capture-race",
            Self::FactDrifted => "fact-drifted",
            Self::UnsupportedPlatform => "unsupported-platform",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequiredPair {
    pub phase: &'static str,
    pub name: &'static str,
    pub scope: &'static str,
}

const CAPTURE_PAIRS: &[RequiredPair] = &[
    RequiredPair {
        phase: "plan",
        name: "plan",
        scope: "workspace",
    },
    RequiredPair {
        phase: "fold",
        name: "foldin",
        scope: "workspace",
    },
    RequiredPair {
        phase: "implement",
        name: "implement",
        scope: "execution",
    },
    RequiredPair {
        phase: "implement",
        name: "onboarding",
        scope: "execution",
    },
    RequiredPair {
        phase: "address-findings",
        name: "implement",
        scope: "execution",
    },
    RequiredPair {
        phase: "address-findings",
        name: "onboarding",
        scope: "execution",
    },
    RequiredPair {
        phase: "advance",
        name: "plan",
        scope: "workspace",
    },
];

pub(crate) fn capture_pairs() -> &'static [RequiredPair] {
    CAPTURE_PAIRS
}

pub(crate) fn required_pairs(phase: &str) -> &'static [RequiredPair] {
    match phase {
        "implement" => &CAPTURE_PAIRS[2..4],
        "address-findings" => &CAPTURE_PAIRS[4..6],
        "fold" => &CAPTURE_PAIRS[1..2],
        "advance" => &CAPTURE_PAIRS[6..7],
        "plan" => &CAPTURE_PAIRS[0..1],
        _ => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSkill {
    pub phase: String,
    pub name: String,
    pub scope: String,
    pub path: Option<String>,
    pub state: String,
    pub digest: Option<String>,
    pub size: Option<u64>,
    pub regular: bool,
    pub reparse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSnapshot {
    pub project_id: String,
    pub track_id: Option<String>,
    pub run_epoch: u64,
    pub sequence: u64,
    pub digest: String,
    pub skills: Vec<ParsedSkill>,
}

pub(crate) fn none(reason: NoEvidenceReason) -> Evidence {
    Evidence::None(reason)
}

pub(crate) fn canonical_bytes(value: &Value) -> Option<Vec<u8>> {
    let mut copy = value.clone();
    let obj = copy.as_object_mut()?;
    obj.remove("digest");
    serde_json::to_vec(&copy).ok()
}

pub(crate) fn digest_of(value: &Value) -> Option<String> {
    let bytes = canonical_bytes(value)?;
    Some(format!(
        "sha256:{}",
        crate::registry_store::sha256_hex(&bytes)
    ))
}

pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::registry_store::sha256_hex(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathShape {
    Ok,
    Relative,
    Remote,
    NonNormal,
    Colon,
    TooLong,
}

#[cfg(windows)]
pub(crate) fn utf16_len(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count()
}

#[cfg(windows)]
fn os_has_colon(value: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().any(|unit| unit == u16::from(b':'))
}

#[cfg(windows)]
pub(crate) fn path_shape(path: &Path) -> PathShape {
    use std::path::Prefix;
    if !path.is_absolute() {
        return PathShape::Relative;
    }
    if utf16_len(path) > MAX_PATH_UTF16 {
        return PathShape::TooLong;
    }
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(_) | Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) => {}
                Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return PathShape::Remote,
                Prefix::DeviceNS(_) => return PathShape::NonNormal,
            },
            Component::RootDir => {}
            Component::Normal(name) => {
                if os_has_colon(name) {
                    return PathShape::Colon;
                }
            }
            Component::CurDir | Component::ParentDir => return PathShape::NonNormal,
        }
    }
    PathShape::Ok
}

#[cfg(not(windows))]
pub(crate) fn path_shape(_path: &Path) -> PathShape {
    PathShape::Relative
}

pub(crate) fn component_inside(path: &Path, root: &Path) -> bool {
    let path_parts: Vec<Component<'_>> = path.components().collect();
    let root_parts: Vec<Component<'_>> = root.components().collect();
    path_parts.len() > root_parts.len()
        && path_parts
            .iter()
            .zip(root_parts.iter())
            .all(|(left, right)| left == right)
}

pub(crate) fn map_io(err: &std::io::Error) -> NoEvidenceReason {
    let denied = err.kind() == std::io::ErrorKind::PermissionDenied
        || err.raw_os_error() == Some(5)
        || err.raw_os_error() == Some(32);
    if denied {
        NoEvidenceReason::ReadDenied
    } else {
        NoEvidenceReason::ReadError
    }
}

#[cfg(windows)]
pub(crate) fn ancestor_chain(path: &Path) -> Option<Vec<PathBuf>> {
    let mut chain = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if chain.len() >= MAX_ANCESTOR_COMPONENTS {
            return None;
        }
        chain.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    chain.reverse();
    Some(chain)
}

#[cfg(windows)]
pub(crate) fn path_allowed(path: &Path, roots: &[PathBuf]) -> Result<(), NoEvidenceReason> {
    #[cfg(test)]
    {
        PATH_ALLOWED_CALLS.with(|cell| cell.set(cell.get().saturating_add(1)));
    }
    match path_shape(path) {
        PathShape::Ok => {}
        PathShape::TooLong => return Err(NoEvidenceReason::PathTooLong),
        PathShape::Relative | PathShape::Remote | PathShape::NonNormal | PathShape::Colon => {
            return Err(NoEvidenceReason::PathNotAllowed);
        }
    }
    if !roots.iter().any(|root| component_inside(path, root)) {
        return Err(NoEvidenceReason::PathNotAllowed);
    }
    let chain = ancestor_chain(path).ok_or(NoEvidenceReason::ReparseAncestor)?;
    for ancestor in &chain {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if metadata_is_reparse(&meta) {
                    return Err(NoEvidenceReason::ReparseAncestor);
                }
                let is_leaf = ancestor == path;
                if !is_leaf && !meta.is_dir() {
                    return Err(NoEvidenceReason::PathNotAllowed);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(NoEvidenceReason::PathNotAllowed),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn path_allowed(_path: &Path, _roots: &[PathBuf]) -> Result<(), NoEvidenceReason> {
    Err(NoEvidenceReason::UnsupportedPlatform)
}

#[cfg(test)]
thread_local! {
    static PATH_ALLOWED_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static SKILL_OPENS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_skill_open() {
    SKILL_OPENS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn take_path_counters() -> (u32, u32) {
    let allowed = PATH_ALLOWED_CALLS.with(|cell| {
        let value = cell.get();
        cell.set(0);
        value
    });
    let opens = SKILL_OPENS.with(|cell| {
        let value = cell.get();
        cell.set(0);
        value
    });
    (allowed, opens)
}

pub(crate) fn parse_strict(bytes: &[u8]) -> Result<Value, NoEvidenceReason> {
    if reject_duplicate_keys(bytes).is_err() {
        return Err(NoEvidenceReason::Malformed);
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => Ok(value),
        Err(_) => Err(NoEvidenceReason::Malformed),
    }
}

fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), ()> {
    let mut scan = Scan { bytes, index: 0 };
    scan.value()?;
    scan.skip_ws();
    if scan.index != scan.bytes.len() {
        return Err(());
    }
    Ok(())
}

struct Scan<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Scan<'_> {
    fn skip_ws(&mut self) {
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() {
                self.index += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }

    fn value(&mut self) -> Result<(), ()> {
        self.skip_ws();
        match self.bump() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                self.string()?;
                Ok(())
            }
            Some(b't') => self.literal(b"rue"),
            Some(b'f') => self.literal(b"alse"),
            Some(b'n') => self.literal(b"ull"),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            _ => Err(()),
        }
    }

    fn literal(&mut self, rest: &[u8]) -> Result<(), ()> {
        for expected in rest {
            if self.bump() != Some(*expected) {
                return Err(());
            }
        }
        Ok(())
    }

    fn number(&mut self) -> Result<(), ()> {
        let first = self.bytes.get(self.index - 1).copied().ok_or(())?;
        if first == b'-' {
            let digit = self.bump().ok_or(())?;
            if !digit.is_ascii_digit() {
                return Err(());
            }
            self.finish_digits(digit)?;
        } else {
            self.finish_digits(first)?;
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            if !self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(());
            }
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.index += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(());
            }
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.index += 1;
            }
        }
        Ok(())
    }

    fn finish_digits(&mut self, first: u8) -> Result<(), ()> {
        if first == b'0' {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(());
            }
            return Ok(());
        }
        if !first.is_ascii_digit() {
            return Err(());
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.index += 1;
        }
        Ok(())
    }

    fn array(&mut self) -> Result<(), ()> {
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.index += 1;
            return Ok(());
        }
        loop {
            self.value()?;
            self.skip_ws();
            match self.bump() {
                Some(b']') => return Ok(()),
                Some(b',') => {
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        return Err(());
                    }
                }
                _ => return Err(()),
            }
        }
    }

    fn object(&mut self) -> Result<(), ()> {
        let mut keys = BTreeSet::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.index += 1;
            return Ok(());
        }
        loop {
            self.skip_ws();
            if self.bump() != Some(b'"') {
                return Err(());
            }
            let key = self.string()?;
            if !keys.insert(key) {
                return Err(());
            }
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err(());
            }
            self.value()?;
            self.skip_ws();
            match self.bump() {
                Some(b'}') => return Ok(()),
                Some(b',') => {
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        return Err(());
                    }
                }
                _ => return Err(()),
            }
        }
    }

    fn string(&mut self) -> Result<String, ()> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let byte = self.bump().ok_or(())?;
            match byte {
                b'"' => {
                    return String::from_utf8(buf).map_err(|_| ());
                }
                b'\\' => {
                    let escaped = self.bump().ok_or(())?;
                    match escaped {
                        b'"' => buf.push(b'"'),
                        b'\\' => buf.push(b'\\'),
                        b'/' => buf.push(b'/'),
                        b'b' => buf.push(0x08),
                        b'f' => buf.push(0x0c),
                        b'n' => buf.push(b'\n'),
                        b'r' => buf.push(b'\r'),
                        b't' => buf.push(b'\t'),
                        b'u' => {
                            let ch = self.unicode_escape()?;
                            let mut tmp = [0u8; 4];
                            buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        }
                        _ => return Err(()),
                    }
                }
                0x00..=0x1f => return Err(()),
                _ => buf.push(byte),
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, ()> {
        let unit = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&unit) {
            if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                return Err(());
            }
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(());
            }
            let value = 0x10000 + (((unit as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
            return char::from_u32(value).ok_or(());
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(());
        }
        char::from_u32(u32::from(unit)).ok_or(())
    }

    fn hex4(&mut self) -> Result<u16, ()> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.bump().ok_or(())?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(()),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }
}

pub(crate) fn schema_gate(value: &Value) -> Result<(), NoEvidenceReason> {
    let obj = value.as_object().ok_or(NoEvidenceReason::Malformed)?;
    let has_version = obj.contains_key("version");
    let has_schema = obj.contains_key("schema");
    let has_schema_version = obj.contains_key("schema_version");
    if has_version && !has_schema && !has_schema_version {
        return Err(NoEvidenceReason::UnsupportedVersion);
    }
    if obj.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(NoEvidenceReason::UnsupportedVersion);
    }
    if obj.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(NoEvidenceReason::UnsupportedVersion);
    }
    Ok(())
}

pub(crate) fn key_whitelist(value: &Value) -> Result<(), NoEvidenceReason> {
    let obj = value.as_object().ok_or(NoEvidenceReason::Malformed)?;
    exact_keys(obj, TOP_KEYS)?;
    let producer = obj
        .get("producer")
        .and_then(Value::as_object)
        .ok_or(NoEvidenceReason::Malformed)?;
    exact_keys(producer, PRODUCER_KEYS)?;
    let skills = obj
        .get("skills")
        .and_then(Value::as_array)
        .ok_or(NoEvidenceReason::Malformed)?;
    for skill in skills {
        let skill = skill.as_object().ok_or(NoEvidenceReason::Malformed)?;
        exact_keys(skill, SKILL_KEYS)?;
    }
    let observations = obj
        .get("observations")
        .and_then(Value::as_array)
        .ok_or(NoEvidenceReason::Malformed)?;
    for observation in observations {
        let observation = observation.as_object().ok_or(NoEvidenceReason::Malformed)?;
        exact_keys(observation, OBSERVATION_KEYS)?;
    }
    Ok(())
}

fn exact_keys(
    obj: &serde_json::Map<String, Value>,
    expected: &[&str],
) -> Result<(), NoEvidenceReason> {
    if obj.len() != expected.len() {
        return Err(NoEvidenceReason::Malformed);
    }
    for key in expected {
        if !obj.contains_key(*key) {
            return Err(NoEvidenceReason::Malformed);
        }
    }
    Ok(())
}

pub(crate) fn check_digest(value: &Value) -> Result<(), NoEvidenceReason> {
    let recorded = value
        .get("digest")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?;
    let computed = digest_of(value).ok_or(NoEvidenceReason::Malformed)?;
    if recorded != computed || !valid_digest(recorded) {
        return Err(NoEvidenceReason::SnapshotDigestMismatch);
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn identity_fields(
    value: &Value,
) -> Result<(String, Option<String>, u64, u64), NoEvidenceReason> {
    let project_id = value
        .get("project_id")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?
        .to_string();
    let track_id = match value.get("track_id") {
        Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        _ => return Err(NoEvidenceReason::Malformed),
    };
    let run_epoch = value
        .get("run_epoch")
        .and_then(Value::as_u64)
        .ok_or(NoEvidenceReason::Malformed)?;
    let sequence = value
        .get("sequence")
        .and_then(Value::as_u64)
        .ok_or(NoEvidenceReason::Malformed)?;
    Ok((project_id, track_id, run_epoch, sequence))
}

pub(crate) fn invariants(value: &Value) -> Result<ParsedSnapshot, NoEvidenceReason> {
    if contains_non_integer(value) {
        return Err(NoEvidenceReason::Malformed);
    }
    let (project_id, track_id, run_epoch, sequence) = identity_fields(value)?;
    if project_id.is_empty()
        || project_id.len() > MAX_STRING_BYTES
        || has_control(&project_id)
        || run_epoch < 1
        || sequence < 1
    {
        return Err(NoEvidenceReason::Malformed);
    }
    match &track_id {
        Some(text) if text.is_empty() || text.len() > MAX_STRING_BYTES || has_control(text) => {
            return Err(NoEvidenceReason::Malformed);
        }
        _ => {}
    }
    let captured_at = required_string(value, "captured_at", MAX_STRING_BYTES)?;
    if has_control(&captured_at) || !valid_captured_at(&captured_at) {
        return Err(NoEvidenceReason::Malformed);
    }
    check_producer(value.get("producer").ok_or(NoEvidenceReason::Malformed)?)?;
    let skills_value = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or(NoEvidenceReason::Malformed)?;
    let observations = value
        .get("observations")
        .and_then(Value::as_array)
        .ok_or(NoEvidenceReason::Malformed)?;
    if skills_value.len() > MAX_SKILL_ENTRIES || observations.len() > MAX_OBSERVATION_ENTRIES {
        return Err(NoEvidenceReason::Malformed);
    }
    let mut skills = Vec::with_capacity(skills_value.len());
    let mut skill_keys = BTreeSet::new();
    for skill in skills_value {
        let parsed = parse_skill(skill)?;
        let key = (
            parsed.phase.clone(),
            parsed.name.clone(),
            parsed.scope.clone(),
        );
        if !skill_keys.insert(key) {
            return Err(NoEvidenceReason::Malformed);
        }
        skills.push(parsed);
    }
    if !is_sorted_skills(&skills) {
        return Err(NoEvidenceReason::Malformed);
    }
    let mut obs_keys = BTreeSet::new();
    let mut previous: Option<(&str, &str)> = None;
    for observation in observations {
        let parsed = parse_observation(observation)?;
        if !obs_keys.insert((parsed.phase.clone(), parsed.name.clone())) {
            return Err(NoEvidenceReason::Malformed);
        }
        if let Some((phase, name)) = previous
            && (parsed.phase.as_str(), parsed.name.as_str()) < (phase, name)
        {
            return Err(NoEvidenceReason::Malformed);
        }
        previous = Some((
            observations_phase(observation)?,
            observations_name(observation)?,
        ));
        let _ = parsed;
    }
    if observations.len() != skills.len() {
        return Err(NoEvidenceReason::Malformed);
    }
    for skill in &skills {
        let observation = observations.iter().find(|item| {
            item.get("phase").and_then(Value::as_str) == Some(skill.phase.as_str())
                && item.get("name").and_then(Value::as_str) == Some(skill.name.as_str())
        });
        let Some(observation) = observation else {
            return Err(NoEvidenceReason::Malformed);
        };
        if !skill_matches_observation(skill, observation) {
            return Err(NoEvidenceReason::Malformed);
        }
    }
    let digest = value
        .get("digest")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?
        .to_string();
    Ok(ParsedSnapshot {
        project_id,
        track_id,
        run_epoch,
        sequence,
        digest,
        skills,
    })
}

fn valid_captured_at(text: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(text) {
        Ok(parsed) => crate::workflow::evidence_stamp::utc_stamp_line(parsed.to_utc()) == text,
        Err(_) => false,
    }
}

fn observations_phase(value: &Value) -> Result<&str, NoEvidenceReason> {
    value
        .get("phase")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)
}

fn observations_name(value: &Value) -> Result<&str, NoEvidenceReason> {
    value
        .get("name")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)
}

fn contains_non_integer(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.as_u64().is_none() && number.as_i64().is_none(),
        Value::Array(items) => items.iter().any(contains_non_integer),
        Value::Object(map) => map.values().any(contains_non_integer),
        _ => false,
    }
}

fn has_control(text: &str) -> bool {
    text.chars().any(|ch| ch.is_control())
}

fn required_string(value: &Value, key: &str, max: usize) -> Result<String, NoEvidenceReason> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?;
    if text.len() > max {
        return Err(NoEvidenceReason::Malformed);
    }
    Ok(text.to_string())
}

fn check_producer(value: &Value) -> Result<(), NoEvidenceReason> {
    if value.get("writer").and_then(Value::as_str) != Some(WRITER_ID) {
        return Err(NoEvidenceReason::Malformed);
    }
    let hook = value
        .get("hook")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?;
    if hook != "run_begin_fresh_run" && hook != "workflow_auto_start" {
        return Err(NoEvidenceReason::Malformed);
    }
    let phase = required_string(value, "phase", 32)?;
    if has_control(&phase) {
        return Err(NoEvidenceReason::Malformed);
    }
    for (key, max) in [("harness", 256usize), ("command", 256), ("model", 256)] {
        optional_bounded(value, key, max)?;
    }
    optional_bounded(value, "role_key", 64)?;
    let product_version = required_string(value, "product_version", 64)?;
    if has_control(&product_version) {
        return Err(NoEvidenceReason::Malformed);
    }
    match value.get("pid") {
        Some(Value::Null) => {}
        Some(Value::Number(number)) => {
            let pid = number.as_u64().ok_or(NoEvidenceReason::Malformed)?;
            if pid < 1 {
                return Err(NoEvidenceReason::Malformed);
            }
        }
        _ => return Err(NoEvidenceReason::Malformed),
    }
    Ok(())
}

fn optional_bounded(value: &Value, key: &str, max: usize) -> Result<(), NoEvidenceReason> {
    match value.get(key) {
        Some(Value::Null) => Ok(()),
        Some(Value::String(text)) => {
            if text.len() > max || has_control(text) {
                Err(NoEvidenceReason::Malformed)
            } else {
                Ok(())
            }
        }
        _ => Err(NoEvidenceReason::Malformed),
    }
}

fn parse_skill(value: &Value) -> Result<ParsedSkill, NoEvidenceReason> {
    let phase = required_string(value, "phase", MAX_STRING_BYTES)?;
    if !SKILL_PHASES.contains(&phase.as_str()) || has_control(&phase) {
        return Err(NoEvidenceReason::Malformed);
    }
    let name = required_string(value, "name", MAX_STRING_BYTES)?;
    if name.is_empty() || has_control(&name) {
        return Err(NoEvidenceReason::Malformed);
    }
    let scope = required_string(value, "scope", MAX_STRING_BYTES)?;
    if scope != "workspace" && scope != "execution" {
        return Err(NoEvidenceReason::Malformed);
    }
    let state = required_string(value, "state", MAX_STRING_BYTES)?;
    if !matches!(
        state.as_str(),
        "present"
            | "absent"
            | "unreadable"
            | "not_regular"
            | "reparse"
            | "oversize"
            | "non_utf8_path"
    ) {
        return Err(NoEvidenceReason::Malformed);
    }
    let path = match value.get("path") {
        Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.len() > MAX_STRING_BYTES || has_control(text) {
                return Err(NoEvidenceReason::Malformed);
            }
            Some(text.clone())
        }
        _ => return Err(NoEvidenceReason::Malformed),
    };
    if state == "non_utf8_path" {
        if path.is_some() {
            return Err(NoEvidenceReason::Malformed);
        }
    } else if path.is_none() {
        return Err(NoEvidenceReason::Malformed);
    }
    let regular = value
        .get("regular")
        .and_then(Value::as_bool)
        .ok_or(NoEvidenceReason::Malformed)?;
    let reparse = value
        .get("reparse")
        .and_then(Value::as_bool)
        .ok_or(NoEvidenceReason::Malformed)?;
    let size = match value.get("size") {
        Some(Value::Null) => None,
        Some(Value::Number(number)) => Some(number.as_u64().ok_or(NoEvidenceReason::Malformed)?),
        _ => return Err(NoEvidenceReason::Malformed),
    };
    let digest = match value.get("digest") {
        Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if !valid_digest(text) {
                return Err(NoEvidenceReason::Malformed);
            }
            Some(text.clone())
        }
        _ => return Err(NoEvidenceReason::Malformed),
    };
    if state == "present" {
        if !regular || reparse || size.is_none() || digest.is_none() {
            return Err(NoEvidenceReason::Malformed);
        }
    } else if size.is_some() || digest.is_some() {
        return Err(NoEvidenceReason::Malformed);
    }
    if state == "reparse" && !reparse {
        return Err(NoEvidenceReason::Malformed);
    }
    if state != "reparse" && reparse {
        return Err(NoEvidenceReason::Malformed);
    }
    if state != "present" && regular && state != "oversize" && state != "unreadable" {
        return Err(NoEvidenceReason::Malformed);
    }
    Ok(ParsedSkill {
        phase,
        name,
        scope,
        path,
        state,
        digest,
        size,
        regular,
        reparse,
    })
}

struct ParsedObservation {
    phase: String,
    name: String,
}

fn parse_observation(value: &Value) -> Result<ParsedObservation, NoEvidenceReason> {
    if value.get("fact").and_then(Value::as_str) != Some("skill_material") {
        return Err(NoEvidenceReason::Malformed);
    }
    let phase = required_string(value, "phase", MAX_STRING_BYTES)?;
    if !SKILL_PHASES.contains(&phase.as_str()) {
        return Err(NoEvidenceReason::Malformed);
    }
    let name = required_string(value, "name", MAX_STRING_BYTES)?;
    let observed = value
        .get("observed")
        .and_then(Value::as_str)
        .ok_or(NoEvidenceReason::Malformed)?;
    if observed != "open_read" && observed != "metadata" {
        return Err(NoEvidenceReason::Malformed);
    }
    let outcome = required_string(value, "outcome", MAX_STRING_BYTES)?;
    if !matches!(
        outcome.as_str(),
        "readable"
            | "absent"
            | "unreadable"
            | "not_regular"
            | "reparse"
            | "oversize"
            | "non_utf8_path"
    ) {
        return Err(NoEvidenceReason::Malformed);
    }
    match value.get("reason") {
        Some(Value::Null) | Some(Value::String(_)) => {}
        _ => return Err(NoEvidenceReason::Malformed),
    }
    Ok(ParsedObservation { phase, name })
}

fn skill_matches_observation(skill: &ParsedSkill, observation: &Value) -> bool {
    let outcome = observation.get("outcome").and_then(Value::as_str);
    let observed = observation.get("observed").and_then(Value::as_str);
    let reason = observation.get("reason");
    if skill.state == "present" {
        return outcome == Some("readable")
            && observed == Some("open_read")
            && reason == Some(&Value::Null);
    }
    if outcome != Some(skill.state.as_str()) || observed != Some("metadata") {
        return false;
    }
    let reason_text = reason.and_then(Value::as_str);
    match skill.state.as_str() {
        "absent" => reason_text == Some("not-found"),
        "unreadable" => matches!(reason_text, Some("open-error" | "io")),
        "reparse" => reason_text == Some("reparse"),
        "not_regular" => reason_text == Some("not-regular"),
        "oversize" => reason_text == Some("oversize"),
        "non_utf8_path" => reason_text == Some("non-utf8-path"),
        _ => false,
    }
}

fn is_sorted_skills(skills: &[ParsedSkill]) -> bool {
    skills.windows(2).all(|pair| {
        let left = (&pair[0].phase, &pair[0].name, &pair[0].scope);
        let right = (&pair[1].phase, &pair[1].name, &pair[1].scope);
        left < right
    })
}

pub(crate) fn object_from(entries: Vec<(&str, Value)>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

#[cfg(test)]
pub(crate) use reader::{
    AfterReadGuard, BetweenReadsGuard, LeafHookGuard, PanicOnceGuard, ReadCounterGuard,
};

pub(crate) fn bounded_copy(text: Option<&str>, max: usize) -> Value {
    match text {
        Some(text) if !text.is_empty() && text.len() <= max && !has_control(text) => {
            Value::String(text.to_string())
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests;
