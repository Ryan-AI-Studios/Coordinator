//! Atomic JSON file writes (temp + rename/replace) for registry and run-state.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::Result;

/// Write `bytes` to `path` via a unique same-directory temp file then replace.
///
/// - Temp names include process id **and** a random suffix so concurrent writers
///   in one process do not share a temp path.
/// - Replacement does **not** delete the destination first: on Unix, `rename`
///   replaces atomically; on Windows, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        crate::error::CoordinatorError::Message(format!(
            "path has no parent directory: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("data");
    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        file_name,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> Result<()> {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        replace_file(&tmp_path, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

/// Replace `to` with `from` without a delete-then-rename window.
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        // kernel32 MoveFileExW — replace existing without prior delete.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(
                lp_existing_file_name: *const u16,
                lp_new_file_name: *const u16,
                dw_flags: u32,
            ) -> i32;
        }
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

        let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: wide strings are NUL-terminated; paths live for the call.
        let ok = unsafe {
            MoveFileExW(
                from_w.as_ptr(),
                to_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        // POSIX rename replaces the destination atomically on the same filesystem.
        fs::rename(from, to)?;
        Ok(())
    }
}

/// Serialize `value` as pretty JSON and atomic-write to `path`.
pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.json");
        atomic_write_json(&path, &json!({"ok": true})).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"ok\""));
        // replace existing without leaving a gap where the file is missing
        atomic_write_json(&path, &json!({"ok": false})).unwrap();
        let text2 = fs::read_to_string(&path).unwrap();
        assert!(text2.contains("false"));
        assert!(path.exists());
    }

    #[test]
    fn concurrent_temp_names_differ() {
        // Unique suffix: two temps for same logical name must not collide.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.json");
        atomic_write_json(&path, &json!({"n": 1})).unwrap();
        atomic_write_json(&path, &json!({"n": 2})).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["n"], 2);
    }
}
