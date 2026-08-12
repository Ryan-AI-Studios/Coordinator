---
name: Coordinator
description: Operator Status Surface for same-machine multi-harness track orchestration
colors:
  bg: "#0f1419"
  panel: "#1a222c"
  panel-2: "#232d3a"
  ink: "#e8eef4"
  muted: "#9aabbc"
  line: "#2f3b4a"
  accent: "#e8a04a"
  ok: "#3dba7a"
  bad: "#e05a5a"
  info: "#5b9fd4"
typography:
  title:
    fontFamily: "Segoe UI, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 600
    letterSpacing: "0.02em"
  body:
    fontFamily: "Segoe UI, system-ui, sans-serif"
    fontSize: "0.82rem"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "Segoe UI, system-ui, sans-serif"
    fontSize: "0.74rem"
    fontWeight: 400
  mono:
    fontFamily: "Cascadia Mono, Consolas, Courier New, monospace"
    fontSize: "0.78rem"
    fontWeight: 400
rounded:
  sm: "6px"
  md: "10px"
  pill: "999px"
spacing:
  1: "0.35rem"
  2: "0.55rem"
  3: "0.75rem"
  4: "1rem"
  5: "1.25rem"
components:
  button-default:
    backgroundColor: "{colors.panel-2}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0.4rem 0.85rem"
  button-primary:
    backgroundColor: "{colors.panel-2}"
    textColor: "#ffe3b5"
    rounded: "{rounded.sm}"
    padding: "0.4rem 0.85rem"
  button-danger:
    backgroundColor: "{colors.panel-2}"
    textColor: "#ffc9c9"
    rounded: "{rounded.sm}"
    padding: "0.4rem 0.85rem"
  project-card:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0.9rem 1rem 1rem"
  status-pill:
    backgroundColor: "{colors.panel-2}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "0.18rem 0.5rem"
---

# Design System: Coordinator

## Overview

**Creative North Star: "The Local Ops Console"**

Coordinator’s Status Surface is an **Operate** product: dense enough for concurrent harness phases, calm enough for hours-long autonomous runs. The visual world is a trusted machine-room console on the operator’s Windows desktop—not a marketing SaaS landing page, not a purple-gradient AI dashboard template.

Density favors scannable status (track, phase, harness role, failure class) over hero marketing. Depth is tonal and structural (panels, separators), not floating card stacks. Motion is rare and purposeful (state change, not decoration).

Tokens below are extracted from `mock/status-surface.html` `:root` (track 0003). CSS variable names are the live source of truth for the mock.

**Key Characteristics:**

- Operate-mode dashboard grammar (scan, act, recover)
- Cool neutral base with a single decisive accent for “needs attention”
- Monospace for IDs, paths, and phase names; UI chrome stays humanist sans
- Explicit anti-SaaS-template posture (no Inter-default purple gradients)

## Colors

Palette strategy: cool industrial neutrals + one high-signal accent for failures/attention. Live CSS variables on `:root` in `mock/status-surface.html`:

| Token / CSS var | Hex | Role |
|-----------------|-----|------|
| **Console Paper** `--bg` | `#0f1419` | Page background |
| **Panel** `--panel` | `#1a222c` | Header, cards, help panels |
| **Panel Raised** `--panel-2` | `#232d3a` | Rows, stats, nested surfaces |
| **Console Ink** `--ink` | `#e8eef4` | Primary text |
| **Muted** `--muted` | `#9aabbc` | Secondary text, labels |
| **Line** `--line` | `#2f3b4a` | Borders / dividers |
| **Attention Signal** `--accent` | `#e8a04a` | Pause/held, primary actions, attention pills |
| **OK** `--ok` | `#3dba7a` | Running / healthy |
| **Bad** `--bad` | `#e05a5a` | Hard fail, Stop danger |
| **Info** `--info` | `#5b9fd4` | Paused state, focus ring |

### Primary

- **Attention Signal** (`--accent` `#e8a04a`): reserved for hard attention, held/paused emphasis, and primary operator actions—not decorative fill. Rarity is the point.

### Neutral

- **Console Paper / Console Ink** (`--bg` / `--ink`): cool-tinted pair (never pure `#000` / `#fff` alone).
- **Panel / Divider** (`--panel`, `--panel-2`, `--line`): structural separation without nested card chrome.

### Named Rules

**The One Alarm Rule.** Accent color is for state that requires operator attention or a primary action—not section decoration.

**The No Purple Gradient Rule.** Reject default AI-dashboard purple-to-blue gradients and glassmorphism glow stacks.

## Typography

**UI sans** (`--sans`): Segoe UI, system-ui, sans-serif — Windows operator chrome.

**Mono** (`--mono`): Cascadia Mono, Consolas, Courier New, monospace — track IDs, paths, harness names, phase tokens, stats.

**Character:** Technical, legible at dense densities; hierarchy by weight and size, not color noise.

### Hierarchy

- **Title** (~1rem / 600) — surface/region names (Status, project titles)
- **Body** (~0.78–0.82rem) — human-readable status and guidance
- **Label** (~0.68–0.74rem, muted) — field labels, table headers
- **Mono** (~0.72–0.8rem) — machine identifiers and compact tables

### Named Rules

**The Scan Hierarchy Rule.** Status tables and phase lists must remain readable at a glance; type scale must not collapse into one weight of gray text.

## Layout

Spatial grammar: **console regions** — sticky header strip, toolbar + Stop/Pause help, optional phase strip, primary multi-project board, footer launch hints.

- Board: CSS grid **2 columns** desktop; **1 column** at `max-width: 980px` (`--break-narrow` intent).
- Rhythm: spacing scale `--space-1`…`--space-5` (0.35rem → 1.25rem); card padding ~0.9–1rem.
- Desktop-first Windows operator viewport; responsive only enough for laptop vs ultrawide—not mobile-first consumer app.

### Named Rules

**The Status Board Rule.** The default view answers: active track, current phase, harness roles, last completion/failure—without hunting nested cards.

## Elevation & Depth

Flat-by-default surfaces with **tonal layering** (background → panel → panel-2 rows). No drop shadows on project cards. Transient UI (toasts, failure detail) may float later; mock uses border + tint only.

### Named Rules

**The Flat Console Rule.** Do not nest cards inside cards to imply hierarchy; use spacing, separators, and row selection.

## Shapes

Slightly rounded controls (`--radius-sm` 6px). Project panels `--radius-md` 10px. Status pills use full pill radius. Borders used sparingly as structure.

## Components

### Buttons

- Default / ghost / primary / danger variants via border tint + text color (not filled marketing CTAs).
- Primary ≈ accent-tinted border + warm text; danger ≈ bad-tinted border + rose text.
- Focus: `outline: 2px solid var(--info)`.

### Status pills

- Uppercase micro-labels for `running` / `attention` / `failed` / `paused` / `idle` with soft tinted backgrounds from semantic colors.

### Project cards

- Panel background, 1px line border; `attention` / `failed` border tints; idle may use dashed border.
- Internal: head (title + actions), key-value rows, session table, note or Failure Artifact.

### Failure Artifact

- Structured panel: path (e.g. `.coordinator/FAILURE.md`), failure class, merge blocked, sessions-for-attach, toast mention.
- Semantic bad border/tint; not a nested marketing card stack.

### Phase chips

- Monospace pipeline stages: done / current / next.

## Do's and Don'ts

### Do:

- **Do** design for operator scan paths: track → phase → harness → failure class.
- **Do** keep Stop/Pause and hard-failure presentation unmistakable (Stop: abort, no merge, sessions left for attach).
- **Do** leave room for monospaced IDs without breaking layout.
- **Do** treat Status Surface as product tool UI (Operate), not marketing (Persuade).
- **Do** map new UI colors to the live `:root` variables above.

### Don't:

- **Don't** use generic AI SaaS template aesthetics (purple gradients, Inter-only, nested glass cards).
- **Don't** invent cloud multi-tenant ops framing or public control-plane chrome.
- **Don't** bury Stop/Pause or failure state behind decorative empty states.
- **Don't** reintroduce seed `[to be resolved]` placeholders for tokens already fixed in the mock CSS.
