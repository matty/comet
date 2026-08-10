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

