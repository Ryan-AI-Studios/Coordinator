//! Local Ops Console: view-model always compiled; Dioxus window behind `--features ui`.

use crate::error::CoordinatorError;

pub mod model;

#[cfg(feature = "ui")]
pub mod app;

/// Hint printed when the operator binary was not built with the window crate.
pub const UI_FEATURE_HINT: &str = "Status Surface requires a rebuild with --features ui (WebView2). Example: cargo run --features ui -- ui";

/// Microsoft Evergreen WebView2 installer (no LAN/browser fallback).
pub const WEBVIEW2_EVERGREEN_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// Operator-facing diagnostic when WebView2 is missing or `dioxus::launch` fails.
pub const WEBVIEW2_MISSING_HINT: &str = "WebView2 Evergreen runtime is required for `coordinator ui`.\n\
Install: https://developer.microsoft.com/microsoft-edge/webview2/\n\
Then retry: cargo run --features ui -- ui";

/// `coordinator ui [--port]`.
///
/// Without `--features ui`, this is a clap-visible subcommand that errors with a rebuild hint.
pub fn run_cli(port: u16) -> Result<(), CoordinatorError> {
    #[cfg(feature = "ui")]
    {
        app::run_surface(port)
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = port;
        Err(CoordinatorError::Message(UI_FEATURE_HINT.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_without_feature_hints_rebuild() {
        if cfg!(feature = "ui") {
            return;
        }
        let err = run_cli(7420).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("--features ui"), "{s}");
    }

    #[test]
    fn webview2_hint_names_evergreen() {
        assert!(WEBVIEW2_MISSING_HINT.contains(WEBVIEW2_EVERGREEN_URL));
        assert!(WEBVIEW2_MISSING_HINT.contains("WebView2"));
    }
}
