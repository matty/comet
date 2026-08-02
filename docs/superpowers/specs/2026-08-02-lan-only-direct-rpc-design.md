# LAN-Only Direct RPC Design

**Status:** Approved

**Date:** 2026-08-02

## Summary

Comet will stop depending on the hosted runtime service. Every running Comet
engine will be authoritative for the agents, repositories, terminals, spaces,
sessions, and documents stored on its own machine. Desktop and TUI clients may
connect to explicitly configured Comet engines on the local network by
hostname or IP address and port.

Connections are direct and non-transitive. If A is configured to connect to B,
and B is configured to connect to C, A cannot discover, view, or control C
through B. A must configure and pair with C independently.

The production runtime will remove WorkOS, organizations, Cloudflare Durable
Object document rooms, device relays, cloud nudges, and R2 attachment storage.
The hosted installer, release artifacts, and update checks remain as a separate
distribution service and are not required for local or LAN operation.

The first implementation supports the desktop and TUI. The iOS app, service
discovery, internet relaying, NAT traversal, offline replicas, and fine-grained
remote roles are outside this scope.

## Goals

- Make local engine startup and operation independent of any account or hosted
  runtime service.
- Allow a user to configure a remote by `hostname-or-ip:port` and connect to it
  directly over the local network.
- Make remote listening opt-in through an “Enable remote connections” setting.
- Authenticate paired devices mutually and encrypt all LAN traffic.
- Keep every engine authoritative for only its own local state.
- Enforce non-transitive access structurally rather than relying on UI policy.
- Present local and directly configured remote servers together in desktop and
  TUI, with all identifiers qualified by their authoritative server.
- Keep configured remotes visible as offline or unreachable without caching
  their spaces, sessions, or transcript data.
- Preserve existing locally owned data through a conservative, reversible
  migration.
- Retain installer and update delivery as an independent internet feature.

## Non-goals

- Automatic discovery through mDNS, Bonjour, broadcast, or a registry.
- Connections over a hosted relay or other internet rendezvous service.
- UPnP, port mapping, NAT traversal, or a claim that the application can
  prevent an administrator from manually exposing its port to the internet.
- Transitive routing, proxying, or discovery between configured servers.
- Peer-to-peer CRDT replication or offline copies of remote state.
- A local reimplementation of the Cloudflare edge service.
- Read-only/operator/admin permission roles. A paired client has one full
  remote-control role in the first release.
- LAN pairing in the iOS app.
- Moving release artifacts or disabling update checks.

## Architectural decision

Comet will use direct authenticated RPC with client-side federation.

The alternatives were rejected for the following reasons:

1. Direct CRDT replication covers documents but not terminals, repositories,
   filesystems, or live agent control, so it would still require RPC. It would
   also copy data away from its authoritative machine and could leak C-derived
   state from B to A.
2. Running a miniature edge service on every engine would preserve room,
   broker, relay, and authentication infrastructure that this change is meant
   to remove. It would also blur the boundary between B's state and state B
   relays for another machine.
3. Direct RPC is a smaller fit for the ownership model, but the existing
   localhost RPC service is too privileged to expose unchanged. The selected
   design therefore adds a separate, default-deny LAN RPC surface and performs
   federation in clients rather than engines.

## Runtime topology

Every engine exposes up to two distinct listeners:

1. **Local IPC** binds to `127.0.0.1` and retains the complete trusted RPC
   surface used by viewports on the same machine. It includes remote registry,
   pairing-client, listener, and local administrative operations.
2. **LAN RPC** exists only while “Enable remote connections” is enabled. It
   binds to a configured interface and port, requires an authenticated paired
   identity, and dispatches through a restricted `RemoteRpcService`.

Desktop and TUI connect separately to the local engine and every configured
remote:

```text
Desktop/TUI on A
 ├── localhost IPC ── A engine
 ├── direct LAN RPC ─ B engine
 └── direct LAN RPC ─ C engine
```

B's LAN service returns only state authoritative to B. It never exposes B's
remote registry, never forwards a request through B's outbound connections,
and never returns a connection handle for another server. B's private B-to-C
configuration has no effect on the A-to-B connection.

## Authority and data ownership

The existing Loro session and workspace documents remain useful as local
persistence and observation models. Their cloud room transports are removed.
Documents load from and save to the authoritative machine's local store.

An engine is authoritative for:

- Spaces whose owning `device_id` is that engine's local device.
- Chats and agent sessions hosted on that engine.
- That engine's repositories, worktrees, diffs, terminals, uploads, harnesses,
  and configured agent accounts.
- Commands submitted directly to that engine and their results.

Remote documents are not replicated into the connecting machine's document
store. When a connection ends, the client removes that server's streamed
spaces, chats, sessions, messages, and diffs from the active combined model.
Only the configured remote entry and its last connection error remain.

## Remote registry

A local-only `RemoteRegistry` persists outbound server configuration. It is
owned by the local engine so desktop and TUI on the same machine see the same
configuration.

Each entry contains:

- Stable remote `server_id` derived from the pinned public identity.
- User-provided hostname or IP address and port.
- User-editable friendly name.
- Pinned server public identity.
- Last known protocol version and connection status.
- Creation and last-success timestamps for diagnostics.

Remote-registry mutation and pairing RPCs are localhost-only. The LAN service
does not expose the registry, even to paired clients.

Registry writes are atomic. Removing a remote deletes its endpoint and pin. It
does not delete the installation-wide private identity, send a request to, or
mutate the remote server.
Revocation is performed separately on the server that granted access.

## Identity, pairing, and transport security

Each Comet installation creates a persistent Ed25519 identity key and a
self-signed X.509 identity certificate. Its stable `server_id` is the
SHA-256 fingerprint of the public key. The private key is stored with
restrictive platform-appropriate filesystem permissions and never leaves the
machine. Normal and pairing transports require TLS 1.3.

Remote pairing is server-initiated:

1. B enables remote connections.
2. The user opens a short-lived pairing session on B.
3. B displays a single-use 128-bit random pairing secret encoded as grouped
   Base32 text.
4. On A, the user adds B's `hostname-or-ip:port` and enters the secret.
5. A and B exchange their public identities through a handshake authenticated
   by the secret.
6. A pins B's server identity. B adds A's client identity to its allowlist.
7. The pairing session is consumed on success or destroyed on expiration.

The secret authenticates the complete pairing transcript rather than being sent
as a bearer value. A sends its public identity and a fresh client nonce. Both
sides compute HMAC-SHA-256 confirmation values over a domain separator, the
server certificate fingerprint, the client public-key fingerprint, and fresh
client/server nonces. B stores A's public identity only after verifying A's
confirmation; A pins B only after verifying B's distinct confirmation. This
binds the out-of-band secret to both identities and prevents a TLS intermediary
from substituting a certificate. The secret is erased when used or expired.
Pairing attempts are rate-limited and logged locally. Unauthenticated pairing
traffic is accepted only while an explicit pairing session is active and
reveals no engine or user data.

Normal RPC connections use TLS 1.3 with mutual certificate authentication. A
validates B's public-key fingerprint against its pin. B uses a pinned-key client
certificate verifier and accepts A only while A's public key remains in B's
paired-client allowlist. Revocation closes A's active connections and rejects
future connections. A server identity mismatch never silently updates the pin
and is reported as `Identity changed`; the user must remove and pair the server
again.

The first release has one authorization role. Pairing grants control comparable
to the local Comet user, including agent execution, terminal control, repository
operations, and access to locally hosted chat content. The UI and documentation
must state this before displaying a pairing secret.

## Restricted LAN RPC service

The LAN dispatcher is default-deny. A method must be enumerated in the remote
allowlist and covered by an authorization test before it can be called over the
LAN listener.

The initial allowlist supports:

- Server identity, capability, protocol-version, and health handshakes.
- Watching locally authoritative spaces, chats, sessions, messages, diffs,
  models, repositories, and live status.
- Creating and managing spaces and chats on the server.
- Sending, steering, interrupting, and answering agent interactions.
- Opening and controlling terminals on the server.
- Repository, branch, worktree, and diff operations needed by remote sessions.
- Uploading attachments directly to the server's local upload area, preserving
  existing size, path-jail, and hash validation.
- Agent operations needed to use accounts already configured on that server.

The LAN service excludes:

- Reading or modifying the server's outbound remote registry.
- Creating pairing sessions, listing paired clients, revoking clients, changing
  device identity, or changing listener configuration.
- Daemon installation, removal, start, stop, or restart.
- Applying Comet updates remotely.
- Exporting authentication secrets or agent credentials.
- Forwarding calls to a target device or returning another server connection.
- Any unclassified internal/debug RPC method.

Remote RPC parameters that currently contain `targetDeviceId` must not trigger
relay behavior. The LAN boundary either rejects such targeting or requires it
to match the receiving server's local device, depending on the method's normal
shape.

## Client-side federation

A shared `RemoteConnectionManager` is used by desktop and TUI. It watches the
local engine's `RemoteRegistry`, maintains one direct connection per entry, and
isolates each connection's subscriptions and failures.

On connection, it:

1. Resolves and connects to the explicitly configured endpoint.
2. Negotiates protocol version and capabilities.
3. Verifies the pinned server identity and performs mutual authentication.
4. Starts subscriptions for that server's authoritative resources.
5. Qualifies every incoming identifier with `server_id` before it enters
   combined client state.

The combined identifier for a remote object is conceptually
`(server_id, object_id)`. User actions retain that origin and are routed through
the same connection. A remote cannot overwrite local or another remote's UI
state by supplying a colliding unqualified identifier.

Aggregation must not be placed inside an engine-facing LAN service. This avoids
turning an engine into a proxy and makes non-transitivity a consequence of the
connection graph.

## Connection state and failure handling

The user-visible connection states are:

- `Connecting`
- `Online`
- `Offline`
- `Unreachable`
- `Identity changed`
- `Incompatible version`

Remote failures are isolated. A failed B connection cannot prevent A's local
state or C's connection from loading. Transient failures use bounded
exponential backoff with jitter. Identity and protocol incompatibility failures
do not retry continuously.

When a remote connection drops, the active combined model immediately removes
that server's streamed content and retains its registry entry with an offline
state. Reconnection rebuilds state from fresh subscriptions; there is no
offline remote cache to reconcile.

A LAN listener bind failure does not prevent the local engine from operating.
It leaves remote listening inactive and reports the exact address/port error in
settings, CLI status, and logs. Invalid, malformed, or oversized requests are
rejected before dispatch.

## Desktop and TUI product surface

Desktop adds a Remote Connections settings section with:

- “Enable remote connections,” off by default.
- Bind address and port.
- A control to begin pairing and display the expiring secret.
- A paired-client list with revoke controls.
- A configured-server list with add, rename, reconnect, and remove actions.
- Explicit connection and identity-error status.

The TUI uses the same combined server model and exposes remote status and
server grouping. Headless and terminal configuration is supported by CLI:

```text
comet remote add <host:port>
comet remote list
comet remote remove <name-or-id>
comet remote listen enable --port <port>
comet remote listen disable
comet remote clients
comet remote revoke <client-id>
```

Exact interactive prompts and optional flags may be refined during planning,
but these operations and their localhost-only authority are required.

The sidebar/navigation model groups spaces and sessions by authoritative
server. An unreachable server remains visible without stale child content.

## Removal of account and hosted runtime concepts

The following production concepts are removed:

- `comet login` and `comet logout` commands.
- WorkOS authorization, token exchange, refresh, session persistence, and JWKS
  verification.
- User and organization selection gates.
- Authentication and organization RPC methods and UI.
- Cloud session/workspace room joins, snapshots, tails, and repair endpoints.
- DeviceRoom host relays, client links, durable nudges, and target-device
  forwarding.
- R2 runtime attachment access.
- `COMET_EDGE_URL`, `COMET_EDGE_TOKEN`, `COMET_WORKOS_CLIENT_ID`,
  `COMET_WORKOS_API_BASE`, `COMET_ORG_ID`, and `COMET_USER_ID` runtime behavior.

`comet headless`, the desktop engine, and the daemon start without an account
gate. `comet status` reports engine state, listener state, paired-client count,
and configured remote status rather than auth and edge state.

“Workspace” remains a local Comet organizational concept for spaces and chats;
it no longer denotes a WorkOS organization or cloud document namespace.

## Existing-data migration

The existing store is scoped under `orgs/{org_id}/{user_id}` and may contain
cloud-replicated rows from several devices. Migration must not expose those
cached rows as if they were authoritative local data.

On first local-only startup:

1. Select the source profile referenced by the existing `session.json`.
2. If no usable session exists and exactly one legacy profile exists, select
   that profile.
3. If several profiles exist and none can be selected safely, stop with an
   actionable command that explicitly selects the source profile. Never merge
   profiles automatically.
4. Copy the selected source into a new local-authoritative store. Keep the
   legacy source untouched for rollback.
5. Rebuild the local workspace index to include only spaces, chats, device rows,
   and session-status rows owned or hosted by the machine's existing
   `device_id`.
6. Preserve locally hosted session documents, run journals, repository metadata,
   uploads, agent-account configuration, and the existing `device_id`.
7. Verify the new store can be opened and its ownership invariants hold.
8. Atomically write the migration completion marker.
9. Remove the obsolete WorkOS `session.json` only after successful completion.

Migration is idempotent. A crash before the completion marker causes the staged
destination to be rebuilt or resumed safely; it never changes the legacy
source. The application must not silently start with an empty store when a
legacy profile exists but cannot be selected or migrated.

## Attachments

Remote attachments use the existing chunked host-upload concept over the direct
LAN RPC connection. The receiving authoritative server writes to its local
uploads area and enforces size limits, content hashes, and path jailing. Prompt
attachments continue to reference paths on the machine running the agent.

No attachment is uploaded to R2 or retained on the connecting client as a
cloud/offline replica. Attachment availability therefore follows the server's
availability.

## Distribution service separation

Installer and update delivery remain available over the internet but are not a
runtime dependency. The distribution deployment retains only installer,
release manifest, and immutable artifact routes.

The updater takes a release-specific base URL, exposed through a name such as
`COMET_RELEASES_URL`, instead of sharing `COMET_EDGE_URL` with runtime
networking. Local operation, LAN pairing, and direct RPC continue when the
distribution endpoint is unreachable.

After the runtime cutover, the edge deployment removes WorkOS routes, JWT
verification, SessionRoom and DeviceRoom Durable Objects, runtime attachment
routes, and their bindings. Release artifacts may continue to use the existing
hosting technology as an implementation detail.

## Protocol compatibility

The LAN handshake includes an explicit protocol version and capability set.
Peers fail closed with `Incompatible version` when they cannot agree on the
minimum safe remote API. Compatibility is checked before subscriptions or
mutating RPC calls begin.

The protocol must preserve the strict LAN allowlist across version skew. A new
internal RPC method is not remotely callable merely because an older peer can
name it.

The protocol is designed without desktop-specific types so a future iOS client
can implement it, but iOS support is not an acceptance criterion for this work.

## Rollout strategy

The work is staged so boundaries are testable before the hosted runtime is
removed:

1. Add local-authoritative storage and migration behind an internal cutover
   boundary.
2. Add persistent device identity, pairing, paired-client storage, remote
   registry, and the restricted LAN listener.
3. Add the shared multi-server connection manager and namespaced combined model.
4. Integrate the combined model and remote settings into desktop and TUI.
5. Switch normal startup to local-only operation; remove login, organizations,
   cloud room clients, relays, and runtime attachment access.
6. Reduce the hosted edge to distribution routes and remove dead dependencies,
   configuration, tests, and documentation.

No intermediate stage may expose the unrestricted localhost RPC service on a
LAN interface.

## Testing and acceptance criteria

### Unit tests

- Remote registry persists endpoints and pins atomically.
- Pairing secrets expire, are single-use, and rate limits are enforced.
- Pairing transcript authentication fails on identity or nonce substitution.
- Revoked clients fail authentication.
- Server identity changes never update a pin automatically.
- Every LAN-callable RPC method is explicitly allowlisted; administrative and
  unknown methods are rejected.
- Combined identifiers include `server_id` and cannot collide across servers.
- Migration selects the correct legacy profile, rejects ambiguity, preserves
  local ownership, excludes foreign device rows, and is idempotent.
- Release URL resolution is independent of removed edge configuration.

### Integration tests

- A can pair with and operate B over a direct connection.
- Given A-to-B and B-to-C, A cannot discover, subscribe to, or call C through B.
- A can access C only after configuring and pairing with C independently.
- A wrong or expired pairing secret reveals no engine data.
- Revocation closes an active connection and prevents reconnection.
- Disconnecting B clears B's streamed content while retaining an offline entry.
- A port conflict leaves local operation working and reports listener failure.
- Protocol incompatibility fails before state or operations are exchanged.
- Remote agent, terminal, repository, diff, and attachment flows operate against
  the authoritative server.
- Unreachable former WorkOS, room, relay, and R2 endpoints do not affect local
  or LAN functionality.
- An unreachable distribution endpoint affects only update status.

### UI tests

- Desktop and TUI group local and remote resources by server.
- Two servers with identical raw object IDs remain distinct.
- Connecting, online, offline, unreachable, identity-changed, and incompatible
  states render correctly.
- Listener enablement is opt-in and pairing warns about the granted authority.
- Removing a remote removes its content and trust record without affecting
  other servers.

### Security acceptance

- The LAN port is closed until remote listening is explicitly enabled.
- Unauthenticated clients cannot access health details beyond the minimum
  pairing transport response and cannot dispatch RPC methods.
- Paired clients cannot invoke excluded administration or proxy methods.
- Oversized and malformed frames are rejected before dispatch.
- Private identity keys and trust registries use restrictive storage
  permissions.
- Logs do not contain private keys, reusable credentials, or pairing secrets.

## Documentation changes

README and architecture documentation must describe:

- Local-first, per-machine authority.
- Manual direct remote configuration.
- Non-transitive connection behavior.
- Listener opt-in, pairing, revocation, and the power granted to paired devices.
- Offline/unreachable behavior and the absence of remote caching.
- Removal of account login and cloud runtime requirements.
- The separate, optional internet dependency for installation and updates.
- Firewall and trusted-LAN considerations, without promising that Comet can
  prevent external port forwarding performed outside the application.

## Final acceptance statement

The design is complete when a fresh Comet installation can operate locally
without an account or hosted runtime service; desktop and TUI on A can directly
pair with and control explicitly configured B and C servers over encrypted LAN
connections; B's connection to C grants A no visibility or access to C; remote
failures do not affect local operation; and only installer/update delivery
retains an optional hosted dependency.
