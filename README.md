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
cargo run
```

Same gate as CI and ledgerful verify: `fmt --check`, `clippy -D warnings`, and `test` on `windows-latest`.

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

Tracks **0001** (crate + CI), **0002** (Impeccable + design context), **0003** (Status Surface mock + module map). Control plane lands in **0004+**.
