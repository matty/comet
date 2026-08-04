# Upstream Sync Helper Design

## Purpose

Provide a safe, interactive way to selectively apply commits from
`https://github.com/zeronsh/comet` to this fork on Windows and Linux. The helper
must make the routine workflow convenient without hiding Git state, pushing
changes, or modifying the user's main branch directly.

## User Interface

The helper is a dependency-free Python script at `scripts/sync-upstream.py`. It
is launched from anywhere inside the repository with:

```text
python scripts/sync-upstream.py
```

The script displays upstream commits that do not have a patch-equivalent change
in the current branch. Each entry includes a selection number, short commit ID,
date, author, and subject. The user selects commits with comma-separated numbers
and inclusive ranges, for example `2`, `1,4`, or `2-5`.

Before making a branch or applying commits, the script displays the final
selection in application order and requires explicit confirmation.

## Workflow

1. Locate the repository root with `git rev-parse` and verify Git is available.
2. Require a clean worktree, including no staged, unstaged, or untracked files.
3. Record the current branch as the integration target. Detached HEAD is
   rejected because there would be no clear branch to return to or integrate
   into.
4. Inspect the `upstream` remote:
   - If absent, add it as `https://github.com/zeronsh/comet.git`.
   - If present and its normalized fetch URL refers to that GitHub repository,
     keep it.
   - If present with any other URL, stop and explain how the user can resolve
     the name collision. Never rewrite an existing remote automatically.
5. Fetch `upstream/main` with pruning.
6. Query commits with Git's patch-equivalence filtering so already
   cherry-picked changes are omitted. Display newest commits first for browsing.
7. Parse the user's numbered selection, reject invalid or empty selections, and
   reorder selected commits into upstream chronological order for application.
8. Ask for confirmation. A response other than `y` or `yes` exits without
   creating a branch or changing commits.
9. Create and switch to a unique branch named
   `sync/upstream-YYYY-MM-DD`, adding `-2`, `-3`, and so on if necessary.
10. Cherry-pick the selected commits one at a time in chronological order.
11. On success, print review commands and the command to fast-forward the
    recorded integration target. Do not merge, delete branches, or push.

The normal successful handoff is:

```text
git diff <target>...HEAD
git log --oneline <target>..HEAD
git switch <target>
git merge --ff-only <sync-branch>
```

## Failure Handling

All Git subprocess failures retain their Git output and produce a concise
contextual error. Failures before branch creation leave repository history
unchanged, except that the expected `upstream` remote may have been added and
fetched.

If a cherry-pick conflicts, the script stops immediately and leaves Git's
cherry-pick state intact. It prints these recovery choices:

```text
git add <resolved-files>
git cherry-pick --continue
git cherry-pick --abort
```

The helper does not automatically abort because conflict resolutions may be
valuable work. Commits later in the selection remain unapplied.

If no eligible upstream commits exist, the script reports that the branch is
already aligned and exits successfully.

## Components

The script keeps Git execution separate from pure decision logic:

- A small Git command wrapper captures output and converts failures into clear
  errors.
- Remote setup and validation owns only the `upstream` configuration.
- Commit discovery converts machine-readable, delimiter-separated Git log
  output into commit records.
- Selection parsing accepts individual numbers and ranges, removes duplicates,
  validates bounds, and returns records in application order.
- Branch selection finds the first unused date-based branch name.
- The top-level workflow performs prompts and state-changing commands.

This separation keeps parsing and policy testable without creating repositories
for every test.

## Testing

Use Python's standard `unittest` framework so no development dependency is
introduced. Tests will cover:

- Individual, comma-separated, and ranged selections.
- Duplicate selections, malformed tokens, reversed ranges, zero, and
  out-of-bounds numbers.
- Conversion from newest-first display order to chronological application
  order.
- Unique branch-name suffix selection.
- Missing, matching, and conflicting upstream remote decisions.
- Git command failure propagation using mocked subprocess results.

A lightweight integration test will create temporary local Git repositories and
verify the successful discovery-and-cherry-pick path without network access.
Tests must run on Windows and Linux with:

```text
python -m unittest discover -s scripts/tests -p "test_*.py"
```

## Documentation

The root README will gain a short "Sync selected upstream changes" section. It
will document prerequisites, invocation, selection syntax, the safety branch,
conflict recovery, and final integration. The script's `--help` output will
summarize the same command-level behavior.

## Non-Goals

- No graphical or full-screen terminal interface.
- No dependency on `fzf`, GitHub CLI, PowerShell modules, or third-party Python
  packages.
- No automatic merge into the target branch, push, branch deletion, or force
  operation.
- No syncing tags, releases, pull-request metadata, or arbitrary files outside
  selected commits.
- No support for changing the upstream repository or branch through flags in
  this initial version.
