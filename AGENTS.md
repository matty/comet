# AGENTS.md

Guidance for any coding agent working in this repository. This is the source of truth;
`CLAUDE.md` points here so the two can't drift.

Comet is a local-first native desktop app (gpui) that runs Claude Code and Codex sessions on
this machine or on explicitly paired machines on the LAN. There is no account, hosted control
plane, or cloud sync. Read `ARCHITECTURE.md` for the trust and process boundaries and
`docs/PARITY.md` for the implemented feature surface when a change touches either.

## Local and LAN authority model

The engine that owns a data directory is authoritative for its spaces, chats,
sessions, agent processes and durable state, whether it is embedded by Desktop
or running headless. A paired LAN client is a trusted remote controller, not a
read-only client and not a data replica: its allowed operations execute on the
host engine, and durable results stay in that engine's local store.

`comet-client` federates separately scoped server buckets in the UI. It does not
merge engine stores, persist a remote's child state locally, or inherit a remote
engine's own remote registry. Equal entity ids on different servers are distinct.
When a remote disconnects, its child state is removed from the local live view.
Do not design LAN features as engine-to-engine sync, assume transitive trust, or
copy remote authoritative data into the connecting machine's durable store.

## Workspace

Rust 2024 workspace; the toolchain is pinned by `rust-toolchain.toml` (stable, with
`rustfmt` and `clippy`). `crates/*` are libraries — `proto` (wire types), `rpc`
(localhost + pinned-TLS transport, pairing), `engine` (storage and authoritative
operations, including the SQLite `store` module), `client` (remote supervision), `ui` (gpui
surface), plus `harness`, `doc`, `identity`, `update`. `apps/comet` is the only binary.

Two non-Rust trees, neither on the runtime path:

- `edge/` — Cloudflare Worker serving installer/release artifacts. npm + wrangler + vitest.
- `scripts/` — bash packaging scripts and `sync-upstream.py`, with Python `unittest`
  suites in `scripts/tests/`.

`apps/ios` is out of scope for this release; don't change it unless asked.

## Verify before claiming done

Run all three and report the actual output — never claim a change works without it:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo nextest run --workspace
```

Tests run under `cargo-nextest`, not plain `cargo test`: a hung test must be killed and named
instead of blocking the run forever, which `.config/nextest.toml`'s `slow-timeout` does — see
its comment for why. `cargo nextest run` needs `cargo-nextest` installed
(`cargo install cargo-nextest --locked`); CI installs a pinned version.

Also run, when the change touches them:

```bash
cd edge && npm install && npm run typecheck && npm test   # edge/
python -m unittest discover -s scripts/tests              # scripts/*.py
```

`.github/workflows/ci.yml` runs the same commands on every PR (with `fmt --check`), except
tests run with `--profile ci`, which adds one retry — a timeout that passes on retry reports as
flaky rather than failing the job, so a green CI run does not guarantee the local
`--profile default` run (no retries) would also be green.
`.github/workflows/release.yml` is separate and only builds nightly releases. Run the gate
locally first — don't use CI as the first place a change is checked.

Clippy is not yet `-D warnings`: the workspace carries 26 pre-existing warnings. Don't add
new ones, and prefer fixing any you touch.

A failure you write off as a known flake must name where it is recorded. "The documented
flake" was claimed once for a titling test the record says was *fixed*, on a branch that
touched titling — a citation nobody can follow is how a real regression gets waved through.
No record to point at? Run it several times in isolation and report the count.

Full procedure: `.agents/workflows/verify.md`.

## Known debt lives in `docs/debt/`

Work that is known-open and deliberately deferred is tracked in
[`docs/debt/README.md`](docs/debt/README.md) — an index row per item, with a page beside it
wherever the *reasoning* matters (a ruling, a corrected premise, a mechanism you would
otherwise re-derive). Read the index before starting anything substantial: several rows name
the slice or the change that owns them, and a few record decisions that look like oversights.

Defer something? Add a row in the same change that defers it. That file's own "How to keep it"
section is the procedure.

## Never test with Fable or Opus

When you drive the app itself — a rendered check, a live provider run, any session you start to
watch a change work — pick a cheap model in the picker. **Never Fable, never Opus.** They are the
expensive tiers and a test run burns them for output nobody keeps; Sonnet or Haiku exercises the
same surface. This is about models chosen *inside Comet* at test time and says nothing about which
model you or a reviewer runs on.

The picker's default is whatever the catalog lists first, so it will hand you Fable unless you
change it. Change it before you send.

## Changing what an RPC method answers with

`cargo build` will not find every consumer. Untyped JSON assertions in `crates/engine/tests/`
index a reply directly, and `apps/ios` decodes it in Swift. When `ListModels` went from an
array to an object, the build stayed clean, all 501 `comet-ui` tests stayed green, and the
model picker was broken at runtime until a later slice fixed it.

Two rules follow. **Decode each reply in exactly one place**, and point its test at the literal
JSON the engine sends — `crates/ui/src/pickers.rs`'s `decode_models_reply` is the worked
example, and its comment records why a test that round-trips through the Rust type would have
stayed green through exactly this failure. **And re-read `PROTOCOL_VERSION`'s own doc comment**
(`crates/proto/src/remote.rs`). A reshaped reply bumps it, because the decode is all-or-nothing.
An added field usually does not — but it *does* when an older peer silently ignoring it would
act on a stale assumption. Version 4 is that case: `runtimeMode` was purely additive, and it
bumped anyway, because a peer that drops the key runs the turn under its own default and a user
who picked `approval-required` here would get an unattended write over there. Ask what the peer
does with the field missing, not whether the shape still decodes.

More generally: where a constant carries its own reason list in a doc comment, that comment is
the record. Read it rather than any spec, plan or handoff file quoting it. `PROTOCOL_VERSION`
has been quoted secondhand twice and was a slice out of date both times, and both times a spec
inherited the wrong number.

## What the providers send

Sanitizing a capture for the corpus runs on an **allowlist**, not a blocklist.
`crates/capture/src/allowlist/{claude,codex}.txt` name every dotted key path whose value
may survive; everything else becomes a numbered placeholder, and equal values share a number so
joins across frames still work. **A field nothing on the list names is redacted by default** — the
standing rule is "nothing decodes it, so it goes," not "nothing recognized it as sensitive." Adding
a path to one of those files is a decision to publish that field's values forever, in this public
repository, and `docs/testing/provider-captures.md` is the review procedure for making that call.

**Field names are published on purpose; map keys are not.** A key that names a field survives
sanitizing — each promoted version's capability sheet (below) is a snapshot of exactly those
names. A key that *is* data, under a path declared in `surface::MAP_PATHS`, redacts by default
like a value. Declaring a new map is one edit serving both: the sheet stops recording the key as
a field, and the sanitizer stops publishing it. A suspected undeclared map now stops sanitization
before staging; its diagnostic names only a structurally redacted path and counts (D77,
[`closed.md`](docs/debt/closed.md)).

**A declared map can name individual children that are field names after all.** A `MapPath`'s
`named_children` list exempts those keys from the fold, so the sheet records them under their own
path and the sanitizer keeps their spelling, while every unlisted sibling still folds. It changes
what a key is *called*, never what its value earns — a named child's value is still default-deny,
and publishing it verbatim is still its own line in `allowlist/*.txt`. Name a child only where
production decodes it: `.params.update.rawInput` names `pattern` and `path` because
`acp::normalize::typed_call` reads exactly those two, and every other tool argument still folds.

`docs/providers/<provider>-<version>.md` is a generated **capability sheet**, one per corpus
version directory (`claude-2.1.228.md`, `claude-2.1.229.md`, `claude-2.1.233.md`,
`claude-2.1.241.md`, `codex-0.147.0.md`, `claude-agent-acp-0.70.0.md`, `codex-acp-1.7.0.md`,
`grok-1.0.5.md` today): every
dotted field path the promoted corpus shows for that version, split by direction; the observed
value vocabulary for a small declared set of discriminator paths (`.type`, `.method`,
`.params.update.sessionUpdate`, the tool-name paths, …); and the exact scenario list — argv, cwd and configured environment — the
evidence was drawn from. It exists for the reason the deleted field snapshot did: a newly
promoted capture, or a new CLI version's added or removed field, **fails the golden test**
(`crates/capture/tests/capture_corpus/capability_sheets.rs`) instead of arriving unnoticed. Grep
`docs/providers/` before assuming a field does not exist — and if you need to know whether a
field is actually *read*, grep the decode sources instead; the sheet records that a field is
present, never whether Comet consumes it.

**`git diff --no-index docs/providers/claude-2.1.228.md docs/providers/claude-2.1.229.md` is the
version-change report.** No differ is built or planned: the sheets are generated markdown, so
diffing two of them is the whole mechanism. **`--no-index` is not optional** — without it git
reads the two paths as pathspecs and diffs each file against the index, which on a clean tree
prints nothing and exits 0. This file carried the command without it until 2026-08-16, so a
reader who ran it as written saw an empty report and no error. Regenerate after promoting a
capture, read what the failing golden test named, and commit the result in the same change that
promotes the capture:

```powershell
$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus
```

**Its blind spot is absence.** A sheet reports fields and vocabulary values that are present; a
capability no capture ever exercised cannot appear in it at all. The sheet makes that limit more
visible than the deleted snapshot did, not less, because it prints the scenario list it speaks
from: before reading a field or value's absence as the CLI lacking a capability, check whether
any scenario in that same sheet would even have produced it. Procedure is in
`docs/testing/provider-captures.md`.

`crates/capture/` is its own workspace member, and nothing in it is on `comet.exe`'s runtime
path — `apps/comet` does not depend on it, `comet-harness` depends on it only as a
**dev-dependency** (D87 stage 7: production physically cannot reach capture machinery, not
merely by convention). The one thing production shares with it is `comet-harness`'s
`launch.rs` — the process-launch description both use, referenced from `capture/` as
`comet_harness::launch::LaunchDescriptor`. Keep it that way: a production type that drifts into
`capture/` makes the boundary unreadable, which is how a design doc came to record the opposite
of the truth.

Inside `capture/`, the recorder itself is provider-neutral. The seam is four members — spawn,
framing, handshake, turn-complete — but only three of them live on
`record::provider::CaptureProvider` (framing, handshake, turn-complete). Spawn lives on
each row's `launch` field in `record::scenarios::SCENARIOS` instead: which launch a
scenario needs varies per scenario as well as per provider (Claude alone needs three — bare model
discovery, non-bare command discovery, and a run), which a provider-level `spawn` method could not
express without a bypass. **Do not move it back onto the trait** — a first draft did exactly that
and it could not tell `command-discovery` from `model-discovery`. Every scenario — discovery and
run alike, for both providers — is a plain function registered as one row in that same table, the
one `record()` dispatches from and `comet-provider-capture --help` renders.

## The gpui fork rev is load-bearing

`gpui` comes from `wingleeio/zed` at a pinned rev, not from crates.io. Comet depends on
fork-only changes: `Window::with_edge_fade`, `Window::paint_backdrop_blur`,
`ImageSource::evict`, a line-wrap fix, and renderer GPU-memory bounds. The crates.io `gpui`
release does not have them and will not compile.

Never bump the rev to "get something newer". Bumping means rebasing the fork's
`comet/line-wrap-closing-punctuation` branch onto the new upstream and re-verifying every
commit listed in the `Cargo.toml` comment above the `gpui` dependency — keep that comment
current when the rev changes.

## Upstream sync

`upstream` is `zeronsh/comet`. Never cherry-pick from it by hand — that desynchronizes the
`.github/upstream-sync.json` ledger. Always use `python scripts/sync-upstream.py`
(`--resume` after resolving a conflict). Full procedure:
`.agents/workflows/sync-upstream.md`.

### Upstream commits are ports, not picks

Assume every upstream commit needs a manual port. On 2026-08-24, all eight upstream PRs in
the batch conflicted **at their first commit** — the fork has diverged most in `crates/ui`,
which is where nearly all upstream activity happens. A clean `git cherry-pick` is the
exception now, so budget for reading the upstream intent and rewriting it against our code.

**Upstream renamed the product to Zeron; this fork keeps Comet** (`b0be3dea`, recorded
not-applicable in the ledger). Directory layout is mostly unaffected — `crates/*/src/` paths
are identical on both sides — so the rename shows up as mechanical token substitution inside
otherwise-shared files:

| Upstream | Here |
| --- | --- |
| `zeron_proto::`, `zeron_ui::`, … | `comet_proto::`, `comet_ui::`, … |
| `ZERON_*` env vars | `COMET_*` |
| `icons::ZERON_LOGO` | `icons::COMET_LOGO` |
| user-visible "Zeron" strings | "Comet" |
| `apps/zeron/`, `dist/zeron.*` | `apps/comet/`, `dist/comet.*` |

Comments referencing zeron's own web sources (`// zeron settings.shortcuts.tsx row: …`) are
upstream's design provenance; keep them accurate or drop them rather than renaming the file
they point at.

**Do not rename this fork to match.** It was measured: of 45 new commits, 26 touched only
identically-named paths and 1 touched a renamed directory, yet every PR still conflicted.
Renaming would not have made them apply — it would only trade a seconds-long substitution
for a data-directory migration and a churn of every `COMET_*` env var this file documents.

**A clean cherry-pick is not a working one.** Upstream tests arrive carrying assumptions
about fields we may not have. `2119bf0c` applied with no conflict and failed to compile:
it asserts a `Chat::source_context` field introduced by a *different* commit in its own PR.
Build and run anything you pick before believing it — a conflict-free apply proves only that
the text merged.

## Windows ships, and no PR ever tests it

`release.yml` builds `comet.exe` on `windows-latest` and gates the release on that job, so
Windows is a shipped platform. `ci.yml` runs entirely on `ubuntu-24.04`. **Nothing in the PR
gate can catch a Windows regression**, which is why the checks below fall to whoever writes
the change, whatever they develop on.

Windows path and process-lifetime behavior has bitten this repo repeatedly — see the
`fix: ... on Windows` commits. When touching harness CLI resolution, terminal exit, or diff
capture, reason about the Windows path explicitly, and say in the PR whether you were able to
run it there.

`scripts/*.sh` (`package-linux.sh`, `package-macos.sh`, `e2e-smoke.sh`) are CI and platform
packaging scripts, not local dev tooling. They are not part of the verify gate on any OS.

Parallel branches live in `.worktrees/` *inside* the repo and are gitignored. Files under
`.worktrees/` belong to another branch — never edit them while working in the main checkout.

## Git

Never commit to `main`. Branch as `fix/…`, `feature/…`, `ci/…`, `test/…`, or `docs/…`, then
open a PR with `gh`. PRs are squash-merged, so the squashed subject carries `(#N)`.
Commit subjects are conventional and lowercase with an optional scope:
`fix(ci): preserve Windows dependency version`, `test(engine): cover concurrent multi-client LAN access`.
Full procedure: `.agents/workflows/commit-pr.md`.

## crates/ui conventions

Read `.agents/rules/gpui-ui.md` before writing UI code. In short: layout constants are plain
numbers and never depend on which color is painted; every color comes from a `Theme` token;
light mode is designed, not inverted; anything caching paint across frames keys on
`theme_generation()`.

## What the user is allowed to see when something fails

Read `.agents/rules/user-facing-errors.md` before writing any surface that can fail or wait.
Two hard rules: the user never sees a raw technical error (no `err.to_string()` on screen),
and no waiting state can last forever — every skeleton needs a reply, a timeout, or a bounded
retry that gives up into something actionable. Failures split into a short `summary` and an
actionable `hint`, with the diagnostic detail left in `tracing`.

## Which provider versions we support

`docs/testing/supported-provider-versions.md` names the oldest Claude Code and codex-cli each
adapter is written against. It is the basis on which a decode may be **deleted**: a tool or
frame no supported version emits does not get one, because that path ships never having been
constructed.

It says nothing about persisted documents. A transcript written by an older Comet must keep
decoding whatever CLI the user now runs — `ToolCall::Todo` survives for exactly that reason.
Provider-version support and document-format support are different axes; conflating them blanks
somebody's history.

Nothing enforces the floor (`docs/debt/` D69–D70). Raising it is a deliberate change with a new
capture, not a side effect of upgrading a local CLI.

## Fields a provider may omit

Read `.agents/rules/optional-wire-fields.md` before consuming an `Option` that came off a
provider's wire. One rule: write the absent case yourself, because a plan's fixtures supply
the field every time and the `None` path otherwise ships never having been constructed. The
trap is downstream of decoding — `None` read as a value rather than as "unknown", which
`None == None` makes look correct at the call site.

## Shared procedures

Agent-agnostic procedures live in `.agents/`. Read the relevant one before starting that kind
of task:

| Task | File |
| --- | --- |
| Verify a change before calling it done | `.agents/workflows/verify.md` |
| Pull selected commits from `upstream` | `.agents/workflows/sync-upstream.md` |
| Commit, push, open a PR | `.agents/workflows/commit-pr.md` |
| Write UI code in `crates/ui` | `.agents/rules/gpui-ui.md` |
| Write a surface that can fail or wait | `.agents/rules/user-facing-errors.md` |
| Consume a field a provider may omit | `.agents/rules/optional-wire-fields.md` |

Claude Code exposes these as the slash commands `/verify`, `/sync-upstream`, and
`/commit-pr` via thin wrappers in `.claude/skills/`, which read the files above rather than
copying them. Other agents should read the files directly.

## Automated formatting

`python scripts/format-file.py <path>` formats one edited file (`rustfmt` for `.rs`,
`ruff`/`black` for `.py` when installed). It also accepts a Claude Code hook payload on
stdin, which is how the `PostToolUse` hook in `.claude/settings.json` invokes it after every
Write/Edit. Agents without a hook mechanism should call it with an explicit path after
editing, or run `cargo fmt --all` before committing. It is a no-op for other file types and
always exits 0.
