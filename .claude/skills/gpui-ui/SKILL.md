---
name: gpui-ui
description: Comet's conventions for gpui UI code in crates/ui — theme tokens and the layout-vs-paint rule, light/dark design rules, the pinned-fork rendering primitives (frosted popovers, edge fades, image eviction), motion helpers, theme-generation cache keying, and the COMET_* runtime diagnostics. Use this whenever work touches crates/ui — adding or restyling a panel, sidebar, popover, dialog, composer, transcript, or terminal view; picking a color, padding, radius, or animation; fixing a light-mode or theme-switch bug; or debugging paint, scroll, blur, or GPU-memory behavior — even when the request sounds like a small tweak and doesn't mention gpui or theming.
---

Read `.agents/rules/gpui-ui.md` and follow it. That file is the shared, agent-agnostic
source; this skill is the Claude Code entry point to it, so the two can't drift.

Then read the module doc comment of the file you're about to change. In `crates/ui` those
headers record why the current structure exists — usually a bug that was expensive to find —
and changing the shape without reading them tends to reintroduce it.

Two failure modes are worth naming up front, because both look fine until someone toggles the
theme:

- A layout value that varies by appearance. Sizes must be palette-independent; if light mode
  seems to need different spacing, the real defect is contrast in the color.
- A cross-frame cache that doesn't key on `theme_generation()`. It will keep painting the old
  palette after a switch until the underlying content changes.
