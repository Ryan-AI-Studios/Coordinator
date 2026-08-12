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
| [`DESIGN.md`](./DESIGN.md) | Visual seed (“Local Ops Console”); re-document after mockup code |
| [`.agents/skills/impeccable/`](./.agents/skills/impeccable/) | Tracked skill payload (Codex / shared agents) |

```powershell
cd C:\dev\coordinator\coordinator
npx --yes impeccable install --scope=project --providers=codex,grok,opencode,claude
npx --yes impeccable check
# In harness: /impeccable init  (or refresh PRODUCT/DESIGN via document)
# Grok: trust project hooks once (/hooks-trust). Codex: approve /hooks after updates.
```

Re-run install/init here if the Status Surface UI subtree moves (ADR-0028). Do **not** use global-only install as the design SoT.

## Status Surface demo (pre-0003)

Early **stack-agnostic** HTML shell for operator multi-project layout (not wired to a Control Plane). Starting point for track **0003**.

| Path | Role |
|------|------|
| [`mock/status-surface.html`](./mock/status-surface.html) | Demo UI: **4 concurrent projects**, pause / CI-wait / hard-fail states |
| [`scripts/start-impeccable-live.ps1`](./scripts/start-impeccable-live.ps1) | After reboot: start static page + Impeccable live inject |
| [`.impeccable/live/config.json`](./.impeccable/live/config.json) | Live inject target (`mock/status-surface.html`) |

```powershell
cd C:\dev\coordinator\coordinator
pwsh .\scripts\start-impeccable-live.ps1
# opens http://127.0.0.1:5500/mock/status-surface.html
# optional (agent session): node .agents/skills/impeccable/scripts/live-poll.mjs
```

This is a **layout demo**, not product orchestration. Real start/resume of projects is Control Plane work (**0004+**).

## Status

Tracks **0001** (crate + CI) and **0002** (Impeccable + design context). Demo mock under `mock/` feeds **0003**; control plane lands in **0004+**.
