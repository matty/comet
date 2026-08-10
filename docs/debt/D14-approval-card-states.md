# D14 — two states the approval card does not distinguish on screen

**Both fixed in [PR #36](https://github.com/matty/comet/pull/36)'s fix wave; the diagnosis below
is kept because the recurrence is the lesson.** The whole-branch review took both as must-fix.
The duplication went in the *composer*, not the card — the row now carries a single
`tracked_upper(label)` eyebrow with no detail line, because the card is the durable record and the
row is transient. The identical paint became a `ApprovalPaint` enum on the row model (not a
caption-string sniff) driving a four-arm match on `(border, wash, tile, tint, icon, caption)`:
a green check for allowed, a close-circle plus a brighter caption for denied, amber for expired.
**No red** — a denial is a choice the user made, not a failure. Every layout literal sits outside
the match, so the 56px invariant now holds by construction rather than by inspection.

**The residual below — the deny note vanishing from the UI — is still open**, and is the part no
fix wave addressed.

Both were found by 1.5's rendered check, neither was reachable by a logic test, and both are
0.2a's findings recurring in a new surface — four slices on, rendering keeps finding what tests
cannot, and this time it found the *same two defects the project already paid for once*. Full
evidence in
`C:\dev\comet\.superpowers\sdd\2026-08-09-slice-1-5-approval-ui\task-7-report.md` (F1, F2).

**The duplication.** While an approval is open, `transcript.rs:approval_card` and
`composer.rs:render_approval` both render the same `approval_chip_content` output — the same
label and the same detail string, ~90px apart with only the status strip between them. Each call
site is individually correct, which is exactly why nothing catches it. It is defensible (the card
is the persistent record, the row is the affordance) but the defence only pays off *after* the
decision, when the row is gone — and the duplication is on screen precisely during the one moment
the user is looking. Answers: suppress the card while its own row is up, or shorten one of the
two.

**The identical paint.** `Allowed`, `Denied` and `Allowed for this session` differ by one
right-aligned word in `theme.text_muted` — same wash, border, weight, size, icon. Allow and Deny
are opposite outcomes carrying zero non-textual signal, so scanning a scrolled transcript you
cannot tell an approval from a refusal without reading a small grey word at the far right edge.
The `Expired` arm already proves the mechanism (it tints `warning_muted`) and costs four lines.
Same code path, same shape: an **open** card differs from a decided one only by the *absence* of
that word, so scrolled back past the composer, answerable and settled look the same.

Related, filed with them because the fix probably shares a decision: **the deny note is written to
the model and then vanishes from the UI.** It reaches `ApprovalDecision::Deny { message }` and is
persisted in the doc part, but nothing renders it — the 56px fixed-height card has nowhere to put
it, and the fixed height is itself a deliberate constraint (a decision landing must not reflow the
transcript under the user's scroll position).
