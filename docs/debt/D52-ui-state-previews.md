# D52 — no way to see a UI state without producing it for real

**Origin:** 4.1's rendered check, 2026-08-12. **State:** closed — answered by slice 4.4
([PR #115](https://github.com/matty/comet/pull/115)) with option 3 below: the `COMET_MOCK_*` knob
family, its register at `docs/testing/mock-states.md`, and the two-directional
`every_mock_knob_is_documented` test. The page stays for the option comparison and for the limit
the chosen route carries — a knob scripts a provider, so a state no provider is the source of is
still out of reach.

## What happened

4.1 added a disc in the composer that fills as the context window fills. Reviewing it means
seeing it at several fills — nearly empty, a quarter, most of the way, full — in both
appearances. Every one of those states requires a chat whose context is actually that full.

Getting there by driving the app cost three real turns on the most expensive model in the
catalog, because the chat under test was locked to a live provider and the model picker's
default is the first row of the catalog. None of those turns produced anything anyone kept.
The states that matter most for review — a nearly-full window, a failing update, a long
session — are exactly the ones that are slowest and most expensive to reach.

The escape was a one-off environment knob, `COMET_CONTEXT_DEMO=<percent>`, which supplies a
reading when there is no real one. It works, and it is the fourth or fifth thing of its shape
in this codebase: `COMET_MOCK_APPROVAL`, `COMET_MOCK_QUESTION`, `COMET_MOCK_HANG`,
`COMET_MOCK_TABLE`, `COMET_MOCK_CODE`, and now this. Each was added by the slice that needed
it, each covers one surface, and none of them compose — you cannot ask for "the approval card
*and* a nearly-full context *and* light mode" in one launch without a bespoke combination.

## Why the row exists rather than a fix

The knobs are not wrong. They are cheap, they live next to the code they fake, and they cost
nothing at runtime. What is missing is the level above them: a way to put the *real* UI into an
arbitrary state and look at it, without a provider, a filled window, or a sequence of clicks
that a screenshot script has to reproduce exactly.

This matters more than a convenience: **the rendered check is where this project keeps finding
the defects the tests cannot see.** 0.2a found four (duplicated copy, log vocabulary on screen,
two states painting identically, opacity under the contrast floor). 1.5 found six. 3.2 found two
tone mistakes. Every one of those came from looking at a real surface in a real state. Anything
that makes reaching a state expensive makes that check rarer, and the check is the control.

## Options, and what each costs

Listed so the next person does not re-derive them. None is chosen.

**1 — A gallery route.** `COMET_OPEN_ROUTE=gallery` renders a page of surfaces in fixed states
side by side, built from the same components. Cheapest to reach a lot of states in one shot, and
it composes. The risk is drift: a gallery that constructs its own props can keep rendering a
component the app no longer uses that way, and then it lies. Mitigation is to build gallery
entries from the same constructors the app calls, never from hand-written literals.

**2 — A seeded fixture store.** A `COMET_DATA_DIR` built by a script: chats, sessions, readings,
approvals, all written through the real doc writers. Highest fidelity — it exercises decode,
watch and render exactly as production does — and it also gives the screenshot script a stable
place to point. Costs a maintained seeding tool, and it goes stale when a schema changes.
Partly exists already: the `_drive.ps1` rigs under `.superpowers/sdd/*/shots/` do a version of
this per slice, unmaintained between slices.

**3 — Keep per-surface knobs, but make them a family.** One documented convention
(`COMET_DEMO_<surface>=<state>`), one place listing them, and a test that every declared state
renders. Smallest change, keeps the per-surface locality, still does not compose.

**4 — Snapshot the element tree.** Not a preview at all: assert structure in a test instead of
looking. Complements the others and catches regressions cheaply, but it cannot answer the
question a rendered check answers ("does this read right to a person?"), which is where the
findings above came from.

## Constraints any answer has to respect

- **A preview must never mask real data.** `COMET_CONTEXT_DEMO` fills in only where there is no
  live reading, which is the property to keep: a demo state that overrides real state can hide
  the bug you were looking for.
- **It must not ship as a user-reachable surface.** A gallery route behind an env var is fine; a
  menu item is a second product to maintain and support.
- **It must use the real components.** A preview built from a parallel implementation tests the
  preview.
- **This machine cannot judge GPU-path features.** Backdrop blur and edge fades degrade silently
  under software rasterization here, so any preview still leaves those needing real hardware —
  see the note in `.agents/rules/gpui-ui.md` about the fork-only primitives.
