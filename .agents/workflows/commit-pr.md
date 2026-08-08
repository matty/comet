# Commit and open a PR

Never commit to `main`. Every change lands through a branch and a PR.

## 1. Branch

Branch off `main` using the repo's prefixes:

| Prefix | For |
| --- | --- |
| `fix/` | bug fixes |
| `feature/` | new capability |
| `ci/` | workflow and release plumbing |
| `test/` | test-only changes |
| `docs/` | documentation |

```bash
git switch -c fix/short-kebab-description
```

If work is already committed on `main` locally, branch from there and reset `main` back —
do not push `main`.

## 2. Verify

Run the gate in [verify.md](verify.md) before committing. Do not open a PR on unverified work.

## 3. Commit

Conventional, lowercase, imperative subject with an optional scope. Real examples from this
repo:

```
fix: make diff capture reliable on Windows
fix(ci): preserve Windows dependency version
test(engine): cover concurrent multi-client LAN access
ci: run nightly releases on a two-hour schedule
docs: clarify TUI removal provenance
```

Keep the subject under ~72 characters and do not append `(#N)` yourself — the squash merge
adds it. Use the body to explain *why*, not to restate the diff. Never use `--no-verify`.

## 4. Push and open the PR

```bash
git push -u origin <branch>
gh pr create --fill
```

PRs are squash-merged. The PR title becomes the squashed commit subject, so it must follow
the same convention as a commit subject. In the body: what changed, why, and the exact
verification commands you ran with their result.

Confirm with the user before merging. Do not merge your own PR unless asked.

### Never force-push unless it is unavoidable

Default to fast-forward pushes. To revise a PR after review, **add a commit** — the review
history stays legible and nothing already published moves.

`--force-with-lease` only protects against clobbering a push you have not seen. It does not
protect a human or a bot mid-review: rewriting a pushed commit can orphan inline review
comments anchored to it and forces a full re-review instead of an incremental one.

There is one routine exception, and it is caused by squash merges. When a base PR merges, its
commits become **one new commit** with a different SHA, so a stacked branch still carries the
originals — which are not in `main`. That branch must be rebased before it can merge:

```bash
git fetch origin
git rebase --onto origin/main <old-base-tip>   # the parent of THIS branch's own first commit
git push --force-with-lease
```

`<old-base-tip>` is the base branch's tip *as this branch saw it*, not the base's current SHA.
Read it out of `git log --oneline <branch>` — passing a SHA that is not an ancestor makes git
replay the already-merged commits and conflict against itself.

Rebase only when a base has actually merged. Say so before doing it, and re-run the gate
afterwards: a rebase produces commits that have never been tested in that arrangement.

### Deleting branches: last, never with `--delete-branch` on a stack

`gh pr merge --delete-branch` removes the branch immediately, and **GitHub closes any PR whose
base branch is deleted**. On a stack that silently closes the next PR as collateral. Recovering
is awkward — a closed PR cannot be retargeted, and it cannot be reopened while its base is
missing, so the base has to be pushed back first just to break the deadlock.

Merge without `--delete-branch` while anything is stacked on the branch, then delete branches
at the end once nothing depends on them:

```bash
gh pr merge <n> --squash
# …after the whole stack has landed:
git diff --stat main..<branch>     # expect empty, or deletions only
git branch -D <branch> && git push origin --delete <branch>
```

`git branch -d` will refuse a squash-merged branch because no merge commit references it.
Confirm the content landed with the `git diff` above before reaching for `-D`.

## 5. Responding to review

Verify a finding before applying it. A reviewer can be right about the symptom and wrong
about the cause, and the fix that follows from the wrong cause still looks like it works.

**Name the competing explanation and rule it out with evidence you can point at.** A flaky
test that a changed wait condition fixes may equally be a product bug whose error arm is
swallowed by a `tracing::warn!`. "It passes now" cannot tell those apart; "the wait resolves
in 2s instead of hitting the 15s timeout" can.

Revise by adding a commit, per the force-push rule above. Reply with what you changed, why
the finding was valid, and the verification you ran — and say plainly when the finding was
right and your original reasoning was wrong. That is the useful part of the record.
