# D26 — `DoneStatus` cannot tell an unattended expiry from a user's Stop

**Closed with protocol version 13.**

`Sessions::expire_unattended` now calls the shared interrupt machinery with
`DoneStatus::Expired`; the public Stop path supplies `Interrupted`. The first
terminal reason wins, and `drive_run` applies it even when a provider reacts to
its cancelled approval resolver by sending its own `Done{completed}` before the
engine's grace deadline. That race existed in the original shape and made the
durable status even less reliable than this page first recorded.

`Expired` maps to the existing aborted document status, cancels still-running
subagent cards, and leaves the session idle. The existing actionable transcript
note remains the visible explanation; no new control or duplicate timeout copy
was added.

`DoneStatus` crosses the RPC boundary inside `AgentEvent::Done`, so the new
variant raises `PROTOCOL_VERSION` from 12 to 13. Literal tests preserve decoding
of the three older values and bind `"expired"`; the end-to-end expiry test binds
the journal status independently from the still-visible transcript note.
