# AGENTS.md

Guidance for any coding agent working in this repository. This is the source of truth;
`CLAUDE.md` points here so the two can't drift.

Comet is a local-first native desktop app (gpui) that runs Claude Code and Codex sessions on
this machine or on explicitly paired machines on the LAN. There is no account, hosted control
plane, or cloud sync. Read `ARCHITECTURE.md` for the trust and process boundaries and
`docs/PARITY.md` for the implemented feature surface when a change touches either.

## Workspace

Rust 2024 workspace; the toolchain is pinned by `rust-toolchain.toml` (stable, with
`rustfmt` and `clippy`). `crates/*` are libraries — `proto` (wire types), `rpc`
(localhost + pinned-TLS transport, pairing), `engine` (storage and authoritative
operations), `client` (remote supervision), `ui` (gpui surface), plus `harness`, `doc`,
`sync`, `identity`, `update`. `apps/comet` is the only binary.

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
cargo test --workspace
```

Also run, when the change touches them:

```bash
cd edge && npm install && npm run typecheck && npm test   # edge/
python -m unittest discover -s scripts/tests              # scripts/*.py
```

`.github/workflows/ci.yml` runs the same commands on every PR (with `fmt --check`).
`.github/workflows/release.yml` is separate and only builds nightly releases. Run the gate
locally first — don't use CI as the first place a change is checked.

Clippy is not yet `-D warnings`: the workspace carries ~24 pre-existing warnings. Don't add
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
`crates/harness/src/capture/allowlist/{claude,codex}.txt` name every dotted key path whose value
may survive; everything else becomes a numbered placeholder, and equal values share a number so
joins across frames still work. **A field nothing on the list names is redacted by default** — the
standing rule is "nothing decodes it, so it goes," not "nothing recognized it as sensitive." Adding
a path to one of those files is a decision to publish that field's values forever, in this public
repository, and `docs/testing/provider-captures.md` is the review procedure for making that call.

**Field names are published on purpose; map keys are not.** A key that names a field survives
sanitizing — `observed-fields.json` is a snapshot of exactly those names. A key that *is* data,
under a path declared in `surface::MAP_PATHS`, redacts by default like a value. Declaring a new
map is one edit serving both: the surface snapshot stops recording the key as a field, and the
sanitizer stops publishing it. A map nobody declared still publishes its keys (D77).

`crates/harness/tests/corpus/observed-fields.json` is a generated snapshot of every field the
promoted corpus shows, per provider and direction. It exists for one reason: a newly promoted
capture, or a new CLI version's added field, **fails the suite** instead of arriving unnoticed.
Grep it before assuming a field does not exist.

Regenerate after promoting a capture, read what the failure listed, and commit the result:

```powershell
$env:COMET_UPDATE_SURFACE = "1"; cargo test -p comet-harness --test capture_corpus observed_fields
```

**The snapshot records no opinion about a field** — not whether Comet reads it, not what it
would cost to. An earlier version did, in a four-state record with a validator and two generated
reports; of 655 entries it held 7 human decisions, and every "consumed" marking came from
matching the field's leaf name against the decode sources, which counted
`.message.diagnostics.cache_miss_reason.type` as read because something names `type`. Removed in
favour of the part that can fail for a real reason. If you need to know whether a field is read,
grep the decode sources — that is what the machine was doing anyway, less accurately.

**Its blind spot is absence.** It reports fields that are present; a capability no capture ever
exercised cannot appear in it at all. Procedure is in `docs/testing/provider-captures.md`.

`crates/harness/src/capture/` is test tooling and nothing in it is on the runtime path. The
one thing production shares with it is `crates/harness/src/launch.rs` — the process-launch
description both use. Keep it that way: a production type that drifts into `capture/` makes
the boundary unreadable, which is how a design doc came to record the opposite of the truth.

Inside `capture/`, the recorder itself is provider-neutral. The seam is four members — spawn,
framing, handshake, turn-complete — but only three of them live on
`capture::record::provider::CaptureProvider` (framing, handshake, turn-complete). Spawn lives on
each row's `launch` field in `capture::record::scenarios::SCENARIOS` instead: which launch a
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
