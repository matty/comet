# D63 — the corpus pipeline has no route for an exploratory capture

**Status:** open. Not a defect in either tool; a gap in what the doc describes.

## What happens

`docs/testing/provider-captures.md` describes exactly one path into the corpus:

1. `comet-provider-capture <provider> <named-scenario>` into an immutable raw root,
2. `comet-provider-sanitize` into an immutable staging name,
3. review raw against staged,
4. copy the reviewed `manifest.json` + `events.jsonl` pair into the corpus.

It also states two prohibitions that close every other path: *"Never copy raw data into the
corpus"* (`:11`) and *"Do not edit staged output by hand"* (`:67`).

The scenario list is closed. Every entry is a `ClaudeRunScript` / `CodexRunScript` variant with
a fixed prompt compiled into the binary.

## Why that is a problem

**A scenario can only be written once you already know what you are looking for.** Naming one
means committing to a prompt, an expected frame shape, and usually an evidence guard. That is
the output of an investigation, not its input.

An exploratory capture is the opposite: an arbitrary prompt against the real CLI to find out
what it does. Comet has a perfectly good tool for that — the per-slice Python rigs under
`C:\dev\superpowers\comet\captures\` — and every phase since 2.2 has used one. The doc does not
mention them, so on paper they do not exist.

## Both slices that hit this went around the doc, differently

**4.2** hand-sanitized a `drive_claude_plan.py` capture into
`crates/harness/tests/corpus/claude/2.1.229/subagent/` and recorded the deviation honestly in
the manifest's own `source` field:

> `captures/2026-08-13-plan-todo-subagent/drive_claude_plan.py raw capture
> (corpus-subagent-2.1.229.jsonl), hand-sanitized for slice 4.2 task 9`

No test reads that entry's frames, which is what keeps it harmless: nothing depends on the
hand-sanitization being correct. (It was additionally protected by the corpus validator's
reciprocal claim checks until those were removed on 2026-08-14; the protection now comes
from the simpler fact that no test names it.) It also produced D58 — two real sanitizer gaps
found *by* doing it by hand, which the tool would never have surfaced.

**4.3** took the other route: added `checklist` and `checklist-resume` scenarios to
`comet-provider-capture`, re-captured through the tool, and sanitized with the real sanitizer.
That cost a tool change and two more live turns, having already captured the same finding with
the Python rig the day before. The evidence was recorded twice.

Neither slice was wrong. They made opposite calls because the doc gives no basis for choosing.

## What would close it

Not a code change first — a doc change. `provider-captures.md` should describe **two** stages
and the bridge between them:

- **Explore** with a rig, against the real CLI, with an arbitrary prompt. Output is raw JSONL
  outside the repo and a written-up finding. Nothing here is corpus evidence.
- **Promote** by writing the scenario the finding justifies, re-capturing through
  `comet-provider-capture`, and running the existing sanitize → review → promote pipeline
  unchanged.

Stated that way, 4.3's route is simply the correct one and 4.2's was a shortcut taken because
the second stage had no scenario to write into. The prohibitions stay exactly as they are; what
changes is that the doc stops implying the first stage is illegitimate.

**One thing to preserve if this is fixed:** hand-sanitizing found sanitizer bugs (D58) that the
automated path silently tolerates, because the tool only redacts what it has rules for and
nobody reads its output field by field. Whatever replaces the shortcut should keep a
review step that looks at values rather than at placeholder counts.

## Related

- **D58** — the two `comet-provider-sanitize` gaps found while hand-sanitizing 4.2's entry.
- **D60** — the scenario name is duplicated across three unsynchronized places, which is friction
  in the "write the scenario" step this page recommends.
- **D61** — a scenario's evidence guard and its prompt live in different files.
