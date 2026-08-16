# Coordinator

Rust-first, same-machine **Windows** orchestrator that drives long-lived AI CLI harnesses through conductor-track workflows.

This directory is the **product git root** (Execution Repo). Planning, ADRs, and conductor tracks live one level up at `C:\dev\coordinator\` and are **not** part of this repository.

## Clone

```text
https://github.com/Ryan-AI-Studios/Coordinator
```

## Install

Requires [rustup](https://rustup.rs/) and [WebView2 Evergreen](https://developer.microsoft.com/microsoft-edge/webview2/). The binary lands in `%USERPROFILE%\.cargo\bin` (must be on PATH). `--locked` honors this repo’s `Cargo.lock`. Default features include the Status Surface. Not published to crates.io — do **not** `cargo install coordinator`.

From a clone:

```powershell
git clone https://github.com/Ryan-AI-Studios/Coordinator
cd Coordinator
cargo install --path . --locked
coordinator --help
coordinator serve
coordinator ui
```

Without a clone:

```powershell
cargo install --git https://github.com/Ryan-AI-Studios/Coordinator --locked
```

CLI-only (no Dioxus / no window): `cargo install --path . --locked --no-default-features`. Installing without `--locked` still gets `ui` (it is default) but may float dependencies — prefer `--locked`.

## Build / test

```powershell
cd C:\dev\coordinator\coordinator   # or your clone root
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --no-default-features
cargo run -- --help
cargo run -- ui
```

Same gate as ledgerful verify: `fmt --check`, `clippy -D warnings`, and `test` on `windows-latest`. CI also runs `cargo test --no-default-features --lib`. Default features include `ui`. Use `--no-default-features` for Control Plane-only iteration.

## Control Plane (local CLI + HTTP)

Minimal local Control Plane: machine **Project Registry**, per-project **run state**, **Stop / Pause / Resume**, **Phase Outcome File** completion signals, and a **localhost-only** JSON API.

| Store | Default (Windows) | Override |
|-------|-------------------|----------|
| Machine home (registry) | `%LOCALAPPDATA%\coordinator\` | `COORDINATOR_HOME` |
| Registry file | `{home}/registry.json` | — |
| Machine config | `{home}/config.json` (`scan_roots`, `role_bindings`, `phase_timeouts_secs`, `hermes`) | — |
| Serve lease | `{home}/serve.json` | written after a successful `serve` bind; deleted on graceful shutdown |
| Per-project run state | `{workspace}/.coordinator/run-state.json` | `COORDINATOR_STATE_DIR` → `{override}/{project_id}/run-state.json`, or registry `state_dir` field |
| Active Phase Outcome | `{state_dir}/outcomes/current.json` | same state-dir rules |
| Last applied outcome | `{state_dir}/outcomes/current.applied.json` | written on successful apply; `current.json` removed |
| Outcome history | `{state_dir}/outcomes/history/` | best-effort snapshots after apply |
| Role outcomes (plan-review) | `{state_dir}/outcomes/roles/{slug}.json` | parallel reviewer slots (`agy`, `opencode`) |
| Review files | `{state_dir}/reviews/{slug}-review.md` | always; copied to track dir when resolvable |
| Review Bundle | `{workspace}/AI-review.md` | assembled after plan-review join; track copy when present |
| Failure Artifact | `{state_dir}/FAILURE.md` | written on hard failure (not operator stop/pause); cleared on fresh `run` |

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

### Orca dogfood (nested default)

First live Project is **`C:\dev\Orca`** (workspace) + **`C:\dev\Orca\OrcaSlicer-ZR`** (execution). State stays at `{workspace}/.coordinator` — do **not** set `COORDINATOR_STATE_DIR` for this path (that would skip the ADR-0014 default). Keep **`auto_merge=false`** on this record so a mistaken implement PR cannot squash-merge the slicer.

**Never** `project scan --root C:\dev --add`. `C:\dev` has many `conductor/conductor.md` trees. Scan only `--root C:\dev\Orca`, or `project add` that path.

```powershell
cd C:\dev\coordinator\coordinator
# Do NOT: cargo run -- project scan --root C:\dev --add
cargo run -- project add C:\dev\Orca --profile nested --auto-merge false --display-name Orca
cargo run -- project show --project C:\dev\Orca
# expect layout_profile=nested, execution_repo → OrcaSlicer-ZR,
# conductor_dir → C:\dev\Orca\conductor, state_dir → C:\dev\Orca\.coordinator, auto_merge=false
cargo run -- project scan --root C:\dev\Orca
# expect already_registered

# Probe only — never --track 0001–0005
# Bare `run` ticks until Idle/Stopped (no separate `wait`).
cargo run -- run --project C:\dev\Orca --track 0099 --driver stub
cargo run -- status --project C:\dev\Orca
# expect Idle / backlog clear; last_event must not contain "skip: deferred"
```

Status JSON for this walk includes `layout_profile`, `execution_repo`, `conductor_dir`, `workflow` (`id`, `driver`, `pending_roles`), `next_track` (null when 0099 completes), and `failure_artifact` (null unless a hard fail). `ci` / `review` stay `null` on a stub walk that never parks on those phases.

### Coordinated dogfood (multi-sibling compatibility)

Second live Project is **`C:\dev\coordinated`** (planning hub) + named siblings under `C:\dev\`. Profile is **`multi_sibling`**. State stays at `{workspace}/.coordinator` — do **not** set `COORDINATOR_STATE_DIR` for this path. Keep **`auto_merge=false`** on this record so a mistaken implement PR cannot squash-merge a sibling. Grok cwd is the primary execution repo **`C:\dev\ledgerful`**.

**Never** `project scan --root C:\dev --add`. **Never** `project scan --root C:\dev\coordinated --add` — scan detect returns **`nested`** + null exec (the hub has no nested product children). Always `project add --profile multi_sibling`.

After this add, the machine has two projects (Orca + coordinated). Every `run` / `wait` / `status` / `stop` / `pause` / `show` **must** pass `--project`.

`project set --execution-repos-json` **replaces** the map — include `ledgerful` again.

```powershell
cd C:\dev\coordinator\coordinator
# Do NOT: cargo run -- project scan --root C:\dev --add
# Do NOT: cargo run -- project scan --root C:\dev\coordinated --add
cargo run -- project add C:\dev\coordinated --profile multi_sibling --auto-merge false --display-name coordinated --execution-repo C:\dev\ledgerful --execution-repo-name ledgerful
$json = '{"ledgerful":"C:\\dev\\ledgerful","ledgerful-action":"C:\\dev\\ledgerful-action","ledgerful-frontend":"C:\\dev\\ledgerful-frontend","ledgerful-web":"C:\\dev\\ledgerful-web"}'
cargo run -- project set --project C:\dev\coordinated --execution-repos-json $json --execution-repo C:\dev\ledgerful
cargo run -- project show --project C:\dev\coordinated
# expect layout_profile=multi_sibling, execution_repo → C:\dev\ledgerful,
# four named execution_repos, conductor_dir → C:\dev\coordinated\conductor,
# state_dir → C:\dev\coordinated\.coordinator, auto_merge=false
cargo run -- project scan --root C:\dev\coordinated
# expect already_registered=true; detected_profile may be nested (expected)

# Probe only — never --track 0001–0187 or 0101
# Bare `run` ticks until Idle/Stopped (no separate `wait`).
cargo run -- run --project C:\dev\coordinated --track 0899 --driver stub
cargo run -- status --project C:\dev\coordinated
# expect Idle / backlog clear; last_event must not contain "skip: deferred"; next_track null
```

Status JSON still exposes only the **primary** `execution_repo` + `layout_profile`. The named map is proven via `project show` (`project.execution_repos` + `resolved.execution_repos`).

When more than one project is registered, omit `--project` and the CLI errors (it does not silently target Orca).

**Local-only:** `coordinator serve` binds **`127.0.0.1` only** (default port **7420**, avoids Impeccable live 5500/8400). Non-loopback bind is rejected.

### Two daily paths

Keep both. After a reboot the operator starts installed `coordinator serve` (or `coordinator ui`) again — there is **no** login installer or Windows Service.

| Path | When | What happens |
|------|------|----------------|
| **Foreground `run`** | One project, stay in the terminal | Bare `coordinator run` writes Running and **ticks** until Idle/Stopped (track 0020). |
| **Always-on `serve`** | Multi-project / close the terminal | `coordinator serve` is the machine ticker. `run` writes Running and **skips wait** when it finds that serve (healthy lease, then 7420, or `--serve-port N`). `--detach` is the explicit write-only hatch. |

```powershell
coordinator serve
coordinator serve --check
coordinator run --project C:\dev\Orca --track 0099 --detach
# or omit --detach: run skips wait when serve health/lease is up
coordinator status --project C:\dev\Orca
# expect ticker.owner = serve
```

**Lease:** `{COORDINATOR_HOME}/serve.json` (not `config.json`). Written after a successful bind; deleted on graceful shutdown. A stale file after a crash is OK — **health JSON** `{ok:true, service:"coordinator"}` is the truth. Unknown `version` is ignored.

**`serve --check`:** one-shot. Does **not** bind and does **not** write a lease. Default probe is the same Auto as `run` (healthy lease, else 7420). `--check --port N` probes N only. Exit **0** + `{ok:true, service, port, source}` when coordinator; exit **1** + `{ok:false, port, source}` otherwise. `source` is `flag` | `lease` | `default`.

**Already listening:** a second `serve` on a live coordinator port does **not** bind. stderr: already listening. Exit **0**. Occupied by a non-coordinator process → error (exit 1).

**Custom-port serve:** `serve --port 7500` writes the lease. A later default `run` finds that port via the lease — `--detach` is no longer required.

**Optional operator login task (not a CLI, not installed by this product):**

```
schtasks /Create /TN "Coordinator serve" /TR "<full-path>\coordinator.exe serve" /SC ONLOGON /IT /F
```

Owner machine policy. `/IT` so it runs only while that user is logged on. `/ru System` is wrong (not interactive). Coordinator does **not** install this.

**Completion contract (hybrid):** the **Phase Outcome File** is the portable done-signal (schema `version: 1`). Hooks, adapters, CLI, and HTTP may write it; **ConPTY / chat pattern-match is not the contract**. Writers must use **temp + replace** (or `coordinator outcome write` / `POST /v1/outcome`) so pollers never read torn JSON.

**Per-phase timeouts:** canonical phases use table defaults (`plan` 1800s, `plan-review` 1200s, `fold` 1200s, `implement` 7200s, `cross-model-review` 2700s, `ci-wait` 3600s, `compact` 600s, `advance` 900s). Resolve order: uniform `COORDINATOR_PHASE_TIMEOUT_SECS` (if set) → **project** `phase_timeouts_secs` → machine `config.json` `phase_timeouts_secs` → table. Set a project override with `project set --phase-timeout PHASE=SECS` (seconds only; `0` is rejected). Machine hand-edit of `config.json` is still valid for machine-wide keys. Leftover `stub:*` phases still use **300s** / `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`. Budget is **frozen while Paused**. On fire, Control Plane synthesizes `failure_class=timeout` via the same apply path (compact timeout **skips**, does not fail) and writes `FAILURE.md`. This is **not** the CLI poll budget (`wait --timeout-secs` or optional `run --timeout-secs`: exit **2**, run stays as it was, Grok stays up). Poll interval: default **500ms** (`COORDINATOR_OUTCOME_POLL_MS`).

Helping Hands / Orca-shaped recipe (plan 1h / implement 3h as seconds). This is **not** `run --timeout-secs`:

```
coordinator project set --project C:\dev\Orca --phase-timeout plan=3600 --phase-timeout implement=10800
coordinator project show --project C:\dev\Orca
```

CLI `run` / `wait` keep the start-of-command `ProjectRecord` snapshot — set timeouts **before** `run`. `serve` reloads the registry each tick.

**Four clocks (do not collapse):** (1) CLI poll budget — `wait --timeout-secs` (default **3600**) or optional `run --timeout-secs N` (`N>0`). Bare `run` has **no** poll budget and ticks until Idle/Stopped. Budget expiry is exit **2**, run unchanged, **no abort**. (2) phase wall clock — project `phase_timeouts_secs` (then machine, then table; env still wins) — `failure_class=timeout` + Stopped + `FAILURE.md` **and** abort/recycle the in-flight Prompt; (3) ACP `session/prompt` timeout — Prompt error mapped onto the phase **and** recycle so the next `start` is a new `session/new`; (4) **progress stall** (default **600s**) — adapter `session/update` / inject heartbeat went silent. First stall this phase: cancel then recycle (`last_event` = `recycle: stall — new session`), stay **Running**, no `FAILURE.md`. Second stall: surface only (`watchdog: stall`) until the phase wall clock. Override via `COORDINATOR_PROGRESS_STALL_SECS` or machine `progress_stall_secs` (`0` disables). Cancel wait: `COORDINATOR_CANCEL_WAIT_SECS` (default **10**; `0` recycles immediately). Operator `stop` still leaves sessions for attach. Ctrl-C during a ticking `run`/`wait` also leaves the run **Running** (no abort, no artifact).

**`run` without `--track`:** retains the prior `track_id` (intentional). Fresh `run` **clears** `next_track` (stale Planner handoff).

### Canonical workflow (`canonical_v1`)

`coordinator run` starts **`plan`**, not `stub:active`. Phase Outcome apply advances the graph (status stays **Running** until `advance` completes).

```
plan → plan-review (agy + opencode join) → fold → implement
  → cross-model-review (Codex→Claude→OpenCode one-shot) → ci-wait (token-idle gh poll)
  → compact → advance
```

| Driver | CLI / env | Behavior |
|--------|-----------|----------|
| `adapter` | default | Inject the phase Role Binding once per phase for plan/fold/implement/advance (default Grok ACP). **plan-review** starts `agy --print` and `opencode run` once each (no operator role JSON). **`cross-model-review` and `ci-wait` never inject** (one-shot review CLIs / token-idle `gh`). Missing / non-Grok long-lived binding **fails** those phases with `permission`. |
| `file_wait` | `--driver file_wait` | No inject; poll `current.json` / `outcomes/roles/*.json`. |
| `stub` | `--driver stub` or `COORDINATOR_WORKFLOW_DRIVER=stub` | Synthesize success each tick so CI can walk the full graph. |

Status JSON includes additive `workflow` `{ id, driver, pending_roles }`, additive `ci` (`pr`, `pr_url`, `head_sha`, `last_summary`, `interval_ms`, `auto_merge`, `merge`) — `null` when phase is not `ci-wait` and no watch state is persisted — and additive `review` (`attempted`, `active`, `verdict`, `report`) — `null` when phase is not `cross-model-review` and no review state is persisted. Existing additive fields include `failure_class`, `next_track`, `phase_started_at`, `run_epoch`, `failure_artifact`. CLI `status` / `GET /v1/status` also attach additive `ticker` (`{ owner: "serve", port }` when coordinator health answers, or `{ owner: "none" }`). Poll-path `run::status` / `from_record` omit `ticker`.

**Plan-review:** adapter starts `agy --print` and `opencode run` once each (cwd = workspace root). Agy uses `--print-timeout` = remaining budget, `--dangerously-skip-permissions`, `--output-format json`. OpenCode uses `--dir` = workspace root, `--format json`, no `--auto`. Review files are source of truth (`agy-review.md` / `opencode-review.md` on the track, copied into `{state_dir}/reviews/`). Join assembles `{workspace}/AI-review.md`. One reviewer may degrade; **both** missing/fail → Stopped. Line endings normalized to `\n`. Override binaries with `COORDINATOR_AGY_BIN` / `COORDINATOR_OPENCODE_BIN` (else Role Binding `command`, else PATH). Optional ignored live smokes: `COORDINATOR_AGY_LIVE=1` and `COORDINATOR_OPENCODE_LIVE=1` (do not reuse `COORDINATOR_REVIEW_LIVE`). The 0011 cross-model gate still uses `opencode run --dir {execution_repo} --format default` with no `--auto`.

**Injected prompt contract:** adapter injects a **per-phase** body (not one shared blurb). Each Grok-bound phase (`plan` / `fold` / `implement` / `advance`) and each plan-review slot names that phase’s skill as an absolute `{workspace|execution}/.agents/skills/<name>/SKILL.md` path (planning skills live above the product git root and are not auto-discovered from `grok_cwd`). `plan`, plan-review, and `implement` include a live-research line (verify pins/APIs against primary sources). Adapter-driven Grok turns complete by **ending the turn** — do **not** run `coordinator outcome write` during that inject (file_wait / hooks still use the CLI). Mid-inject CLI writes that change `state.phase` are ignored when the turn returns (`apply_turn` skips if the live phase drifted from the injected phase).

**`next_track`:** On adapter `advance`, the Planner’s last matching reply line `next_track: <id>` or `next_track: null` is copied into outcome metadata (`null` / `none` / empty clears a stale id; omitting the line leaves `state.next_track` untouched). CLI `--next-track` remains the file_wait path. On `advance` success: valid track dir → auto-start at `plan`; null/empty → Idle (`workflow: backlog clear`); unknown id → Idle (does not fail the completed track). Pause holds auto-start until resume.

**Compact:** capability-gated; timeout/failure **skips** (not a hard gate). Adapter errors surface as `compact: skipped — {reason}` (still no Failure Artifact).

### Failure Artifact + toast

Hard failure (apply `status=failure`, including timeout synthesis and adapter fail) writes `{state_dir}/FAILURE.md` and best-effort shows a Windows toast. **Operator `stop` / `pause` do not notify** (stop is not a Failure Class). Toast errors never fail apply.

| Item | Behavior |
|------|----------|
| Artifact | Atomic markdown: project/track/phase/class/epoch + fenced `last_event` / message + `recommended_action` (advisory — no auto-retry) |
| Toast | `tauri-winrt-notification` 0.8.1, PowerShell AUMID (no installer). Title `Coordinator: {class}`. Disabled with `COORDINATOR_NOTIFY=off` |
| Adapters | `NotifyAdapter` trait: Artifact + Toast + Log + **opt-in Hermes** (HMAC V2 POST of unchanged `NotifyEvent` JSON). Default **off**. Adapter errors never undo `FAILURE.md` or skip toast. |
| Surfaces | `coordinator failure show` prints the markdown; `GET /v1/failure` returns `{path, body}` or **404**. `coordinator notify hermes-test` probes Hermes only (no artifact, no toast). |
| Status | Additive `failure_artifact` path (`null` when the file is absent). Existing `failure_class` / `run_epoch` / `phase_started_at` / `next_track` are also on status JSON |

### Hermes notify (opt-in)

Coordinator is **not** a Telegram client and does not hold a bot token. Hermes Agent (typically on WSL) is operator-owned — this crate does not install, start, or bundle it.

Hard failure may POST `NotifyEvent` JSON to a **loopback** Hermes inbound webhook. Toast + `FAILURE.md` still fire first and still succeed when Hermes is off or the POST fails. Operator `stop` / `pause` never POST.

| Item | Rule |
|------|------|
| Config | `{COORDINATOR_HOME}/config.json` additive `hermes.enabled` (default `false`) + `hermes.webhook_url`. **Do not** store the HMAC secret. |
| Env | `COORDINATOR_HERMES=off` force-disables. `COORDINATOR_HERMES_URL` overrides the URL. `COORDINATOR_HERMES_SECRET` is required to POST. `COORDINATOR_NOTIFY=off` still skips **toast only**. |
| URL | `http://` + literal host in `{127.0.0.1, localhost, ::1, 127.0.0.0/8}` + non-empty path. Docs and examples use **`http://127.0.0.1:8644/...`** (IPv4-deterministic for WSL2). `https://`, non-loopback, empty path, and userinfo are rejected. No redirects off-box. |
| Auth | Hermes generic **HMAC V2**: `X-Webhook-Signature-V2` + `X-Webhook-Timestamp` over `{timestamp}.{body}` (lowercase hex, no `sha256=` prefix). Unsigned POST is forbidden. |
| Idempotency | `X-Request-ID` is **Coordinator’s** key `{project_id}:{run_epoch}:{phase}:{failure_class}` (Hermes caches any stable string for 1 hour). |

```json
{
  "version": 1,
  "hermes": {
    "enabled": true,
    "webhook_url": "http://127.0.0.1:8644/webhooks/coordinator-failure"
  }
}
```

```powershell
$env:COORDINATOR_HERMES_SECRET = "<same as Hermes route secret>"
cargo run -- notify hermes-test
```

Hermes must be reachable from Windows at **`127.0.0.1:8644`**. Recommended route (`~/.hermes/config.yaml` on the Hermes host) — **`deliver_only: true` is mandatory** so a hard fail does not wake an LLM turn:

```yaml
platforms:
  webhook:
    enabled: true
    extra:
      port: 8644
      secret: "<same as COORDINATOR_HERMES_SECRET>"
      routes:
        coordinator-failure:
          secret: "<same>"
          deliver: telegram
          deliver_only: true
          prompt: |
            Coordinator {failure_class}
            project: {project_id}
            track: {track_id}
            phase: {phase}
            {message}
```

Omit `events` (or do not filter on GitHub event types).

`COORDINATOR_NOTIFY=off` is the CI / headless default path. `cargo test` never requires a visible toast (recording adapter).

There are **no skip slots**. `cross-model-review` is a real one-shot gate (track 0011). `ci-wait` is a real phase (track 0010). 0010 still treats predecessor success as the Review Gate — filling this slot makes that hook honest.

### Cross-model review gate

After implement, Coordinator runs a **fresh, read-only CLI exec** (not a Grok Session) in Role Binding order **Codex → Claude → OpenCode**. Defaults ship `model: null` (omit `-m` / `--model`). File slug is the binding `harness` field (`review.codex.md`). Coordinator **does not** write `review.md`.

| Item | Behavior |
|------|----------|
| Spawn cwd | Primary **execution repo**. No execution repo → `permission` (`cross-model: no execution repo`) |
| Codex | `exec -C {exec} -s read-only --ephemeral -o {tmp} --output-schema {shipped}` — **no** `--add-dir` (writable), **no** `exec review` |
| Claude | `-p` + `--permission-mode dontAsk` + Read/Glob/Grep only — **no `--bare`**. `--add-dir {workspace_root}` is tool access only |
| OpenCode | `run --dir {exec} --format default` — **no `--auto`**. Hang backstop = remaining phase budget |
| Windows | `resolve_command` (PATH + `PATHEXT`). Never spawn `.ps1`. `.cmd`/`.bat` via `cmd.exe /C`. Env: `NO_COLOR=1` |
| Budget | Do not start a tier if remaining **&lt; 60s** → `timeout`. Process timeout = remaining budget (not the 30s `gh` cap) |
| Verdict | `## Verdict: PASS \| PASS WITH DEFERRED P3 \| FAIL` (or JSON `verdict`). Findings P0/P1/P2/critical/high/medium override PASS. Unparseable falls through |
| Fallback | Exhaustion / missing binary / auth / crash / unparseable → next tier. **FAIL / &gt;low = `difficulty`, no fallback** |
| Empty chain | All exhausted → `model_exhaustion`. All missing/auth → `permission`. Else `harness_crash` |
| Pause / stop | Pause **finishes** this phase then holds at `ci-wait` Paused. Stop aborts; does not apply success; not a Failure Class |
| Reports | `{state_dir}/reviews/cross-model-{slug}.md` and `{track_dir}/review.{slug}.md`. Stub last_event: `cross-model: stub (no review)` |
| Env | `COORDINATOR_CODEX_BIN`, `COORDINATOR_CLAUDE_BIN`, `COORDINATOR_OPENCODE_BIN` |

Default `cargo test` uses a scripted `ReviewBackend` and never needs Codex/Claude/OpenCode auth. Optional live smoke: `$env:COORDINATOR_REVIEW_LIVE='1'; cargo test review_live -- --ignored --nocapture`.

### Token-idle CI wait + auto squash-merge

After a successful Review Gate, Coordinator watches CI **outside** any model Session (ADR-0011). The serve/`wait` loop already wakes ~every 500ms; `ci-wait` does **not** sleep and does **not** spawn `gh` on every wake.

| Item | Behavior |
|------|----------|
| Tool | `gh` CLI from the **execution repo** cwd (`COORDINATOR_GH_BIN` or `gh` / `gh.exe`). Env: `GH_PROMPT_DISABLED=1`, `NO_COLOR=1`. No `octocrab`, no webhooks, no `gh pr checks --watch` |
| Target | Persisted `RunState.ci` → implement `metadata.pr_number`/`pr_url` → `gh pr view` → `gh pr list --head` → default-branch `HEAD` sha (`gh run list`). Feature branch with no PR stays pending (`ci-wait: waiting for PR`) until the 3600s phase timeout |
| Checks | `gh pr checks {n} --json bucket,name,state` (all checks, **not** `--required`). Draft PR stays pending (`ci-wait: waiting (draft PR)`). Already `MERGED` is green. Any `fail`/`cancel` → `ci_failed`. Empty check list is green |
| Default branch | `gh run list --commit {sha}`. Empty runs for &lt; 2 min stay pending; then treat as “no CI configured” (green, **no merge**) |
| Interval | 15s (0–2 min) → 30s (2–10 min) → 60s (≥10 min), cap 120s. Reset to 15s when the check/run set changes. Tests: `COORDINATOR_CI_POLL_MS` is a **fixed** interval |
| Merge | Green PR + Project `auto_merge=true` (default) → `gh pr merge {n} --squash`. **No** `--admin`, **no** `--delete-branch`. `auto_merge=false` succeeds with `ci-wait: green; merge skipped (auto_merge=false)`. Operator `stop` never merges. Pause **finishes** `ci-wait` (and `cross-model-review`) then stays Paused at the successor |
| Fail | `failure_class=ci_failed` → Stopped + Failure Artifact + toast. No auto-retry of the workflow |
| Process cap | Each `gh`/`git` spawn: 30s then kill (transient, `ci-wait: gh timed out`) |

`project add` / `project set` accept `--auto-merge true|false` (omit = default on / leave unchanged). HTTP `POST /v1/projects` and `/v1/projects/set` take optional `auto_merge`. Old `registry.json` records without the field load as **true**.

Default `cargo test` uses a scripted `CiBackend` and never needs `gh` auth. Optional live smoke: `$env:COORDINATOR_GH_LIVE='1'; cargo test ci_live -- --ignored --nocapture`.

### Grok harness adapter (ACP)

Long-lived **Grok Build** sessions use `grok agent stdio` (JSON-RPC 2.0, line-delimited). This is the primary drive surface (ADR-0001). Headless `grok -p` is not the pool model. Completion is still the **Phase Outcome File** (`source: "adapter"` when the adapter writes it). ConPTY scraping is not the contract.

| Item | Behavior |
|------|----------|
| Spawn | `grok agent [-m {model}] stdio` (`-m` only when the phase Role Binding has a non-empty `model`) — `initialize` → `authenticate` (`methodId`, `_meta.headless`) → `session/new` `{ cwd, mcpServers: [] }` |
| Cwd | Layout-resolved `execution_repo` if set, else `workspace_root` |
| Prompt | `session/prompt` with `sessionId` + content-block array; `session/update` chunks collected |
| Compact | Inject `/compact` via `session/prompt` (not a separate RPC). If unsupported: `supports_compact=false` and skip (ADR-0021), do not fail the run |
| Auth | Operator `grok login` (`cached_token`) or `XAI_API_KEY` (`xai.api_key`). Coordinator does **not** own OAuth |
| Stop vs shutdown | `coordinator stop` aborts the phase and **leaves the Grok process alive** for attach. `harness grok shutdown` is teardown: in-process pool, then a quick holder `Shutdown` RPC, then `taskkill /F /PID` on persist `pid` then `holder_pid`. Persist is always written `alive: false`; returned status matches the file. `taskkill` “not found” is success. Missing `taskkill` is not a shutdown error. |
| Pause | New injects are refused while Paused; the child stays up |
| Pool | One Grok ACP session per `project_id`. CLI `start`, HTTP `POST /v1/harness/grok/start`, and adapter ticks from `run` / `wait` / `serve` detach a localhost holder so a later `prompt` does not pin the poll loop. In-process spawn is for tests / `insert_test_session`. |
| Persist | `{state_dir}/harness-grok.json` (session id / pid / holder pid / control addr / alive) — not a transcript |

Optional project hooks (`Stop`, `SessionEnd`, `PreCompact`, `PostCompact`) may also write `outcomes/current.json` (`source: file`). Project hooks need a one-time `grok` `/hooks-trust`. The adapter-written outcome is the automation path.

Live tests are **not** required for CI:

```powershell
$env:COORDINATOR_GROK_LIVE = "1"
cargo test grok_live -- --ignored --nocapture
```

`COORDINATOR_GROK_BIN` overrides the `grok` executable (absolute path) for the **grok** harness. Adapter ticks for plan/fold/implement/advance resolve the **phase** Role Binding (`plan` → `planner`, `implement` → `implementor`, `fold`/`advance` inherit `planner` unless optional `fold`/`next` keys are present with a non-empty `command`). CLI / HTTP `harness grok start` still uses implementor-then-planner (no phase context). Default resolution walks `PATH` + Windows `PATHEXT` (no `which` crate). A non-empty binding `model` is passed as `grok agent -m {model} stdio` on **new** session start only; a reused live session keeps its model. Non-Grok `harness` values have no long-lived adapter yet and fail `permission`.

Default `role_bindings` in `config.json`:

```json
{
  "planner": { "harness": "grok", "command": "grok", "model": null },
  "implementor": { "harness": "grok", "command": "grok", "model": null },
  "plan_reviewer_agy": { "harness": "antigravity", "command": "agy", "model": null },
  "plan_reviewer_opencode": { "harness": "opencode", "command": "opencode", "model": null }
}
```

Optional keys `fold` and `next` are recognized when the operator writes them; they are **not** inserted into old configs. Saved configs that only have `planner` + `implementor` gain the reviewer keys on load.

```powershell
# Smoke (temp home + fake project)
$env:COORDINATOR_HOME = "$env:TEMP\coordinator-cp-smoke"
New-Item -ItemType Directory -Force -Path $env:COORDINATOR_HOME | Out-Null
$proj = Join-Path $env:TEMP "coordinator-fake-project"
New-Item -ItemType Directory -Force -Path $proj | Out-Null

cargo run -- project add $proj
cargo run -- project list
cargo run -- run --project $proj --track 0005 --driver stub
# expect Idle (backlog clear) — stub walks the full graph; no separate `wait`
cargo run -- status --project $proj

# file-wait / serve-owned / already-running: `wait` still attaches
# cargo run -- run --project $proj --track 0005 --driver stub --detach
# cargo run -- wait --project $proj --timeout-secs 30

# file-wait single phase
cargo run -- run --project $proj --track 0005 --driver file_wait --detach
cargo run -- outcome write --project $proj --phase plan --status success
cargo run -- status --project $proj
# expect Running / plan-review

# leftover stub timeout (tests / old state): COORDINATOR_STUB_PHASE_TIMEOUT_SECS
# canonical uniform override for tests: COORDINATOR_PHASE_TIMEOUT_SECS

# HTTP (separate terminal) — always-on ticker
cargo run -- serve --port 7420
# cargo run -- serve --check
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
    [--auto-merge true|false]
    [--phase-timeout PHASE=SECS]...
coordinator project list
coordinator project show [--project <path|id>]
coordinator project set [--project …]
    [--profile …] [--execution-repo …] [--conductor-dir …] [--state-dir …]
    [--display-name …] [--execution-repos-json <json>] [--execution-repo-name …]
    [--auto-merge true|false]
    [--phase-timeout PHASE=SECS]... [--clear-phase-timeout PHASE]...
    [--clear-phase-timeouts]
coordinator project scan [--root <path>]... [--add] [--dry-run] [--save-root]
coordinator status [--project <path|id>]
coordinator run [--project <path|id>] [--track <id>] [--driver adapter|file_wait|stub] [--detach] [--timeout-secs N] [--serve-port N]
coordinator pause [--project <path|id>]
coordinator resume [--project <path|id>]
coordinator stop [--project <path|id>]
coordinator outcome write --phase <id> --status success|failure
    [--failure-class <enum>] [--message <text>] [--project …]
    [--next-track <id>] [--source cli]
coordinator outcome show [--project …]
coordinator failure show [--project …]
coordinator notify hermes-test [--project …]
coordinator wait [--project …] [--timeout-secs N]
coordinator harness grok start [--project …]
coordinator harness grok prompt --text <…> | --file <path> [--project …]
coordinator harness grok compact [--project …]
coordinator harness grok status [--project …]
coordinator harness grok shutdown [--project …]
coordinator serve [--port <u16>] [--check]   # default 7420, 127.0.0.1 only
```

HTTP: `POST/GET /v1/projects` (layout fields + optional `auto_merge`), `POST /v1/projects/set`, `POST /v1/projects/scan`, plus run/status/outcome routes (`POST /v1/run` accepts optional `driver`), `GET /v1/failure` (200 `{path, body}` or 404), and `/v1/harness/grok/{start,prompt,compact,status,shutdown}`. Status JSON includes additive `layout_profile`, `execution_repo`, `conductor_dir` (resolved), `workflow` (`id`, `driver`, `pending_roles`), `ci` (watch object or `null`), `failure_artifact` (path or `null`), optional `harness.grok` (`alive`, `session_id`, `cwd`, `supports_compact`) when a session exists, and additive `ticker` (`owner` `serve` + `port`, or `owner` `none`).

### Phase Outcome schema v1

```json
{
  "version": 1,
  "phase": "plan",
  "status": "success",
  "failure_class": null,
  "message": "optional human/agent note",
  "written_at": "2026-08-12T12:00:00Z",
  "source": "cli",
  "metadata": {
    "next_track": null,
    "role": null,
    "pr_number": null,
    "pr_url": null
  }
}
```

| Field | Rules |
|-------|--------|
| `version` | Must be `1` |
| `phase` | Non-empty; must match current run-state phase to apply |
| `status` | `success` \| `failure` |
| `failure_class` | Required when `failure`; must be null on `success`. Values: `permission`, `model_exhaustion`, `difficulty`, `harness_crash`, `timeout`, `ci_failed` |
| `source` | `file` \| `http` \| `cli` \| `timeout` \| `test` \| `adapter` |
| `metadata.next_track` | Optional; copied to status on success |
| `metadata.role` | `planner` / `implementor` / `plan_reviewer_agy` / `plan_reviewer_opencode`; unknown ignored. Optional Role Binding keys `fold` / `next` rebind those phases — they are not plan-review consume slugs. |
| `metadata.pr_number` / `pr_url` | Optional implement hint; copied onto `RunState.ci` when present |
| `run_epoch` | Optional; when present must match run-state epoch |

**Apply (single path):** leftover `stub:*` success → Idle / `stub:completed` (Paused stays Paused). Canonical success → **successor phase, stay Running** (Paused stays Paused). Canonical failure → Stopped and **keeps the failed phase id**. Operator `stop` still sets `stub:stopped`. Idle/Stopped and phase mismatch reject for CLI/HTTP. After apply: history best-effort → `current.applied.json` → remove `current.json` → hash on run-state.

### `run` / `wait` exit codes

| Code | Meaning |
|------|---------|
| **0** | An outcome was **applied** (success **or** failure, including synthesized **phase** timeout) |
| **2** | Wait budget (`--timeout-secs`) expired **without** an applied outcome. The run is unchanged; Grok is not killed; no `FAILURE.md`. |
| **1** (or other) | Invalid args, unknown project, or other control-plane error |

`wait --timeout-secs` (and optional `run --timeout-secs`) is a **CLI poll budget**. Bare `run` ticks with **no** poll deadline until Idle/Stopped. Default `run` (no `--detach`) skips the wait loop when coordinator `/health` answers on the **lease port or 7420** (stderr: serve owns the ticker). `run --serve-port N` probes N only. `--detach` is the explicit write-only hatch (still conflicts with `--timeout-secs` and `--serve-port`). Custom-port serve no longer requires `--detach` when the lease exists. It is not the phase wall clock (`failure_class=timeout` + Stopped + artifact + abort), not the ACP `session/prompt` timeout (recycle), and not the progress stall (first fire: recycle + stay Running; second: `watchdog: stall` until the wall clock). Adapter inject from `run` / `wait` / `serve` starts the detached holder without blocking the poll loop, so the wait budget can expire while a prompt is still in flight **without aborting it**. `run --timeout-secs 0` is rejected (omit the flag for unlimited). `wait --timeout-secs 0` still expires immediately.

Scripts that want “success only” must inspect `status` / `failure_class` after exit 0 (e.g. `coordinator status`).

Stop aborts advancement with **no merge**; `last_event` records **sessions left for attach**. After Stopped, further outcomes are ignored until a new `run`. `harness grok shutdown` is the explicit teardown (kills the holder / persist pid, including a child that `wait` used to own in-process).

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
  phase = "plan"
  status = "success"
  failure_class = $null
  message = "hook Stop"
  written_at = (Get-Date).ToUniversalTime().ToString("o")
  source = "file"
} | ConvertTo-Json | Set-Content -Path $tmp -Encoding utf8
Move-Item -Force $tmp $dst
```

**Grok / other CLI hooks:** same file contract; prefer `source=file` for hooks. The Grok ACP adapter writes `source=adapter` after a successful (or failed) prompt when a phase is Running. Hooks need `/hooks-trust` if used.

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

## Status Surface (track 0014)

Live **Dioxus Desktop** window (WebView2) bound to in-process Control Plane `api::*`. CLI remains the automation entry.

```powershell
# Requires Microsoft Edge WebView2 Evergreen:
# https://developer.microsoft.com/microsoft-edge/webview2/
coordinator ui
# or from a clone: cargo run -- ui
# optional: coordinator ui --port 7420
```

- Feature `ui` **is** default. Default `cargo test` / clippy compile the window crate (do **not** launch WebView2). CI also runs `cargo test --no-default-features --lib` so the compile-out hint path does not bitrot.
- If WebView2 is missing, `coordinator ui` prints an Evergreen install hint and exits non-zero (no panic, no LAN/browser fallback).
- Bind stays `127.0.0.1` only. The window attaches via **health JSON / lease**: if the requested port (or a healthy lease when the requested port is default 7420) is coordinator, it does **not** start a second serve. Occupied by a non-coordinator process is a warning, not “serve already up.” Window still uses in-process `api::*`. Header `.stat` shows `Ticker serve :N` or `Ticker none`.
- Pause all / Stop selected / Resume match ADR-0024. Stop copy includes **sessions left for attach**. Stop never writes `FAILURE.md` and never calls `harness grok shutdown`.
- Add project is an explicit absolute path (never `project scan --add` of `C:\dev`).

Built with `--no-default-features`, `coordinator ui` is still listed in `--help` but errors with a rebuild hint (`cargo install --path . --locked`).

## Status Surface mock (track 0003)

**Visual contract** for the live window above (static HTML/CSS). Do not invent a new visual language.

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

This mock remains the **visual reference** after 0014. Live start/resume is `coordinator ui` plus the Control Plane CLI.

## Status

Tracks **0001** (crate + CI), **0002** (Impeccable + design context), **0003** (Status Surface mock + module map), **0004** (Control Plane skeleton), **0005** (Phase Outcome File + apply + wait/timeout), **0006** (layout profiles + scan), **0007** (Grok ACP adapter + session pool), **0008** (canonical workflow runner), **0009** (stop/pause + Failure Artifact + toast), **0010** (token-idle `ci-wait` + auto squash-merge), **0011** (cross-model review), **0012** (Orca dogfood), **0013** (wait + session teardown), **0014** (live Dioxus Status Surface), **0015** (Hermes notify adapter), **0016** (coordinated multi-sibling dogfood), **0026** (harness progress watchdog — detect + surface).