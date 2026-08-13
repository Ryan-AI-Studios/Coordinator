# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Dioxus Desktop 0.7 (WebView2) behind Cargo feature `ui`; `mock/status-surface.html` remains the visual contract. Control plane and CLI are Rust-first (ADR-0004). CLI stays the automation entry (ADR-0017).

## Users

Primary users are **operators** on a same-machine **Windows** workstation who run multi-harness conductor-track workflows (plan → review → implement → CI → next track). They need to see session/phase health, stop/pause, and failures without babysitting token-burning model sessions.

## Product Purpose

**Coordinator** drives long-lived interactive AI harnesses through full conductor-track workflows: phases advance on Completion Signals, stay fully autonomous after a track starts (including Planner-chosen Next Natural Track), with CLI plus a Status Surface for operator visibility.

Success means an operator can start a track (or multi-track run), walk away, and only be interrupted for hard failures / quota exhaustion—not for phase handoffs or CI waits.

## Positioning

Same-machine Windows session orchestrator with hybrid Completion Signals and a built-in canonical workflow DAG—not a remote agent farm, not a multi-tenant SaaS control plane, and not free-form per-project workflow DAGs as the primary robustness story.

## Operating Context

- Nested workspace: planning at Workspace Root; product git only under the nested product folder
- Projects are separate folders; Session pool per Project; shared harness binaries
- Operator workflows: start track, watch Status Surface, Stop (abort phase) / Pause (finish phase then hold), Failure Artifact + Windows toast on hard fail
- Dogfood target later: `C:\dev\Orca` (nested layout); Ledgerful multi-sibling remains a supported Layout Profile

## Capabilities and Constraints

- Local-only Control Plane (no public internet exposure in v1)
- Token-idle CI wait outside model sessions
- Strict review gate: findings above low are not deferrable
- Status Surface is Dioxus Desktop (WebView2); mock HTML stays the visual reference
- Planning docs and conductor tracks must never ship inside product git
- Open: default-on `ui` feature / installer bundle (owner call after first ship)

## Brand Commitments

- Product name: **Coordinator**
- Voice: precise, operator-facing, technical—not marketing SaaS tone
- No requirement to invent customer logos, fake testimonials, or remote-farm branding

## Evidence on Hand

- Product intent: planning tree `SHARED-UNDERSTANDING.md` and ADRs 0001–0028 (outside this git repo)
- Product code today: Rust Control Plane + live Status Surface (`cargo run --features ui -- ui`); mock under `mock/status-surface.html` is the visual contract
- Design skills: project-scope Impeccable under `.agents/skills/impeccable/`

## Product Principles

1. **Operator clarity over decorative density** — Status Surface must answer “what is running, what blocked, what next” at a glance.
2. **Local machine truth** — paths, sessions, and failures are local; never frame as cloud multi-tenant ops.
3. **Autonomy with interruptibility** — full autonomy after start, always-visible Stop/Pause and failure signals.
4. **Docs-vs-product split** — design context for UI ships with product git; planning SoT stays outside.
5. **Mock is the visual contract** — live Dioxus surface matches `mock/status-surface.html`; do not invent a new visual language.

## Accessibility & Inclusion

Target operators on desktop Windows; Status Surface should meet practical WCAG 2.2 AA for interactive controls when UI lands (focus, contrast, non-color-only status). Exact standard confirmation remains open until mockup review.
