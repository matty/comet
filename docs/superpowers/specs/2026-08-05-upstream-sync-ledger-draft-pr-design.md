# Upstream Sync Ledger and Draft PR Design

## Purpose

Extend `scripts/sync-upstream.py` so repeated sync runs share an auditable
record of upstream decisions. The helper must stop resurfacing commits that
were adapted in this fork or deliberately rejected, continue resurfacing
deferred work, and turn each confirmed run into a reviewable draft pull
request.

## Repository Ledger

Store sync state in `.github/upstream-sync.json`. The document has a schema
version, a current record keyed by full upstream commit SHA, and an append-only
list of runs.

Each current commit record contains:

- The upstream SHA and subject.
- One outcome: `implemented`, `not-applicable`, or `deferred`.
- The decision date and a human-readable note.
- An optional local commit SHA for an implementation.

Each run contains a unique identifier, date, target branch, sync branch, and
the upstream commits considered with their outcomes. Deferred commits can
appear in multiple runs. Their current record reflects the most recent
decision while the run list preserves the earlier decisions.

The loader rejects unknown schema versions, malformed commit IDs, unsupported
outcomes, inconsistent keys and embedded SHAs, and duplicate run identifiers.
The writer produces stable, human-reviewable formatting and ordering.

## Initial State

Seed the ledger with all earlier upstream picks that can be traced to this
fork:

| Upstream | Local implementation | Change |
| --- | --- | --- |
| `ff124f4142144285b8f10df152837f55c35ab20a` | `6a6f6b8fc3c1895f076d2b1b9208eb034b7df67f` | Show owning device in session rows |
| `760ea9a44b5d7b69b68fccbca39cf7fff66879fc` | `adc28704de3159960b4a9494ce824ab5296113e9` | Bound GPU memory |
| `ff5e483baf2a25c994c86949183c6ab6a6469612` | `4a3c10163dd8e3bdb4994f4c6a7c00b333340315` | Add native Windows caption controls |
| `7b52ce1f70b3dddf13756358c4dc1f9d810a0bad` | `3e5ea92a87775cd1f56e9ad1790a0a037c95cec6` | Remove the TUI |

The Windows caption reference intentionally points to the merged fork PR
commit because the upstream change was adapted through that PR rather than
retained as a patch-equivalent commit.

Seed the following newly fetched commits as `not-applicable`:

- `b01943979d3732fd615ab2c002185dbb1016a601`, which changes the removed
  `edge/src/session-room.ts` hosted workspace room.
- `c1243c5ab187f39c58905edc8b195504a005a51f`, which also changes that removed
  hosted workspace room.
- `79c8e22898d8c7b45150c06a8c6e97de64cfbd0d`, which exposes a hosted
  workspace snapshot endpoint whose backing service is absent from the
  LAN-only fork.

These seeded decisions form one explicitly identified bootstrap run dated
2026-08-05. The run is a reconstruction of pre-ledger history rather than a
claim that all seven decisions happened in one earlier invocation. This keeps
their provenance in both the current records and run history without inventing
unknown historical run boundaries.

## Discovery and Classification

Continue using Git patch-equivalence filtering as the first discovery layer.
Apply the ledger as a second layer:

- Omit `implemented` commits.
- Omit `not-applicable` commits.
- Include `deferred` commits so they can be reconsidered.

Display eligible commits newest first. The user selects commits to implement
by cherry-pick using the existing number and range syntax. For every
unselected commit, prompt for `deferred` or `not-applicable`; pressing Enter
chooses `deferred`.

Before changing Git state, print a complete summary grouped by outcome and
require explicit confirmation. A declined confirmation does not create a
branch or update the ledger.

## Successful Run

Before prompting, validate a clean attached branch, the fixed `upstream`
remote, a usable `origin` remote, GitHub CLI availability, and GitHub
authentication. Fetch `upstream/main` before discovery.

After confirmation:

1. Create a unique `sync/upstream-YYYY-MM-DD` branch from the starting target.
2. Cherry-pick selected commits one at a time in upstream chronological order.
3. After every successful pick, record the new local `HEAD` SHA as the
   implementation of the corresponding upstream SHA.
4. Update `.github/upstream-sync.json` with current decisions and the full run.
5. Commit the ledger update as a dedicated metadata commit on the sync branch.
6. Push the branch to `origin` with upstream tracking.
7. Open a draft pull request against the starting target using
   `gh pr create --draft`.

The generated PR title identifies the upstream sync date. Its body groups
implemented, not-applicable, and deferred commits, and includes both upstream
and local SHAs where available.

A run containing only decision changes still creates a sync branch, ledger
commit, push, and draft bookkeeping PR. This ensures shared decisions reach
the repository through review rather than remaining local state.

## Resume and Failure Handling

Write pending run state beneath `.git` immediately before branch creation. It
contains the target branch, sync branch, ordered selected commits,
classifications, completed upstream-to-local mappings, and the current
workflow phase. It contains no credentials.

If a cherry-pick conflicts, leave Git's conflict state intact and print the
existing manual recovery commands. After the user resolves the conflict and
runs `git cherry-pick --continue`, they continue with:

```text
python scripts/sync-upstream.py --resume
```

Resume verifies that the repository, branch, and completed commits match the
pending state before applying later commits. It then writes and commits the
ledger, pushes, and creates the draft PR.

Push and GitHub CLI failures preserve the completed local sync branch and
pending state. Re-running `--resume` retries from the recorded phase. Before
creating a PR, resume checks for an existing PR for the same head and base; it
returns that PR rather than creating a duplicate. Successful completion
removes pending state.

The helper never rewrites a remote, overwrites a branch, force-pushes, merges,
or changes an existing PR's review state. Git and GitHub failures retain concise
command context without printing environment values.

## Components

Keep the dependency-free Python structure and separate responsibilities:

- Ledger parsing, validation, filtering, and stable serialization are pure
  functions.
- Run construction and classification are pure policy functions.
- The existing Git adapter owns Git commands and local SHA lookup.
- A small GitHub CLI adapter owns authentication checks, PR lookup, and draft
  PR creation.
- A pending-state store beneath `.git` owns resumability.
- The top-level workflow coordinates prompts and state transitions.

No GitHub API library, YAML parser, or other third-party Python dependency is
introduced. The external `gh` executable becomes a documented prerequisite
for confirmed runs that create PRs.

## Testing

Extend the standard-library `unittest` suite to cover:

- Ledger parsing, schema validation, stable serialization, and duplicate run
  rejection.
- Filtering of implemented and not-applicable commits while retaining deferred
  commits.
- The seeded upstream-to-local mappings and hosted-service decisions.
- Default deferred classification and explicit not-applicable classification.
- Full run serialization and repeat deferrals across runs.
- Successful cherry-pick-to-local-SHA mapping.
- Bookkeeping-only runs.
- Pending-state creation, validation, conflict resume, and cleanup.
- Push command construction and GitHub authentication errors.
- Draft PR title and body generation.
- Idempotent resume after push or PR-creation failures.
- Detection and reuse of an existing matching PR.

The local-repository integration test covers discovery, cherry-picking, ledger
commit creation, and resume behavior without network access. GitHub CLI calls
are mocked so the suite remains deterministic on Windows and Linux.

## Documentation

Update the root README and script help to describe:

- The committed ledger and three outcomes.
- Selection and default-deferred classification.
- The `gh` installation and authentication prerequisite.
- Automatic sync-branch push and draft PR creation.
- Conflict recovery and `--resume`.
- Bookkeeping-only draft PRs.

## Non-Goals

- Automatically deciding whether a commit applies to the LAN-only fork.
- Automatically resolving cherry-pick conflicts.
- Opening ready-for-review PRs.
- Merging PRs, deleting branches, or force-pushing.
- Synchronizing upstream tags, releases, or arbitrary branches.
