# D29 — an administrative CLI call counts as a supervisor

Presence counts **connections**, not watching humans. `EngineRpc::attached`
(`crates/engine/src/rpc.rs`) hands out a `PresenceLease` for every RPC
connection, which is what makes the hook impossible to forget for a new
transport — and it is also why a script counts.

`comet status`, `comet remote list`, `comet remote add`, `comet remote clients`
and the rest all reach the daemon over the same localhost IPC websocket the
Desktop app uses (`apps/comet/src/remote_cli.rs`, `local_client` →
`connect_ws`). Each one therefore:

1. clears `unattended_since` on connect, and
2. stamps a **fresh** stretch from the moment it exits.

## The failure worth recording

A monitoring cron running `comet status` every five minutes means
`unattended_since` never survives five minutes. A parked approval on that daemon
**never expires** — precisely the outcome slice 1.9 exists to prevent — and
nothing in the log says why, because from the engine's point of view a
supervisor keeps arriving.

## Resolution

The first client frame is now a connection hello carrying `supervising`. Every
ordinary constructor sends `true`; the shared `remote_cli::local_client`
constructor used by `comet status` and every online `comet remote …` command
sends `false`. `serve_connection_guarded` delays its one `attached()` call until
that frame is decoded and carries the declared intent through every transport.
`EngineRpc` returns no `PresenceLease` for an administrative connection, so it
neither clears nor restarts `unattended_since`.

Compatibility stays safe in both directions. A new server treats an old first
request, a missing hello, and a hello missing `supervising` as supervising. An
old server ignores the new hello frame and continues counting the administrative
connection, which is the prior fail-safe behavior: expiry may be delayed, never
caused in front of a present user.

That additive field does not independently require a `PROTOCOL_VERSION` bump
under the constant's own rule, because an older peer ignoring it preserves the
safe historical behavior. This change ships at version 13 because D26's new
`DoneStatus::Expired` enum variant does require the all-or-nothing decode bump.

Literal wire tests pin the old/missing defaults and the administrative value;
transport, engine, and CLI-constructor tests pin the lease and timestamp effects.
