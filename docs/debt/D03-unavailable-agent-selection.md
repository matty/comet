# D3 — corrected 2026-08-08: the selection is by design

0.2a logged this as "an unavailable agent can still be the selected one" and
deferred it to Phase 1. **The premise was wrong.** A session is locked to the
agent it was created with, by design — you cannot change agents mid-session. So
an unavailable agent holding the selection is the correct and only possible
behaviour, not a bug, and there is nothing for Phase 1's `RuntimeMode` picker to
fix here. Do not "fix" it.

Two things stop this from being a hole:

- New sessions can't reach the state. 0.2's picker greys out an unavailable
  agent and `pick_harness` refuses it, so the lock is only ever taken on an agent
  that probed available.
- An existing session whose CLI later disappears fails visibly on send.
  `sessions.rs`'s module contract is that every dying path carries its own
  visible error, spawn failure named among them, and 0.2a rewrote that copy to
  "Agent CLI not found: …" with the install/override action in it.

**Residual, genuinely minor:** availability is read in `crates/ui/src/pickers.rs`
and nowhere else in the UI, so a locked session learns its agent is gone *after*
the user types and sends, not before. Since the user cannot switch agents, the
only way out is install-or-override — which the post-send error already names.
So this is signal *timing*, not a dead end. Worth a composer hint if it ever
bites someone; not worth scheduling.

(The post-send behaviour is read from the module contract and the error copy,
not from a rendered run. Confirm by rendering before acting on it.)
