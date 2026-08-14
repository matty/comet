# Provider capture corpus

The harness corpus preserves reviewed provider evidence under
`crates/harness/tests/corpus/<provider>/<version>/<scenario>/`. Each scenario has a
`manifest.json` and ordered `events.jsonl`; `index.json` maps a named claim to exact event
sequences and a source consumer. Tests read those literal payload bytes, never a round trip through
Comet's own wire types.

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

Compare raw and staged artifacts recursively before copying anything into Git. Confirm:

- provider, CLI version, platform, capture timestamp, scenario, purpose, command, cwd, configured
  environment, channels, and exit outcome match the observation;
- event sequences and channels are complete and ordered, and selected neighborhoods prove the
  claim without relying on sanitized provider prose;
- every placeholder definition, use, kind, and redaction count is reciprocal and consistent;
- no username, home/repository/temp path, email/account identity, hostname or server name, machine
  identifier, credential/token, attachment bytes, unrecognized absolute path, or policy-redacted
  user/provider prose survives;
- command inputs, approval joins, terminal status, and repeated identifiers retain the exact safe
  semantics required by the scenario.

If sanitization leaks, corrupts semantics, or accepts an unknown sensitive shape, stop. Add a
captured-shape failing test, fix the fail-closed structural rule, re-sanitize to a fresh staging
name, and repeat the complete review. Do not edit staged output by hand.

## Promote and index

Copy only the reviewed `manifest.json` and `events.jsonl` pair into the observed version/scenario
directory. Add or update the claim in `index.json` with literal sequence/channel selectors. The
manifest `consumers` must be the exact reciprocal set derived from the index, and each consumer
source must name its claim ID. Claimless scenario artifacts use an empty consumer list; never
invent a claim merely to populate it.

Run the focused gate after every promotion:

```powershell
cargo test -p comet-harness --test capture_corpus
cargo test -p comet-harness
```

Before committing, deliberately break each new contract once, observe a meaningful named failure,
restore it, and rerun green. Finish with the repository gate from `AGENTS.md`.

## The second index: the provider surface map

`index.json` answers *"is this claim backed?"*. It cannot answer *"what does the provider send,
and do we read it?"* — a promoted capture full of unread fields is invisible to it, because
nothing points at those fields and inventing a claim to populate a consumer list is forbidden
above, correctly.

So a second index sits beside it. `crates/harness/tests/corpus/dispositions.json` carries one
entry per observed field per direction — `consumed`, `not-applicable`, `deferred` or `unknown` —
and [`docs/testing/provider-surface/`](provider-surface/) holds the rendered report per provider.
Both are generated and committed:

```powershell
$env:COMET_UPDATE_SURFACE = "1"; cargo test -p comet-harness --test capture_corpus surface_report
```

**Promotion is what makes a field visible to the map**, so a scenario promoted here shows up
there on the next regeneration — as `unknown`, until somebody decides. The suite fails when an
observed field has no entry at all, which is how a new CLI version's added field arrives as a
test failure rather than as a bug.

**Adopting new surface takes a second key**, deliberately:

```powershell
$env:COMET_ADOPT_FIELDS = "1"   # only alongside COMET_UPDATE_SURFACE, and only after reading them
```

Without it, regenerating **fails** and lists the fields the corpus has never shown before.
Otherwise the fix for "this field has no disposition" would be the command that silences it, and
a CLI version's forty new fields would join a backlog of hundreds with nobody having read what
arrived.

Two rules the record keeps, enforced by the same suite. A `deferred` entry names a **real** row
in `docs/debt/README.md` and a `how` note saying what consuming it would touch; a
`not-applicable` entry names a reason and **must not** carry a debt row, because the debt index
tracks work deferred and a field that is null in every frame is not work owed.

The report prints a value only where the value's own grammar makes that safe, and a redacted
value reports its kind rather than its content. That is the same test the sanitizer uses, on
purpose: two answers to "is this safe to show" would eventually disagree.

## Provider contradictions

A live contradiction stops retries and promotion for that scenario. Preserve its ignored
`partial-capture.json`, record only non-sensitive structural facts, and do not relax a safety
contract, fabricate a compatible frame, change global account state, or retry without a new design
and fresh authorization.

Before starting another scenario, decide whether the contradiction invalidates a contract that
scenario shares. If it does, stop the shared group. If it does not, the independent scenario may
run, sanitize, and promote on its own evidence. One failed scenario must not erase provenance for
another. The resolution is an explicit design update, removal of an unsupported claim, or a
documented deferral.
