# D102 — Grok's command push ships with three fields unevidenced, and no Grok capture is promoted yet

**Status: open, unowned. Blocker 2 is CLOSED (2026-08-29); Blocker 1's fallback ruling stands and
nothing is promoted.** The first blocker is a decision already made (redact three decoded fields
rather than publish this operator's personal skill inventory). The second — `comet-provider-sanitize`
rejecting every Grok capture outright — was a real design question and is now answered:
`surface::escape_path_segment` escapes a key's own delimiters where it joins a dotted path, so a
vendor-namespaced key can no longer impersonate nesting and no longer needs refusing. **All three
raw Grok captures sanitize end to end today** (discovery, `run-grok`, `steer-grok`); what remains
is the *promotion*, which publishes a real turn's evidence into a public repository and is a
reviewed decision under `docs/testing/provider-captures.md`, not an automatic consequence.
codex-acp and claude-agent-acp are unaffected by either blocker and are promoted
(`crates/harness/tests/corpus/{codex-acp,claude-agent-acp}/`).

**The consequence compounds, and is worth stating plainly.** Grok is the one ACP agent this fork
actually ships — Hermes cannot run at all yet (D104) — and it is the one with the least
monitoring of any adapter Comet drives, entirely because this row is still open:

- **No drift sheet.** `docs/providers/` has one for every corpus version of Claude Code and
  codex-cli, and for codex-acp and claude-agent-acp — the two ACP adapters nobody actually runs
  in production, kept only as comparison points. Grok, the one that ships, has none: a sheet
  needs a promoted corpus, and nothing is promoted. Since 2026-08-29 that is a decision waiting
  to be taken rather than a mechanism that refuses — the three raw captures sanitize, so a
  promotion is a review away, not a fix away.
- **No supported-version floor.** `docs/testing/supported-provider-versions.md` names one for
  Claude Code and codex-cli only (D110 names this explicitly).
- **No runnable live suite.** `crates/harness/tests/grok_live.rs` is `#[ignore]`d — its usage
  assertion has not run since the free quota it needs was exhausted on 2026-08-28.

Finding 1 of the 2026-08-29 whole-branch review — Grok's token usage silently dropped on every
healthy turn, a seam bug between two otherwise-correct PRs — is exactly the shape of drift this
gap exists to catch and did not: it surfaced from reading the settle code's own two comments
against each other, not from a sheet, a floor check, or a live run noticing the meter stayed
empty. Closing this row does not fix that class of bug by itself, but it is the precondition for
every mechanism that would have caught it sooner.

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

## Blocker 2 — CLOSED 2026-08-29. The sanitizer rejected every Grok capture outright; keys are now escaped instead

**Resolved by escaping, not by an allowlist of permitted keys.** `surface::escape_path_segment`
escapes `\`, `.`, `[`, `]`, `{` and `}` wherever a real key is joined into a dotted path, in both
path builders (`sanitize::Redactor::sanitize_value_tree` and `surface::Visit::walk`, which must
agree or a sheet and an allowlist would spell one field two ways). The generated `[]` and `{}`
markers are never escaped — only characters that came out of a provider's key. `validate_key`'s
outright rejection is gone along with the `AmbiguousObjectKey` variant; every other fail-closed
check in that function is untouched.

**The enumerate-the-known-keys shape sketched below is what the evidence ruled out.** Reading the
three raw captures directly turned up ten dotted keys, not the nine this page listed: the nine
`x.ai/*` field names, plus the model id **`grok-4.6`**, which Grok uses as a *map key* under
`usage.modelUsage`. A model id is data and cannot be pre-reviewed one literal at a time, so a
reviewed literal-key list would have needed a carve-out for half the cases on the first day it
shipped. Escaping needs none: a field name publishes because field names publish, and a map key
still faces `allows_prefix`'s default-deny. What the old check protected against is protected
better — the impersonation is now structurally impossible rather than refused — and the test that
proves it (`a_dotted_key_cannot_borrow_a_listed_paths_permission`) fails with the flat key's value
surviving verbatim when the escape is removed.

**Also found while acting on this, and fixed in the same change**: `.params.update.usage.modelUsage`
was not in `surface::MAP_PATHS`, only `.result._meta.usage.modelUsage` was. Grok sends the same
map on the `turn_completed` notification as well as the prompt reply, so the notification copy was
a live instance of D77 — `grok-4.6` would have published as if it were a reviewed field name and
the capability sheet would have recorded a model id as a field. Both paths are declared now.

**Evidence the fix works, on real captures rather than fixtures.** All three raw Grok recordings
sanitize (`comet-provider-sanitize <raw> .comet-provider-captures/staging/…`, exit 0), and the
staged output was checked directly: `grok-4.6` survives at four *value* positions, every one of
them an already-allowlisted model-id path, and at zero key positions — the `modelUsage` key stages
as `<V42>`. The command push stages as placeholders throughout, so Blocker 1's ruling holds in
practice. No `C:\Users\…` path survives in any of the three.

### What it looked like before (kept, because the mechanism explains the escape)

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

`session-discovery-grok`, `run-grok` and `steer-grok` are still named in `EXEMPT_UNCAPTURED` in
both `crates/harness/src/capture/record.rs` and
`crates/harness/tests/capture_corpus/scenario_coverage.rs`, but the reason changed with the fix:
not "the sanitizer refuses them" any more, only "nothing is promoted yet". Both comments were
rewritten rather than left saying the old thing.

### The three session-config paths, reviewed 2026-08-28 and spelled 2026-08-29

Task review (2026-08-28) confirmed the allowlist review itself was sound but ruled that three
lines should not ship in `crates/harness/src/capture/allowlist/acp.txt` until the spelling was
decided: `.result._meta.x.ai/sessionConfig.options[].category`, `...[].id`, and `...[].label`.
Each is genuinely decoded — `grok.rs`'s `config_options`/`ladder_from_config`/
`models_from_discovery` reads exactly these three fields to build the model and reasoning-effort
picker. They were approved as PATHS, not as a SPELLING, because either candidate spelling was
wrong: escaped *somehow*, matching nothing the sanitizer would produce; unescaped,
`.result._meta.x.ai/sessionConfig` is byte-for-byte the string a nested path `.result` →
`._meta` → `.x` → `.ai/sessionConfig` would build.

**Both are now on `acp.txt`, escaped**, which is what the sanitizer builds and therefore the only
spelling that matches:

```
.result._meta.x\.ai/sessionConfig.options[].category
.result._meta.x\.ai/sessionConfig.options[].id
.result._meta.x\.ai/sessionConfig.options[].label
```

Confirmed against a real capture rather than a fixture: after the change, the discovery capture's
novel-path report lists `.result._meta.x\.ai/sessionConfig.options[].description` (unlisted, so
withheld) and does **not** list `.category`/`.id`/`.label` — they matched these three lines and
survived.

## An unrelated third sanitizer gap — FIXED 2026-08-29, was worked around

Distinct from both blockers above — not a Grok-specific dotted-key problem — and it did not block
the 2026-08-28 promotion because a workaround existed on this machine. It would have blocked
anyone else's.

**Fixed**: `Redactor::add_uncovered_program_root` derives a `<PROGRAM_DIR>` root from the
capture's own `command.program` when none of the eight `RedactionRoots` categories already covers
it. Derived at sanitize time rather than recorded as a ninth manifest field on purpose — the
program is in every capture, including ones already recorded, so the fix reaches them. It is
**only** added when uncovered, because `add_path` ranks roots by string length and an
unconditional program root would outrank `<HOME>` (`C:\Users\me\.grok\bin` is the longer string)
and silently respell every capture that works today. Both directions are tested
(`a_program_outside_every_declared_root_still_sanitizes`,
`a_program_under_an_existing_root_keeps_that_roots_spelling`); the first fails with
`UnrecognizedAbsolutePath { location: "command.object[5]" }` when the fix is disabled, the second
passes either way, which is what shows it is a preserved behaviour and not a new one.

Verified against the original failing evidence rather than a fixture: the 2026-08-28 raw
`acp-session-discovery-codex-acp-*` capture, the one recorded with the system Node, sanitizes now
and its manifest reads `"program": "<PROGRAM_DIR>\\node.exe"` with argv still spelled
`<HOME>\AppData\Roaming\npm\…`.

The record of what it was follows, because the workaround is baked into two promoted manifests.

`comet-provider-sanitize` also failed sanitizing the codex-acp and claude-agent-acp discovery
captures on first attempt: `capture contains an unrecognized absolute path at command.object[5]`.
The cause is `command.program` — on this machine, the ACP-adapter rows spawn `node`, and the
system-wide install resolves to `C:\Program Files\nodejs\node.exe`. `sanitize_paths_and_validate`
only redacts an absolute path that matches one of `RedactionRoots`' known categories (`cwd`,
`home`, `temp`, `codex_home`, `claude_config_dir`, `approval_target`, `trusted_powershell`) —
`C:\Program Files\nodejs\` is a genuinely different location, under none of them, and
`sanitize_paths_and_validate` hard-fails on anything left over rather than publishing an
unrecognized path verbatim.

**Worked around, not fixed: both captures were recorded again with `--executable` pointed at a
different, valid `node.exe` that happens to already live under `<HOME>` on this machine** —
`C:\Users\coding\AppData\Local\hermes\node\node.exe` (Node v22.23.2, bundled with the Hermes
agent tool, versus the system install's v24.16.0). That is why both promoted manifests
(`crates/harness/tests/corpus/{codex-acp,claude-agent-acp}/*/*/manifest.json`) and both
capability sheets (`docs/providers/{codex-acp-1.7.0,claude-agent-acp-0.70.0}.md`) show argv
launching a Hermes-bundled interpreter rather than this machine's ordinary Node install — an
ordinary capture-operator choice (`--executable`, ordinary CLI usage), not a code change, and
the ACP wire content itself is unaffected by which Node binary runs the adapter's JS entry.
A capture operator with only a standard system-wide Node install (the common case: nvm, Program
Files, `/usr/local/bin`, most package managers) had no such alternate binary to reach for and hit
the identical `UnrecognizedAbsolutePath` failure with nothing in the tree explaining why. That is
what `<PROGRAM_DIR>` above fixes; the `--node-home`-style per-row override this paragraph used to
propose as the alternative is not needed and was not built.

**The two promoted manifests keep the workaround's spelling**, because they were sanitized before
the fix and nothing re-sanitizes a promoted capture. A future re-recording of those rows with an
ordinary Node install will read `<PROGRAM_DIR>\node.exe` instead, which is a sheet-visible change
and not a regression.

**One more thing this same divergence explains, worth stating plainly rather than leaving
implicit: the manifest's `cli_version` field and the corpus/sheet's version number name two
different programs.** `cli_version` is `probe_version(&launch.program)` — literally `node
--version` for both ACP adapter rows, because `program` for those rows genuinely is `node`, not
the agent. Both promoted manifests read `"cli_version": "v22.23.2"` for exactly that reason (the
Hermes-bundled interpreter's own version). The corpus directory (`1.7.0`, `0.70.0`) and the sheet
title come from an entirely different field, `agentInfo.version` in the `initialize` reply — the
adapter package's own version, read once at review time and used to name the promotion, never
recorded in the manifest itself. Nothing in the manifest or the sheet says these are two
different fields describing two different programs; a reader comparing `cli_version` against the
sheet title and finding them unrelated has found this, not a data-entry error.

## What is preserved for whoever picks this up

- Three real, successful Grok recordings survive `comet-provider-capture`'s own run — `session/new`,
  `session/prompt`, and the queued second `session/prompt` all completed and were captured. Raw
  evidence:
  `C:\dev\superpowers\comet\captures\acp-raw-2026-08-28\acp-session-discovery-grok-*\` (discovery)
  and `C:\dev\superpowers\comet\captures\acp-raw-2026-08-28-run-steer\acp-{run,steer}-grok-*\`
  (the two new scenarios this task added). These are raw, unsanitized, and carry this operator's
  personal environment per Blocker 1 above — never promote from them directly. All three pass
  `comet-provider-sanitize` as of 2026-08-29, so whoever takes the promotion up starts by
  re-running them into fresh staging and reading the novel-path report the normal way. That
  report is long (Grok's own announcements, tips, subscription fields, hostname, plus the whole
  command push) and every row is a publish-or-withhold decision, which is the work this row's
  remaining half consists of.
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
