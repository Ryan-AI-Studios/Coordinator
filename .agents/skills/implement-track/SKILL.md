---
name: implement-track
description: >
  Implement one assigned Coordinator conductor track end-to-end in C:\dev\coordinator\coordinator.
  Load with product onboarding first. Subagents implement/review loop; codex-review cross-model
  gate until nothing above low remains; fresh clean cross-model final gate. Full CI is a blocking
  gate. Then always publish: branch -> PR -> wait CI green -> squash-merge -> prune. Updates
  conductor.md, review.md, and deferred.md. Use when the user says implement track, execute track,
  /implement-track, or go on track N.
---

# Implement a conductor track (Coordinator)

identity{
  product: "Coordinator"
  product_path: "C:\\dev\\coordinator\\coordinator"
  planning_root: "C:\\dev\\coordinator"
  conductor_root: "C:\\dev\\coordinator\\conductor"
  load_with: "onboarding (product .agents/skills/onboarding)"
  source_of_truth: "conductor.md + <track>/spec.md + plan.md under C:\\dev\\coordinator\\conductor"
  do_not:
    - clear gate with open critical/high/medium findings
    - commit planning docs into the product repo
    - invent product decisions
    - deviate from track plan/spec
    - mark Completed without review.md + conductor + deferred updates for unfinished lows
  must:
    - follow plan.md phases and spec DoD as written
    - update deferred.md at finish with every residual low not implemented
    - run ledgerful + ai-brains from PRODUCT cwd when inited
    - always publish: feature branch -> PR -> wait CI green -> squash-merge -> prune
}

## When this skill applies

- `Implement track 0001` / `execute track 0001` / `/implement-track 0001` / `go on 0028`

**Not this skill:** `/plan-track`, `/review-track`, `/fold-in` alone.

If track is not **Ready** or **In progress**, stop and report.

## Paths

```
PRODUCT=C:\dev\coordinator\coordinator
CONDUCTOR=C:\dev\coordinator\conductor
REGISTRY=C:\dev\coordinator\conductor\conductor.md
DEFERRED=C:\dev\coordinator\conductor\deferred.md
TRACK=C:\dev\coordinator\conductor\<####-Name>
```

## Severity

| Severity | Gate |
|----------|------|
| critical / high / medium | **Block** completion. Must be verified fixed. |
| low | Fix if easy; else **APPEND** to `DEFERRED` |

Regression caused by this work is always **high**. Placeholder faking DoD is **blocking**.

## Standing orders

1. **Knowledge is stale** — re-verify APIs, harnesses, crates live.
2. **Plan fidelity** — only spec + plan; stop if blocked.
3. **Deferred at finish (mandatory)** — every residual low → `deferred.md`.
4. **ledgerful + ai-brains** from **`PRODUCT` cwd only** (`C:\dev\coordinator\coordinator`).
5. **Docs-out-of-product** — never git-add planning paths (`conductor/`, `docs/adr/`, etc.).
6. **Always publish** — `/implement-track` / go / execute is approval for push → PR → wait CI green → squash-merge → prune.
7. **Mission** — advance Coordinator product (or named dogfood enabler).

## Loop (end-to-end)

```
0  orient + deferred scan + tools preflight + re-verify pins
1  mark In progress; branch off main
2  implementation brief from plan (orchestrator)
3  TDD implement (red → green) + targeted checks
4  internal review vs DoD
5  fix → re-review until internal clean of >low (lows dispositioned)
6  CROSS-MODEL: codex-review (fresh)
7  FINAL GATE: clean cross-model with no open >low
8  full CI check + write review.md + conductor Completed + deferred update
9  PUBLISH (always): push branch → PR → wait CI green → squash-merge → prune
10 final report to owner
```

## Phase 0 — Orient

```powershell
cd C:\dev\coordinator\coordinator
ai-brains preflight --summary
ai-brains sync query "<track topic>"
ledgerful doctor --json
ledgerful ledger status --compact
ledgerful change-context --json
```

If tools not inited: continue; note in `review.md`. **Do not init in planning root.**

## Phase 1 — Branch

```powershell
cd C:\dev\coordinator\coordinator
git fetch origin
git checkout main
git pull --ff-only
git checkout -b track/<####-short-name>
```

## Phase 2–3 — Implement + internal review

- Edits only under `PRODUCT` (or named execution path).
- Orchestrator owns `conductor.md` / `deferred.md` / `review.md`.
- Implementer lists unfinished lows for deferred append.

Targeted checks:

```powershell
cd C:\dev\coordinator\coordinator
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# or track-specific commands from spec §8
```

## Phase 4 — Cross-model gate

Load **`codex-review`**. Any validated finding **above low** → fix → internal re-review → **fresh**
codex-review. Final gate must be a **new** clean pass.

## Phase 5 — Governance finalize

1. `TRACK\review.md` — DoD matrix, evidence, codex verdict, deferred lows list
2. `conductor.md` → **Completed**
3. **`deferred.md` mandatory append** of residual lows; resolve landed rows
4. Light-update `planner.md` if next-work snapshot changed
5. Optional: `ai-brains pin "DECISION: …"` from product cwd

## Phase 6 — Publish (always — this is the finish line)

`/implement-track` / **go** / **execute** is standing owner approval for this **entire** sequence.

```powershell
cd C:\dev\coordinator\coordinator
git fetch --all --prune
git push -u origin HEAD
gh pr create --title "track(####): <short objective>" --body "..."
gh run list --branch track/<####-short-name>
gh run watch <id> --exit-status    # wait until every CI job is green
gh pr merge --squash --delete-branch
git fetch --all --prune
git checkout main
git reset --hard origin/main
git branch -d track/<####-short-name>
git remote prune origin
```

## Anti-patterns

- Implementing without reading spec/plan
- Freestyle scope / next-track sneak-in
- Finishing without deferred.md append
- Skipping codex or reusing stale codex as final gate
- Busy-polling CI (use `gh run watch`)
- Committing planning tree into product git
- Stopping at local Completed without PR / CI wait / squash-merge / prune
- Force-pushing or pushing directly to main

## Relation to other skills

| Skill | Role |
|-------|------|
| **onboarding** (product) | Orientation |
| **implement-track** (this) | Execute |
| **codex-review** | Cross-model completion audit |
| **plan-track** / **review-track** / **fold-in** (planning tree) | Write/audit/fold plans |
| **ledgerful** / **ai-brains** | Intelligence (product cwd) |
