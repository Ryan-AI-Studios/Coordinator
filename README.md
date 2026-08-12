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
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo run
```

CI (GitHub Actions) runs `fmt --check`, `clippy -D warnings`, and `test` on `windows-latest`.

## Tools (product cwd only)

```powershell
ai-brains preflight --summary
ledgerful doctor --json
ledgerful change-context --json
```

Do **not** initialize ledgerful or ai-brains in the planning parent folder.

## Status

Bootstrap track **0001**: minimal binary crate + CI. Control plane, adapters, and Status Surface land in later tracks.
