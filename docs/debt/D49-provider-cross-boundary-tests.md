# D49 — provider subprocess tests stop at the harness boundary

The fake executables drive the Claude and Codex adapters directly. Engine tests
exercise discovery caches and session operations through in-process harness
doubles. No selected test starts a fake provider and carries its events through
the harness, engine authority, RPC encoding and client-facing reply in one path.

**Why this is debt.** Both layers can pass independently while disagreeing about
lifetime, interaction routing, cache invalidation or terminal state. This is most
valuable where process behaviour matters and an in-process double cannot stand
in for it.

Fix shape: add a small cross-boundary suite using injectable fake executable
paths. Cover only contracts that have failed or cross ownership boundaries:
model/command discovery, one approval decision, resume/fallback, cancellation,
and the final terminal state. Claude attachments are a candidate once D43 gives
the fake a launch/input assertion for them.

Do not duplicate every harness transcript through the engine and UI. Most frame
normalization belongs in the existing harness suites; the cross-boundary tests
should prove wiring and authority, not become a second provider protocol suite.
