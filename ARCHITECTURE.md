# Comet architecture

Comet is a local-first controller for coding agents. Every running engine owns
one local store and is authoritative for its own resources. There is no hosted
runtime, account gate, organization scope, relay, cloud document room, or
cloud-to-local migration path.

## Process model

```text
Desktop ── in-memory or localhost RPC ── engine A
                                      direct TLS RPC
                                            │
Desktop federation on A ─────────────── engine B
                                            │
                                     B's local store
```

`comet` opens the Desktop app. If no engine owns the configured data directory,
the Desktop embeds one and also exposes its localhost IPC endpoint. If an engine
is already running, the Desktop connects to it. No daemon installation is
required.

`comet headless` runs the same engine without a UI. `comet daemon install` is an
optional launchd/systemd-user deployment for an always-on or headless machine;
it is not a prerequisite for Desktop operation. The data-directory instance
lock prevents two engines from owning one store.

## Authority and federation

An engine owns only its local spaces, chats, sessions, repositories, terminals,
uploads, attachments, and agent processes. Desktop uses `comet-client` to
combine the local engine and each explicitly configured remote into server
buckets. Entity IDs are interpreted with their server ID, so equal local IDs on
different machines do not collide.

The remote registry is local configuration. A client connects separately to
every configured `hostname-or-ip:port`; an engine never proxies access to its
own remotes. Consequently A-to-B plus B-to-C does not imply A-to-C. The client
does not subscribe to B's remote registry.

When a remote disconnects, the server row remains and becomes offline or
unreachable. All remote child state is removed immediately. Remote data is not
persisted, merged, replayed, or presented as current while disconnected.

## Listeners and trust

Local IPC and LAN RPC are separate listeners and service objects:

- Local IPC is bound to loopback and exposes local administration.
- LAN listening is disabled by default and must be enabled explicitly.
- Endpoints are manually configured DNS hostnames, IPv4 addresses, or bracketed
  IPv6 addresses with a nonzero port. There is no network discovery.
- The LAN service is a default-deny allowlist. It exposes operational methods,
  not listener, trust, or remote-registry administration.

Each installation creates a persistent Ed25519 identity. Pairing uses a
single-use, 128-bit secret with a five-minute lifetime. TLS 1.3 protects the
connection; handshake confirmation binds both identities and nonces, and the
result stores a SHA-256 public-key fingerprint at each end. A changed identity
is a terminal `identityChanged` state until the user explicitly reconnects or
re-pairs. Revoking a client updates persistent trust and closes its active
connection.

The initial permission model has one trusted-client role with full operational
control. A trusted client can start and steer agents, open/write/resize/close
terminals, inspect repositories/refs/diffs, and upload/read attachments owned by
this engine. Pair only devices whose operators should receive that power.

## Data and RPC boundaries

The engine stores local Loro documents and journals under the fixed local data
root. Startup neither reads legacy account sessions nor selects, copies, or
merges hosted profiles. There is no `login`, `logout`, or `migrate` CLI surface.

`comet-proto` defines server-scoped identifiers, endpoints, connection states,
handshakes, and entity payloads. `comet-rpc` provides localhost WebSocket RPC,
pinned TLS transport, and pairing. `comet-engine` owns storage and authoritative
operations. `comet-client` supervises direct connections, and `comet-ui` is the
supported remote UX surface.

`comet-proto`'s `PROTOCOL_VERSION` gates LAN pairing with an exact match. A new
*field* on an existing wire type normally stays additive when absence has a
genuinely conservative meaning. It still bumps when an older peer silently
ignoring the field would execute a materially different request — permission
mode and command-selected harness are the worked examples. A new *variant* of
an enum that crosses the RPC boundary inside a decoded container (such as
`MessagePart` inside a `TranscriptFrame`) also bumps, because the container
decode is all-or-nothing and the receiving side has no tolerant arm for an
unknown variant. A new RPC *method* needs no bump — an older peer answers
`UnknownMethod`, which the UI already translates into "restart to update".

## Network and release boundary

LAN control is intended for local networks. Operators must permit the selected
listener port through the server host's firewall and should not publish it to
the internet. Comet has no relay or traversal service.

Release distribution is the only optional Comet-operated online infrastructure
path. The engine does not depend on it: update failures affect update status
only. The distribution worker serves the installer and release artifacts, and
manifests/checksums are pinned to `matty/comet`. `COMET_RELEASES_URL` overrides
only where release files are fetched; it grants no runtime authority. Agent
CLIs, provider OAuth, and provider usage APIs are separate integrations and may
contact Claude, OpenAI, or another configured agent provider.

## Verification boundaries

- `crates/client/tests/federation_topology.rs` uses memory-backed RPC fixtures to
  prove client-level A/B/C topology, operational routing, explicit A-to-C
  configuration, and offline-without-cache state. It does not exercise network
  sockets, production `LanConnector`, pairing/TLS, persisted registries, or real
  engines.
- `crates/rpc/tests/secure_lan.rs` covers pins, pairing expiry/consumption,
  malformed frames, authentication, and identity changes.
- `crates/engine/tests/remote_access.rs` covers opt-in/bind behavior, port
  conflicts, revocation, the LAN allowlist, local ownership, terminal and
  attachment restrictions.
- Engine repository/terminal/upload and mock-harness suites cover the allowed
  operations themselves; Desktop tests cover server grouping/routing.

The iOS client is outside the LAN-remote scope for this release.

A physical two/three-machine pairing, TLS, reconnect, and host-firewall smoke
test remains a release-candidate gate because no in-memory topology test can
validate those deployed boundaries together.
