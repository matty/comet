# D102 — Grok's command push ships with three fields unevidenced, and the corpus cannot record Grok at all yet

**Status: open, two distinct blockers.** The first is a decision already made (redact three
decoded fields rather than publish this operator's personal skill inventory). The second is a
harder, unrelated finding discovered while trying to act on the first: `comet-provider-sanitize`
structurally rejects every Grok capture today, so neither blocker's resolution can currently reach
a promoted corpus directory. codex-acp and claude-agent-acp are unaffected by either and are
promoted (`crates/harness/tests/corpus/{codex-acp,claude-agent-acp}/`).

## Blocker 1 — the command push mixes Grok's own commands with the operator's personal skills

`normalize::commands()` (`crates/harness/src/acp/normalize.rs`) is a real, registered decode: it
reads `.params.update.availableCommands[].name`/`.description`/`.input.hint` off the last
`available_commands_update` frame and feeds the slash-command picker. On the machine this was
captured from, that frame is not Grok's own small built-in list — it is Grok's built-ins
*plus* every skill installed under this operator's `~/.agents/skills/`, `~/.claude/plugins/` and
its own `~/.grok/bundled/skills/`, by name, full description, and (on the `_meta` siblings) a raw
filesystem path (`_meta.path`) into this operator's home directory.

**Isolation was the first plan, and it does not work.** In order, what was actually tried and
established (2026-08-28), so the next attempt does not repeat all four:

1. **`USERPROFILE`/`HOME` do not move Grok's home at all.** Set both, confirmed the child process
   genuinely received the override (`[Environment]::GetEnvironmentVariable`), and `grok.exe du`
   still reported the real `~/.grok`'s actual disk usage (435.7 MB) — Grok resolves its home some
   other way, not through either variable.
2. **`GROK_HOME` does work**, and is the mechanism Grok's own bundled docs name ("override with
   `GROK_HOME`"; not in `--help`, only in the markdown embedded in the binary). Confirmed two
   independent ways: `grok.exe du` then reports `$GROK_HOME` with only freshly-created `docs`/
   `logs` (449 KB) instead of the real 435.7 MB, and a capture's `authMethods` genuinely lost the
   real home's cached OAuth token (`[cached_token, grok.com]` → `[grok.com]` only,
   `defaultAuthMethodId: null`).
3. **A seeded `auth.json` is required and sufficient for the handshake.** With no credential in
   the isolated `GROK_HOME`, `session/new` fails outright:
   `{"code":-32000,"message":"Authentication required","data":"no auth method id provided"}` — no
   command push at all, because the session never opens. Copying only `~/.grok/auth.json` (nothing
   else) — the same "seed with credentials only" pattern `provider-captures.md` already documents
   for Claude's `--claude-config-dir` — lets it complete.
4. **The command push still names this operator's personal skills anyway.** With `GROK_HOME`
   isolated and only `auth.json` seeded, `session/update`'s push carried 31 commands: 11 Grok
   built-ins and **20 entries under `~/.agents/skills/*` and `~/.claude/plugins/*`**, `_meta.path`
   included — Grok reads these from the real OS user profile, not from `GROK_HOME`.
5. **The one documented mitigation had no measurable effect.** Grok's bundled docs also name
   `GROK_CLAUDE_SKILLS_ENABLED`/`GROK_CURSOR_SKILLS_ENABLED`/`GROK_CODEX_SKILLS_ENABLED` config
   toggles ("Grok scans the Claude and Cursor skill directories by default. To stop scanning a
   vendor, set ... to `false`"). Tried both `false` and `0` as literals, two separate recordings:
   byte-identical 45-entry command list both times, all 20 personal entries unchanged. There is
   also **no documented toggle for `~/.agents/skills` at all** — Grok's own vendor list is only
   `cursor`/`claude`/`codex`.
6. **One further lead was checked and closed cleanly.** The handshake's `_meta["x.ai/pluginDirs"]`
   looked like a configured directory list worth redirecting. It is not — confirmed directly from
   the raw JSON, it is a bare boolean capability flag (`true`), with no schema to seed.

**Ruling (human sign-off, 2026-08-28): fallback authorized.** Between publishing this operator's
personal skill inventory in a public repository and a noisier corpus, the noisier corpus wins
every time. `.params.update.availableCommands[].name`, `.description` and `.input.hint` stay OFF
`crates/harness/src/capture/allowlist/acp.txt` — REDACT, so every command row sanitizes to a
placeholder — even though the path is genuinely decoded. Their `_meta` siblings
(`.scope`/`.path`/`.bareName`/`.qualifiedName`/`.pluginName`) were already REDACT on ordinary
undecoded grounds. Redacting publishes nothing, so this needed no further review beyond the
sign-off already given.

**What would close this properly**: a capture taken on a clean Windows user account or a fresh VM
with no personal skills, plugins, or bundled agent tooling installed under its own home directory
— nothing short of that separates "Grok's own command surface" from "this developer's tool
inventory," because the contamination is sourced from the real OS user profile regardless of every
env-var and config lever Grok documents.

## Blocker 2 — `comet-provider-sanitize` rejects every Grok capture outright, before Blocker 1 even matters

Found while trying to sanitize the reviewed captures. `validate_key` in
`crates/harness/src/capture/sanitize.rs` rejects any object key containing `.`, `[` or `]` before
any allowlist question is asked of it — deliberately, to stop a key like `"result.platformOs"`
from impersonating the nested path `.result.platformOs`. Its own doc comment: *"No key in any
promoted capture contains one of these; if a real provider ever emits a dotted key, that is a
design question about path encoding, not something to escape past on the day it arrives."*

**Grok's own vendor-namespace convention is exactly that: a dotted key.** Every `_meta` object it
sends uses `x.ai/<name>` keys — `x.ai/sessionConfig`, `x.ai/sessionDetail`, `x.ai/mcp/sdk`,
`x.ai/pluginDirs`, `x.ai/fs_notify`, `x.ai/hooks`, `x.ai/capabilities`,
`x.ai/schedulerBackgroundLoops`, and `x.ai/tool` on a `tool_call` frame — and the literal `.` in
`x.ai` trips `AmbiguousObjectKey` on the very first reply (`initialize`'s own
`agentCapabilities._meta`). Confirmed directly: `comet-provider-sanitize` on a real Grok discovery
raw capture fails with `capture contains an object key that would impersonate a nested path`,
`exit code: 2`, before any command-push content is even reached. **No Grok capture — discovery,
`run-grok`, or `steer-grok` — can be sanitized or promoted until this is resolved.** Both
`run-grok` and `steer-grok` were recorded successfully (raw evidence preserved, see below) and
independently confirmed the same rejection.

This is why `session-discovery-grok`, `run-grok` and `steer-grok` are named in
`EXEMPT_UNCAPTURED` in both `crates/harness/src/capture/record.rs` and
`crates/harness/tests/capture_corpus/scenario_coverage.rs`, while codex-acp and claude-agent-acp
(neither of which uses a dotted key anywhere) are promoted and correctly absent from both lists.

**This needs a deliberate design decision, per the check's own comment, not a workaround chosen
under this task's time budget.** A narrow, reviewable shape worth considering when someone does
take it up: a small, explicitly-enumerated allowlist of literal key strings permitted to contain a
path delimiter (the same "declare it once, review it once" shape `surface::MAP_PATHS` and
`allowlist::NAMED_LEAVES` already use), checked before `AmbiguousObjectKey` fires — which would
leave the check's actual protection (anything NOT on that reviewed list still rejects
unconditionally) fully intact. Not attempted here; recorded as the shape worth trying, not a
decision to build it this way.

## What is preserved for whoever picks this up

- Three real, successful Grok recordings survive `comet-provider-capture`'s own run — `session/new`,
  `session/prompt`, and the queued second `session/prompt` all completed and were captured — the
  rejection happens at the SANITIZE step, not the record step. Raw evidence:
  `C:\dev\superpowers\comet\captures\acp-raw-2026-08-28\acp-session-discovery-grok-*\` (discovery)
  and `C:\dev\superpowers\comet\captures\acp-raw-2026-08-28-run-steer\acp-{run,steer}-grok-*\`
  (the two new scenarios this task added). These are raw, unsanitized, and carry this operator's
  personal environment per Blocker 1 above — do not promote from them directly even after
  Blocker 2 is fixed; re-run them through the sanitizer once it can accept a Grok capture, and
  read its novel-path report the normal way.
- `steer-grok`'s capture additionally confirmed `rawInput` genuinely appears on real tool-call
  frames (Grok's `read_file`/`grep` tools ran mid-turn, keyed by that tool's own parameter names —
  `target_file`, `pattern`, ...) and populated it with this operator's own filesystem paths, the
  same shape of problem as the command push. `.params.update.rawInput` and the ACP usage
  breakdown's `.result._meta.usage.modelUsage` are declared in `surface::MAP_PATHS` in this same
  change, so neither publishes a key by accident once Grok capture is unblocked.
- All Step 4/5 code — `run-grok`/`steer-grok` scenario rows, production's shared
  `crate::acp::grok::run_launch` wired as the recorder's launch builder, `corpus_provider_name`
  routing each ACP agent to its own top-level corpus directory — is in place, built, and passes its
  own unit tests. Nothing there is blocked; only the sanitize step is.

## Related

- **D91** — the Claude analog of Blocker 1 (a capture inheriting the capturer's own configuration).
  Its fix (`--claude-config-dir`) does not transfer here: Claude's contamination is scoped to one
  environment variable's config home, while Grok's is sourced from the real OS user profile
  through at least two independent, non-overridable paths.
- **D73** — the same "union across every tool's own parameter names" shape this row's `rawInput`
  finding is an instance of, for Claude.
- **D77** (`README.md`) — a map nobody declares still publishes its keys; the reasoning
  `rawInput`'s and `usage.modelUsage`'s `MAP_PATHS` declarations apply.
