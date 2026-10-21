//! Read-only Windows process snapshot for the 0108 self-update gate.
//!
//! This module deliberately does **not** shell out to a task-list tool and never
//! kills anything: it takes one `TH32CS_SNAPPROCESS` snapshot and returns image
//! names. The gate treats a failed or empty snapshot as unreadable (fail closed).

use serde::Serialize;

use crate::error::Result;

/// One process-table row: pid plus image name (`szExeFile`, not a full path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub image: String,
}

/// Build-tool images that block a deploy while a tree build/test may be running.
///
/// Machine-wide by design (OD-108-4): we cannot cheaply prove another process's
/// cwd. `cargo-clippy.exe` is included because `cargo clippy` runs the rustup
/// `cargo.exe` proxy and then the toolchain's `cargo-clippy.exe`;
/// `clippy-driver.exe` covers the lint process itself.
pub(crate) const BUILD_TOOL_IMAGES: &[&str] = &[
    "cargo.exe",
    "cargo-nextest.exe",
    "cargo-clippy.exe",
    "rustc.exe",
    "rustdoc.exe",
    "rustfmt.exe",
    "clippy-driver.exe",
];

/// The installed product image. Matched exactly, never as a prefix: the test
/// binary is `coordinator-<hash>.exe` and must not trip the peer arm.
pub(crate) const PEER_IMAGE: &str = "coordinator.exe";

/// ASCII case-insensitive, exact match against the build-tool image set.
pub(crate) fn is_build_tool_image(image: &str) -> bool {
    BUILD_TOOL_IMAGES
        .iter()
        .any(|candidate| image.eq_ignore_ascii_case(candidate))
}

/// ASCII case-insensitive, exact match against `coordinator.exe`.
pub(crate) fn is_peer_image(image: &str) -> bool {
    image.eq_ignore_ascii_case(PEER_IMAGE)
}

/// Injectable process source (tests use scripted/fake rows).
pub(crate) trait ProcessSource {
    fn snapshot(&self) -> Result<Vec<ProcessRow>>;
}

/// Production source: the real kernel32 snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WindowsProcessSource;

impl ProcessSource for WindowsProcessSource {
    fn snapshot(&self) -> Result<Vec<ProcessRow>> {
        snapshot()
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    use crate::error::{CoordinatorError, Result};

    use super::ProcessRow;

    pub(super) const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    /// Documented `PROCESSENTRY32W` layout (winbase/tlhelp32). 568 bytes on x64.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct ProcessEntry32W {
        pub dw_size: u32,
        pub cnt_usage: u32,
        pub th32_process_id: u32,
        pub th32_default_heap_id: usize,
        pub th32_module_id: u32,
        pub cnt_threads: u32,
        pub th32_parent_process_id: u32,
        pub pc_pri_class_base: i32,
        pub dw_flags: u32,
        pub sz_exe_file: [u16; 260],
    }

    impl Default for ProcessEntry32W {
        fn default() -> Self {
            Self {
                dw_size: 0,
                cnt_usage: 0,
                th32_process_id: 0,
                th32_default_heap_id: 0,
                th32_module_id: 0,
                cnt_threads: 0,
                th32_parent_process_id: 0,
                pc_pri_class_base: 0,
                dw_flags: 0,
                sz_exe_file: [0; 260],
            }
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    const ERROR_NO_MORE_FILES: u32 = 18;

    struct SnapshotHandle(*mut c_void);

    impl Drop for SnapshotHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateToolhelp32Snapshot and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn image_name(raw: &[u16; 260]) -> String {
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        char::decode_utf16(raw[..len].iter().copied())
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }

    pub(super) fn snapshot() -> Result<Vec<ProcessRow>> {
        // SAFETY: TH32CS_SNAPPROCESS ignores the pid argument (0 is documented).
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw.is_null() || raw as isize == -1 {
            return Err(CoordinatorError::Message(
                "process snapshot failed".to_string(),
            ));
        }
        let handle = SnapshotHandle(raw);

        let mut entry = ProcessEntry32W {
            dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
            ..ProcessEntry32W::default()
        };
        // SAFETY: entry is a valid, correctly sized PROCESSENTRY32W buffer.
        let first = unsafe { Process32FirstW(handle.0, &mut entry) };
        if first == 0 {
            // SAFETY: thread-local last-error read immediately after the call.
            let code = unsafe { GetLastError() };
            return Err(CoordinatorError::Message(format!(
                "process snapshot failed: Process32FirstW returned 0 (error {code})"
            )));
        }

        let mut rows = Vec::new();
        loop {
            rows.push(ProcessRow {
                pid: entry.th32_process_id,
                image: image_name(&entry.sz_exe_file),
            });
            // SAFETY: same buffer; Process32NextW keeps dw_size at the struct size.
            let next = unsafe { Process32NextW(handle.0, &mut entry) };
            if next == 0 {
                // SAFETY: last-error must be read immediately; anything other than
                // "no more files" is an unreadable snapshot, never "quiet machine".
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(CoordinatorError::Message(format!(
                    "process snapshot failed: Process32NextW error {code}"
                )));
            }
        }

        if rows.is_empty() {
            // A live machine always has processes; an empty set means the API lied.
            return Err(CoordinatorError::Message(
                "process snapshot failed: empty row set".to_string(),
            ));
        }
        Ok(rows)
    }
}

#[cfg(windows)]
pub(crate) fn snapshot() -> Result<Vec<ProcessRow>> {
    sys::snapshot()
}

/// Non-Windows stub: fail closed, never open.
#[cfg(not(windows))]
pub(crate) fn snapshot() -> Result<Vec<ProcessRow>> {
    Err(crate::error::CoordinatorError::Message(
        "process snapshot unavailable on this platform".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_update_build_tool_match_is_ascii_case_insensitive() {
        assert!(is_build_tool_image("CARGO.EXE"));
        assert!(is_build_tool_image("Rustc.Exe"));
        assert!(is_build_tool_image("cargo-clippy.exe"));
        assert!(is_build_tool_image("cargo-nextest.exe"));
        assert!(is_build_tool_image("rustdoc.exe"));
        assert!(is_build_tool_image("rustfmt.exe"));
        assert!(is_build_tool_image("clippy-driver.exe"));
        assert!(!is_build_tool_image("coordinator-596c578ab90769fc.exe"));
        assert!(!is_build_tool_image("cargo-nextest.exe.bak"));
        assert!(!is_build_tool_image("carg"));
    }

    #[test]
    fn self_update_peer_match_is_exact() {
        assert!(is_peer_image("coordinator.exe"));
        assert!(is_peer_image("COORDINATOR.EXE"));
        assert!(!is_peer_image("coordinator-596c578ab90769fc.exe"));
        assert!(!is_peer_image("coordinator.exe.bak"));
        assert!(!is_peer_image("coordinator"));
    }

    #[cfg(windows)]
    #[test]
    fn self_update_process_snapshot_lists_own_pid() {
        let rows = snapshot().expect("snapshot");
        assert!(!rows.is_empty());
        let own = std::process::id();
        assert!(
            rows.iter().any(|row| row.pid == own),
            "our pid {own} missing from {} rows",
            rows.len()
        );
    }

    #[cfg(all(windows, target_pointer_width = "64"))]
    #[test]
    fn process_entry_layout_matches_windows() {
        assert_eq!(std::mem::size_of::<sys::ProcessEntry32W>(), 568);
    }
}
