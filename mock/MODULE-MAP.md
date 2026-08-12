# Status Surface — Module Map

**Track:** 0003-StatusSurfaceMockup  
**Source mock:** [`status-surface.html`](./status-surface.html)  
**Stack:** Mockup-first, stack-agnostic. Eventual lean **Dioxus** unless mockup proves otherwise (ADR-0018). This map names UI regions and future data/concepts only — not crate splits or APIs.

Use this when implementing Control Plane shells (0004+) and real Status Surface UI so panel boundaries stay stable.

## Panel → operator job → future data / component

| UI region | Operator job | Future data / component (names only) |
|-----------|--------------|--------------------------------------|
| Global header / stats | Fleet glance (project counts, active, attention, idle) | Project registry aggregate |
| Global Pause all / Stop selected | Fleet control with ADR-0024 semantics | Control plane stop/pause API |
| Ops help (Stop vs Pause) | Clarify abort-vs-hold without reading ADRs | Static help or inline docs from product copy |
| Phase strip (selected project) | Pipeline position at a glance | Conductor state + phase list + `next_track` |
| Project card | Per-project run health | Project session pool + track state |
| Track / phase / next / layout rows | Position + Layout Profile awareness | Conductor state; layout profiles (nested / multi-sibling / single-root) |
| Session table | Role × harness × idle | Role bindings + harness adapters |
| Notes strip | Recover / contextual guidance | Soft status, pause/stop copy |
| Failure Artifact panel | Hard-fail recover path | Failure Artifact (path/class) + notify toast (ADR-0020) |
| Idle / empty surface | Backlog clear / no active track | Project with empty track slot |
| Add project | Register another workspace folder | Project registry scan / add |

## Demo state anchors (mock HTML)

| State | Anchor |
|-------|--------|
| Parallel plan reviewers | `article[data-state="parallel-plan-review"]` (agy + opencode both `active`) |
| Pause | `article[data-state="paused"]` |
| Token-idle CI | `article[data-state="token-idle-ci"]` |
| Hard failure + artifact | `article[data-state="hard-failure"]` · `[data-region="failure-artifact"]` |
| Idle / no active track | `article[data-state="idle"]` |
| Stop semantics | Header `.ops-help`, Stop button `title` attributes, idle card Stop note |
| Multi-project overview | `main#projects` grid (2-col desktop, 1-col ≤980px) |
| Layout profiles | rows labeled nested / multi-sibling / single-root |

## Explicit non-decisions

- No permanent UI framework lock in this track.
- No live Control Plane wiring; mock is static HTML/CSS.
- Notify Adapter (Hermes/Telegram) is later; toast is shown as copy only.
