# Upstream Sync Helper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a safe interactive Python helper that lets Windows and Linux users select and cherry-pick non-equivalent commits from `zeronsh/comet` onto a temporary sync branch.

**Architecture:** Keep the implementation in one dependency-free script with pure functions for parsing and policy, a `Git` adapter for subprocess calls, and a thin interactive workflow. Unit tests exercise pure logic and mocked failures; a local-repository integration test exercises discovery and cherry-picking without network access.

**Tech Stack:** Python 3 standard library (`argparse`, `dataclasses`, `datetime`, `pathlib`, `subprocess`, `unittest`, `unittest.mock`, `tempfile`) and Git CLI.

## Global Constraints

- Support Windows and Linux.
- Add no third-party Python, GitHub CLI, `fzf`, or PowerShell-module dependency.
- Use the fixed upstream repository `https://github.com/zeronsh/comet.git` and branch `main`.
- Never overwrite a conflicting `upstream` remote.
- Require a clean worktree and an attached branch before branch creation.
- Never merge, push, delete branches, force an operation, or auto-abort a conflict.
- Display commits newest-first but cherry-pick selected commits in upstream chronological order.
- A non-affirmative confirmation must make no history change.

---

### Task 1: Selection and naming policy

**Files:**
- Create: `scripts/sync-upstream.py`
- Create: `scripts/tests/test_sync_upstream.py`

**Interfaces:**
- Produces: `Commit(oid: str, short_oid: str, date: str, author: str, subject: str)`.
- Produces: `parse_selection(text: str, commits: list[Commit]) -> list[Commit]`.
- Produces: `next_branch_name(existing: set[str], day: date) -> str`.
- Produces: `normalize_github_url(url: str) -> str` and `is_expected_upstream(url: str) -> bool`.

- [ ] **Step 1: Add failing tests for selection parsing and application order**

Load the hyphenated script with `importlib.util.spec_from_file_location`, create four commits in chronological order, then display them with `list(reversed(commits))`. Add tests asserting:

```python
def test_parse_single_selection():
    assert [c.oid for c in sync.parse_selection("2", DISPLAYED)] == ["c3"]

def test_parse_list_and_range_returns_chronological_order():
    selected = sync.parse_selection("1,3-4", DISPLAYED)
    assert [c.oid for c in selected] == ["c1", "c2", "c4"]

def test_parse_selection_removes_duplicates():
    selected = sync.parse_selection("1,1,1-2", DISPLAYED)
    assert [c.oid for c in selected] == ["c3", "c4"]
```

Add table-driven rejection cases for `""`, `"0"`, `"5"`, `"x"`, `"1--2"`, and `"3-1"`, all raising `ValueError` with a non-empty message.

- [ ] **Step 2: Run the focused tests and confirm the expected failure**

Run:

```text
python -m unittest scripts.tests.test_sync_upstream.SelectionTests -v
```

Expected: failure while loading `scripts/sync-upstream.py` because the file or required names do not exist.

- [ ] **Step 3: Implement commit records and selection parsing**

Add the immutable record and parser:

```python
@dataclass(frozen=True)
class Commit:
    oid: str
    short_oid: str
    date: str
    author: str
    subject: str


def parse_selection(text: str, commits: list[Commit]) -> list[Commit]:
    if not text.strip():
        raise ValueError("Select at least one commit.")
    indexes: set[int] = set()
    for token in (part.strip() for part in text.split(",")):
        if not token:
            raise ValueError("Selection contains an empty item.")
        if "-" in token:
            bounds = token.split("-")
            if len(bounds) != 2 or not all(part.isdigit() for part in bounds):
                raise ValueError(f"Invalid range: {token}")
            start, end = map(int, bounds)
            if start > end:
                raise ValueError(f"Range must be ascending: {token}")
            indexes.update(range(start, end + 1))
        elif token.isdigit():
            indexes.add(int(token))
        else:
            raise ValueError(f"Invalid selection: {token}")
    if min(indexes) < 1 or max(indexes) > len(commits):
        raise ValueError(f"Choose numbers from 1 to {len(commits)}.")
    chosen = [commits[index - 1] for index in indexes]
    position = {commit.oid: index for index, commit in enumerate(commits)}
    return sorted(chosen, key=lambda commit: position[commit.oid], reverse=True)
```

- [ ] **Step 4: Add failing tests for branch naming and URL normalization**

Assert these exact policies:

```python
def test_next_branch_name_uses_first_available_suffix():
    existing = {"sync/upstream-2026-08-04", "sync/upstream-2026-08-04-2"}
    assert sync.next_branch_name(existing, date(2026, 8, 4)) == "sync/upstream-2026-08-04-3"

def test_expected_upstream_accepts_https_ssh_and_trailing_git():
    assert sync.is_expected_upstream("https://github.com/zeronsh/comet.git")
    assert sync.is_expected_upstream("git@github.com:zeronsh/comet.git")
    assert sync.is_expected_upstream("ssh://git@github.com/zeronsh/comet")

def test_expected_upstream_rejects_another_repository():
    assert not sync.is_expected_upstream("https://github.com/example/comet.git")
```

- [ ] **Step 5: Implement branch naming and URL normalization, then run tests**

Normalize GitHub HTTPS, SCP-like SSH, and `ssh://` forms to lowercase
`github.com/owner/repository`, stripping a terminal `.git` and `/`. Implement
`next_branch_name` with an unsuffixed first choice followed by suffixes starting
at `-2`.

Run:

```text
python -m unittest scripts.tests.test_sync_upstream -v
```

Expected: all Task 1 tests pass.

- [ ] **Step 6: Commit the independently tested policy layer**

```text
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: add upstream sync selection policy"
```

---

### Task 2: Git adapter, repository validation, and discovery

**Files:**
- Modify: `scripts/sync-upstream.py`
- Modify: `scripts/tests/test_sync_upstream.py`

**Interfaces:**
- Consumes: `Commit`, `is_expected_upstream`, and `next_branch_name` from Task 1.
- Produces: `GitError(RuntimeError)` with `args_list`, `returncode`, `stdout`, and `stderr` attributes.
- Produces: `Git(cwd: Path | None = None)` with `run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]`.
- Produces: `repository_root(git: Git) -> Path`, `validate_repository(git: Git) -> str`, `ensure_upstream(git: Git) -> None`, `discover_commits(git: Git, target: str) -> list[Commit]`, and `existing_branches(git: Git) -> set[str]`.

- [ ] **Step 1: Add failing tests for Git error preservation and repository checks**

Mock `subprocess.run` to return exit code 7, stdout `partial`, and stderr
`fatal: example`, then assert `Git.run("fetch")` raises `GitError` retaining all
four values. Mock the `Git.run` method for these validation cases:

```python
def test_validate_repository_rejects_dirty_worktree():
    git = ScriptedGit({("status", "--porcelain"): "?? local.txt\n"})
    with self.assertRaisesRegex(sync.SyncError, "clean worktree"):
        sync.validate_repository(git)

def test_validate_repository_rejects_detached_head():
    git = ScriptedGit({
        ("status", "--porcelain"): "",
        ("symbolic-ref", "--quiet", "--short", "HEAD"): sync.GitError(
            ["symbolic-ref", "--quiet", "--short", "HEAD"], 1, "", ""
        ),
    })
    with self.assertRaisesRegex(sync.SyncError, "detached HEAD"):
        sync.validate_repository(git)
```

Also assert a clean attached branch returns its exact name.

- [ ] **Step 2: Run repository tests and verify they fail**

Run:

```text
python -m unittest scripts.tests.test_sync_upstream.GitAdapterTests scripts.tests.test_sync_upstream.RepositoryTests -v
```

Expected: errors stating `Git`, `GitError`, or validation functions are missing.

- [ ] **Step 3: Implement the Git adapter and validation**

`Git.run` must call:

```python
subprocess.run(
    ["git", *args],
    cwd=self.cwd,
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
```

Convert `FileNotFoundError` to `SyncError("Git is not installed or is not on PATH.")`. Define `GitError.__init__(args_list: list[str], returncode: int, stdout: str, stderr: str)` and retain those inputs as attributes. When `check` is true and the return code is nonzero, raise `GitError`. `repository_root` uses `git rev-parse --show-toplevel`. `validate_repository` checks `git status --porcelain` before `git symbolic-ref --quiet --short HEAD` and returns the target branch.

- [ ] **Step 4: Add failing tests for upstream setup and commit discovery**

Use a command-recording fake to assert:

- A missing remote runs `remote add upstream https://github.com/zeronsh/comet.git`.
- A matching HTTPS or SSH remote causes no mutation.
- A conflicting URL raises `SyncError` containing both `upstream` and the found URL.
- Discovery invokes `git log --right-only --cherry-pick --topo-order` over `<target>...upstream/main` with NUL-separated fields.
- NUL-delimited output becomes `Commit` records and remains newest-first.
- Empty output returns an empty list.

- [ ] **Step 5: Implement upstream policy, discovery, and branch enumeration**

Use `git remote get-url upstream` with `check=False` to distinguish an absent
remote. After validation or addition, the caller will run `git fetch --prune
upstream main`. Discover with:

```text
git log --right-only --cherry-pick --topo-order --format=%H%x00%h%x00%cs%x00%an%x00%s <target>...upstream/main
```

Parse each output line into exactly five NUL-separated fields, rejecting any
other field count with `SyncError("Git returned malformed commit data.")`.
Enumerate local branch names
with `git for-each-ref --format=%(refname:short) refs/heads/`.

- [ ] **Step 6: Run the Git-facing unit tests**

Run:

```text
python -m unittest scripts.tests.test_sync_upstream -v
```

Expected: all Task 1 and Task 2 tests pass.

- [ ] **Step 7: Commit the Git integration layer**

```text
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: discover eligible upstream commits"
```

---

### Task 3: Interactive workflow and conflict handoff

**Files:**
- Modify: `scripts/sync-upstream.py`
- Modify: `scripts/tests/test_sync_upstream.py`

**Interfaces:**
- Consumes: all Task 1 and Task 2 interfaces.
- Produces: `format_commit(index: int | None, commit: Commit) -> str`.
- Produces: `run_workflow(git: Git, input_fn: Callable[[str], str], output: TextIO, day: date) -> int`.
- Produces: `main(argv: Sequence[str] | None = None) -> int`.

- [ ] **Step 1: Add failing workflow tests with a stateful fake Git adapter**

Create a fake that records commands and returns scripted results. Cover these
observable flows:

1. No commits: fetch occurs, output contains `already aligned`, no branch is created, return code is 0.
2. Invalid selection followed by `2`: output contains the validation message and selection is requested again.
3. Confirmation `n`: no `switch -c` or `cherry-pick` occurs, return code is 0.
4. Confirmation `yes`: `switch -c <unique-name>` occurs once, followed by one `cherry-pick <oid>` command per selected commit in chronological order.
5. Cherry-pick failure: return code is 1 and output contains both `git cherry-pick --continue` and `git cherry-pick --abort`; later commits are not attempted.

- [ ] **Step 2: Run workflow tests and verify they fail**

Run:

```text
python -m unittest scripts.tests.test_sync_upstream.WorkflowTests -v
```

Expected: failure because `run_workflow` and formatting functions are missing.

- [ ] **Step 3: Implement prompts, display, and state-changing workflow**

Implement this exact order:

```python
target = validate_repository(git)
ensure_upstream(git)
git.run("fetch", "--prune", "upstream", "main")
commits = discover_commits(git, target)
```

If commits exist, print the numbered list and loop until `parse_selection`
succeeds. Print `Selected commits (oldest first):`, the unnumbered selection,
and prompt `Create a sync branch and cherry-pick these commits? [y/N] `. Exit
without mutation unless the stripped lowercase response is `y` or `yes`.

Choose a branch with `next_branch_name(existing_branches(git), day)`, run
`git switch -c <branch>`, and cherry-pick each commit separately. On a failed
cherry-pick, print the conflict recovery commands and return 1 without aborting.
On success, print the exact target and sync branch in the diff, log, switch, and
fast-forward merge commands, then return 0.

- [ ] **Step 4: Add and implement command-line help and top-level error handling**

Add a test that `main(["--help"])` raises `SystemExit(0)` with help text
containing `Select and cherry-pick commits from zeronsh/comet`. Implement
`argparse.ArgumentParser` with that description and no workflow-changing flags.
Construct `Git`, set its working directory to the discovered repository root,
and call `run_workflow`. Catch `SyncError`, print `error: <message>` to stderr,
and return 1. End the script with:

```python
if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 5: Run all unit tests and manual help smoke test**

Run:

```text
python -m unittest discover -s scripts/tests -p "test_*.py" -v
python scripts/sync-upstream.py --help
```

Expected: all tests pass and help exits 0 without fetching or changing Git state.

- [ ] **Step 6: Commit the interactive workflow**

```text
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: add interactive upstream sync workflow"
```

---

### Task 4: Offline integration test and user documentation

**Files:**
- Modify: `scripts/tests/test_sync_upstream.py`
- Modify: `README.md`

**Interfaces:**
- Consumes: the completed `scripts/sync-upstream.py` command and all public workflow functions.
- Produces: documented invocation and an offline end-to-end regression test.

- [ ] **Step 1: Add a failing local-repository integration test**

In a temporary directory, create a bare `upstream.git`, clone it twice as
`source` and `fork`, configure repository-local test identity, and create a
shared base commit. In `source`, add two sequential commits and push them to
`main`. In `fork`, add the bare repository as `upstream`, then invoke the script
with subprocess input selecting both displayed commits and confirming `yes`.

Assert:

- Exit code is 0.
- The checked-out branch starts with `sync/upstream-`.
- `git log --format=%s main..HEAD` contains the two source subjects in newest-first log order.
- `git status --porcelain` is empty.
- The original `main` ref still points to the shared base commit.

- [ ] **Step 2: Run the integration test and confirm any uncovered behavior fails**

Run:

```text
python -m unittest scripts.tests.test_sync_upstream.LocalRepositoryIntegrationTests -v
```

Expected before final test-fixture adjustments: failure exposing any mismatch in
default branch naming, Git identity, selection order, or subprocess input.

- [ ] **Step 3: Make the minimum portability fixes required by the integration test**

Initialize temporary repositories with `git init --initial-branch=main`, set
`user.name` and `user.email` locally, pass paths as separate subprocess
arguments, and avoid shell invocation. If the product script—not the fixture—is
responsible for a failure, change only that behavior and add a focused unit test
beside the integration test before rerunning.

- [ ] **Step 4: Document the workflow in the root README**

Add `## Sync selected upstream changes` with:

```text
python scripts/sync-upstream.py
```

Document Python 3 and Git prerequisites, automatic setup of the fixed
`upstream` remote, numbered selection syntax (`2`, `1,4`, `2-5`), the clean
worktree requirement, automatic `sync/upstream-YYYY-MM-DD` branch, conflict
recovery with `git cherry-pick --continue` or `git cherry-pick --abort`, and the
printed fast-forward integration commands. State explicitly that the helper
does not merge or push.

- [ ] **Step 5: Run full verification**

Run:

```text
python -m unittest discover -s scripts/tests -p "test_*.py" -v
python scripts/sync-upstream.py --help
git diff --check
git status --short
```

Expected: all tests pass, help exits 0, `git diff --check` emits nothing, and
status contains only the intended script, test, README, and plan changes for the
current task.

- [ ] **Step 6: Commit documentation and integration coverage**

```text
git add README.md scripts/tests/test_sync_upstream.py
git commit -m "docs: explain selective upstream sync"
```

---

### Task 5: Final behavior review

**Files:**
- Review: `scripts/sync-upstream.py`
- Review: `scripts/tests/test_sync_upstream.py`
- Review: `README.md`

**Interfaces:**
- Consumes: all completed functionality.
- Produces: verified handoff with no additional behavior.

- [ ] **Step 1: Compare implementation against every design requirement**

Confirm the implementation: validates clean attached state; safely recognizes
or adds the fixed remote; fetches with pruning; omits patch-equivalent commits;
lists newest-first; applies oldest-first; confirms before branch creation;
creates a unique branch; preserves conflict state; prints integration commands;
and never merges, pushes, deletes, forces, or automatically aborts.

- [ ] **Step 2: Run final verification from a clean process**

```text
python -m unittest discover -s scripts/tests -p "test_*.py" -v
python scripts/sync-upstream.py --help
git diff --check HEAD
git status --short --branch
```

Expected: all tests pass, help exits 0, no whitespace errors are reported, and
the branch status accurately reflects only the intended committed work.

- [ ] **Step 3: Inspect the final commit series**

```text
git log --oneline --decorate -6
```

Expected: separate commits for the policy layer, Git discovery, interactive
workflow, and documentation/integration coverage, with no unrelated changes.
