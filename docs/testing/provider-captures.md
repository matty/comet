# Provider capture corpus

The harness corpus preserves reviewed provider evidence under
`crates/harness/tests/corpus/<provider>/<version>/<scenario>/`. Each scenario has a
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
cargo run -p comet-harness --bin comet-provider-capture -- --help
```

**What actually makes the corpus safe to commit is `comet-provider-sanitize`'s allowlist, not a
review step downstream of it.** `crates/harness/src/capture/allowlist/{claude,codex}.txt` name
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

## Capture and sanitize

Choose an immutable raw root for one observation. Token-free discovery, for example:

```powershell
cargo run -p comet-harness --bin comet-provider-capture -- claude model-discovery --cwd <DISPOSABLE_DIR> --raw-root .comet-provider-captures\raw\<RUN> --timeout-seconds 30
cargo run -p comet-harness --bin comet-provider-capture -- codex model-discovery --cwd <DISPOSABLE_DIR> --codex-home <CODEX_HOME> --raw-root .comet-provider-captures\raw\<RUN> --timeout-seconds 30
```

Turn scenarios add the acknowledgment only after separate authorization. Resume additionally needs
the exact prior `--resume-id`; Claude attachment needs `--attachment`; Codex
`approval-on-request` needs an empty external `--approval-target`. Never improvise a substitute
scenario when preflight or the protocol rejects the requested one.

Sanitize each successful raw directory immediately into a new immutable staging name:

```powershell
cargo run -p comet-harness --bin comet-provider-sanitize -- <RAW_CAPTURE_DIR> .comet-provider-captures\staging\<REVIEW_NAME>
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
cargo test -p comet-harness --test capture_corpus
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
manifest-token property and the field snapshot all stay green.

**When a report row names a key rather than a field, the object is a map and belongs in
`surface::MAP_PATHS`.** That is the triage step for a novel map: the declaration is one edit, and
it decides both questions at once — the field snapshot stops recording the key as a field name,
and the sanitizer stops publishing it. Until it is declared, the keys ride through
([D77](../debt/README.md)).

Before committing, deliberately break each new contract once, observe a meaningful named failure,
restore it, and rerun green. Finish with the repository gate from `AGENTS.md`.

## The field snapshot

`crates/harness/tests/corpus/observed-fields.json` is a generated snapshot recording what the
corpus shows, per provider and direction, and nothing else. Promotion is what makes a field
appear in it, so a scenario promoted here changes it on the next regeneration:

```powershell
$env:COMET_UPDATE_SURFACE = "1"; cargo test -p comet-harness --test capture_corpus observed_fields
```

**Read the failure before you regenerate.** It names each arriving field and the frame it first
appears in, and that list is the point of the whole mechanism — a new CLI version's added field
arrives as a test failure rather than as a bug six weeks later. It also reports fields that went
*away*, which is how a dropped or re-recorded scenario announces itself.

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
