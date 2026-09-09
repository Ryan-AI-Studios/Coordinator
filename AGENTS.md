# AGENTS.md — Coordinator (product)

Rust-first multi-harness track orchestrator. **This directory is the product git root.**

Planning, conductor tracks, ADRs, and shared understanding live **one level up**:

`C:\dev\coordinator\` (not in this repo’s commits).

## Workspace split

| Path | Role |
|------|------|
| `C:\dev\coordinator\coordinator\` | **This repo** — product code only |
| `C:\dev\coordinator\` (except this folder) | Planning docs, ADRs |
| `C:\dev\coordinator\conductor\` | Track registry / specs / plans |

**Never** commit `conductor/`, `docs/adr/`, `SHARED-UNDERSTANDING.md`, or planner handoff into this repo.

## Tools (always product cwd)

Init once when the repo is ready:

```powershell
cd C:\dev\coordinator\coordinator
ai-brains context
ledgerful init
```

Every coding session (when inited):

```powershell
cd C:\dev\coordinator\coordinator
ai-brains preflight --summary
ledgerful doctor --json
ledgerful change-context --json
```

Prefer `ledgerful … --json` when parsing. See `.agents/skills/ledgerful` and `ai-brains`.

## Build / test

```powershell
cd C:\dev\coordinator\coordinator
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --no-default-features
cargo run -- --help
cargo run -- ui
```

Ledgerful verify steps (when configured) must match these real cargo commands.

Control Plane entrypoints: `project add|list|show|set|scan` (`--auto-merge true|false`; `--notify-progress true|false` on set; `--phase-timeout PHASE=SECS` on add/set), `doctor [--project]` (pretty JSON; exit 1 if a required harness is missing/auth), `run|status|pause|resume|stop` (`run --driver adapter|file_wait|stub`; `run --serve-port N`; `run --skip-preflight`), `outcome write|show`, `failure show|resolve`, `notify hermes-test [--progress]`, `wait`, `harness grok start|prompt|compact|status|shutdown`, `roles show|use grok|cursor`, `serve` (`--port`, `--check`; 127.0.0.1:7420), `ui` (Status Surface; WebView2 Evergreen; default-on `ui`). View-model tests: `cargo test ui`. Live window: `cargo run -- ui` (not required in CI). Mock HTML remains the visual reference (`mock/status-surface.html`). Layout/profile tests: `cargo test layout`, `cargo test scan`, `cargo test registry`. Phase Outcome tests: `cargo test outcome`. Harness tests: `cargo test harness`. Workflow tests: `cargo test workflow`. Notify tests: `cargo test notify`. CI-wait tests: `cargo test ci`. Cross-model review tests: `cargo test review`. Env overrides: `COORDINATOR_HOME`, `COORDINATOR_STATE_DIR`, `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`, `COORDINATOR_PHASE_TIMEOUT_SECS`, `COORDINATOR_WORKFLOW_DRIVER`, `COORDINATOR_OUTCOME_POLL_MS`, `COORDINATOR_NOTIFY` (`off` disables Windows toasts; does not disable Hermes), `COORDINATOR_NOTIFY_PROGRESS` (`1`/`true`/`on` enables progress POSTs; `off` force-disables), `COORDINATOR_HERMES` (`off` force-disables Hermes), `COORDINATOR_HERMES_URL`, `COORDINATOR_HERMES_SECRET` (env-only; never persist), `COORDINATOR_HERMES_LIVE`, `COORDINATOR_GROK_BIN`, `COORDINATOR_GROK_LIVE`, `COORDINATOR_CURSOR_BIN`, `COORDINATOR_CURSOR_LIVE`, `COORDINATOR_GH_BIN`, `COORDINATOR_CI_POLL_MS` (fixed ci-wait interval), `COORDINATOR_GH_LIVE`, `COORDINATOR_CODEX_BIN`, `COORDINATOR_CLAUDE_BIN`, `COORDINATOR_OPENCODE_BIN`, `COORDINATOR_OPENCODE_LIVE` (ignored plan-review `opencode run` smoke), `COORDINATOR_AGY_BIN` (plan-review Antigravity one-shot), `COORDINATOR_AGY_LIVE`, `COORDINATOR_REVIEW_LIVE`, `COORDINATOR_PROGRESS_STALL_SECS` (adapter stall interval; `0` disables), `COORDINATOR_CANCEL_WAIT_SECS` (after `session/cancel` before recycle; default 10; `0` recycles now). Adapter **plan-review** starts `agy --print` and `opencode run` (no operator role JSON). The 0011 gate still uses `opencode run --dir {exec} --format default` with prompt on stdin and no `--auto`. Plan-review `opencode run` also pipes the prompt on stdin (not argv). Live Grok smoke (ignored by default): `$env:COORDINATOR_GROK_LIVE='1'; cargo test grok_live -- --ignored --nocapture`. Live Cursor ACP smoke (ignored): `$env:COORDINATOR_CURSOR_LIVE='1'; cargo test cursor_live -- --ignored --nocapture`. Live `gh` smoke (ignored): `$env:COORDINATOR_GH_LIVE='1'; cargo test ci_live -- --ignored --nocapture`. Live review smoke (ignored): `$env:COORDINATOR_REVIEW_LIVE='1'; cargo test review_live -- --ignored --nocapture`. Live agy plan-review (ignored): `$env:COORDINATOR_AGY_LIVE='1'; cargo test agy_live -- --ignored --nocapture`. Live opencode plan-review (ignored): `$env:COORDINATOR_OPENCODE_LIVE='1'; cargo test opencode_live -- --ignored --nocapture`. See product README for Layout Profiles, Phase Outcome, Grok/Cursor ACP (`/compact` vs `/summarize`, `roles use grok|cursor`), the canonical workflow, Failure Artifact / toast, token-idle CI + auto-merge, and the cross-model review gate.

## Code style

- Rust edition and formatting as set by `rustfmt` / project `Cargo.toml`  
- Prefer small, testable modules; deep modules over sprawling glue  
- No secrets in git; OAuth stays with each harness  

## Agent entry points

| Intent | Skill |
|--------|--------|
| Orient | `.agents/skills/onboarding` |
| Implement track | `.agents/skills/implement-track` |
| Cross-model gate | `.agents/skills/codex-review` (local; not shipped) |
| Plan only | `C:\dev\coordinator\.agents\skills\plan-track` |

Adapter injects name the phase skill as `{workspace|execution}/.agents/skills/<phase>/SKILL.md` (planning skills under the workspace root are not visible from the product `grok_cwd`).

**Live autonomous walk:** `--project C:\dev\Helping-Hands --track 0099` only. Do not drive HH **0001–0013**. Do not drop review files or `outcome write`.

## PR discipline

- Feature branch → PR → CI green → squash-merge (default for later tracks)  
- Bootstrap track **0001** allowed direct push to `main` per track plan  
- Do not busy-poll CI  
- Do not force-push shared history without owner confirmation  

## Review focus

- Plan fidelity vs `conductor/<track>/`  
- Wrong cwd for ledgerful/ai-brains  
- Planning files staged into product  
- Autonomy safety (timeouts, stop/pause, failure classes) when touching orchestration core  

**Two-project CLI:** after more than one registry project, omit `--project` when cwd uniquely matches a registered workspace or execution repo; else last-used (`{COORDINATOR_HOME}/last-used.json`) if still registered; else error. `--project` is required when cwd is outside and last-used is unset/stale. Ambiguous cwd does not fall through to last-used. HTTP omit never uses serve cwd. `doctor` omit remains valid (machine-wide; does not infer).

**Omit `--track`:** starts the first exact `Ready — not started` row when `track_id` is unset or Idle after `workflow: backlog clear`; else retain. Never Proposed/HITL/trailing notes. Fail closed → `--track`. Live autonomous walk still names `--track 0099` for Helping Hands (HH has no exact Ready today; omit would error).

**`address-findings`:** after a cross-model GateFail, adapter injects this named phase (implementor Role Binding + implement/onboarding skill paths), then a fresh gate; cap 2 then `difficulty` Stop.

**Scan footgun:** never `project scan --root C:\dev --add` — `C:\dev` has many conductor markers (Orca, coordinator, coordinated, …). Scan a single workspace (`--root C:\dev\Orca`) or add one project at a time.

**Scan footgun (coordinated):** `project scan --add` of `C:\dev\coordinated` would register **`nested`** (scan never returns `multi_sibling`; the hub has no nested product children). Use explicit `project add --profile multi_sibling`.

**Wait vs phase timeout vs stall vs shutdown:** Bare `run` ticks until Idle/Stopped (no poll budget). Default `run` Auto-detects serve (healthy lease then 7420); `--serve-port N` probes N only. If health JSON is coordinator (`ok` + `service=coordinator`), default `run` skips the wait loop. `--detach` is the explicit write-only hatch (conflicts with `--timeout-secs` and `--serve-port`). Optional `run --timeout-secs N` (`N>0`) is the same CLI poll budget as `wait` (exit **2**, run unchanged, Grok stays up, **no abort**). Ctrl-C during tick leaves the run **Running** (no abort, no artifact); `stop` from another terminal if you want to hold. `wait` remains the attach poller (default 3600) for detach / serve-owned machines. Phase wall clock is `failure_class=timeout` + Stopped + `FAILURE.md` **and** abort/recycle of the in-flight Prompt. Progress stall (default 600s, `COORDINATOR_PROGRESS_STALL_SECS` / machine `progress_stall_secs`, `0` disables) recycles on the first fire this phase unless sidecar `tool_in_flight`: `session/cancel` then recycle (`recycle: stall — new session`), stay **Running**, no artifact. `session/update` text is **not** a skip. A live ACP tool or TerminalHub child keeps `tool_in_flight` and the first stall surfaces `watchdog: stall` only. A second stall only surfaces `watchdog: stall` until the phase clock. One-shot `cross-model-review` / `plan-review` children use the **same 600s knob** against stdio/artifact progress: a stall tree-kills that child (exit 124, `reviewer stall — no progress for`) and 0011 falls through / plan-review degrades — it is **not** ACP recycle / `tool_in_flight`. Cancel wait: `COORDINATOR_CANCEL_WAIT_SECS` (default 10; `0` = recycle now). `harness grok shutdown` kills the holder / persist pid (pid-kill fallback) and writes `alive: false`. Operator `stop` (CLI or Status Surface) does not kill sessions and does not write `FAILURE.md`.
