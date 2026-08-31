# D49 — provider subprocess coverage crosses the engine boundary

This was coverage debt, not a discovered runtime defect: the provider and
engine layers already behaved correctly, but their separate tests did not prove
the selected path end to end.

The focused Codex suite now starts the fake executable and carries the selected
contracts through the harness, engine authority and RPC. It covers:

- live, paged Codex model discovery and the explicit empty commands endpoint;
- native rejected-resume fallback to a fresh durable session;
- deny-and-interrupt approval through RPC transcript delivery; and
- durable journal, session, and document terminal state.

The scope stays deliberately narrow. Protocol frame matrices remain
harness-owned, and Claude attachments remain D43-owned. This is wiring and
authority coverage, not a second provider protocol suite.
