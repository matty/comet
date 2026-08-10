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

## Why it is not fixed here

**It fails in the safe direction.** The bad outcome is a turn that stays parked
and answerable, not a valid run killed or a decision invented. Compare the
alternative shape of this bug: an administrative call *not* counting while a
Desktop client shares the same code path would expire runs in front of a present
user.

**Telling the two apart is a design change.** It needs the client to declare
what it is — a supervising viewport versus a one-shot administrative call — which
means a new field in the hello handshake, a `PROTOCOL_VERSION` question (D12's
cost), and a default for older clients that is wrong either way. That is a
slice's worth of decision, not a patch, and slice 1.9 deliberately added no new
user-facing or wire-facing control.

## How to apply, if it ever needs closing

Carry the intent on `ServerHello` (a `presence: bool`, defaulting to `true` so an
older client keeps today's behavior) and have `attached()` skip the lease when a
client declares itself administrative. `comet status` and the `remote`
subcommands are the only callers that would set it, and they already share one
constructor.

Until then: an operator who wants the bound to actually fire must not poll a
daemon on a schedule shorter than the bound.
