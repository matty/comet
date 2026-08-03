# LAN-only feature status

This checklist records the current native product after removal of the hosted
runtime. It is not a compatibility promise for the former cloud architecture.

## Supported surfaces

| Area | Status | Notes |
| --- | --- | --- |
| Local Desktop | done | Embeds the local engine when no engine is running; otherwise attaches over localhost IPC. |
| Local TUI | done | Attaches to a local engine or auto-starts detached headless mode; `--no-spawn` is available. |
| Headless engine | done | Foreground operation plus optional launchd/systemd-user daemon management. |
| Desktop remote UX | done | Manual add/remove/reconnect, listener settings, pairing, trusted-client list and revoke. |
| TUI remote UX | done | Server-grouped direct remotes, server-scoped selection/routing, offline states. |
| Mobile remote UX | out of scope | Desktop and TUI are the supported clients for this release. |

## Local authority and direct remotes

| Requirement | Status | Evidence/behavior |
| --- | --- | --- |
| No hosted account gate | done | Fresh local engine starts without an account or runtime endpoint. |
| No cloud migration path | done | No `comet login`, `comet logout`, or `comet migrate`; no profile selector/copy/merge startup path. |
| Local instance authoritative | done | Each engine serves only resources owned by its local store. |
| Listener opt-in | done | LAN listener defaults disabled; enable and bind are explicit. |
| Manual endpoints | done | DNS, IPv4, and bracketed IPv6 plus a nonzero port; no discovery service. |
| Direct/non-transitive topology | done | A never consumes B's remote registry; A must configure C itself. |
| Offline without cached children | done | Server entry remains offline/unreachable while spaces, chats, and sessions are cleared. |
| Stable identity and trust | done | Persistent Ed25519 identity, TLS 1.3, pinned fingerprints, single-use expiring pairing secrets. |
| Active revocation | done | Revocation persists and closes the trusted client's live connection. |
| Full trusted-client control | done | Explicit operational allowlist covers agents, terminals, repositories/diffs, and attachments. |
| Administrative isolation | done | Remote callers cannot change listener/trust/remote configuration or target another server. |

## Operations

| Feature | Status | Notes |
| --- | --- | --- |
| Agent sessions | done | Create, queue/steer, cancel, archive, resume, and stream mock/Claude/Codex harness events. |
| Terminals | done | Open, watch, write, resize, close; ownership checked by authoritative service. |
| Repositories | done | Repository, ref, status, diff, commit-file, and worktree operations. |
| Attachments | done | Chunked upload/commit/read with hash and local-chat ownership checks. |
| Server grouping | done | Equal resource IDs on different servers remain distinct in Desktop and TUI. |
| Recovery states | done | Retry/backoff for transport failures; explicit identity/version states require user action. |

## Distribution

| Requirement | Status | Notes |
| --- | --- | --- |
| Runtime works offline | done | Release endpoint is not used for engine authority, LAN connections, or local RPC. |
| Optional updater | done | `COMET_RELEASES_URL` affects release traffic only; failure is non-fatal. |
| Repository provenance | done | Release workflow, updater, and installer require `matty/comet` manifest attribution and checksums. |
| Distribution-only worker | done | Serves installer and `/releases/*`; hosted auth, rooms, relays, and runtime attachments are absent. |

## Deliberate limits

- LAN endpoints are configured manually; there is no mDNS, relay, traversal,
  registry, or internet-facing remote service.
- Trust has one full-control role. Fine-grained permissions are deferred.
- Remote content is not cached or synchronized for offline use.
- A separately configured direct connection is required for every remote.
- Mobile/iOS remote support is outside this release.
- Release downloads are optional internet traffic and remain separate from the
  LAN-only runtime.
