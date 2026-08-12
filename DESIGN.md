---
name: Coordinator
description: Operator Status Surface for same-machine multi-harness track orchestration
---

<!-- SEED: established before Status Surface implementation; re-run $impeccable document once mock HTML/CSS (track 0003) or product UI exists to capture real tokens and components. -->

# Design System: Coordinator

## Overview

**Creative North Star: "The Local Ops Console"**

Coordinator’s Status Surface is an **Operate** product: dense enough for concurrent harness phases, calm enough for hours-long autonomous runs. The visual world should feel like a trusted machine-room console on the operator’s Windows desktop—not a marketing SaaS landing page, not a purple-gradient AI dashboard template.

Density favors scannable status (track, phase, harness role, failure class) over hero marketing. Depth is tonal and structural (panels, separators), not floating card stacks. Motion is rare and purposeful (state change, not decoration).

**Key Characteristics:**

- Operate-mode dashboard grammar (scan, act, recover)
- Cool neutral base with a single decisive accent for “needs attention”
- Monospace allowed for IDs, paths, and phase names; UI chrome stays humanist sans
- Explicit anti-SaaS-template posture (no Inter-default purple gradients)

## Colors

Palette strategy: **cool industrial neutrals + one high-signal accent** for failures/attention. Exact hex values `[to be resolved during implementation / mockup]`.

### Primary

- **Attention Signal** (accent): reserved for hard failures, blocked gates, and primary operator actions—not decorative fill. Rarity is the point.

### Neutral

- **Console Paper / Console Ink**: cool-tinted backgrounds and text (never pure `#000` / pure `#fff` as the only pair).
- **Panel / Divider**: subtle structural separation without nested card chrome.

### Named Rules

**The One Alarm Rule.** Accent color is for state that requires operator attention or a primary action—not section decoration.

**The No Purple Gradient Rule.** Reject default AI-dashboard purple-to-blue gradients and glassmorphism glow stacks.

## Typography

**Display / UI sans:** `[to be resolved during implementation]` — prefer a distinctive but readable UI face; avoid Arial/Inter-as-default autopilot.

**Mono:** `[to be resolved]` for track IDs, paths, harness names, phase tokens.

**Character:** Technical, legible at dense densities; hierarchy by weight and size, not color noise.

### Hierarchy

- **Title** — surface/region names (Status, Sessions, Failures)
- **Body** — human-readable status and guidance
- **Label / Mono** — machine identifiers and compact tables

### Named Rules

**The Scan Hierarchy Rule.** Status tables and phase lists must remain readable at a glance; type scale must not collapse into one weight of gray text.

## Layout

Spatial grammar: **console regions** (header strip, primary status board, session/harness list, failure detail). Prefer fixed logical regions over free-form marketing grids. Desktop-first Windows operator viewport; responsive only enough for laptop vs ultrawide—not mobile-first consumer app.

Rhythm: consistent internal padding; dense but not cramped. Exact spacing scale `[to be resolved during mockup]`.

### Named Rules

**The Status Board Rule.** The default view answers: active track, current phase, harness roles, last completion/failure—without hunting nested cards.

## Elevation & Depth

Flat-by-default surfaces with **tonal layering** (background → panel → selected row). Shadows only for floating transient UI (toasts, modal failure detail)—not for every card.

### Named Rules

**The Flat Console Rule.** Do not nest cards inside cards to imply hierarchy; use spacing, separators, and row selection.

## Shapes

Slightly rounded controls (operator tooling, not consumer “pill everything”). Borders used sparingly as structure, not as default box-around-everything.

Exact radii `[to be resolved during mockup]`.

## Do's and Don'ts

### Do:

- **Do** design for operator scan paths: track → phase → harness → failure class.
- **Do** keep Stop/Pause and hard-failure presentation unmistakable.
- **Do** leave room for monospaced IDs without breaking layout.
- **Do** treat Status Surface as product tool UI (Operate), not marketing (Persuade).

### Don't:

- **Don't** use generic AI SaaS template aesthetics (purple gradients, Inter-only, nested glass cards).
- **Don't** invent cloud multi-tenant ops framing or public control-plane chrome.
- **Don't** bury Stop/Pause or failure state behind decorative empty states.
- **Don't** lock a permanent component library in this seed—re-document after mockup code exists.
