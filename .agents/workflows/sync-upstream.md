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

**An `adapted` row's `local_commit` is `null` on every row the helper writes** — the
interactive flow above never asks for one. `classify_unselected` builds an adapted decision
with the field left at its `None` default, `build_sync_run` attaches a local sha only to rows
it labels `implemented` (the picks it made itself), and the pending-state validator rejects a
classification that carries one. The *schema* is deliberately looser than that: `LANDED` in
`scripts/sync-upstream.py` admits a local sha on either landed outcome, and its own comment
says an adapted row "may carry the local commit that landed it" — so a hand-added sha on an
adapted row still validates. Nothing writes one today, and all 16 adapted rows are `null`.

Read that `null` as the ordinary shape of a hand-port, not a broken row. The field holds one
sha, and a hand-port routinely lands across several local commits, folds into unrelated work,
or is otherwise not traceable to a single one. `note` is where the real local reference lives,
including a split across several PRs — see the `2133bae5a2` row in
`.github/upstream-sync.json`, a single upstream commit hand-ported in three pieces: its `note`
names **#123** and **#124** by number and the third only as "this PR", because the note was
written before that PR merged as **#130** (`fix(harness): settle ACP turns on either completion
signal, and bound a stalled prompt`). A split port is answered that way, not by inventing a
multi-value `local_commit`.

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
