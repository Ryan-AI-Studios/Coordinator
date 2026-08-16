//! Dioxus Desktop Local Ops Console (`--features ui`). In-process `api::*` only.

use std::io::ErrorKind;
use std::path::PathBuf;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

use crate::error::CoordinatorError;
use crate::ui::model::{
    CardPrimaryAction, CardState, ChipKind, FleetSnapshot, ProjectCard, add_project,
    card_primary_action, load_fleet, pause_all, resume_selected, run_selected, selected_is_paused,
    stop_selected,
};
use crate::ui::{WEBVIEW2_MISSING_HINT, model::card_title};

/// Tokens + layout copied from `mock/status-surface.html` `:root` / `DESIGN.md`.
pub const SURFACE_CSS: &str = r#"
:root {
  --bg: #0f1419;
  --panel: #1a222c;
  --panel-2: #232d3a;
  --ink: #e8eef4;
  --muted: #9aabbc;
  --line: #2f3b4a;
  --accent: #e8a04a;
  --ok: #3dba7a;
  --bad: #e05a5a;
  --info: #5b9fd4;
  --mono: "Cascadia Mono", "Consolas", "Courier New", monospace;
  --sans: "Segoe UI", system-ui, sans-serif;
  --radius-sm: 6px;
  --radius-md: 10px;
  --space-1: 0.35rem;
  --space-2: 0.55rem;
  --space-3: 0.75rem;
  --space-4: 1rem;
  --space-5: 1.25rem;
  --break-narrow: 980px;
}
* { box-sizing: border-box; }
html, body { margin: 0; min-height: 100%; }
body, #main {
  font-family: var(--sans);
  background: var(--bg);
  color: var(--ink);
  line-height: 1.4;
}
code, .mono { font-family: var(--mono); }
header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: 0.85rem var(--space-5);
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  position: sticky;
  top: 0;
  z-index: 10;
}
header h1 { margin: 0; font-size: 1rem; font-weight: 600; letter-spacing: 0.02em; }
header .sub { color: var(--muted); font-size: 0.8rem; margin-top: 0.2rem; }
.header-meta {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  flex-wrap: wrap;
  justify-content: flex-end;
}
.stat {
  font-family: var(--mono);
  font-size: 0.78rem;
  color: var(--muted);
  padding: var(--space-1) 0.6rem;
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  background: var(--panel-2);
}
.stat strong { color: var(--ink); font-weight: 600; }
.controls { display: flex; gap: 0.5rem; flex-wrap: wrap; }
button {
  font: inherit;
  border: 1px solid var(--line);
  background: var(--panel-2);
  color: var(--ink);
  padding: 0.4rem 0.85rem;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
button:focus-visible { outline: 2px solid var(--info); outline-offset: 2px; }
button.danger { border-color: color-mix(in srgb, var(--bad) 50%, var(--line)); color: #ffc9c9; }
button.primary { border-color: color-mix(in srgb, var(--accent) 45%, var(--line)); color: #ffe3b5; }
button.ghost { color: var(--muted); }
.toolbar {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: var(--space-3);
  align-items: start;
  padding: var(--space-3) var(--space-5) 0;
}
@media (max-width: 980px) {
  .toolbar { grid-template-columns: 1fr; }
}
.toolbar p {
  margin: 0;
  color: var(--muted);
  font-size: 0.875rem;
  max-width: 52rem;
  line-height: 1.45;
}
.ops-help {
  font-size: 0.8rem;
  color: var(--muted);
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
  min-width: min(22rem, 100%);
  max-width: 26rem;
}
.ops-help strong { color: var(--ink); font-weight: 600; }
.ops-help dl { margin: 0.4rem 0 0; display: grid; gap: 0.35rem; }
.ops-help dt { font-family: var(--mono); font-size: 0.72rem; color: #c9d6e4; }
.ops-help dd { margin: 0 0 0.15rem 0.6rem; color: var(--muted); }
.phase-strip {
  margin: var(--space-3) var(--space-5) 0;
  padding: var(--space-2) var(--space-3);
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-md);
}
.phase-strip .caption { margin: 0 0 var(--space-2); font-size: 0.74rem; color: var(--muted); }
.phase-strip .caption strong { color: var(--ink); font-family: var(--mono); font-weight: 600; }
.chips { display: flex; flex-wrap: wrap; gap: 0.4rem; }
.chip {
  font-family: var(--mono);
  font-size: 0.75rem;
  padding: 0.22rem 0.5rem;
  border-radius: 999px;
  border: 1px solid var(--line);
  color: var(--muted);
  background: var(--panel-2);
}
.chip.done { color: #9df0c0; border-color: color-mix(in srgb, var(--ok) 40%, var(--line)); }
.chip.current {
  color: #ffd79a;
  border-color: color-mix(in srgb, var(--accent) 50%, var(--line));
  background: color-mix(in srgb, var(--accent) 12%, var(--panel-2));
}
.chip.next { color: #c5d0dc; }
main#projects {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-4);
  padding: var(--space-4) var(--space-5) var(--space-4);
}
@media (max-width: 980px) {
  main#projects { grid-template-columns: 1fr; }
  header { flex-direction: column; align-items: stretch; }
  .header-meta { justify-content: flex-start; }
}
.project {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-md);
  padding: 0.9rem var(--space-4) var(--space-4);
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  min-height: 100%;
  cursor: pointer;
}
.project.attention { border-color: color-mix(in srgb, var(--accent) 55%, var(--line)); }
.project.failed { border-color: color-mix(in srgb, var(--bad) 55%, var(--line)); }
.project.idle-surface { border-style: dashed; }
.project.selected { outline: 2px solid var(--info); outline-offset: 2px; }
.project-head { display: flex; justify-content: space-between; gap: var(--space-3); align-items: flex-start; }
.project-head h2 { margin: 0; font-size: 0.95rem; font-weight: 600; }
.project-path {
  margin: 0.25rem 0 0;
  font-family: var(--mono);
  font-size: 0.78rem;
  color: var(--muted);
  word-break: break-all;
}
.project-actions { display: flex; gap: var(--space-1); flex-shrink: 0; }
.project-actions button { padding: 0.28rem 0.55rem; font-size: 0.75rem; }
.kv { display: grid; gap: 0.4rem; }
.kv .row {
  display: grid;
  grid-template-columns: 5.5rem 1fr auto;
  gap: 0.5rem;
  align-items: center;
  padding: 0.45rem var(--space-2);
  background: var(--panel-2);
  border-radius: 7px;
}
.label { color: var(--muted); font-size: 0.78rem; }
.value {
  font-family: var(--mono);
  font-size: 0.82rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pill {
  font-size: 0.75rem;
  font-weight: 600;
  padding: 0.2rem 0.55rem;
  border-radius: 999px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  white-space: nowrap;
}
.pill.running { background: color-mix(in srgb, var(--ok) 22%, transparent); color: #9df0c0; }
.pill.attention { background: color-mix(in srgb, var(--accent) 22%, transparent); color: #ffd79a; }
.pill.failed { background: color-mix(in srgb, var(--bad) 22%, transparent); color: #ffb0b0; }
.pill.paused { background: color-mix(in srgb, var(--info) 22%, transparent); color: #b7d9f5; }
.pill.idle { background: color-mix(in srgb, var(--muted) 18%, transparent); color: #c5d0dc; }
.sessions { width: 100%; border-collapse: collapse; font-size: 0.82rem; }
.sessions th, .sessions td {
  text-align: left;
  padding: 0.4rem 0.25rem;
  border-bottom: 1px solid var(--line);
}
.sessions th {
  color: var(--muted);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  font-weight: 600;
}
.sessions td.mono { font-family: var(--mono); font-size: 0.78rem; }
.note {
  font-size: 0.82rem;
  color: var(--muted);
  line-height: 1.4;
  margin: 0;
  padding: var(--space-2) 0.65rem;
  background: var(--panel-2);
  border-radius: 7px;
  border: 1px solid var(--line);
}
.note.warn { border-color: color-mix(in srgb, var(--accent) 40%, var(--line)); color: #e8d4b0; }
.note.err { border-color: color-mix(in srgb, var(--bad) 40%, var(--line)); color: #f0c0c0; }
.artifact {
  border: 1px solid color-mix(in srgb, var(--bad) 45%, var(--line));
  border-radius: var(--radius-md);
  background: color-mix(in srgb, var(--bad) 8%, var(--panel-2));
  padding: var(--space-2) 0.7rem;
  display: grid;
  gap: 0.4rem;
}
.artifact h3 { margin: 0; font-size: 0.78rem; font-weight: 600; color: #ffb0b0; }
.artifact .meta { display: grid; gap: 0.25rem; font-size: 0.74rem; color: #f0c0c0; }
.artifact .meta span { display: grid; grid-template-columns: 4.5rem 1fr; gap: 0.4rem; }
.artifact .meta .k { color: var(--muted); }
.artifact .meta .v { font-family: var(--mono); font-size: 0.72rem; word-break: break-all; }
.artifact pre {
  margin: 0;
  white-space: pre-wrap;
  font-family: var(--mono);
  font-size: 0.72rem;
  color: #f0c0c0;
}
.empty {
  margin: 0;
  padding: 1rem 0.65rem;
  text-align: center;
  color: var(--muted);
  font-size: 0.8rem;
  border: 1px dashed var(--line);
  border-radius: 7px;
  background: transparent;
}
.empty strong { color: var(--ink); font-weight: 600; }
footer {
  padding: 0 var(--space-5) 1.5rem;
  color: var(--muted);
  font-size: 0.82rem;
  line-height: 1.45;
}
footer code { font-family: var(--mono); font-size: 0.74rem; color: #c9d6e4; }
.add-form {
  display: grid;
  gap: var(--space-2);
  margin-top: var(--space-3);
  padding: var(--space-3);
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-md);
}
.add-form label { font-size: 0.78rem; color: var(--muted); display: grid; gap: 0.25rem; }
.add-form input {
  font: inherit;
  font-family: var(--mono);
  font-size: 0.78rem;
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  background: var(--panel-2);
  color: var(--ink);
}
.add-form input:focus-visible { outline: 2px solid var(--info); outline-offset: 2px; }
.banner {
  margin: var(--space-3) var(--space-5) 0;
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-sm);
  border: 1px solid color-mix(in srgb, var(--bad) 45%, var(--line));
  color: #f0c0c0;
  font-size: 0.82rem;
}
"#;

pub const WINDOW_TITLE: &str = "Coordinator — Local Ops Console";

/// First-launch desktop size so the 2-col mock contract is visible (break is 980px).
pub const DEFAULT_INNER_WIDTH: f64 = 1280.0;
pub const DEFAULT_INNER_HEIGHT: f64 = 860.0;

/// Probe Evergreen WebView2 via the well-known EdgeUpdate client GUIDs.
pub fn webview2_available() -> bool {
    #[cfg(windows)]
    {
        const KEYS: &[&str] = &[
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
            r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
            r"HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        ];
        for key in KEYS {
            let ok = std::process::Command::new("reg")
                .args(["query", key, "/v", "pv"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// If `127.0.0.1:port` is free, start `server::serve` on a background Runtime.
/// `AddrInUse` → leave the existing serve alone; window still uses in-process `api::*`.
/// The `JoinHandle` is dropped — process exit reaps the thread.
fn maybe_spawn_serve(port: u16) {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_listener) => {
            // Drop the probe listener so `serve` can bind the same loopback port.
        }
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            eprintln!(
                "coordinator ui: serve already up on 127.0.0.1:{port}; window uses in-process api"
            );
            return;
        }
        Err(e) => {
            eprintln!("coordinator ui: cannot probe 127.0.0.1:{port}: {e}; skipping owned serve");
            return;
        }
    }

    let _handle = std::thread::Builder::new()
        .name("coordinator-ui-serve".into())
        .spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("coordinator ui: failed to start serve runtime: {e}");
                    return;
                }
            };
            if let Err(e) = rt.block_on(crate::server::serve(port)) {
                eprintln!("coordinator ui: serve exited: {e}");
            }
        });
}

/// Main-thread WebView2 launch. Never binds LAN. Never panics on missing runtime.
pub fn run_surface(port: u16) -> Result<(), CoordinatorError> {
    if !webview2_available() {
        eprintln!("{WEBVIEW2_MISSING_HINT}");
        return Err(CoordinatorError::Message(
            "WebView2 Evergreen runtime is not installed".into(),
        ));
    }

    maybe_spawn_serve(port);

    let launched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dioxus::LaunchBuilder::new()
            .with_cfg(
                Config::new()
                    .with_window(
                        WindowBuilder::new()
                            .with_title(WINDOW_TITLE)
                            .with_inner_size(LogicalSize::new(
                                DEFAULT_INNER_WIDTH,
                                DEFAULT_INNER_HEIGHT,
                            )),
                    )
                    .with_menu(None)
                    .with_background_color((15, 20, 25, 255)),
            )
            .launch(LocalOpsConsole);
    }));
    if launched.is_err() {
        eprintln!("{WEBVIEW2_MISSING_HINT}");
        return Err(CoordinatorError::Message(
            "failed to start Status Surface (WebView2)".into(),
        ));
    }
    Ok(())
}

#[allow(non_snake_case)]
fn LocalOpsConsole() -> Element {
    let mut fleet = use_signal(FleetSnapshot::default);
    let mut banner = use_signal(|| None::<String>);
    let mut add_path = use_signal(String::new);
    let mut add_name = use_signal(String::new);
    let mut run_track = use_signal(String::new);

    use_future(move || async move {
        loop {
            let selected = fleet.peek().selected.clone();
            match tokio::task::spawn_blocking(move || load_fleet(selected.as_deref())).await {
                Ok(Ok(next)) => fleet.set(next),
                Ok(Err(e)) => {
                    let mut cur = fleet();
                    cur.last_error = Some(e.to_string());
                    fleet.set(cur);
                }
                Err(e) => {
                    let mut cur = fleet();
                    cur.last_error = Some(e.to_string());
                    fleet.set(cur);
                }
            }
            let _ = tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(1000));
            })
            .await;
        }
    });

    let snap = fleet();
    let paused = selected_is_paused(&snap);
    let err = banner().or(snap.last_error.clone()).unwrap_or_default();

    rsx! {
        style { "{SURFACE_CSS}" }
        header {
            div {
                h1 { "Coordinator · Status Surface" }
                div { class: "sub", "Multi-project session pools · Local Ops Console" }
            }
            div { class: "header-meta",
                span { class: "stat", title: "Registered projects", "Projects " strong { "{snap.counts.projects}" } }
                span { class: "stat", title: "Active phases (models or CP watch)", "Active " strong { "{snap.counts.active}" } }
                span { class: "stat", title: "Needs operator attention", "Attention " strong { "{snap.counts.attention}" } }
                span { class: "stat", title: "No active track", "Idle " strong { "{snap.counts.idle}" } }
                div { class: "controls",
                    button {
                        class: "primary",
                        title: "Pause: finish current phase on each active project, then hold next phase/track until Resume",
                        onclick: move |_| {
                            match pause_all() {
                                Ok(_) => {
                                    banner.set(None);
                                    refresh(&mut fleet);
                                }
                                Err(e) => banner.set(Some(e.to_string())),
                            }
                        },
                        "Pause all"
                    }
                    if paused {
                        button {
                            class: "primary",
                            title: "Resume: continue pipeline after held phase",
                            onclick: move |_| {
                                let id = fleet.peek().selected.clone();
                                if let Some(id) = id {
                                    match resume_selected(&id) {
                                        Ok(_) => {
                                            banner.set(None);
                                            refresh(&mut fleet);
                                        }
                                        Err(e) => banner.set(Some(e.to_string())),
                                    }
                                }
                            },
                            "Resume"
                        }
                    }
                    button {
                        class: "danger",
                        title: "Stop selected: abort current phase · no merge · sessions left for attach",
                        onclick: move |_| {
                            let id = fleet.peek().selected.clone();
                            if let Some(id) = id {
                                match stop_selected(&id) {
                                    Ok(_) => {
                                        banner.set(None);
                                        refresh(&mut fleet);
                                    }
                                    Err(e) => banner.set(Some(e.to_string())),
                                }
                            } else {
                                banner.set(Some("select a project first".into()));
                            }
                        },
                        "Stop selected"
                    }
                }
            }
        }
        div { class: "toolbar",
            p {
                "Coordinator keeps a "
                strong { "session pool per project" }
                ". CLI remains the automation entry ("
                code { "coordinator run" }
                " / "
                code { "wait" }
                "). This window is visibility + interrupt. Mock HTML stays the visual reference."
            }
            aside { class: "ops-help", "aria-label": "Stop versus Pause",
                strong { "Operator controls (ADR-0024)" }
                dl {
                    dt { "Stop" }
                    dd {
                        "Abort current phase · "
                        strong { "no merge" }
                        " · "
                        strong { "sessions left for attach" }
                    }
                    dt { "Pause" }
                    dd { "Finish current phase, then hold next phase/track until Resume" }
                }
            }
        }
        if !err.is_empty() {
            div { class: "banner", "{err}" }
        }
        section { class: "phase-strip", "aria-label": "Selected project phase strip",
            p { class: "caption", "{snap.phase_caption}" }
            div { class: "chips",
                for chip in snap.phase_chips.iter() {
                    span { class: chip_class(chip.kind), "{chip.label}" }
                }
            }
        }
        main { id: "projects",
            if snap.cards.is_empty() {
                article { class: "project idle-surface", "data-state": "idle",
                    p { class: "empty",
                        strong { "Empty registry" }
                        " — add an absolute project path below. No crash."
                    }
                }
            } else {
                for card in snap.cards.iter() {
                    { project_card(card, snap.selected.as_deref(), fleet, banner) }
                }
            }
        }
        footer {
            div { class: "add-form",
                strong { "Add project" }
                p { "Explicit absolute path only. Never scan-add C:\\dev." }
                label {
                    "Path (required, absolute)"
                    input {
                        value: "{add_path}",
                        placeholder: r"C:\dev\Orca",
                        oninput: move |e| add_path.set(e.value()),
                    }
                }
                label {
                    "Display name (optional)"
                    input {
                        value: "{add_name}",
                        oninput: move |e| add_name.set(e.value()),
                    }
                }
                label {
                    "Run track id (optional, Idle/Stopped)"
                    input {
                        value: "{run_track}",
                        placeholder: "0014-StatusSurfaceApp",
                        oninput: move |e| run_track.set(e.value()),
                    }
                }
                div { class: "controls",
                    button {
                        class: "primary",
                        onclick: move |_| {
                            let path = PathBuf::from(add_path().trim());
                            let name = {
                                let n = add_name();
                                let t = n.trim();
                                if t.is_empty() { None } else { Some(t.to_string()) }
                            };
                            match add_project(&path, name) {
                                Ok(_) => {
                                    banner.set(None);
                                    add_path.set(String::new());
                                    add_name.set(String::new());
                                    refresh(&mut fleet);
                                }
                                Err(e) => banner.set(Some(e.to_string())),
                            }
                        },
                        "Add project"
                    }
                    button {
                        class: "ghost",
                        title: "Start the canonical workflow on the selected Idle/Stopped project",
                        onclick: move |_| {
                            let id = fleet.peek().selected.clone();
                            if let Some(id) = id {
                                let track = {
                                    let t = run_track();
                                    let t = t.trim();
                                    if t.is_empty() { None } else { Some(t.to_string()) }
                                };
                                match run_selected(&id, track) {
                                    Ok(_) => {
                                        banner.set(None);
                                        refresh(&mut fleet);
                                    }
                                    Err(e) => banner.set(Some(e.to_string())),
                                }
                            } else {
                                banner.set(Some("select a project first".into()));
                            }
                        },
                        "Run selected"
                    }
                }
            }
            p {
                "Visual contract: "
                code { "mock/status-surface.html" }
                " · loopback only (127.0.0.1). WebView2 Evergreen required."
            }
        }
    }
}

fn refresh(fleet: &mut Signal<FleetSnapshot>) {
    let selected = fleet.peek().selected.clone();
    match load_fleet(selected.as_deref()) {
        Ok(next) => fleet.set(next),
        Err(e) => {
            let mut cur = fleet();
            cur.last_error = Some(e.to_string());
            fleet.set(cur);
        }
    }
}

fn chip_class(kind: ChipKind) -> &'static str {
    match kind {
        ChipKind::Done => "chip done",
        ChipKind::Current => "chip current",
        ChipKind::Next => "chip next",
    }
}

fn card_classes(card: &ProjectCard, selected: bool) -> String {
    let mut classes = String::from("project");
    match card.card_state {
        CardState::Paused => classes.push_str(" attention"),
        CardState::HardFailure => classes.push_str(" failed"),
        CardState::Idle => classes.push_str(" idle-surface"),
        _ => {}
    }
    if selected {
        classes.push_str(" selected");
    }
    classes
}

fn status_pill(state: CardState) -> (&'static str, &'static str) {
    match state {
        CardState::ParallelPlanReview | CardState::TokenIdleCi | CardState::Running => {
            ("pill running", "running")
        }
        CardState::Paused => ("pill paused", "paused"),
        CardState::HardFailure => ("pill failed", "failed"),
        CardState::Idle => ("pill idle", "idle"),
    }
}

fn project_card(
    card: &ProjectCard,
    selected_id: Option<&str>,
    mut fleet: Signal<FleetSnapshot>,
    mut banner: Signal<Option<String>>,
) -> Element {
    let id = card.view.project_id.clone();
    let id_select = id.clone();
    let id_resume = id.clone();
    let id_run = id.clone();
    let id_pause = id.clone();
    let id_stop = id.clone();
    let selected = selected_id == Some(id.as_str());
    let classes = card_classes(card, selected);
    let state = card.card_state.as_data_state();
    let title = card_title(&card.view);
    let path = card.view.path.display().to_string();
    let track = card.view.track_id.clone().unwrap_or_else(|| "—".into());
    let phase = card.view.phase.clone();
    let next = card.view.next_track.clone().unwrap_or_else(|| "—".into());
    let layout = card.view.layout_profile.as_str().to_string();
    let (pill_class, pill_label) = status_pill(card.card_state);
    let primary = card_primary_action(&card.view);
    let sessions = card.sessions.clone();
    let note = note_for(card);
    let failure = if selected {
        fleet.peek().failure.clone()
    } else {
        None
    };

    rsx! {
        article {
            class: "{classes}",
            "data-state": "{state}",
            onclick: move |_| {
                let mut next = fleet();
                next.selected = Some(id_select.clone());
                if let Ok(built) = load_fleet(Some(id_select.as_str())) {
                    fleet.set(built);
                } else {
                    fleet.set(next);
                }
            },
            div { class: "project-head",
                div {
                    h2 { "{title}" }
                    p { class: "project-path", "{path}" }
                }
                div { class: "project-actions",
                    if primary == CardPrimaryAction::Resume {
                        button {
                            class: "primary",
                            title: "Resume: continue pipeline after held phase",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                match resume_selected(&id_resume) {
                                    Ok(_) => {
                                        banner.set(None);
                                        refresh(&mut fleet);
                                    }
                                    Err(e) => banner.set(Some(e.to_string())),
                                }
                            },
                            "Resume"
                        }
                    } else if primary == CardPrimaryAction::Run {
                        button {
                            class: "primary",
                            title: "Start the canonical workflow (CLI remains the automation entry)",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                match run_selected(&id_run, None) {
                                    Ok(_) => {
                                        banner.set(None);
                                        refresh(&mut fleet);
                                    }
                                    Err(e) => banner.set(Some(e.to_string())),
                                }
                            },
                            "Run"
                        }
                    } else {
                        button {
                            class: "primary",
                            title: "Pause: finish current phase, hold next until Resume",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                match crate::api::cmd_pause(Some(&id_pause)) {
                                    Ok(_) => {
                                        banner.set(None);
                                        refresh(&mut fleet);
                                    }
                                    Err(e) => banner.set(Some(e.to_string())),
                                }
                            },
                            "Pause"
                        }
                    }
                    button {
                        class: "danger",
                        title: "Stop: abort current phase · no merge · sessions left for attach",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            match stop_selected(&id_stop) {
                                Ok(_) => {
                                    banner.set(None);
                                    refresh(&mut fleet);
                                }
                                Err(e) => banner.set(Some(e.to_string())),
                            }
                        },
                        "Stop"
                    }
                }
            }
            div { class: "kv",
                div { class: "row",
                    span { class: "label", "Track" }
                    span { class: "value", "{track}" }
                    span { class: "{pill_class}", "{pill_label}" }
                }
                div { class: "row",
                    span { class: "label", "Phase" }
                    span { class: "value", "{phase}" }
                    span { class: "pill idle", "live" }
                }
                div { class: "row",
                    span { class: "label", "Next" }
                    span { class: "value", "{next}" }
                    span { class: "pill idle", "queued" }
                }
                div { class: "row",
                    span { class: "label", "Layout" }
                    span { class: "value", "{layout}" }
                    span { class: "pill idle", "profile" }
                }
            }
            table { class: "sessions",
                thead {
                    tr {
                        th { "Role" }
                        th { "Harness" }
                        th { "State" }
                        th { "Detail" }
                    }
                }
                tbody {
                    for row in sessions.iter() {
                        tr {
                            td { "{row.role}" }
                            td { class: "mono", "{row.harness}" }
                            td { span { class: session_pill(&row.state), "{row.state}" } }
                            td { class: "mono", "{row.detail}" }
                        }
                    }
                }
            }
            if let Some(note) = note {
                p { class: note.0, "{note.1}" }
            }
            if let Some(panel) = failure {
                div { class: "artifact", "data-region": "failure-artifact",
                    h3 { "Failure Artifact" }
                    div { class: "meta",
                        span {
                            span { class: "k", "Path" }
                            span { class: "v", "{panel.path.display()}" }
                        }
                    }
                    pre { "{panel.body}" }
                }
            }
        }
    }
}

fn session_pill(state: &str) -> &'static str {
    match state {
        "alive" | "active" => "pill running",
        "dead" | "failed" => "pill failed",
        "no grok session" | "done" => "pill idle",
        _ => "pill idle",
    }
}

fn note_for(card: &ProjectCard) -> Option<(&'static str, String)> {
    match card.card_state {
        CardState::Paused => Some((
            "note warn",
            "Pause (not Stop): finish current phase, hold next phase/track until Resume. Sessions remain for attach.".into(),
        )),
        CardState::HardFailure => Some((
            "note err",
            "Hard failure. See Failure Artifact. Stop is not a failure class and does not write FAILURE.md.".into(),
        )),
        CardState::TokenIdleCi => {
            let summary = card
                .view
                .ci
                .as_ref()
                .and_then(|c| c.last_summary.clone())
                .unwrap_or_else(|| "CI outside model sessions — no token burn.".into());
            Some(("note", summary))
        }
        CardState::Idle => Some((
            "note",
            "Idle / no active track. Stop on a running project aborts the phase, blocks merge, and leaves sessions for attach.".into(),
        )),
        CardState::ParallelPlanReview => Some((
            "note",
            "Parallel plan reviewers (agy + opencode). Not a single waiting reviewer.".into(),
        )),
        CardState::Running => {
            if card.view.stall.is_some() || card.view.last_event.starts_with("recycle:") {
                Some(("note warn", card.view.last_event.clone()))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_css_has_tokens_and_narrow_break() {
        assert!(SURFACE_CSS.contains("--bg: #0f1419"));
        assert!(SURFACE_CSS.contains("--accent: #e8a04a"));
        assert!(SURFACE_CSS.contains("--ok: #3dba7a"));
        assert!(SURFACE_CSS.contains("--bad: #e05a5a"));
        assert!(SURFACE_CSS.contains("--info: #5b9fd4"));
        assert!(SURFACE_CSS.contains("color-mix"));
        assert!(SURFACE_CSS.contains("@media (max-width: 980px)"));
        assert!(SURFACE_CSS.contains("grid-template-columns: 1fr"));
        assert_eq!(WINDOW_TITLE, "Coordinator — Local Ops Console");
        const { assert!(DEFAULT_INNER_WIDTH > 980.0) };
    }

    #[test]
    fn running_card_warns_from_last_event_when_stalled() {
        use crate::registry::ProjectRecord;
        use crate::state::{RunState, RunStatus, StallView};
        use chrono::Utc;
        use uuid::Uuid;

        let dir = tempfile::tempdir().unwrap();
        let rec = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        };
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = crate::workflow::graph::PHASE_PLAN.into();
        state.last_event = "watchdog: stall — no harness progress for 12s".into();
        state.stalled_at = Some(Utc::now());
        let mut view = crate::state::StatusView::from_record(&rec, &state);
        view.stall = Some(StallView {
            since: Utc::now(),
            idle_secs: 12,
        });
        let card = ProjectCard {
            view,
            card_state: CardState::Running,
            sessions: Vec::new(),
        };
        let (cls, text) = note_for(&card).expect("stall note");
        assert_eq!(cls, "note warn");
        assert!(text.contains("watchdog: stall"));
        assert_ne!(card.card_state, CardState::HardFailure);
    }

    #[test]
    fn running_card_warns_from_recycle_last_event() {
        use crate::registry::ProjectRecord;
        use crate::state::{RunState, RunStatus};
        use chrono::Utc;
        use uuid::Uuid;

        let dir = tempfile::tempdir().unwrap();
        let rec = ProjectRecord {
            id: Uuid::new_v4().to_string(),
            path: dir.path().to_path_buf(),
            display_name: None,
            layout_profile: crate::layout::LayoutProfile::Nested,
            conductor_dir: None,
            execution_repo: None,
            execution_repos: std::collections::BTreeMap::new(),
            state_dir: None,
            auto_merge: true,
            phase_timeouts_secs: std::collections::BTreeMap::new(),
            created_at: Utc::now(),
        };
        let mut state = RunState::idle(&rec.id);
        state.status = RunStatus::Running;
        state.phase = crate::workflow::graph::PHASE_PLAN.into();
        state.last_event = crate::harness::abort::RECYCLE_STALL_EVENT.into();
        let view = crate::state::StatusView::from_record(&rec, &state);
        let card = ProjectCard {
            view,
            card_state: CardState::Running,
            sessions: Vec::new(),
        };
        let (cls, text) = note_for(&card).expect("recycle note");
        assert_eq!(cls, "note warn");
        assert!(text.starts_with("recycle:"));
        assert_ne!(card.card_state, CardState::HardFailure);
    }
}
