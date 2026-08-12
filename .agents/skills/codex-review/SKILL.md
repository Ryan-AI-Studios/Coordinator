---
name: codex-review
description: >
  Cross-model track completion audit for Coordinator after implementation and internal review
  fixes. Verifies every DoD item, finds placeholders, incomplete wiring, regressions, and weak
  evidence. Read-only. Orchestrator fixes and re-invokes until nothing above low remains; final
  gate requires a fresh clean pass. Use when implement skill reaches the codex gate or user asks
  for codex/cross-model track review.
---

# Track Completion Review (Cross-Model) — Coordinator

Read-only audit. The **orchestrator** (implement skill) selects the track, implements, fixes,
runs gates, manages `deferred.md`, and decides completion.

## Handoff (required)

```text
TRACK: <####-Name or absolute track directory under C:\dev\coordinator\conductor>
```

Optional:

```text
REPOS: C:\dev\coordinator\coordinator
SCOPE: <base/commit range/working tree/PR>
IMPLEMENTED: <brief summary>
KNOWN GATES: <cargo/CI results observed>
FOCUS: <extra risks>
```

```text
ROOT=C:\dev\coordinator
DEFERRED=C:\dev\coordinator\conductor\deferred.md
PRODUCT=C:\dev\coordinator\coordinator
```

Raw output: `C:\dev\coordinator\conductor\<track>\review.codex.md` (or `review.claude.md`).  
Orchestrator writes canonical `review.md`.

## Rules

* **Never** modify product files, governance, Git state, or `deferred.md`.  
* Read every requirement, plan phase, risk, and DoD item.  
* Do not claim a command passed unless observed (or honestly “reported by orchestrator”).  
* No invented or style-only findings.  
* Do not overturn locked ADRs; flag product questions for the owner.  
* Planning markdown must **not** have been committed under product.

## Product context

* **Mission:** multi-harness Windows track orchestrator; long-lived Sessions; Completion Signals.  
* **Stack:** Rust-first; local Control Plane; Role Bindings configurable.  
* **Tools:** ledgerful + ai-brains belong in product tree when used.  
* **v1 non-goals:** internet control plane, headless-default, Hermes hard-dep, free-form DAG primary.

## Audit sections

1. Requirements / DoD / plan fidelity matrix  
2. Completeness sweep (TODO/FIXME/stub/placeholder/fake success)  
3. Wiring (end-to-end for claimed behavior: e.g. phase → outcome file → next phase)  
4. Correctness / regression / autonomy safety (stop/pause, failure classes)  
5. Tests / evidence honesty  
6. Docs / governance (no planning-in-product)

## Severity map

| Reviewer | Implement | Deferrable? |
|----------|-----------|-------------|
| P0 | critical | No |
| P1 | high | No |
| P2 | medium | No |
| P3 | low | Yes if difficult / non-DoD |

## Output template

```text
# Track Completion Audit — <TRACK>
## Verdict: PASS | PASS WITH DEFERRED P3 | FAIL
## Scope Reviewed
## Requirement and DoD Matrix
## Findings
## Completeness Sweep
## Wiring and Regression Review
## Verification Evidence
## Deferred Candidates
## Completion Decision
```

## Reviewers (cross-model order)

1. **Codex Primary**  
2. **Claude Secondary** when Codex unavailable  
3. Optional OpenCode tertiary  

### Codex Primary (Windows)

```powershell
$TrackDir = "C:\dev\coordinator\conductor\<####-Name>"
$PrimaryRepo = "C:\dev\coordinator\coordinator"
$Prompt = @"
You are the independent completion reviewer for Coordinator track <TRACK>.
Track directory: $TrackDir
Product repo: $PrimaryRepo
Planning root (read-only): C:\dev\coordinator
READ-ONLY; never modify files or Git.
Audit every DoD against implementation. Flag planning docs committed into product.
"@

codex exec -C $PrimaryRepo -s read-only `
  -m gpt-5.4 -c 'model_reasoning_effort="high"' `
  --add-dir "C:\dev\coordinator" --ephemeral `
  -o "$TrackDir\review.codex.md" $Prompt
```

Fallback chain (when exhausted): Claude Code → OpenCode (per product Role Binding defaults).

### Ledgerful under pure RO

Optional `ledgerful ledger status --json` / `change-context --json` if `.ledgerful` exists.  
**Skip** `doctor` / `index` / `scan --impact` / `verify` inside pure RO review.

## Orchestrator loop (after return)

1. Classify findings  
2. Fix validated P0–P2 and easy P3  
3. Record qualifying difficult P3/lows in `deferred.md`  
4. Re-run internal review as needed  
5. **Re-invoke this skill** for a **fresh** pass  
6. Final gate = last cross-model clean of >low  
