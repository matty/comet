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
