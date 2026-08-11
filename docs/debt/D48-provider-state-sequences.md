# D48 — named scenarios do not cover event-order state

Each provider fake selects one hand-written transcript from the prompt. This is
clear and useful for feature examples, but JSON-RPC responses, server requests
and notifications can interleave in more orders than those transcripts name.
Claude's stream has the same problem around steering, approval responses,
repeated init frames and terminal output.

**Why this is debt.** A parser or session loop can pass every named scenario yet
hang, double-complete or misroute a response under a valid ordering the fake
never emits. Adding another scenario for every ordering will eventually make the
fixtures harder to audit than the production state machine.

Fix shape: after D45 supplies reusable fault actions, model a small set of legal
provider states and generate bounded event sequences. Assert invariants rather
than exact transcripts: request ids resolve at most once, no normal event follows
a terminal result, cancellation settles, pending approvals fail closed, and the
child is reaped. Start with Codex because its request ids and JSON-RPC states make
the model explicit; add Claude only where generated ordering buys coverage over
focused examples.

This is later-stage hardening, not a replacement for literal capture tests or the
readable happy-path scenarios. D8–D10 remain the owners of diagnostic policy for
unknown and malformed frames.
