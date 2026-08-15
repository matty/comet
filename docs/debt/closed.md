# Closed debt

Kept for the reasoning, not the status. Each of these merged;
`README.md` carries the row and the PR. Delete an entry only if its
explanation stops being useful to someone reading the code it touched.

## D1 — empty `ReasoningDelta` while parked

**The bug.** `sessions.rs`'s run loop treats the first event after parking as the
next turn beginning:

```rust
if !parked_notice && idle_since.take().is_some() {
    inner.set_status(&chat_id, SessionStatus::Working, true);
}
// ... 7 lines later
if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
    continue;
}
```

An empty reasoning delta is a **pure heartbeat** — redacted thinking and
tool-input-generation windows stream them with no text, hundreds per long turn —
and persistent sessions emit them between turns too. So the heartbeat clears
`idle_since` and flips the status to Working, then `continue`s without folding
anything. No `Done` is coming. The session sits Working forever, and because the
reaper's `select!` arm is gated on `idle_since.is_some()`, the child is never
released.

**Why it is the same bug 0b.1 fixed.** 0b.1 found this exact wedge for
`AgentEvent::Notice` and fixed it with the `parked_notice` guard. It recorded the
`ReasoningDelta` instance as pre-existing on `main` and out of that slice's
scope. Same one-line class of guard covers it.

**The fix.** Move the empty-heartbeat `continue` *above* the turn-start block, so
a heartbeat is dropped before it can transition any state. `touch_session` still
runs above both, which is the only thing a heartbeat is legitimately for. A real
turn's first non-empty event still flips the status.

## D2 — optional wire fields

**The lesson.** 0b.1's only escaped P1 was keyless notice collapse: two emitters
passed an `Option` straight through from the wire, the fold two files away
compared them, and an absent key was read as "these are the same thing" instead
of "no evidence either way". Two unrelated CLI messages merged into one chip in a
persisted, LAN-replayed transcript.

**Why it escaped.** Three per-task reviews and a whole-branch review all missed
it, because **every test and fixture in the slice supplied a key**. The plan's
fixtures were written from the happy path, so the `None` case was never
constructed anywhere in the slice.

**Where it lands.** `.agents/rules/` in the repo, so it binds every agent and not
just the next session — 0b.2 and Phase 1 both add optional wire fields.

## D6 — unclaimed control requests are counted but unanswered

0b.2's sink 3 records a diagnostic for every inbound Claude `control_request`
whose subtype is not `can_use_tool`, and deliberately does not reply. `sdk.d.ts`
documents that hosts must answer unrecognized `request_user_dialog` kinds with
`{behavior:"cancelled"}`. Replying is a behaviour change to a frame the real-CLI
capture never saw fire (zero `control_request`s even with
`--permission-prompt-tool stdio`), so its frequency is unknown rather than zero,
and it was out of 0b.2's scope. The count sits on the return path and never
delays an answer, so adding the reply later is additive.

## D60 — the scenario name lived in three unsynchronized places

**The bug.** `comet-provider-capture`'s `--help` text, its `supported_pair()`
argument check, and its scenario dispatch `match` each spelled out the same set
of `(provider, name)` pairs by hand, with nothing tying the three together.
Adding `checklist` to two of the three (the help text and the dispatch) but not
the third produced a binary whose `--help` advertised the scenario and whose
argument validation refused it — *"Scenario \"checklist\" is not supported for
claude. Run with --help to see the valid pairs."* — pointing the operator back
at the text that had just told them otherwise.

**The fix.** The provider-capture-simplification design (stage 4,
"neutral-recorder") replaced all three copies with one:
`crates/harness/src/capture/record/scenarios.rs`'s `SCENARIOS: &[ScenarioSpec]`
declares each scenario's name, purpose, provider, runtime mode and argument
requirements exactly once. `comet-provider-capture.rs` generates its `--help`
text and validates arguments by looking a `(provider, name)` pair up in the
table (`scenario()`), and `record()` dispatches off the same table by the same
lookup. There is no second copy left to drift, and `scenarios.rs`'s own test
`every_scenario_name_the_binary_advertises_is_in_the_table` pins the row count
so an added or removed scenario cannot pass silently.

## D61 — the checklist evidence guard encoded a prompt's content, from a different file

**The bug.** `recording.rs`'s Claude run loop required `created.len() >= 2 &&
!updated.is_empty()` to accept a `checklist` capture — two distinct confirmed
creates plus at least one confirmed update — and a non-empty
`updated.difference(&created)` to accept a `checklist-resume` capture, i.e. at
least one update to a task the process had never created. Those thresholds were
correct only because `capture/checklist.rs`'s prompt happened to ask the model
for exactly that much — two facts in two files, agreeing today, with nothing
enforcing that they keep agreeing.

**And the row that filed this said "4 and 2".** Those were the *prompt's*
tool-call counts, not the predicate's, and they were quoted onward into a plan
and a doc comment before anyone read the deleted code. Quoting a threshold from
a prompt rather than from the code is the same mistake one level up, which is
why the predicate is written out above rather than summarized. Worse, per the design's own safety principle: a model that ignored
the instructions and did nothing produced *a recording of a model ignoring
instructions*, which is real evidence of real CLI behavior, and the guard threw
that recording away rather than keep it — reacting to a frame's content and
aborting, not driving.

**The fix.** The neutral-recorder stage deletes the guard along with the rest
of `recording.rs`, per design §3.2 ("delete every frame check that aborts").
The ported `checklist`/`checklist-resume` scenario bodies
(`capture/record/scenarios/claude.rs`) record whatever the model actually does,
with no mutation count enforced. `fake_claude.rs`'s `checklist_no_tasks`
fixture is the case the guard used to bail on — a model that replies with plain
text and calls neither `TaskCreate` nor `TaskUpdate` — and the neutral recorder
now returns a successful capture holding every frame, evidence intact.

