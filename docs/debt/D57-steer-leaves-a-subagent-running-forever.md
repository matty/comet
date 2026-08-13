# D57 — a subagent left running across a steer is `Running` forever

**Deliberate, per a user ruling on this branch, not an oversight — recorded here because the
three sites that produce this residual state don't reference each other, so a reader who finds
only one of them will conclude it's a bug and go looking for the other two.**

## The three sites

**1. A steer finishes the pre-steer segment `Complete`, not `Streaming`.**
The free module-level fn `drive_run` (`crates/engine/src/sessions.rs:1488`, not an associated fn
of `SessionsEngine`)'s `Steered` arm (`:1749-1784`) calls `finish_segment(...,
MessageStatus::Complete)` at line 1774. Whatever `MessagePart::Subagent` is sitting in that
segment — including one still `Running` — is written into a **finished** entry.

**2. `cancel_running_subagents` is deliberately not called at that boundary.**
`crates/engine/src/sessions.rs:1440-1467`, in `cancel_running_subagents`'s own doc comment, spells
out why: a steer only queues a line for the CLI's stdin (`SessionsEngine::steer`, the harness's
steer arm in `comet_harness::claude`) — Comet sends nothing that would abort a subagent still
running at that point, and what the CLI process does with it from there is uncaptured, which the
comment is careful to mark as an inference rather than an observation: it "most likely keeps
running and completes, but that is an inference, not an observation." Stamping it `Cancelled` at
the steer boundary "would assert an outcome nobody observed." The function runs only from the
`Done` arm (a run that genuinely ended), never from `Steered` — see the comment's own contrast with
`expire_open_approvals`, which is deliberately called from both boundaries because approvals are a
different case: dropping a parked resolver *causes* the "not approved" reading, so `Expired`
becomes true by the sweep's own act regardless of where it runs. Cancelling a subagent has no
equivalent causal act — the sweep would only be a label, not an event — which is exactly why it is
confined to the one boundary where the label is true.

**3. The crash-path sweep (task 7) can't reach it either, for an unrelated reason.**
`DocHost::mark_abandoned_streams` (`crates/engine/src/doc_host.rs:159-172`) is the recovery sweep
that stamps a device's abandoned entries `Aborted` after an engine restart. Its guard at line 163
is `entry.status == Some(MessageStatus::Streaming)`. The pre-steer segment is `Complete` (site 1),
so this sweep — added specifically to catch a `Subagent` left `Running` by a crashed process
(`doc_host.rs:1012-1017`'s own test comment names that exact case) — never examines it. Two
independent guards, each correct on its own terms, compound into one segment neither one covers.

## The residual state, and what proves it

A subagent still `Running` when the user steers stays `Running` in that segment for the life of
the chat. Nothing downstream revisits it: `fold_event_into_parts` clears the accumulator on
`Steered`, so a later `SubagentUpdated` for that `task_id` lands in a new, unrelated accumulator
and is dropped as "an update for a `task_id` this accumulator never saw" (`parts.rs:429-432`); the
`Done` arm's own `cancel_running_subagents` call only ever sees the **current** segment's parts,
never a prior, already-finished one.

Proven end-to-end, not just at the fold-unit level: `crates/engine/tests/e2e.rs:2614-2675`,
`a_steer_over_a_running_subagent_does_not_stamp_it_cancelled`, drives a real dispatch → subagent
start → steer → post-steer completion through `SessionsEngine` and asserts the persisted
`MessagePart::Subagent`'s status is still `Running`. Its `wait_for` gates on transcript TEXT
containing `"steered"`, not on the run reaching `Done` — the coalesced flush persists that text
before the fold that finally settles the entry, so the assertion runs slightly ahead of `Done`,
not after it.

The fold-level twin, `sessions.rs:2561`'s `a_steer_boundary_expires_approvals_but_leaves_a_running_
subagent_alone`, calls `expire_open_approvals` **directly** on a hand-built `folded` slice — it
does not drive the `Steered` arm's dispatch path at all, so it would stay green even if that arm
gained a `cancel_running_subagents` call alongside `expire_open_approvals`. It is the e2e test
above, not this one, that actually pins the arm calling `expire_open_approvals` alone.

## Why this is accepted, not a bug to fix

A stale `Running` reading is honest about what is unknown; a manufactured `Cancelled` would assert
an outcome Comet never observed. The alternative — polling or otherwise tracking the child process
across a steer to learn its real fate — is out of scope for this slice and was not attempted.

## How to apply, if it ever needs closing

Any fix has to answer the question `cancel_running_subagents`'s doc poses and currently declines:
how would Comet learn a steered-past subagent's real outcome, rather than merely relabeling
ignorance as a different guess? Two directions, neither explored here: (a) give
`mark_abandoned_streams` (or a sibling sweep) a mode that also visits `Complete` entries carrying
an orphaned `Running` `Subagent` part, stamping it something distinct from both `Cancelled` and
`Running` — an honest "outcome unknown" state, which needs a new `SubagentStatus` variant and a
`PROTOCOL_VERSION` question; or (b) have the harness keep listening for that `task_id`'s frames
after a steer and attribute them to the orphaned segment out-of-band, which conflicts with
`fold_event_into_parts` clearing the accumulator on `Steered` by design and would need that design
revisited first.
