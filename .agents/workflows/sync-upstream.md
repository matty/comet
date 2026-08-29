# Sync upstream

`upstream` is `https://github.com/zeronsh/comet.git`; `origin` is this fork.

**Never cherry-pick from `upstream` by hand.** Doing so desynchronizes the committed
`.github/upstream-sync.json` ledger, and the skipped commit reappears on every future run.
Every upstream commit enters this repo through `scripts/sync-upstream.py`.

## Preconditions

Python 3, Git, and the GitHub CLI authenticated (`gh auth login`). Run from a clean worktree
with a branch checked out — the helper refuses detached HEAD, requires an `origin` push
remote, and validates GitHub auth before touching history.

## Run

```bash
python scripts/sync-upstream.py
```

The helper adds the fixed `upstream` remote if missing (and refuses to continue if an
existing `upstream` points elsewhere — it never overwrites it), fetches `main`, and lists
commits not already resolved by Git patch-equivalence or the ledger.

Select one commit (`2`), a list (`1,4`), or a range (`2-5`). A blank selection is a
bookkeeping-only run. Unselected commits are classified `deferred` (reappear next run),
`not-applicable` with a reason, or `adapted` with a reference (both hidden permanently, as
are implemented ones).

**`adapted` is for a change we hand-ported.** The fork has diverged far enough that a clean
cherry-pick is the exception, not the rule — see AGENTS.md, "Upstream commits are ports,
not picks". Such a commit never earns an `implemented` row, because the helper only writes
that for a pick it made itself; without `adapted` the only way to stop it reappearing every
run was to call it `not-applicable`, which records the opposite of what happened. Give it
the PR or commit that carried the change.

**An `adapted` row's `local_commit` is always `null`** — the interactive flow above never asks
for one, because the schema assumes one upstream commit maps to one local commit and a
hand-port routinely breaks that assumption: one upstream commit landed across several local
commits, folded into unrelated work, or otherwise not traceable to a single sha. `note` is
where the real local reference lives, including a split across several PRs — see the
`2133bae5a2` row in `.github/upstream-sync.json`, a single upstream commit hand-ported in
three pieces across `#123`, `#124` and `#130`, with all three named in `note`. Read a `null`
`local_commit` on an `adapted` row as that outcome's ordinary shape, not a broken row —
this is a decision the schema makes on purpose, not a gap to fix by inventing a multi-value
field.

After `y`/`yes` confirms the summary, the helper creates a `sync/upstream-YYYY-MM-DD`
branch, cherry-picks chronologically, records and commits the ledger, pushes to `origin`,
and opens a draft PR. Any other answer exits without changing anything.

## Conflicts

```bash
git add <resolved-files>
git cherry-pick --continue
python scripts/sync-upstream.py --resume
```

Or abandon the pick with `git cherry-pick --abort` and run the same `--resume` to clear the
pending run. `--resume` only records the commit when it sits directly on the pre-pick commit
and keeps the upstream subject; anything else stops with the pending state intact rather
than falsely marking the upstream commit implemented. Use `--resume` after a failed push or
PR step too — completed phases are not repeated and a matching PR is reused.

The helper never merges, force-pushes, deletes branches, changes a PR's review state, or
overwrites remotes. If you find yourself wanting any of those, stop and ask.

## After

Cherry-picked upstream code still has to pass this repo's gate — see
[verify.md](verify.md). Upstream does not develop on Windows; check the Windows path of
anything touching process spawning, paths, or terminals.
