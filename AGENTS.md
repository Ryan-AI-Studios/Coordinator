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

Control Plane entrypoints: `project add|list|show|set|scan`, `run|status|pause|resume|stop`, `outcome write|show`, `wait`, `serve` (127.0.0.1:7420). Layout/profile tests: `cargo test layout`, `cargo test scan`, `cargo test registry`. Phase Outcome tests: `cargo test outcome`. Env overrides: `COORDINATOR_HOME`, `COORDINATOR_STATE_DIR`, `COORDINATOR_STUB_PHASE_TIMEOUT_SECS`, `COORDINATOR_OUTCOME_POLL_MS`. See product README for Layout Profiles and the Phase Outcome contract.

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
