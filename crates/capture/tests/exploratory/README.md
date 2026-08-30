# Exploratory captures (D63)

This directory is the destination for an exploratory capture that is worth keeping for the next
person, but is **not** promoted corpus evidence. See
`docs/testing/provider-captures.md`'s "Exploratory captures" section for the full procedure; this
file is the short version, next to where the entries live.

An exploratory capture answers a question — "what does this CLI actually send for X" — with an
arbitrary prompt against the real provider, run with a rig outside this crate (the per-slice
Python rigs this repository's agents keep under their own scratch storage are the usual tool; a
one-off script works too). It is the opposite of a named `comet-provider-capture` scenario, which
can only be written once the shape you are looking for is already known.

Nothing under `tests/corpus/` may cite anything here, and nothing here is walked by
`corpus_root()`/`promoted_scenarios()`, the capability sheet golden test, the allowlist property
tests, the decode coverage lints, or the version floor/coverage policy — those all walk
`tests/corpus` specifically, never this directory. That is a structural fact, not a convention:
see `comet_capture::exploratory_root`'s doc comment.

## Adding a finding worth keeping

1. Sanitize the raw output with the real `comet-provider-sanitize`, same allowlist, same rules —
   an exploratory capture is not an excuse to skip it; it is raw provider output like any other.
2. Create `tests/exploratory/<short-name>/`, holding whatever of the sanitized `manifest.json` /
   `events.jsonl` pair is worth keeping, plus a short `finding.md` describing the question, the
   prompt, and the answer.
3. **Every such entry must carry a file named exactly
   `NOT-CORPUS-EVIDENCE.md`** (`comet_capture::EXPLORATORY_MARKER_FILENAME`) alongside its data,
   with content to the effect of: *"Exploratory capture, not promoted corpus evidence. Nothing
   decodes or tests against this. See docs/testing/provider-captures.md."* This is what lets a
   guard test (`crates/capture/tests/exploratory_boundary.rs`) — and a human copying files by
   hand — tell the two trees apart on sight, not just by which directory something currently sits
   in.
4. If the finding justifies a permanent scenario, write it into
   `crates/capture/src/record/scenarios.rs`, re-capture through `comet-provider-capture`, and run
   the existing sanitize → review → promote pipeline unchanged. Promoting is never a copy from
   here into `tests/corpus/` — the pipeline does not shortcut for having already looked once.

If a finding is better recorded as prose than as data (a ruling, a source-read, a gap with no
capture behind it — the shape most `docs/debt/` pages already take), it does not need an entry
here at all; write it up there instead.
