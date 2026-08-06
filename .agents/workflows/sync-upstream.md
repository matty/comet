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
bookkeeping-only run. Unselected commits are classified `deferred` (reappear next run) or
`not-applicable` with a reason (hidden permanently, as are implemented ones).

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
