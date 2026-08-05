# Upstream Sync Ledger and Draft PR Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the upstream sync helper remember reviewed commits across runs, resume safely after partial failures, and push each confirmed run as a draft GitHub pull request.

**Architecture:** Keep the dependency-free implementation in `scripts/sync-upstream.py`, extending its pure data/policy functions and its existing Git adapter while adding a parallel GitHub CLI adapter. Commit durable decisions to `.github/upstream-sync.json`; keep only incomplete workflow state beneath `.git` so conflict and network recovery are idempotent.

**Tech Stack:** Python 3 standard library (`argparse`, `dataclasses`, `datetime`, `json`, `pathlib`, `subprocess`, `unittest`), Git, GitHub CLI (`gh`), JSON.

## Global Constraints

- Preserve Windows and Linux support and add no Python package dependency.
- Keep `https://github.com/zeronsh/comet.git` and `upstream/main` fixed.
- Outcomes are exactly `implemented`, `not-applicable`, and `deferred`.
- Patch-equivalence filtering remains the first discovery layer; ledger filtering is the second.
- `implemented` and `not-applicable` commits stay hidden; `deferred` commits reappear.
- No branch is created until the user confirms the complete run summary.
- Push only to `origin`; open only draft PRs against the branch where the run began.
- Never rewrite remotes, overwrite branches, force-push, merge, delete branches, or change an existing PR's review state.
- Leave cherry-pick conflict state intact and resume only after the user completes `git cherry-pick --continue`.
- Never store credentials or environment values in the ledger, pending state, errors, or PR body.

## File Structure

- Create `.github/upstream-sync.json`: versioned committed state, current decisions, and append-only run history.
- Modify `scripts/sync-upstream.py`: ledger models and validation, classifications, pending-state machine, GitHub CLI adapter, push, resume, and draft PR workflow.
- Create `scripts/tests/test_upstream_sync_ledger.py`: focused ledger schema, seed, filtering, update, and serialization tests.
- Modify `scripts/tests/test_sync_upstream.py`: prompt, Git/GitHub adapter, state-machine, resume, PR, and integration coverage.
- Modify `README.md`: prerequisites, decision model, draft PR behavior, and recovery instructions.

---

### Task 1: Seed and validate the committed ledger

**Files:**
- Create: `.github/upstream-sync.json`
- Modify: `scripts/sync-upstream.py:1-23`
- Create: `scripts/tests/test_upstream_sync_ledger.py`

**Interfaces:**
- Consumes: existing `Commit` records from `scripts/sync-upstream.py`.
- Produces: `LedgerEntry`, `RunDecision`, `SyncRun`, and `Ledger`; `load_ledger(path: Path) -> Ledger`; `serialize_ledger(ledger: Ledger) -> str`; `write_ledger(path: Path, ledger: Ledger) -> None`.

- [ ] **Step 1: Add failing seed and schema tests**

Create `scripts/tests/test_upstream_sync_ledger.py` using the same `importlib.util.spec_from_file_location` loader as the existing test file. Add tests that load the real repository ledger and assert:

```python
class SeedLedgerTests(unittest.TestCase):
    def test_seed_contains_every_traced_implementation(self):
        ledger = sync.load_ledger(ROOT / ".github" / "upstream-sync.json")
        expected = {
            "ff124f4142144285b8f10df152837f55c35ab20a":
                "6a6f6b8fc3c1895f076d2b1b9208eb034b7df67f",
            "760ea9a44b5d7b69b68fccbca39cf7fff66879fc":
                "adc28704de3159960b4a9494ce824ab5296113e9",
            "ff5e483baf2a25c994c86949183c6ab6a6469612":
                "4a3c10163dd8e3bdb4994f4c6a7c00b333340315",
            "7b52ce1f70b3dddf13756358c4dc1f9d810a0bad":
                "3e5ea92a87775cd1f56e9ad1790a0a037c95cec6",
        }
        self.assertEqual(
            {sha: ledger.commits[sha].local_commit for sha in expected},
            expected,
        )
        self.assertTrue(all(ledger.commits[sha].outcome == "implemented"
                            for sha in expected))

    def test_seed_marks_hosted_workspace_changes_not_applicable(self):
        ledger = sync.load_ledger(ROOT / ".github" / "upstream-sync.json")
        hosted = {
            "b01943979d3732fd615ab2c002185dbb1016a601",
            "c1243c5ab187f39c58905edc8b195504a005a51f",
            "79c8e22898d8c7b45150c06a8c6e97de64cfbd0d",
        }
        self.assertEqual(
            {sha: ledger.commits[sha].outcome for sha in hosted},
            {sha: "not-applicable" for sha in hosted},
        )
        self.assertEqual(ledger.runs[0].run_id, "bootstrap-2026-08-05")
        self.assertEqual(ledger.runs[0].kind, "bootstrap")
```

Also use temporary JSON files to assert rejection of schema version `2`, a short SHA, a key/embedded-SHA mismatch, an unknown outcome, a missing note, a `local_commit` on a non-implemented outcome, duplicate run IDs, and a bootstrap run with a non-null `sync_branch`. Assert that a manually recorded `implemented` entry may omit its optional local reference.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest scripts.tests.test_upstream_sync_ledger -v`

Expected: FAIL because the ledger and model/loading functions do not exist.

- [ ] **Step 3: Add the ledger models, loader, serializer, and seed file**

Add these immutable records near `Commit`:

```python
OUTCOMES = frozenset({"implemented", "not-applicable", "deferred"})
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")

@dataclass(frozen=True)
class LedgerEntry:
    upstream_sha: str
    subject: str
    outcome: str
    decision_date: str
    note: str
    local_commit: str | None = None

@dataclass(frozen=True)
class RunDecision:
    upstream_sha: str
    subject: str
    outcome: str
    note: str
    local_commit: str | None = None

@dataclass(frozen=True)
class SyncRun:
    run_id: str
    kind: str
    date: str
    target_branch: str
    sync_branch: str | None
    decisions: tuple[RunDecision, ...]

@dataclass(frozen=True)
class Ledger:
    schema_version: int
    commits: dict[str, LedgerEntry]
    runs: tuple[SyncRun, ...]
```

Implement strict dictionary-to-record conversion. `kind` is `bootstrap` or `sync`; only `bootstrap` permits `sync_branch=None`. Require a non-empty note and full lowercase upstream SHAs. Permit `local_commit` only for `implemented`; when present it must be a full lowercase SHA, but it remains optional for manually adapted implementations. `serialize_ledger` must use `json.dumps(..., indent=2, sort_keys=True) + "\n"`; `write_ledger` opens the path with `encoding="utf-8", newline="\n"` and writes that serialized text.

Create `.github/upstream-sync.json` with `schema_version: 1`, seven current records, and one `bootstrap-2026-08-05` run. Use the exact four mappings above. Give the three hosted changes notes naming the removed `edge/src/session-room.ts` service or absent hosted workspace snapshot backing service. Set the bootstrap target to `main`, `sync_branch` to `null`, and include all seven decisions.

- [ ] **Step 4: Run ledger tests and the existing suite**

Run: `python -m unittest scripts.tests.test_upstream_sync_ledger scripts.tests.test_sync_upstream -v`

Expected: PASS.

- [ ] **Step 5: Commit the ledger foundation**

```bash
git add .github/upstream-sync.json scripts/sync-upstream.py scripts/tests/test_upstream_sync_ledger.py
git commit -m "feat: seed upstream sync decision ledger"
```

---

### Task 2: Filter and classify commits into auditable runs

**Files:**
- Modify: `scripts/sync-upstream.py:96-158,175-243`
- Modify: `scripts/tests/test_upstream_sync_ledger.py`
- Modify: `scripts/tests/test_sync_upstream.py:185-298,505-596`

**Interfaces:**
- Consumes: `Commit`, `Ledger`, `LedgerEntry`, `RunDecision`, and `SyncRun` from Task 1.
- Produces: `filter_resolved(commits: list[Commit], ledger: Ledger) -> list[Commit]`; `parse_selection(text: str, commits: list[Commit], allow_empty: bool = False) -> list[Commit]`; `classify_unselected(commits: list[Commit], selected_oids: set[str], input_fn: Callable[[str], str], output: TextIO) -> list[RunDecision]`; `build_sync_run(day: date, target: str, sync_branch: str, selected: list[Commit], local_commits: dict[str, str], classifications: list[RunDecision]) -> SyncRun`; `apply_run(ledger: Ledger, run: SyncRun) -> Ledger`.

- [ ] **Step 1: Add failing filtering and classification tests**

Cover these exact policies:

```python
def test_filter_resolved_keeps_only_deferred_and_unknown():
    commits = [implemented, not_applicable, deferred, unknown]
    self.assertEqual(
        [c.oid for c in sync.filter_resolved(commits, ledger)],
        [deferred.oid, unknown.oid],
    )

def test_unselected_defaults_to_deferred():
    decisions = sync.classify_unselected(
        [commit], set(), lambda prompt: "", io.StringIO()
    )
    self.assertEqual(decisions[0].outcome, "deferred")
    self.assertEqual(decisions[0].note, "Deferred during interactive review.")

def test_not_applicable_requires_a_reason():
    answers = iter(["n", "", "Hosted workspace service was removed."])
    decisions = sync.classify_unselected(
        [commit], set(), lambda prompt: next(answers), io.StringIO()
    )
    self.assertEqual(decisions[0].outcome, "not-applicable")
    self.assertEqual(decisions[0].note,
                     "Hosted workspace service was removed.")
```

Also test that `parse_selection("", commits, allow_empty=True)` returns an empty list while the default still rejects empty input, selected commits are not prompted, invalid classification input is retried, `build_sync_run` uses the sync branch as its unique run ID, `apply_run` updates current deferred state while preserving older runs, and duplicate run IDs are rejected.

- [ ] **Step 2: Run focused tests to verify failure**

Run: `python -m unittest scripts.tests.test_upstream_sync_ledger scripts.tests.test_sync_upstream.WorkflowTests -v`

Expected: FAIL because filtering, classification, and run-update functions are absent.

- [ ] **Step 3: Implement pure decision policy**

Implement:

```python
def filter_resolved(commits: list[Commit], ledger: Ledger) -> list[Commit]:
    return [
        commit for commit in commits
        if ledger.commits.get(commit.oid) is None
        or ledger.commits[commit.oid].outcome == "deferred"
    ]
```

Extend `parse_selection` with `allow_empty`; only a blank selection in that explicit mode returns `[]`, preserving all existing validation otherwise. `classify_unselected` prompts `Commit <short> was not selected: [d]efer/[n]ot-applicable [d] `. Blank or `d` yields the fixed deferred note. `n` prompts `Reason this commit is not applicable: ` until non-empty. Selected commits become `RunDecision(..., outcome="implemented", note="Cherry-picked by upstream sync helper.")` only after the workflow supplies the resulting local SHA.

`build_sync_run` groups all decisions in upstream chronological order and uses the final sync branch name, such as `sync/upstream-2026-08-05-2`, for both `run_id` and `sync_branch`. Branch-name uniqueness therefore guarantees run-ID uniqueness. `apply_run` appends the run and updates current entries without mutating its input.

Load the ledger before discovery in `run_workflow`, pass discovered commits through `filter_resolved`, and change the empty result message to `No unresolved upstream commits.` Do not yet change branch creation, pushing, or PR behavior.

- [ ] **Step 4: Run focused and full tests**

Run: `python -m unittest discover -s scripts/tests -p "test_*.py" -v`

Expected: PASS.

- [ ] **Step 5: Commit filtering and classification**

```bash
git add scripts/sync-upstream.py scripts/tests/test_upstream_sync_ledger.py scripts/tests/test_sync_upstream.py
git commit -m "feat: classify upstream sync decisions"
```

---

### Task 3: Add GitHub preflight and draft PR construction

**Files:**
- Modify: `scripts/sync-upstream.py:24-62,167-174,266-299`
- Modify: `scripts/tests/test_sync_upstream.py:119-184,299-427`

**Interfaces:**
- Consumes: `GitError`-style subprocess error formatting and `SyncRun` from Tasks 1-2.
- Produces: `GhError`, `Gh.run(*args: str, check: bool = True) -> CompletedProcess[str]`; `validate_origin(git: Git) -> None`; `validate_github(gh: Gh) -> None`; `format_pr(run: SyncRun) -> tuple[str, str]`; `find_existing_pr(...) -> str | None`; `create_draft_pr(...) -> str`.

- [ ] **Step 1: Add failing GitHub adapter and formatting tests**

Add a `ScriptedGh` fake parallel to `ScriptedGit`. Assert:

```python
def test_github_preflight_checks_authentication():
    gh = ScriptedGh({("auth", "status"): completed(0)})
    sync.validate_github(gh)
    self.assertEqual(gh.commands, [("auth", "status")])

def test_pr_body_groups_all_outcomes():
    title, body = sync.format_pr(run_with_three_outcomes)
    self.assertEqual(title, "Sync upstream commits (2026-08-05)")
    self.assertIn("## Implemented", body)
    self.assertIn("`upstreamsha` → `localsha`", body)
    self.assertIn("## Not applicable", body)
    self.assertIn("## Deferred", body)

def test_create_pr_is_draft_against_original_target():
    url = sync.create_draft_pr(gh, run, title, body)
    self.assertIn(("pr", "create", "--draft", "--base", "main",
                   "--head", run.sync_branch, "--title", title,
                   "--body", body), gh.commands)
```

Test `validate_origin` rejects a missing origin before mutations. Test missing `gh`, failed `gh auth status`, concise `GhError` detail, no matching PR, one matching PR URL, malformed multi-result JSON, and reuse of a matching open PR.

- [ ] **Step 2: Run adapter tests to verify failure**

Run: `python -m unittest scripts.tests.test_sync_upstream.GitHubAdapterTests scripts.tests.test_sync_upstream.PullRequestTests -v`

Expected: FAIL because the GitHub adapter and PR helpers do not exist.

- [ ] **Step 3: Implement preflight, lookup, and draft creation**

Model `GhError` after `GitError`; `Gh.run` invokes `gh` with captured UTF-8 output and converts `FileNotFoundError` to `SyncError("GitHub CLI is not installed or is not on PATH.")`. `validate_github` runs `gh auth status`. `validate_origin` uses `git remote get-url origin` and reports a missing remote without creating it.

`find_existing_pr` runs:

```text
gh pr list --state open --base <target> --head <sync-branch> --json url
```

Parse JSON in Python. Return `None` for an empty list, the URL for one item, and raise `SyncError` for malformed data or multiple matching PRs. `create_draft_pr` passes title and body as direct subprocess arguments, not shell text, and returns the URL printed by `gh pr create`.

`format_pr` emits stable Markdown headings in implemented, not-applicable, deferred order. Include the full subject and short upstream/local SHAs; include notes for non-implemented decisions. Omit empty groups.

Add preflight calls after repository/upstream validation and before interactive selection. Do not push or create a PR from the workflow until Task 5.

- [ ] **Step 4: Run adapter and full tests**

Run: `python -m unittest discover -s scripts/tests -p "test_*.py" -v`

Expected: PASS.

- [ ] **Step 5: Commit the GitHub boundary**

```bash
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: add draft PR integration boundary"
```

---

### Task 4: Persist and validate resumable pending runs

**Files:**
- Modify: `scripts/sync-upstream.py:12-23,64-77,149-166`
- Modify: `scripts/tests/test_sync_upstream.py`

**Interfaces:**
- Consumes: `RunDecision`, `SyncRun`, and `Git`.
- Produces: `SyncCancelled`; `PendingRun`; `pending_state_path(git: Git) -> Path`; `load_pending(path: Path) -> PendingRun`; `write_pending(path: Path, pending: PendingRun) -> None`; `clear_pending(path: Path) -> None`; `record_pick_start(pending: PendingRun, commit: Commit, pre_pick_head: str) -> PendingRun`; `record_pick_success(pending: PendingRun, local_commit: str) -> PendingRun`; `verify_resumed_pick(pending: PendingRun, current_head: str) -> PendingRun`.

- [ ] **Step 1: Add failing pending-state tests**

Define test fixtures for these phases: `prepared`, `cherry-picking`, `ledger-committed`, and `pushed`. Assert stable JSON round trips, rejection of unknown phases, rejection when `local_commits` contains more entries than `selected`, rejection when its keys are not a chronological prefix of `selected`, rejection when `active_upstream_sha` does not equal the next selected SHA, and rejection when branch or target differs.

Add resume verification tests:

```python
def test_resume_records_manually_continued_cherry_pick():
    pending = pending_with_active_pick(pre_pick_head="a" * 40)
    updated = sync.verify_resumed_pick(pending, current_head="b" * 40)
    self.assertEqual(updated.local_commits[pending.active_upstream_sha],
                     "b" * 40)
    self.assertIsNone(updated.active_upstream_sha)

def test_resume_rejects_unfinished_cherry_pick():
    git = ScriptedGit({("rev-parse", "-q", "--verify", "CHERRY_PICK_HEAD"):
                       completed(0, stdout="c" * 40)})
    with self.assertRaisesRegex(sync.SyncError, "cherry-pick --continue"):
        sync.ensure_cherry_pick_resolved(git)
```

Also test that current `HEAD` equal to `pre_pick_head` raises `SyncCancelled`, pending-file cleanup only after success or a detected abort, and path resolution through `git rev-parse --git-path upstream-sync-state.json`.

- [ ] **Step 2: Run pending-state tests to verify failure**

Run: `python -m unittest scripts.tests.test_sync_upstream.PendingStateTests -v`

Expected: FAIL because pending-state functions do not exist.

- [ ] **Step 3: Implement pending state and transition helpers**

Use this immutable shape:

```python
@dataclass(frozen=True)
class PendingRun:
    schema_version: int
    phase: str
    target_branch: str
    sync_branch: str
    run_id: str
    run_date: str
    selected: tuple[Commit, ...]
    classifications: tuple[RunDecision, ...]
    local_commits: dict[str, str]
    active_upstream_sha: str | None = None
    pre_pick_head: str | None = None
```

Resolve a relative `--git-path` result against `git.cwd`. Write pending JSON atomically by writing a sibling `.tmp` with `Path.write_text` and replacing the target with `Path.replace`. Before each `git cherry-pick`, store the active upstream SHA and current full `HEAD`. After success, map that upstream SHA to the new full `HEAD` and clear active fields.

The `prepared` phase supports the boundary between pending-state creation and branch creation. On resume, if the target is still checked out and the sync branch is absent, create it and advance to `cherry-picking`; if the sync branch is already checked out, advance without recreating it; reject every other branch arrangement.

During `cherry-picking`, require the recorded sync branch and reject an extant `CHERRY_PICK_HEAD`. A changed `HEAD` records the manually continued pick. An unchanged `HEAD` raises `SyncCancelled`; the resume workflow clears pending state, leaves the sync branch intact, and tells the user the run was cancelled and which target branch to switch back to. Do not infer success from subject text or abbreviated IDs.

- [ ] **Step 4: Run pending-state and full tests**

Run: `python -m unittest discover -s scripts/tests -p "test_*.py" -v`

Expected: PASS.

- [ ] **Step 5: Commit resumable state support**

```bash
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: persist resumable upstream sync state"
```

---

### Task 5: Orchestrate cherry-picks, ledger commit, push, and draft PR

**Files:**
- Modify: `scripts/sync-upstream.py:175-299`
- Modify: `scripts/tests/test_sync_upstream.py:26-298,316-393`

**Interfaces:**
- Consumes: every interface from Tasks 1-4.
- Produces: `start_workflow(...) -> int`; `resume_workflow(...) -> int`; `finish_workflow(...) -> int`; CLI flag `--resume`.

- [ ] **Step 1: Add failing state-machine tests**

Replace the old successful-workflow expectation with assertions that the helper:

1. Loads and filters the ledger.
2. Prompts for selection and unselected classifications.
3. Prints grouped decisions and confirms once.
4. Creates pending state before `git switch -c`.
5. Cherry-picks oldest first and records each resulting `HEAD`.
6. Writes and commits `.github/upstream-sync.json`.
7. Runs `git push --set-upstream origin <sync-branch>`.
8. Reuses an existing matching PR or runs `gh pr create --draft`.
9. Clears pending state and prints the PR URL.

Add explicit tests for declined confirmation, no unresolved commits, a bookkeeping-only run, conflict before any ledger write, resume after manual conflict resolution, cleanup after `git cherry-pick --abort`, resume from `ledger-committed`, resume from `pushed`, push failure, PR failure, existing PR reuse, and `--resume` without pending state.

The bookkeeping-only assertion must contain no `cherry-pick` command but must contain branch creation, ledger commit, push, and draft PR creation.

- [ ] **Step 2: Run workflow tests to verify failure**

Run: `python -m unittest scripts.tests.test_sync_upstream.WorkflowTests scripts.tests.test_sync_upstream.ResumeWorkflowTests scripts.tests.test_sync_upstream.CliTests -v`

Expected: FAIL because the workflow does not yet persist, push, create PRs, or resume.

- [ ] **Step 3: Implement the phase-driven workflow**

Split orchestration into:

```python
def start_workflow(
    git: Git, gh: Gh, input_fn: Callable[[str], str], output: TextIO,
    day: date, ledger_path: Path,
) -> int: ...

def resume_workflow(
    git: Git, gh: Gh, output: TextIO, ledger_path: Path,
    pending_path: Path,
) -> int: ...

def finish_workflow(
    git: Git, gh: Gh, output: TextIO, ledger_path: Path,
    pending_path: Path, pending: PendingRun,
) -> int: ...
```

`start_workflow` performs preflight, discovery, classification, summary, and confirmation. It creates and writes `PendingRun` immediately before branch creation. It then delegates to `resume_workflow` so the same code handles first execution and retries.

`resume_workflow` validates recorded state, accounts for a manually continued active pick, applies remaining picks with pending writes around each, and delegates to `finish_workflow`. If active-pick verification raises `SyncCancelled`, it clears pending state, prints that the run was cancelled and names the target branch, and returns nonzero without deleting or switching branches.

`finish_workflow` constructs implemented decisions from recorded local SHAs, applies the run to the latest on-disk ledger, writes it, runs `git add -- .github/upstream-sync.json`, and commits with `chore: record upstream sync <run-id>`. Set phase `ledger-committed` only after the commit succeeds, `pushed` only after push succeeds, then find or create the PR. Clear pending state only after obtaining the PR URL.

Update argparse with `--resume`. Normal mode refuses to start if pending state already exists and directs the user to resume. Resume mode allows the tracked ledger modification created by its own earlier phase but otherwise applies phase-specific cleanliness checks. Catch `GhError` next to `GitError` with the same one-line detail policy.

- [ ] **Step 4: Run workflow and integration tests**

Run: `python -m unittest discover -s scripts/tests -p "test_*.py" -v`

Expected: PASS.

- [ ] **Step 5: Commit the complete workflow**

```bash
git add scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "feat: open draft PRs for upstream sync runs"
```

---

### Task 6: Exercise real Git behavior and document the workflow

**Files:**
- Modify: `scripts/tests/test_sync_upstream.py:26-118`
- Modify: `README.md:86-111`
- Modify: `scripts/sync-upstream.py:266-287`

**Interfaces:**
- Consumes: completed CLI and workflow from Task 5.
- Produces: user-facing help and an offline integration test covering the durable Git artifacts.

- [ ] **Step 1: Extend the integration and CLI tests first**

Update `LocalRepositoryIntegrationTests` to seed a minimal ledger and call `start_workflow` with real `Git(cwd=fork)`, a `ScriptedGh` fake, and `mock.patch.object(sync, "is_expected_upstream", return_value=True)` for the local bare upstream. Assert:

```python
self.assertEqual(git("rev-parse", "--abbrev-ref", "HEAD", cwd=fork),
                 sync_branch)
self.assertEqual(git("log", "-1", "--format=%s", cwd=fork),
                 f"chore: record upstream sync {run_id}")
ledger = json.loads((fork / ".github" / "upstream-sync.json").read_text())
self.assertEqual(ledger["commits"][upstream_sha]["outcome"], "implemented")
self.assertRegex(
    ledger["commits"][upstream_sha]["local_commit"], r"^[0-9a-f]{40}$"
)
self.assertFalse((git_dir / "upstream-sync-state.json").exists())
```

Add a second integration test that forces a conflict, completes it with ordinary Git commands, invokes `--resume`, and verifies the remaining commit and ledger metadata are completed once. Update help tests to require `--resume`, `gh auth login`, committed ledger, push, and draft PR language.

- [ ] **Step 2: Run integration and help tests to verify failure**

Run: `python -m unittest scripts.tests.test_sync_upstream.LocalRepositoryIntegrationTests scripts.tests.test_sync_upstream.CliTests -v`

Expected: FAIL until the integration harness and help text reflect the new workflow.

- [ ] **Step 3: Update README and command help**

Replace the current no-push handoff with documentation that requires Python 3, Git, `gh`, and an authenticated `gh auth login`. Explain the three ledger outcomes, that blank classification defers, and that confirmed runs create a sync branch, commit `.github/upstream-sync.json`, push to `origin`, and open a draft PR.

Document conflict recovery exactly:

```text
git add <resolved-files>
git cherry-pick --continue
python scripts/sync-upstream.py --resume
```

Explain that `--resume` is also the retry command after push or PR creation fails and that bookkeeping-only decisions still create a draft PR. Retain the warnings that the helper never merges, force-pushes, deletes branches, or overwrites remotes.

- [ ] **Step 4: Run all verification commands**

Run:

```text
python -m unittest discover -s scripts/tests -p "test_*.py" -v
python scripts/sync-upstream.py --help
git diff --check
```

Expected: all tests PASS, help exits `0` and describes ledger/draft/resume behavior, and `git diff --check` prints nothing.

- [ ] **Step 5: Commit documentation and integration coverage**

```bash
git add README.md scripts/sync-upstream.py scripts/tests/test_sync_upstream.py
git commit -m "docs: explain tracked upstream sync PRs"
```

---

### Task 7: Final regression and scope verification

**Files:**
- Review: `.github/upstream-sync.json`
- Review: `scripts/sync-upstream.py`
- Review: `scripts/tests/test_upstream_sync_ledger.py`
- Review: `scripts/tests/test_sync_upstream.py`
- Review: `README.md`

**Interfaces:**
- Consumes: completed implementation.
- Produces: verification evidence and a clean, review-ready branch.

- [ ] **Step 1: Run the complete Python test suite from a clean process**

Run: `python -m unittest discover -s scripts/tests -p "test_*.py" -v`

Expected: every test passes with no network access.

- [ ] **Step 2: Verify static repository invariants**

Run:

```text
python scripts/sync-upstream.py --help
python -m json.tool .github/upstream-sync.json
git diff --check main...HEAD
git status --short
```

Expected: help succeeds; JSON validation succeeds; diff check is empty; status is clean.

- [ ] **Step 3: Inspect the final diff for scope and secrets**

Run:

```text
git diff --stat main...HEAD
git diff main...HEAD -- .github/upstream-sync.json scripts/sync-upstream.py scripts/tests/test_upstream_sync_ledger.py scripts/tests/test_sync_upstream.py README.md
git diff main...HEAD | rg -n -i "token|password|secret|authorization"
```

Expected: only the five planned implementation files plus the approved design/plan docs are changed; the final search finds no credential values.

- [ ] **Step 4: Commit any verification-only corrections**

If verification required a correction, rerun Steps 1-3, then commit only the corrected planned files with a specific message. If no correction was required, do not create an empty commit.
