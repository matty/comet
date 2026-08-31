# D136 — orphan Codex turn notifications

## The ruling

After `turn/completed`, turn-scoped provider content has no transcript owner.
Comet drops it rather than buffering it for a later turn or inventing an
unattached transcript event, either of which would misattribute old provider
content to a user request.

The gate covers only `item/agentMessage/delta`,
`item/reasoning/textDelta`, `item/reasoning/summaryTextDelta`,
`turn/plan/updated`, `item/started`, `item/completed`, and
`thread/tokenUsage/updated`. Session-scoped `account/rateLimits/updated`,
`mcpServer/*`, and `thread/environment/disconnected` notices remain
deliverable after completion, as do lifecycle, error, malformed-frame, and
transport handling.

## The follow-up fence

A rejected `turn/steer` response is resolved outside the FIFO notification
queue. It can therefore be followed by the old turn's `turn/completed` and
an orphaned delta before the fallback `turn/start` response. The fallback now
immediately adopts that response's turn id and emits `Steered`, restoring
normal follow-up steering and interrupt ownership. Separately, a content fence
keyed by that id drops turn-scoped content until the matching ordered
`turn/started` notification clears only that fence. The old queued delta is
therefore discarded rather than attached to the follow-up entry. Lifecycle and
completion notifications stay live while the fence is set: a missing or delayed
`turn/started` cannot block `Done`, and a later follow-up response replaces any
stale fence. This is an ordering fence, not a buffer or a synthetic owner.

The real spawned fake-Codex regressions
`codex_orphaned_turn_events_after_done_are_dropped_but_session_notices_survive`
and `rejected_steer_drops_an_orphan_queued_before_the_follow_up` prove both
boundaries: the first preserves a post-completion 85% rate-limit notice while
dropping the orphan, and the second permits only follow-up text after `Steered`.

No `PROTOCOL_VERSION` change is needed. The fix neither changes a wire shape
nor expands an event or consumer contract; it drops provider content that has
no valid owner before it reaches the existing consumer.
