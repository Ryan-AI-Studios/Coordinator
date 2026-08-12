# Coordinator

Rust-first, same-machine **Windows** orchestrator that drives long-lived AI CLI harnesses through conductor-track workflows.

This directory is the **product git root** (Execution Repo). Planning, ADRs, and conductor tracks live one level up at `C:\dev\coordinator\` and are **not** part of this repository.

## Clone

```text
https://github.com/Ryan-AI-Studios/Coordinator
```

## Build / test

```powershell
cd C:\dev\coordinator\coordinator   # or your clone root
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --help
```

Same gate as CI and ledgerful verify: `fmt --check`, `clippy -D warnings`, and `test` on `windows-latest`.

## Control Plane (local CLI + HTTP)

Minimal local Control Plane: machine **Project Registry**, per-project **run state**, **Stop / Pause / Resume** state machine (stub phases), and a **localhost-only** JSON API.

| Store | Default (Windows) | Override |
|-------|-------------------|----------|
| Machine home (registry) | `%LOCALAPPDATA%\coordinator\` | `COORDINATOR_HOME` |
| Registry file | `{home}/registry.json` | — |
| Per-project run state | `{workspace}/.coordinator/run-state.json` | `COORDINATOR_STATE_DIR` → `{override}/{project_id}/run-state.json`, or registry `state_dir` field |

**Local-only:** `coordinator serve` binds **`127.0.0.1` only** (default port **7420**, avoids Impeccable live 5500/8400). Non-loopback bind is rejected.

**Stub phases:** a project left in `Running` with phase `stub:active` does **not** auto-advance or time out. Heartbeats / Phase Outcomes → later tracks (**0005+**).

```powershell
# Smoke (temp home + fake project)
$env:COORDINATOR_HOME = "$env:TEMP\coordinator-cp-smoke"
New-Item -ItemType Directory -Force -Path $env:COORDINATOR_HOME | Out-Null
$proj = Join-Path $env:TEMP "coordinator-fake-project"
New-Item -ItemType Directory -Force -Path $proj | Out-Null

cargo run -- project add $proj
cargo run -- project list
cargo run -- run --project $proj
cargo run -- status --project $proj
cargo run -- pause --project $proj
cargo run -- resume --project $proj
cargo run -- stop --project $proj
cargo run -- status --project $proj

# HTTP (separate terminal)
cargo run -- serve --port 7420
# Invoke-RestMethod http://127.0.0.1:7420/health
# Invoke-RestMethod http://127.0.0.1:7420/v1/status
```

CLI surface:

```text
coordinator project add <path>
coordinator project list
coordinator status [--project <path|id>]
coordinator run [--project <path|id>] [--track <id>]
coordinator pause [--project <path|id>]
coordinator resume [--project <path|id>]
coordinator stop [--project <path|id>]
coordinator serve [--port <u16>]   # default 7420, 127.0.0.1 only
```

Stop aborts advancement with **no merge**; `last_event` records sessions-for-attach deferred (real harness attach → **0007+**).

## Tools (product cwd only)

```powershell
ai-brains preflight --summary
ledgerful doctor --json
ledgerful change-context --json
```

Do **not** initialize ledgerful or ai-brains in the planning parent folder.

## Design context (Impeccable)

Project-scope Impeccable lives in **this product tree** (ADR-0028 long-term home). Design SoT files:

| File | Role |
|------|------|
| [`PRODUCT.md`](./PRODUCT.md) | Who/what/why for operators + Status Surface |
| [`DESIGN.md`](./DESIGN.md) | Visual system (“Local Ops Console”); tokens from `mock/status-surface.html` |
| [`.agents/skills/impeccable/`](./.agents/skills/impeccable/) | Tracked skill payload (Codex / shared agents) |

```powershell
cd C:\dev\coordinator\coordinator
npx --yes impeccable install --scope=project --providers=codex,grok,opencode,claude
npx --yes impeccable check
# In harness: /impeccable init  (or refresh PRODUCT/DESIGN via document)
# Grok: trust project hooks once (/hooks-trust). Codex: approve /hooks after updates.
```

Re-run install/init here if the Status Surface UI subtree moves (ADR-0028). Do **not** use global-only install as the design SoT.

## Status Surface mock (track 0003)

**Stack-agnostic** operator Status Surface mock (not wired to a Control Plane). Visual contract for later UI / Control Plane work (**0004+**).

| Path | Role |
|------|------|
| [`mock/status-surface.html`](./mock/status-surface.html) | Mock UI: multi-project board + state coverage (idle, parallel plan-review, CI wait, pause, stop, failure artifact) |
| [`mock/MODULE-MAP.md`](./mock/MODULE-MAP.md) | Panel → future data/component map for implementors |
| [`scripts/start-impeccable-live.ps1`](./scripts/start-impeccable-live.ps1) | After reboot: static page + Impeccable live inject |
| [`.impeccable/live/config.json`](./.impeccable/live/config.json) | Live inject target (`mock/status-surface.html`) |
| [`DESIGN.md`](./DESIGN.md) | Tokens extracted from mock `:root` CSS variables |

```powershell
cd C:\dev\coordinator\coordinator
pwsh .\scripts\start-impeccable-live.ps1
# opens http://127.0.0.1:5500/mock/status-surface.html
# fallback: open mock\status-surface.html (file://) or any static server
# optional (agent session): node .agents/skills/impeccable/scripts/live-poll.mjs
```

**State walkthrough cues:** Coordinator = parallel plan review (agy + opencode); Orca = pause; Ledgerful = token-idle CI; AI-Brains = hard fail + Failure Artifact; Demo-Idle = no active track. Header help = Stop vs Pause. Check narrow width (~980px) once.

This is a **visual contract**, not product orchestration. Real start/resume is Control Plane work (**0004+**).

## Status

Tracks **0001** (crate + CI), **0002** (Impeccable + design context), **0003** (Status Surface mock + module map), **0004** (Control Plane skeleton: CLI + localhost API + registry + stop/pause stubs).