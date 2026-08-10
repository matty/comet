# D11 — the remembered request carries the previous turn's mode

`last_requests` is stamped at dispatch (`crates/engine/src/sessions.rs`) and is
preferred over `request_from_chat_row` on three paths: the steer-turned-run and
`RespondInput` dead-run fallbacks (`crates/engine/src/doc_host.rs`) and
auto-resume (`sessions.rs`).

Slice 1.1 made the chat row the persisted source of truth for `RuntimeMode` and
tested that read carefully. These three paths do not use it. So "the mode the user
chose" and "the mode the run executes under" can differ — and the divergence runs
in the permissive direction: a user tightens a chat to `approval-required`, steers
the live turn, and the steered run executes under the previous, looser mode.

**Inert today**, because nothing can store a non-default mode until the picker
lands. Documented in place rather than fixed, because changing dispatch behaviour
was out of scope for an inert slice.

**Owner: 1.8**, the slice that first makes a mode changeable mid-chat. The
decision it must write down rather than let emerge: does a mid-chat mode change
apply to a run already in flight, or only to the next send? Either answer is
defensible; the current behaviour is whichever path happens to run.

Note this is the same shape as the `set_chat_config` whole-value-replace hazard
that produced 1.1's one real bug — the chat row and the in-flight run are two
sources of truth for the same value, and only one of them is being kept current.
