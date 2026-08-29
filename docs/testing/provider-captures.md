# Provider capture corpus

The harness corpus preserves reviewed provider evidence under
`crates/capture/tests/corpus/<provider>/<version>/<scenario>/`. Each scenario has a
`manifest.json` and ordered `events.jsonl`. Tests read a frame by its scenario directory and
sequence number, from the literal payload bytes, never a round trip through Comet's own wire
types.

## Safety boundary

Raw and staging data belong only under `.comet-provider-captures/`, which is ignored by Git. Never
copy raw data into the corpus. Discovery scenarios are token-free. Every turn scenario can call a
model and the runner rejects it unless the operator deliberately passes
`--acknowledge-token-spend` after checking the provider, model, runtime mode, cwd, and prompt.

Use inexpensive test models only. Claude runs use Haiku or Sonnet, never Fable or Opus. Run one
provider process at a time with a hard timeout. Use disposable, non-repository directories for
write or approval scenarios. Logged-out Codex discovery requires an explicit empty `--codex-home`
and rejects ambient `OPENAI_API_KEY` or `CODEX_ACCESS_TOKEN`; do not alter the real login home.

List the supported scenarios and options without contacting a provider:

```powershell
cargo run -p comet-capture --bin comet-provider-capture -- --help
```

That text is generated, not hand-maintained: `crates/capture/src/record/scenarios.rs`'s
`SCENARIOS` table is the single place a scenario's name, provider, purpose and argument
requirements are declared, and both `--help` and `record()`'s own dispatch read it. There is no
scenario name or flag requirement to keep in sync with this document — if `--help` and this
procedure ever disagree, `--help` is current and this file is stale.

**What actually makes the corpus safe to commit is `comet-provider-sanitize`'s allowlist, not a
review step downstream of it.** `crates/capture/src/allowlist/{claude,codex}.txt` name
every dotted key path whose value may survive sanitizing, one file per provider. Everything not
named there becomes a numbered placeholder (`<V210>`), with equal values sharing a number so joins
across frames still work; six identifier kinds — session, thread, turn, tool-use, machine, request
— keep a readable typed name instead of a bare number. **A field nothing on the list names is
redacted by default.** That is the standing rule, not a fallback for the shapes nobody got around
to recognizing: sanitizing never asks "does this look sensitive," only "is this path on the list,"
and the fail-closed answer to everything else is the same no matter what the field turns out to
hold.

Adding a line to `claude.txt` or `codex.txt` is a decision to publish every value that path will
ever carry, forever, in this public repository — not a judgment about whether today's capture
happens to show something harmless.

**A key that contains a `.`, `[`, `]`, `{` or `}` is escaped with a backslash where it joins the
path, and an allowlist line has to be written the same way.** Grok names every `_meta` entry it
sends after its own vendor namespace (`x.ai/sessionConfig`, `x.ai/hooks`, …), so its session-config
lines in `acp.txt` read `.result._meta.x\.ai/sessionConfig.options[].id`. Written unescaped, that
line would instead match the *nested* path `_meta` → `x` → `ai/sessionConfig` — nothing real, and
the very impersonation the escape exists to prevent. **Never retype one of these**: the sanitize
run prints an escaped-key section under the novel-path report listing exactly the strings a listed
line has to equal, and copying from there is the whole procedure. That section is informational —
an escaped key is harmless by construction — but it is the one place a new provider's vendor
naming becomes visible, so read it once per provider.

## Capture and sanitize

Choose an immutable raw root for one observation. Token-free discovery, for example:

```powershell
cargo run -p comet-capture --bin comet-provider-capture -- claude model-discovery --claude-config-dir <EMPTY_DIR> --cwd <DISPOSABLE_DIR> --raw-root .comet-provider-captures\raw\<RUN> --timeout-seconds 30
cargo run -p comet-capture --bin comet-provider-capture -- codex model-discovery --cwd <DISPOSABLE_DIR> --codex-home <CODEX_HOME> --raw-root .comet-provider-captures\raw\<RUN> --timeout-seconds 30
```

Turn scenarios add the acknowledgment only after separate authorization. Resume additionally needs
the exact prior `--resume-id`; Claude attachment needs `--attachment`; Codex
`approval-on-request` needs an empty external `--approval-target`. Never improvise a substitute
scenario when preflight or the protocol rejects the requested one.

### Which configuration home a Claude capture reads

Claude Code reads its configuration home regardless of `--cwd`, so an unisolated capture records
the operator's skills, plugins, MCP servers, hooks and locally configured models as if they were
the CLI's own surface ([D91](../debt/D91-captures-inherit-the-capturer-s-claude-config.md)).
`--claude-config-dir` sets `CLAUDE_CONFIG_DIR` for the spawn; the manifest records the variable,
so the archive shows which captures were isolated and which predate the flag.

Two homes, and the difference is authentication:

- **Empty** — required by `model-discovery`, which the binary refuses to run without it. `--bare`
  never reads OAuth or the keychain, so an empty home costs that scenario nothing.
- **Seeded with `.credentials.json` copied from the live home** — for `command-discovery` and the
  run scenarios, which do read credentials. An empty home logs those out, which records Claude's
  logged-out surface under a name that claims otherwise. The recorder cannot validate this home
  (it is deliberately not empty), so it is the capturer's responsibility to seed it with nothing
  else.

Credentials never reach the wire, so no seeded value enters the archive; `account.email` and its
siblings are withheld by the allowlist like any unlisted field.

**A command count is not a version signal even under isolation.** Two `command-discovery` captures
minutes apart on the same machine, same CLI and same seeded home answered 46 and 48 commands: the
two extra were `extra-usage` and `usage-credits`, which track account state on the server. Read a
`commands` or `tools` count as a floor on what the CLI offers, never as a number to diff.

Sanitize each successful raw directory immediately into a new immutable staging name:

```powershell
cargo run -p comet-capture --bin comet-provider-sanitize -- <RAW_CAPTURE_DIR> .comet-provider-captures\staging\<REVIEW_NAME>
```

`partial-capture.json` is quarantined failure evidence. The sanitizer rejects it, and it must never
be staged or promoted.

## Review before promotion

`comet-provider-sanitize` prints a **novel-path report** after every run: each dotted path it
withheld a value for, how many distinct values it saw at that path, and a shape summary (`string`,
`number`, `bool`, `mixed`) — never the value itself. That report is the discovery mechanism, and
reading it is the review: triage never requires opening withheld content, because none of it is in
the report to open.

For each row, open the raw capture locally and look at what the path actually holds, then decide:

- **Nothing worth allowing** — leave it. The placeholder stands, and the same row reappears on the
  next sanitize run of a scenario that touches that path.
- **Worth publishing forever** — add the path as a new line to `claude.txt` or `codex.txt`,
  re-sanitize to a fresh staging name, and confirm the row is gone from the report.
- **Something that must never survive** — a credential, a token, attachment bytes, an unrecognized
  absolute path. Leave it withheld. If the fail-closed structural scan should already have caught
  the shape and didn't, add a captured-shape failing test, fix the rule, and re-sanitize to a fresh
  staging name.

An empty report ("none — every field on the wire was already allowlisted") means nothing *new*
showed up, not that nothing was captured.

Separately from the allowlist decision, compare raw and staged artifacts to confirm the promoted
frames still prove what the scenario is for:

- provider, CLI version, platform, capture timestamp, scenario, purpose, command, cwd, configured
  environment, channels, and exit outcome match the observation;
- event sequences and channels are complete and ordered, and the selected frames prove what the
  scenario is for, without relying on sanitized provider prose;
- command inputs, approval joins, terminal status, and repeated identifiers retain the exact safe
  semantics required by the scenario.

Do not edit staged output by hand.

Before promoting, also check
[`docs/debt/D73-tool-argument-union-paths.md`](../debt/D73-tool-argument-union-paths.md): seven
paths on `claude.txt` are allowlisted as a union across today's five known tools rather than
reviewed per tool, and that page must be settled — not merely re-read — before the next capture is
promoted to the corpus. A new capture is exactly the event that could exercise a sixth, unreviewed
tool (including a third-party MCP tool) through one of those already-approved paths.

## Promote

Copy the reviewed `manifest.json` and `events.jsonl` pair into the observed version/scenario
directory.

Run the focused gate after every promotion:

```powershell
cargo test -p comet-capture --test capture_corpus
cargo test -p comet-harness
```

`capture_corpus` includes `allowlist_property::every_committed_value_is_allowlisted_or_a_placeholder`
— a total property, not a sample, over every scalar in every committed `events.jsonl`: each one is
either at an allowlisted path or is a placeholder, with no exception. That property is the gate.
The manual review above decides what belongs on the allowlist; this test is what actually stops an
escape from being promoted, and it fails loudly, over the whole corpus, if one gets through.

Its sibling `every_committed_map_key_is_allowlisted_or_a_placeholder` does the same in key
position, and the two are not interchangeable: the scalar property walks `String`/`Number` leaves,
so an identifier sitting in an object *key* was invisible to it. A rogue key planted under
`.modelUsage` with no scalars beneath it fails the key property while the scalar property, the
manifest-token property and the capability sheet's golden test all stay green.

**When a report row names a key rather than a field, the object is a map and belongs in
`surface::MAP_PATHS`.** That is the triage step for a novel map: the declaration is one edit, and
it decides both questions at once — the capability sheet stops recording the key as a field name,
and the sanitizer stops publishing it. Until it is declared, the keys ride through
([D77](../debt/README.md)).

Before committing, deliberately break each new contract once, observe a meaningful named failure,
restore it, and rerun green. Finish with the repository gate from `AGENTS.md`.

## The capability sheet

`docs/providers/<provider>-<version>.md` is a generated **capability sheet**, one per corpus
version directory, recording what that version's corpus shows and nothing else: a Fields section
(every dotted path, per direction), a Vocabulary section (the observed value set for the small
declared discriminator paths), and a Scenarios section naming the exact evidence — argv, cwd,
configured environment — both are drawn from. Promotion is what changes what a sheet shows, so a
scenario promoted here changes its version's sheet on the next regeneration:

```powershell
$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus
```

**Read the failing golden test before you regenerate.**
`every_corpus_version_matches_its_committed_sheet`
(`crates/capture/tests/capture_corpus/capability_sheets.rs`) names the line at which the
generated sheet first diverges from the one committed, and that is the point of the whole
mechanism — a new CLI version's added, removed or reshaped field arrives as a test failure rather
than as a bug six weeks later. It does not name the individual frame or scenario a field first
appeared in; grep the version's own corpus directory for the field's leaf name if you need that —
the dotted path is a walker construction and never appears in `events.jsonl` verbatim.

**A path in the sheet's Fields section is not always spelled the way `claude.txt`/`codex.txt`
spell the same field.** The sanitizer suffixes every array position with `[]`, including an array
whose elements are plain scalars, because it decides allow-or-redact once per element regardless
of what the element is. The sheet's walker (`Visit::walk`,
`crates/capture/src/surface.rs`) does not: it records a field's path once, when it visits
the *key* that names it, before it knows whether the value turns out to be an array — and a
scalar-only array is never revisited with the `[]` suffix, because nothing inside it is an object
whose own keys would trigger another recording. An array of *objects* does get the suffix (each
element's own keys re-enter the object arm and record their own `[]`-suffixed paths), so the gap
is specific to arrays of scalars. Concretely: `allowlist/claude.txt` writes
`.tool_use_result.matches[]`, and the field is genuinely present in `claude-2.1.229.md`'s
evidence, but its Fields section shows `.tool_use_result.matches` — no brackets. Grepping a sheet
for an allowlist path verbatim can come up empty even when the field is right there; check the
bare field name too before concluding it is absent.

**Promoting a new version means committing its capability sheet in the same change.** A version
directory with no matching `docs/providers/<provider>-<version>.md` fails the golden test
outright — the newly-promoted-capture case this mechanism exists to catch, not merely an
out-of-date sheet.
`git diff --no-index docs/providers/claude-2.1.228.md docs/providers/claude-2.1.229.md` is the
version-change report; no differ is built or planned on top of it. **`--no-index` is load-bearing:**
without it the two paths are pathspecs, so git diffs each file against the index and prints
nothing on a clean tree — an empty report that exits 0 and looks like "no changes between these
versions".

**When that diff shows a changed or removed field, check whether a fixture asserts on it.** A
fixture cannot fail when a provider changes — the sheet's golden test is the drift alarm, not a
corpus-replaying fixture, which was considered and cut for exactly that duplication — so this
manual read is what actually reconnects a moved field to the hand-typed literal that assumed it.

## Provider contradictions

A live contradiction stops retries and promotion for that scenario. Preserve its ignored
`partial-capture.json`, record only non-sensitive structural facts, and do not relax a safety
contract, fabricate a compatible frame, change global account state, or retry without a new design
and fresh authorization.

Before starting another scenario, decide whether the contradiction invalidates a contract that
scenario shares. If it does, stop the shared group. If it does not, the independent scenario may
run, sanitize, and promote on its own evidence. One failed scenario must not erase provenance for
another. The resolution is an explicit design update, removal of an unsupported assertion, or a
documented deferral.
