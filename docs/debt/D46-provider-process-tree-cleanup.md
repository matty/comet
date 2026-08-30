# D46 — cancellation proves only the direct child is gone

**Status: closed for the Claude and Codex adapters on both shipped platforms** ([PR #197](https://github.com/matty/comet/pull/197), 2026-08-30). The policy this page asked for was decided first and is stated in the row: reach the tree through an OS-native grouping established *at spawn* (a Unix process group signalled with `killpg`; a Windows Job Object with `KILL_ON_JOB_CLOSE`), never by scanning for processes afterwards — which is the hazard the last paragraph below warns about, and it stands. Two residuals: the Windows spawn-to-assign window is not atomic, and the ACP adapter still has no Job Object, which is now **D133**. Everything below is the original page, kept because it is the reasoning the fix was measured against.

The Claude and Codex wedge fixtures have no descendants. Their tests prove that
the fake executable is reaped, but a real provider owns longer-lived children:
shells, command-safety helpers and MCP servers.

Both platform helpers signal or terminate only the direct pid. Windows adds a
second blind spot: its graceful termination helper is intentionally a no-op, and
the ordinary PR gate runs on Ubuntu. Neither Unix descendant cleanup nor Windows
process-tree behaviour is continuously exercised.

**Why this is debt.** A cancelled session can look complete in Comet while a
provider-owned grandchild keeps running, holding files or consuming resources.
The current fixtures cannot distinguish that leak from clean shutdown.

Fix shape: add a native fixture that starts a grandchild, records both pids, then
ignores cancellation. The test should verify the required cleanup policy for the
whole provider-owned tree on each shipped platform. Decide that policy before
changing termination code; killing an arbitrary process tree is platform-specific
and must not reach processes the provider did not create.
