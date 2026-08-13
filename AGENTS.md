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
cargo run -- --help
```

Ledgerful verify steps (when configured) must match these real cargo commands.

Control Plane entrypoints: `project add|list|show|set|scan` (`--auto-merge true|false`), `run|status|pause|resume|stop` (`run --driver adapter|file_wait|stub`), `outcome write|show`, `failure show`, `wait`, `harness grok start|prompt|compact|status|shutdown`, `serve` (127.0.0.1:7420). Layout/profile tests: `cargo test layout`, `cargo test scan`, `cargo test registry`. Phase Outcome tests: `cargo test outcome`. Harness tests: `cargo test harness`. Workflow tests: `cargo test workflow`. Notify tests: `cargo test notify`. CI-wait tests: `cargo test ci`. Cross-model review tests: `cargo test review`. Env overrides: `COORDINATOR_HOME`, `COORDINATOR_STATE_DIR`, `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`, `COORDINATOR_PHASE_TIMEOUT_SECS`, `COORDINATOR_WORKFLOW_DRIVER`, `COORDINATOR_OUTCOME_POLL_MS`, `COORDINATOR_NOTIFY` (`off` disables Windows toasts), `COORDINATOR_GROK_BIN`, `COORDINATOR_GROK_LIVE`, `COORDINATOR_GH_BIN`, `COORDINATOR_CI_POLL_MS` (fixed ci-wait interval), `COORDINATOR_GH_LIVE`, `COORDINATOR_CODEX_BIN`, `COORDINATOR_CLAUDE_BIN`, `COORDINATOR_OPENCODE_BIN`, `COORDINATOR_REVIEW_LIVE`. Live Grok smoke (ignored by default): `$env:COORDINATOR_GROK_LIVE='1'; cargo test grok_live -- --ignored --nocapture`. Live `gh` smoke (ignored): `$env:COORDINATOR_GH_LIVE='1'; cargo test ci_live -- --ignored --nocapture`. Live review smoke (ignored): `$env:COORDINATOR_REVIEW_LIVE='1'; cargo test review_live -- --ignored --nocapture`. See product README for Layout Profiles, Phase Outcome, the Grok ACP adapter, the canonical workflow, Failure Artifact / toast, token-idle CI + auto-merge, and the cross-model review gate.

## Code style

- Rust edition and formatting as set by `rustfmt` / project `Cargo.toml`  
- Prefer small, testable modules; deep modules over sprawling glue  
- No secrets in git; OAuth stays with each harness  

## Agent entry points

| Intent | Skill |
|--------|--------|
| Orient | `.agents/skills/onboarding` |
| Implement track | `.agents/skills/implement` |
| Cross-model gate | `.agents/skills/codex-review` |
| Plan only | `C:\dev\coordinator\.agents\skills\plan` |

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

**Scan footgun:** never `project scan --root C:\dev --add` — `C:\dev` has many conductor markers (Orca, coordinator, coordinated, …). Scan a single workspace (`--root C:\dev\Orca`) or add one project at a time.

**Wait vs phase timeout vs shutdown:** `wait --timeout-secs` is a CLI poll budget (exit **2**, run unchanged, Grok stays up). Phase wall clock is `failure_class=timeout` + Stopped + `FAILURE.md`. `harness grok shutdown` kills the holder / persist pid (pid-kill fallback) and writes `alive: false`. Operator `stop` does not kill sessions.  
