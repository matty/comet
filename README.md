# Comet

Comet is a native Desktop and terminal UI for running Claude Code and Codex
sessions on this machine or on explicitly configured Comet machines on your
local network.

![Comet running a Claude Code session](docs/screenshot.png)

Comet has no account, hosted control plane, or cloud synchronization service.
The Comet instance running on each machine is authoritative for that machine's
agents, repositories, terminals, chats, sessions, and attachments.

## Run locally

Running the Desktop app starts an engine in the app when no local engine is
already running. `comet tui` attaches to an existing local engine or starts a
detached `comet headless` process itself. You do not need to install a daemon
for ordinary Desktop or TUI use.

An installed daemon is useful only when the engine must survive logouts or run
continuously on a headless machine:

```bash
comet daemon install
comet daemon status
```

The Linux installer installs this optional systemd user service because its
target is an always-on headless host:

```bash
curl -fsSL https://comet.zeron.sh/install.sh | sh
```

Day-to-day commands:

```bash
comet                    # Desktop UI; embeds the local engine when needed
comet tui                # terminal UI; attaches or auto-starts an engine
comet headless           # engine only, in the foreground
comet status             # local engine, listener, trust, and remote status
comet update             # optional release check/download
```

There is no `comet login`, `comet logout`, or `comet migrate` command. Existing
cloud profiles are not imported or merged into the local store.

## Connect two Comet machines

Incoming remote connections are disabled by default. On server B, choose a LAN
bind address, enable listening, then create a five-minute single-use pairing
secret:

```bash
comet remote listen --enable --bind 0.0.0.0:27655
comet remote pair
```

On client A, add B using the hostname or IP address and port. The secret is
prompted for without placing it in shell history:

```bash
comet remote add buildbox.local:27655 --name "Build box"
# IPv4: comet remote add 192.168.1.20:27655
# IPv6: comet remote add '[fe80::20]:27655'
comet remote list
```

Allow inbound TCP traffic to the selected port in B's host firewall. Do not
expose it to the public internet; this release is intended for trusted local
networks. Pairing uses TLS 1.3 and pins both device identities, but a paired
client receives full operational control: it can run agents, operate terminals,
read repositories and diffs, and transfer chat attachments on B.

Review and revoke trust on B with:

```bash
comet remote clients
comet remote revoke sha256:<client-id>
```

Desktop and TUI group data by server. A stopped or unreachable server remains
listed as offline/unreachable, but its child spaces, chats, and sessions are
cleared; Comet does not cache or reconcile remote content.

Connections are direct and non-transitive. If A connects to B and B connects to
C, A cannot see or control C. A must configure and pair C separately.

## Updates are separate

Local and LAN operation requires no internet connection. Installer and update
traffic is a separate optional distribution path. Release manifests are pinned
to the `matty/comet` repository. `COMET_RELEASES_URL` may select a mirror, but
the mirror must still serve a correctly attributed and checksummed manifest.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the trust and process boundaries and
[docs/PARITY.md](docs/PARITY.md) for the implemented feature surface.

Licensed under the [MIT License](LICENSE).
