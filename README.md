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

Minimal local Control Plane: machine **Project Registry**, per-project **run state**, **Stop / Pause / Resume**, **Phase Outcome File** completion signals, and a **localhost-only** JSON API.

| Store | Default (Windows) | Override |
|-------|-------------------|----------|
| Machine home (registry) | `%LOCALAPPDATA%\coordinator\` | `COORDINATOR_HOME` |
| Registry file | `{home}/registry.json` | — |
| Machine config | `{home}/config.json` (`scan_roots`) | — |
| Per-project run state | `{workspace}/.coordinator/run-state.json` | `COORDINATOR_STATE_DIR` → `{override}/{project_id}/run-state.json`, or registry `state_dir` field |
| Active Phase Outcome | `{state_dir}/outcomes/current.json` | same state-dir rules |
| Last applied outcome | `{state_dir}/outcomes/current.applied.json` | written on successful apply; `current.json` removed |

Empty `COORDINATOR_HOME` or `COORDINATOR_STATE_DIR` is rejected.

### Layout Profiles (path bindings)

Each registered project has a **layout profile** that tells the Control Plane how to resolve Workspace Root, conductor dir, execution repo(s), and state dir. Session pool key for later harness tracks is **`project_id` + workspace `path`** (path is immutable via `project set`).

| Profile | Workspace | Conductor (default) | Execution | Typical use |
|---------|-----------|---------------------|-----------|-------------|
| **`nested`** (default) | registry `path` | `{ws}/conductor` | primary `execution_repo` (or auto-detect one child product) | Orca, Coordinator itself |
| **`multi_sibling`** | hub path | `{ws}/conductor` | named `execution_repos` map + optional primary | coordinated-style hubs |
| **`single_root`** | registry `path` | `{ws}/conductor` | **always** workspace root on resolve | monorepo with conductor inside product |

**Nested auto-detect (on add / scan):** inspect **immediate child directories only** (never the workspace root). Eligible child has `Cargo.toml` or `.git`, basename not in `{conductor, .git, .agents, docs, mock}`. Exactly one match → store as `execution_repo`; zero or many → leave null. Root-level `.git` / `Cargo.toml` alone does **not** count as nested product (that shape is `single_root`).

**`single_root` inert fields:** setting `execution_repo` / `execution_repos` via add/set is allowed but **ignored on resolve** (execution is always the workspace root).

**Profile flip:** changing `layout_profile` does **not** auto-clear stored path fields. `resolve` is profile-specific; `project show` prints **raw** record fields and **resolved** paths so mismatches are visible.

**Scan (ADR-0026):** `project scan --root <dir>` checks the root and its **immediate children** for `conductor/conductor.md`. No deep recursion; directory junctions/symlinks are single entries (marker only, no descendant walk). Dry-run is default; `--add` registers new hits. CI/tests must pass explicit `--root` (do not rely on default `C:\dev`). Default `scan_roots` in `config.json` is `["C:\\dev"]` on Windows when that path exists, else `[]`.

```powershell
# Nested fixture
$ws = "$env:TEMP\coord-nested"
New-Item -ItemType Directory -Force -Path "$ws\conductor","$ws\ProductApp" | Out-Null
Set-Content "$ws\conductor\conductor.md" "# tracks"
Set-Content "$ws\ProductApp\Cargo.toml" "[package]`nname=`"p`"`nversion=`"0.1.0`""
cargo run -- project add $ws --profile nested
cargo run -- project show --project $ws
# expect layout_profile=nested, execution_repo → ProductApp

# Multi-sibling hub
cargo run -- project add $hub --profile multi_sibling
cargo run -- project set --project $hub --execution-repos-json '{\"app\":\"C:\\dev\\app\"}'

# Scan dry-run then add
cargo run -- project scan --root $scanRoot --dry-run
cargo run -- project scan --root $scanRoot --add
```

When nested `execution_repo` is null, `project show` includes:

`hint: coordinator project set --project <id|path> --execution-repo <path>`

**Local-only:** `coordinator serve` binds **`127.0.0.1` only** (default port **7420**, avoids Impeccable live 5500/8400). Non-loopback bind is rejected.

**Completion contract (hybrid):** the **Phase Outcome File** is the portable done-signal (schema `version: 1`). Hooks, adapters, CLI, and HTTP may write it; **ConPTY / chat pattern-match is not the contract**. Writers must use **temp + replace** (or `coordinator outcome write` / `POST /v1/outcome`) so pollers never read torn JSON.

**Stub phase timeout:** while `Running`, phase `stub:active` has a wall budget (default **300s**, env `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`). Budget is **frozen while Paused**. On fire, Control Plane synthesizes `failure_class=timeout` via the same apply path. Poll interval: default **500ms** (`COORDINATOR_OUTCOME_POLL_MS`).

**`run` without `--track`:** retains the prior `track_id` (intentional). Clearing track is a workflow concern for later tracks.

```powershell
# Smoke (temp home + fake project)
$env:COORDINATOR_HOME = "$env:TEMP\coordinator-cp-smoke"
New-Item -ItemType Directory -Force -Path $env:COORDINATOR_HOME | Out-Null
$proj = Join-Path $env:TEMP "coordinator-fake-project"
New-Item -ItemType Directory -Force -Path $proj | Out-Null

cargo run -- project add $proj
cargo run -- project list
cargo run -- run --project $proj --track 0005
cargo run -- status --project $proj
cargo run -- outcome write --project $proj --phase stub:active --status success
cargo run -- status --project $proj
# expect Idle / stub:completed

# timeout smoke
$env:COORDINATOR_STUB_PHASE_TIMEOUT_SECS = "2"
cargo run -- run --project $proj
cargo run -- wait --project $proj --timeout-secs 10
cargo run -- status --project $proj
# expect Stopped / failure_class timeout

# HTTP (separate terminal)
cargo run -- serve --port 7420
# Invoke-RestMethod http://127.0.0.1:7420/health
# Invoke-RestMethod http://127.0.0.1:7420/v1/status
# POST http://127.0.0.1:7420/v1/outcome  body = Phase Outcome JSON
```

CLI surface:

```text
coordinator project add <path>
    [--profile nested|multi_sibling|single_root]
    [--execution-repo <path>] [--conductor-dir <path>] [--state-dir <path>]
    [--display-name <name>] [--execution-repo-name <name>]
coordinator project list
coordinator project show [--project <path|id>]
coordinator project set [--project …]
    [--profile …] [--execution-repo …] [--conductor-dir …] [--state-dir …]
    [--display-name …] [--execution-repos-json <json>] [--execution-repo-name …]
coordinator project scan [--root <path>]... [--add] [--dry-run] [--save-root]
coordinator status [--project <path|id>]
coordinator run [--project <path|id>] [--track <id>]
coordinator pause [--project <path|id>]
coordinator resume [--project <path|id>]
coordinator stop [--project <path|id>]
coordinator outcome write --phase <id> --status success|failure
    [--failure-class <enum>] [--message <text>] [--project …]
    [--next-track <id>] [--source cli]
coordinator outcome show [--project …]
coordinator wait [--project …] [--timeout-secs N]
coordinator serve [--port <u16>]   # default 7420, 127.0.0.1 only
```

HTTP: `POST/GET /v1/projects` (layout fields), `POST /v1/projects/set`, `POST /v1/projects/scan`, plus run/status/outcome routes. Status JSON includes additive `layout_profile`, `execution_repo`, `conductor_dir` (resolved).

### Phase Outcome schema v1

```json
{
  "version": 1,
  "phase": "stub:active",
  "status": "success",
  "failure_class": null,
  "message": "optional human/agent note",
  "written_at": "2026-08-12T12:00:00Z",
  "source": "cli",
  "metadata": {
    "next_track": null,
    "role": null
  }
}
```

| Field | Rules |
|-------|--------|
| `version` | Must be `1` |
| `phase` | Non-empty; must match current run-state phase to apply |
| `status` | `success` \| `failure` |
| `failure_class` | Required when `failure`; must be null on `success`. Values: `permission`, `model_exhaustion`, `difficulty`, `harness_crash`, `timeout`, `ci_failed` |
| `source` | `file` \| `http` \| `cli` \| `timeout` \| `test` |
| `metadata.next_track` | Optional; copied to status on success |
| `metadata.role` | Free-form this release (parallel roles later) |
| `run_epoch` | Optional; when present must match run-state epoch |

**Apply (single path):** Running+success → Idle / `stub:completed`; Paused+success → stay Paused / `stub:completed`; failure → Stopped / `stub:failed` + class. Idle/Stopped and phase mismatch reject for CLI/HTTP. After apply: history best-effort → `current.applied.json` → remove `current.json` → hash on run-state.

### `wait` exit codes

| Code | Meaning |
|------|---------|
| **0** | An outcome was **applied** (success **or** failure, including synthesized timeout) |
| **2** | Wait budget (`--timeout-secs`) expired **without** an applied outcome |
| **1** (or other) | Invalid args, unknown project, or other control-plane error |

Scripts that want “success only” must inspect `status` / `failure_class` after exit 0 (e.g. `coordinator status`).

Stop aborts advancement with **no merge**; `last_event` records sessions-for-attach deferred (real harness attach → **0007+**). After Stopped, further outcomes are ignored until a new `run`.

### Illustrative hook writers (docs only)

Hooks may drop `outcomes/current.json` with atomic replace. Event names drift across harness versions — treat the following as **illustrative**, not a shipped adapter:

**Claude Code** (event name verified 2026-08: `Stop` when the model finishes a turn):

```powershell
# Illustrative: after a Stop hook decides the phase is done
$state = Join-Path $env:COORDINATOR_PROJECT ".coordinator\outcomes"
New-Item -ItemType Directory -Force -Path $state | Out-Null
$tmp = Join-Path $state "current.json.tmp"
$dst = Join-Path $state "current.json"
@{
  version = 1
  phase = "stub:active"
  status = "success"
  failure_class = $null
  message = "hook Stop"
  written_at = (Get-Date).ToUniversalTime().ToString("o")
  source = "file"
} | ConvertTo-Json | Set-Content -Path $tmp -Encoding utf8
Move-Item -Force $tmp $dst
```

**Grok / other CLI hooks:** same file contract; prefer `source=file`. Full Grok Session adapter → **0007**.

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

Tracks **0001** (crate + CI), **0002** (Impeccable + design context), **0003** (Status Surface mock + module map), **0004** (Control Plane skeleton), **0005** (Phase Outcome File + apply + wait/timeout).