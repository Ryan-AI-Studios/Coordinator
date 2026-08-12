---
name: onboarding
description: >
  Load at session start for Coordinator product implementation in C:\dev\coordinator\coordinator,
  before execute/implement work. Fresh-implementor orientation: docs-vs-product split, conductor
  tracks (outside this repo), tools cwd (ledgerful + ai-brains init here), owner/agent roles, PR/CI.
  Use when the user says onboarding, implement track, execute track, /onboarding, or starts product
  code work here.
---

# Coordinator — Product Implementor Onboarding

identity{
  product: "Coordinator"
  product_repo_name: "coordinator"
  product_path: "C:\\dev\\coordinator\\coordinator"
  planning_root: "C:\\dev\\coordinator"
  conductor_root: "C:\\dev\\coordinator\\conductor"
  os: "Windows"
  stack: "Rust-first (Control Plane, harness adapters, optional Status Surface later)"
  rule: "NEVER commit planning docs, ADRs, conductor tracks, or SHARED-UNDERSTANDING into this repo"
}

## What you are shipping

**Coordinator** drives long-lived AI CLI harnesses through conductor-track workflows on the same
Windows machine: phase chaining on Completion Signals, full autonomy after start (Stop/Pause),
Planner-chosen next tracks, token-idle CI wait, configurable Role Bindings and Layout Profiles.

One-sentence SoT: `C:\dev\coordinator\SHARED-UNDERSTANDING.md`.

## Hard workspace split (load-bearing)

| Path | Role | Git / ships? |
|------|------|----------------|
| `C:\dev\coordinator\coordinator\` | **This repo** — product code | **Yes** |
| `C:\dev\coordinator\` (except product) | Planning, ADRs, research | **No** |
| `C:\dev\coordinator\conductor\` | Track registry + spec/plan/review | **No** |

**Forbidden in product commits:** `conductor/`, ADRs, grill notes, planner handoff as product source.

You **read** governance from the planning tree; you **edit product** only under this path
(unless a track is planning-only — rare).

## Authority order (implement sessions)

1. User / run prompt (e.g. “Implement track 0001”)  
2. `C:\dev\coordinator\SHARED-UNDERSTANDING.md`  
3. Active track `spec.md` then `plan.md` under `conductor\<track>\`  
4. `conductor\conductor.md`  
5. `conductor\deferred.md`  
6. `docs\adr\*.md`  
7. `planner.md` + planning skill `C:\dev\coordinator\.agents\skills\onboarding`  
8. **This skill** + product **`AGENTS.md`**  
9. External docs / web (training data is stale)

## Session start (do this first)

```
session_start:
  1. Read C:\dev\coordinator\planner.md
  2. Read C:\dev\coordinator\SHARED-UNDERSTANDING.md
  3. Read C:\dev\coordinator\conductor\conductor.md
  4. Read C:\dev\coordinator\conductor\deferred.md
  5. Open assigned track: conductor\<####-Name>\{spec.md,plan.md}
  6. Tools (when inited): ai-brains + ledgerful — ALWAYS cwd = this product root
  7. If execute/implement: load .agents/skills/implement/SKILL.md next
```

## Tools (product cwd only — always)

**Init here once (owner/agent when repo is ready):**

```powershell
cd C:\dev\coordinator\coordinator
ai-brains context
ledgerful init
```

**Every session:**

```powershell
cd C:\dev\coordinator\coordinator
ai-brains preflight --summary
ledgerful doctor --json
ledgerful change-context --json   # before meaningful edits
```

| Tool | Project skill |
|------|---------------|
| **ai-brains** | `.agents/skills/ai-brains` |
| **ledgerful** | `.agents/skills/ledgerful` |
| **implement** | `.agents/skills/implement` |
| **codex-review** | `.agents/skills/codex-review` |

Skills may also exist under `C:\dev\coordinator\.agents\skills\`; **CLI cwd is always this repo**.

| State | Action |
|-------|--------|
| Not inited | Skip tool CLIs; note in `review.md` |
| CLI missing | Continue without; say so |
| Inited | Every command from **this** cwd |

## Conductor (where work is defined)

Tracks live **only** under planning:

```
C:\dev\coordinator\conductor\
  conductor.md
  deferred.md
  ####-PascalDescription\
    spec.md  plan.md  review.md
```

| Owner says | You do |
|------------|--------|
| `track N` / `/plan` | Planning only — do not execute |
| `Review track N` | Read-only plan audit |
| `Implement track N` | Load **implement**; deliver DoD |

### Plan fidelity

- Implement **only** the active track’s `spec.md` + `plan.md`.  
- No freestyle design or “while we’re here” refactors.  
- If plan is wrong → **stop and report**.

### Deferred fold-in

1. Read entire `deferred.md` at start.  
2. Absorb only rows already claimed in `spec.md` §9.  
3. At finish: append **every residual low** to `deferred.md`. Medium+ **block**.  
4. Resolve rows this track lands.

## Stack expectations

- **Rust** for orchestrator / Control Plane (ADR-0004).  
- Windows-first; local-only Control Plane (ADR-0002).  
- Long-lived harness drive (not headless-default) (ADR-0001).  
- Prefer `cargo fmt` / `clippy` / tests as the product grows; wire ledgerful `required_verifications` to real commands after scaffold.

## Owner vs agent

| Owner (HITL) | Agent |
|--------------|--------|
| Product decisions, public risk | Code, CI, commits, PRs |
| Force-push / delete tags | Propose; wait |
| Hermes/Telegram ops | Notify Adapter after core (ADR-0025) |

## PR + CI discipline

- Feature branch → PR → CI green → squash-merge (when `auto_merge` / owner policy allows).  
- Do **not** busy-poll CI; wait with long interval / `gh run watch`.  
- Never stage planning paths into product commits.

## After onboarding

| Intent | Skill |
|--------|--------|
| Execute Ready track | **`implement`** |
| Cross-model audit | **`codex-review`** |
| Plan only | `C:\dev\coordinator\.agents\skills\plan` |
| Audit a plan | `C:\dev\coordinator\.agents\skills\review-track` |

---

*Pair with planning onboarding: `C:\dev\coordinator\.agents\skills\onboarding\SKILL.md`.*
