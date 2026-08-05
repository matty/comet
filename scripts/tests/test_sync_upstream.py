import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from datetime import date
from dataclasses import replace
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).parents[1] / "sync-upstream.py"
SPEC = importlib.util.spec_from_file_location("sync_upstream", SCRIPT_PATH)
sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync)

COMMITS = [
    sync.Commit("c1", "c1", "2026-08-01", "Alice", "First"),
    sync.Commit("c2", "c2", "2026-08-02", "Bob", "Second"),
    sync.Commit("c3", "c3", "2026-08-03", "Carol", "Third"),
    sync.Commit("c4", "c4", "2026-08-04", "Dave", "Fourth"),
]
DISPLAYED = list(reversed(COMMITS))


class LocalRepositoryIntegrationTests(unittest.TestCase):
    def test_selected_upstream_commits_are_cherry_picked_without_moving_main(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            upstream = root / "upstream.git"
            source = root / "source"
            fork = root / "fork"

            def git(*args, cwd=None):
                return subprocess.run(
                    ["git", *args],
                    cwd=cwd,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=True,
                )

            git("init", "--bare", "--initial-branch=main", str(upstream))
            git("clone", str(upstream), str(source))
            git("config", "user.name", "Comet Test", cwd=source)
            git("config", "user.email", "comet-test@example.invalid", cwd=source)
            (source / "shared.txt").write_text("base\n", encoding="utf-8")
            git("add", "shared.txt", cwd=source)
            git("commit", "-m", "Shared base", cwd=source)
            git("push", "origin", "main", cwd=source)

            git("clone", str(upstream), str(fork))
            git("config", "user.name", "Comet Test", cwd=fork)
            git("config", "user.email", "comet-test@example.invalid", cwd=fork)
            (fork / ".github").mkdir()
            ledger_path = fork / ".github" / "upstream-sync.json"
            sync.write_ledger(ledger_path, sync.Ledger(1, {}, ()))
            git("add", ".github/upstream-sync.json", cwd=fork)
            git("commit", "-m", "Seed empty upstream ledger", cwd=fork)
            base_oid = git("rev-parse", "main", cwd=fork).stdout.strip()

            (source / "first.txt").write_text("first\n", encoding="utf-8")
            git("add", "first.txt", cwd=source)
            git("commit", "-m", "First upstream change", cwd=source)
            (source / "second.txt").write_text("second\n", encoding="utf-8")
            git("add", "second.txt", cwd=source)
            git("commit", "-m", "Second upstream change", cwd=source)
            git("push", "origin", "main", cwd=source)

            git("remote", "add", "upstream", str(upstream), cwd=fork)
            answers = iter(["1,2", "yes"])
            output = io.StringIO()
            with mock.patch.object(sync, "is_expected_upstream", return_value=True):
                result = sync.start_workflow(
                    sync.Git(fork),
                    TrackedWorkflowGh(),
                    lambda prompt: next(answers),
                    output,
                    date(2026, 8, 5),
                    ledger_path,
                )

            self.assertEqual(result, 0)
            eligible_output = output.getvalue().split("Run summary:", 1)[0]
            self.assertLess(
                eligible_output.index("Second upstream change"),
                eligible_output.index("First upstream change"),
            )
            branch = git(
                "symbolic-ref", "--quiet", "--short", "HEAD", cwd=fork
            ).stdout.strip()
            self.assertTrue(branch.startswith("sync/upstream-"), branch)
            subjects = git(
                "log", "--format=%s", "main..HEAD", cwd=fork
            ).stdout.splitlines()
            self.assertEqual(
                subjects,
                [
                    "chore: record upstream sync sync/upstream-2026-08-05",
                    "Second upstream change",
                    "First upstream change",
                ],
            )
            ledger = sync.load_ledger(ledger_path)
            self.assertEqual(len(ledger.runs), 1)
            self.assertTrue(
                all(entry.outcome == "implemented"
                    for entry in ledger.commits.values())
            )
            self.assertEqual(git("status", "--porcelain", cwd=fork).stdout, "")
            self.assertEqual(git("rev-parse", "main", cwd=fork).stdout.strip(), base_oid)

    def test_conflicted_pick_resumes_and_records_remaining_commits_once(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            upstream = root / "upstream.git"
            source = root / "source"
            fork = root / "fork"

            def git(*args, cwd=None):
                return subprocess.run(
                    ["git", *args],
                    cwd=cwd,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=True,
                )

            git("init", "--bare", "--initial-branch=main", str(upstream))
            git("clone", str(upstream), str(source))
            git("config", "user.name", "Comet Test", cwd=source)
            git("config", "user.email", "comet-test@example.invalid", cwd=source)
            (source / "shared.txt").write_text("base\n", encoding="utf-8")
            git("add", "shared.txt", cwd=source)
            git("commit", "-m", "Shared base", cwd=source)
            git("push", "origin", "main", cwd=source)

            git("clone", str(upstream), str(fork))
            git("config", "user.name", "Comet Test", cwd=fork)
            git("config", "user.email", "comet-test@example.invalid", cwd=fork)
            (fork / ".github").mkdir()
            ledger_path = fork / ".github" / "upstream-sync.json"
            sync.write_ledger(ledger_path, sync.Ledger(1, {}, ()))
            git("add", ".github/upstream-sync.json", cwd=fork)
            git("commit", "-m", "Seed empty upstream ledger", cwd=fork)
            (fork / "shared.txt").write_text("fork\n", encoding="utf-8")
            git("add", "shared.txt", cwd=fork)
            git("commit", "-m", "Fork edits shared file", cwd=fork)
            main_oid = git("rev-parse", "main", cwd=fork).stdout.strip()

            (source / "shared.txt").write_text("upstream\n", encoding="utf-8")
            git("add", "shared.txt", cwd=source)
            git("commit", "-m", "Upstream edits shared file", cwd=source)
            conflict_oid = git("rev-parse", "HEAD", cwd=source).stdout.strip()
            (source / "later.txt").write_text("later\n", encoding="utf-8")
            git("add", "later.txt", cwd=source)
            git("commit", "-m", "Later upstream change", cwd=source)
            later_oid = git("rev-parse", "HEAD", cwd=source).stdout.strip()
            git("push", "origin", "main", cwd=source)

            git("remote", "add", "upstream", str(upstream), cwd=fork)
            adapter = sync.Git(fork)
            gh = TrackedWorkflowGh()
            answers = iter(["1,2", "yes"])
            output = io.StringIO()
            with mock.patch.object(sync, "is_expected_upstream", return_value=True):
                result = sync.start_workflow(
                    adapter,
                    gh,
                    lambda prompt: next(answers),
                    output,
                    date(2026, 8, 5),
                    ledger_path,
                )

            self.assertEqual(result, 1)
            self.assertIn("git cherry-pick --continue", output.getvalue())
            pending_path = sync.pending_state_path(adapter)
            self.assertTrue(pending_path.exists())
            (fork / "shared.txt").write_text(
                "fork plus upstream\n", encoding="utf-8"
            )
            git("add", "shared.txt", cwd=fork)
            git("cherry-pick", "--continue", cwd=fork)

            resume_output = io.StringIO()
            result = sync.resume_workflow(
                adapter, gh, resume_output, ledger_path, pending_path
            )

            self.assertEqual(result, 0)
            self.assertFalse(pending_path.exists())
            self.assertTrue((fork / "later.txt").exists())
            ledger = sync.load_ledger(ledger_path)
            self.assertEqual(len(ledger.runs), 1)
            self.assertEqual(ledger.commits[conflict_oid].outcome, "implemented")
            self.assertEqual(ledger.commits[later_oid].outcome, "implemented")
            self.assertRegex(
                ledger.commits[conflict_oid].local_commit,
                r"^[0-9a-f]{40}$",
            )
            self.assertRegex(
                ledger.commits[later_oid].local_commit,
                r"^[0-9a-f]{40}$",
            )
            self.assertEqual(git("status", "--porcelain", cwd=fork).stdout, "")
            self.assertEqual(git("rev-parse", "main", cwd=fork).stdout.strip(), main_oid)


class ScriptedGit:
    def __init__(self, responses):
        self.responses = responses
        self.commands = []

    def run(self, *args, check=True):
        self.commands.append((args, check))
        response = self.responses[args]
        if isinstance(response, Exception):
            raise response
        if isinstance(response, subprocess.CompletedProcess):
            return response
        return subprocess.CompletedProcess(args, 0, response, "")


class ScriptedGh:
    def __init__(self, responses):
        self.responses = responses
        self.commands = []

    def run(self, *args, check=True):
        self.commands.append((args, check))
        response = self.responses[args]
        if isinstance(response, Exception):
            raise response
        if not isinstance(response, subprocess.CompletedProcess):
            response = subprocess.CompletedProcess(args, 0, response, "")
        if check and response.returncode:
            raise sync.GhError(
                list(args), response.returncode, response.stdout, response.stderr
            )
        return response


class TrackedWorkflowGit:
    def __init__(
        self,
        root,
        commits,
        local_commits=None,
        cherry_pick_failure=None,
        push_failure=None,
    ):
        self.cwd = Path(root)
        self.commits = commits
        self.local_commits = local_commits or {
            commit.oid: f"{index + 1:x}" * 40
            for index, commit in enumerate(reversed(commits))
        }
        self.cherry_pick_failure = cherry_pick_failure
        self.push_failure = push_failure
        self.current_branch = "main"
        self.branches = {"main"}
        self.head = "f" * 40
        self.cherry_pick_head = None
        self.commands = []

    @property
    def pending_path(self):
        return self.cwd / ".git" / "upstream-sync-state.json"

    def complete_conflict(self, local_commit):
        self.head = local_commit
        self.cherry_pick_head = None

    def abort_conflict(self):
        self.cherry_pick_head = None

    def run(self, *args, check=True):
        self.commands.append((args, check))
        returncode = 0
        stdout = ""
        stderr = ""
        if args == ("status", "--porcelain"):
            pass
        elif args == ("symbolic-ref", "--quiet", "--short", "HEAD"):
            stdout = self.current_branch + "\n"
        elif args == ("remote", "get-url", "upstream"):
            stdout = "https://github.com/zeronsh/comet.git\n"
        elif args == ("remote", "get-url", "origin"):
            stdout = "https://github.com/matty/comet\n"
        elif args == ("fetch", "--prune", "upstream", "main"):
            pass
        elif args[:5] == (
            "log",
            "--right-only",
            "--cherry-pick",
            "--topo-order",
            "--format=%H%x00%h%x00%cs%x00%an%x00%s",
        ):
            stdout = "".join(
                f"{commit.oid}\x00{commit.short_oid}\x00{commit.date}\x00"
                f"{commit.author}\x00{commit.subject}\n"
                for commit in self.commits
            )
        elif args == (
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/",
        ):
            stdout = "".join(f"{branch}\n" for branch in sorted(self.branches))
        elif args == (
            "rev-parse",
            "--git-path",
            "upstream-sync-state.json",
        ):
            stdout = str(self.pending_path) + "\n"
        elif args[:2] == ("switch", "-c"):
            branch = args[2]
            if branch in self.branches:
                returncode = 1
                stderr = "branch exists"
            else:
                self.branches.add(branch)
                self.current_branch = branch
        elif args == ("rev-parse", "HEAD"):
            stdout = self.head + "\n"
        elif args == ("rev-parse", "-q", "--verify", "CHERRY_PICK_HEAD"):
            if self.cherry_pick_head is None:
                returncode = 1
            else:
                stdout = self.cherry_pick_head + "\n"
        elif args[:1] == ("cherry-pick",):
            oid = args[1]
            if oid == self.cherry_pick_failure:
                returncode = 1
                stderr = "conflict in tracked.txt"
                self.cherry_pick_head = oid
            else:
                self.head = self.local_commits[oid]
        elif args == ("add", "--", ".github/upstream-sync.json"):
            pass
        elif args[:2] == ("commit", "-m"):
            self.head = "e" * 40
        elif args[:4] == ("push", "--set-upstream", "origin", args[-1]):
            if self.push_failure is not None:
                returncode = self.push_failure.returncode
                stdout = self.push_failure.stdout
                stderr = self.push_failure.stderr
        else:
            raise AssertionError(f"Unexpected Git command: {args!r}")
        result = subprocess.CompletedProcess(args, returncode, stdout, stderr)
        if check and returncode:
            raise sync.GitError(list(args), returncode, stdout, stderr)
        return result


class TrackedWorkflowGh:
    def __init__(self, existing_url=None, create_failure=None):
        self.existing_url = existing_url
        self.create_failure = create_failure
        self.commands = []
        self.created = False

    def run(self, *args, check=True):
        self.commands.append((args, check))
        if args == ("auth", "status"):
            result = subprocess.CompletedProcess(args, 0, "", "")
        elif args[:2] == ("pr", "list"):
            matches = [] if self.existing_url is None else [{"url": self.existing_url}]
            result = subprocess.CompletedProcess(args, 0, json.dumps(matches), "")
        elif args[:2] == ("pr", "create"):
            self.created = True
            if self.create_failure is not None:
                result = self.create_failure
            else:
                result = subprocess.CompletedProcess(
                    args, 0, "https://example/pr/7\n", ""
                )
        else:
            raise AssertionError(f"Unexpected gh command: {args!r}")
        if check and result.returncode:
            raise sync.GhError(
                list(args), result.returncode, result.stdout, result.stderr
            )
        return result


class FormattingTests(unittest.TestCase):
    def test_format_commit_numbers_only_browsing_entries(self):
        commit = sync.Commit("oid", "abc1234", "2026-08-04", "Alice", "Subject")

        self.assertEqual(
            sync.format_commit(2, commit),
            "2. abc1234  2026-08-04  Alice  Subject",
        )
        self.assertEqual(
            sync.format_commit(None, commit),
            "abc1234  2026-08-04  Alice  Subject",
        )


class TrackedWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / ".git").mkdir()
        (self.root / ".github").mkdir()
        self.ledger_path = self.root / ".github" / "upstream-sync.json"
        sync.write_ledger(self.ledger_path, sync.Ledger(1, {}, ()))
        self.oldest = sync.Commit(
            "a" * 40, "a" * 7, "2026-08-05", "Alice", "Oldest"
        )
        self.newest = sync.Commit(
            "b" * 40, "b" * 7, "2026-08-05", "Bob", "Newest"
        )
        self.displayed = [self.newest, self.oldest]

    def start(self, git, gh, answers):
        answer_iter = iter(answers)
        output = io.StringIO()
        result = sync.start_workflow(
            git,
            gh,
            lambda prompt: next(answer_iter),
            output,
            date(2026, 8, 5),
            self.ledger_path,
        )
        return result, output.getvalue()

    def test_confirmed_run_cherry_picks_updates_ledger_pushes_and_opens_draft(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        gh = TrackedWorkflowGh()

        result, output = self.start(git, gh, ["1-2", "y"])

        self.assertEqual(result, 0)
        cherry_picks = [
            args[1]
            for args, _ in git.commands
            if args[:1] == ("cherry-pick",)
        ]
        self.assertEqual(cherry_picks, [self.oldest.oid, self.newest.oid])
        ledger = sync.load_ledger(self.ledger_path)
        self.assertEqual(
            ledger.commits[self.oldest.oid].local_commit, "1" * 40
        )
        self.assertEqual(
            ledger.commits[self.newest.oid].local_commit, "2" * 40
        )
        self.assertEqual(ledger.runs[-1].sync_branch, git.current_branch)
        self.assertTrue(
            any(args[:3] == ("push", "--set-upstream", "origin")
                for args, _ in git.commands)
        )
        self.assertTrue(gh.created)
        self.assertFalse(git.pending_path.exists())
        self.assertIn("https://example/pr/7", output)

    def test_declined_confirmation_does_not_create_pending_state_or_branch(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        gh = TrackedWorkflowGh()
        original = self.ledger_path.read_text(encoding="utf-8")

        result, _ = self.start(git, gh, ["1", "d", "n"])

        self.assertEqual(result, 0)
        self.assertEqual(git.current_branch, "main")
        self.assertFalse(git.pending_path.exists())
        self.assertEqual(self.ledger_path.read_text(encoding="utf-8"), original)
        self.assertFalse(gh.created)

    def test_no_unresolved_commits_exits_without_branch(self):
        entries = {
            commit.oid: sync.LedgerEntry(
                commit.oid,
                commit.subject,
                "not-applicable",
                "2026-08-05",
                "Not used by this fork.",
            )
            for commit in self.displayed
        }
        sync.write_ledger(self.ledger_path, sync.Ledger(1, entries, ()))
        git = TrackedWorkflowGit(self.root, self.displayed)
        gh = TrackedWorkflowGh()

        result, output = self.start(git, gh, [])

        self.assertEqual(result, 0)
        self.assertIn("No unresolved upstream commits", output)
        self.assertEqual(git.current_branch, "main")

    def test_ledger_run_id_reserves_branch_name_after_local_branch_deletion(self):
        prior = sync.SyncRun(
            "sync/upstream-2026-08-05",
            "sync",
            "2026-08-05",
            "main",
            "sync/upstream-2026-08-05",
            (
                sync.RunDecision(
                    self.oldest.oid,
                    self.oldest.subject,
                    "deferred",
                    "Deferred during interactive review.",
                ),
            ),
        )
        sync.write_ledger(self.ledger_path, sync.Ledger(1, {}, (prior,)))
        git = TrackedWorkflowGit(self.root, self.displayed)

        result, _ = self.start(
            git, TrackedWorkflowGh(), ["1-2", "y"]
        )

        self.assertEqual(result, 0)
        self.assertEqual(git.current_branch, "sync/upstream-2026-08-05-2")

    def test_bookkeeping_only_run_still_pushes_and_opens_draft(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        gh = TrackedWorkflowGh()

        result, _ = self.start(
            git, gh, ["", "d", "n", "Not used by this fork.", "y"]
        )

        self.assertEqual(result, 0)
        self.assertFalse(
            any(args[:1] == ("cherry-pick",) for args, _ in git.commands)
        )
        ledger = sync.load_ledger(self.ledger_path)
        self.assertEqual(ledger.commits[self.newest.oid].outcome, "deferred")
        self.assertEqual(
            ledger.commits[self.oldest.oid].outcome, "not-applicable"
        )
        self.assertTrue(gh.created)

    def test_conflict_preserves_ordered_pending_state_before_ledger_write(self):
        git = TrackedWorkflowGit(
            self.root, self.displayed, cherry_pick_failure=self.oldest.oid
        )
        gh = TrackedWorkflowGh()

        result, output = self.start(git, gh, ["1-2", "y"])

        self.assertEqual(result, 1)
        pending = sync.load_pending(git.pending_path)
        self.assertEqual(pending.phase, "cherry-picking")
        self.assertEqual(pending.active_upstream_sha, self.oldest.oid)
        self.assertEqual(pending.considered, tuple(self.displayed))
        self.assertEqual(sync.load_ledger(self.ledger_path).commits, {})
        self.assertIn("git cherry-pick --continue", output)
        self.assertFalse(gh.created)

    def test_resume_after_manual_conflict_continues_remaining_commits(self):
        git = TrackedWorkflowGit(
            self.root, self.displayed, cherry_pick_failure=self.oldest.oid
        )
        gh = TrackedWorkflowGh()
        self.start(git, gh, ["1-2", "y"])
        git.cherry_pick_failure = None
        git.complete_conflict("1" * 40)

        output = io.StringIO()
        result = sync.resume_workflow(
            git, gh, output, self.ledger_path, git.pending_path
        )

        self.assertEqual(result, 0)
        ledger = sync.load_ledger(self.ledger_path)
        self.assertEqual(ledger.commits[self.oldest.oid].local_commit, "1" * 40)
        self.assertEqual(ledger.commits[self.newest.oid].local_commit, "2" * 40)
        self.assertFalse(git.pending_path.exists())

    def test_resume_after_abort_clears_pending_without_deleting_branch(self):
        git = TrackedWorkflowGit(
            self.root, self.displayed, cherry_pick_failure=self.oldest.oid
        )
        gh = TrackedWorkflowGh()
        self.start(git, gh, ["1-2", "y"])
        git.abort_conflict()
        output = io.StringIO()

        result = sync.resume_workflow(
            git, gh, output, self.ledger_path, git.pending_path
        )

        self.assertEqual(result, 1)
        self.assertFalse(git.pending_path.exists())
        self.assertIn("cancelled", output.getvalue().lower())
        self.assertIn("main", output.getvalue())
        self.assertIn("sync/upstream-2026-08-05", git.branches)

    def pending_after_picks(self, phase):
        branch = "sync/upstream-2026-08-05"
        return sync.PendingRun(
            1,
            phase,
            "main",
            branch,
            branch,
            "2026-08-05",
            (self.oldest, self.newest),
            (),
            {self.oldest.oid: "1" * 40, self.newest.oid: "2" * 40},
            considered=tuple(self.displayed),
        )

    def test_resume_from_ledger_committed_only_pushes_then_opens_pr(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        branch = "sync/upstream-2026-08-05"
        git.current_branch = branch
        git.branches.add(branch)
        pending = self.pending_after_picks("ledger-committed")
        sync.write_pending(git.pending_path, pending)
        gh = TrackedWorkflowGh()

        result = sync.resume_workflow(
            git, gh, io.StringIO(), self.ledger_path, git.pending_path
        )

        self.assertEqual(result, 0)
        self.assertTrue(
            any(args[:1] == ("push",) for args, _ in git.commands)
        )
        self.assertFalse(
            any(args[:1] in {("cherry-pick",), ("commit",)}
                for args, _ in git.commands)
        )

    def test_resume_from_pushed_reuses_existing_pr(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        branch = "sync/upstream-2026-08-05"
        git.current_branch = branch
        git.branches.add(branch)
        sync.write_pending(git.pending_path, self.pending_after_picks("pushed"))
        gh = TrackedWorkflowGh(existing_url="https://example/pr/existing")
        output = io.StringIO()

        result = sync.resume_workflow(
            git, gh, output, self.ledger_path, git.pending_path
        )

        self.assertEqual(result, 0)
        self.assertFalse(gh.created)
        self.assertIn("https://example/pr/existing", output.getvalue())
        self.assertFalse(
            any(args[:1] == ("push",) for args, _ in git.commands)
        )

    def test_push_failure_keeps_ledger_committed_phase_for_retry(self):
        failure = subprocess.CompletedProcess(
            ["git", "push"], 3, "", "network unavailable"
        )
        git = TrackedWorkflowGit(
            self.root, self.displayed, push_failure=failure
        )
        gh = TrackedWorkflowGh()

        with self.assertRaises(sync.GitError):
            self.start(git, gh, ["1-2", "y"])

        pending = sync.load_pending(git.pending_path)
        self.assertEqual(pending.phase, "ledger-committed")
        git.push_failure = None
        self.assertEqual(
            sync.resume_workflow(
                git, gh, io.StringIO(), self.ledger_path, git.pending_path
            ),
            0,
        )

    def test_pr_failure_keeps_pushed_phase_for_retry(self):
        failure = subprocess.CompletedProcess(
            ["gh", "pr", "create"], 4, "", "GitHub unavailable"
        )
        git = TrackedWorkflowGit(self.root, self.displayed)
        gh = TrackedWorkflowGh(create_failure=failure)

        with self.assertRaises(sync.GhError):
            self.start(git, gh, ["1-2", "y"])

        pending = sync.load_pending(git.pending_path)
        self.assertEqual(pending.phase, "pushed")
        gh.create_failure = None
        self.assertEqual(
            sync.resume_workflow(
                git, gh, io.StringIO(), self.ledger_path, git.pending_path
            ),
            0,
        )

    def test_resume_without_pending_state_is_rejected(self):
        git = TrackedWorkflowGit(self.root, self.displayed)
        with self.assertRaisesRegex(sync.SyncError, "No pending"):
            sync.resume_workflow(
                git,
                TrackedWorkflowGh(),
                io.StringIO(),
                self.ledger_path,
                git.pending_path,
            )


class FailureDetailTests(unittest.TestCase):
    def test_failure_detail_prefers_stderr_and_collapses_display_whitespace(self):
        self.assertEqual(
            sync.format_failure_detail(
                "stdout detail", "fatal: first line\n  hint:\tretry\n", 9
            ),
            "fatal: first line hint: retry",
        )

    def test_failure_detail_falls_back_to_stdout_then_exit_status(self):
        self.assertEqual(
            sync.format_failure_detail("fetch stopped\n", "", 8),
            "fetch stopped",
        )
        self.assertEqual(sync.format_failure_detail("", "", 8), "exit status 8")


class CliTests(unittest.TestCase):
    @mock.patch.object(sync, "resume_workflow", return_value=0)
    @mock.patch.object(
        sync,
        "pending_state_path",
        return_value=Path("C:/repo/.git/upstream-sync-state.json"),
    )
    @mock.patch.object(
        sync, "repository_root", return_value=Path("C:/repo")
    )
    def test_resume_flag_dispatches_to_resume_workflow(
        self, repository_root, pending_state_path, resume_workflow
    ):
        result = sync.main(["--resume"])
        self.assertEqual(result, 0)
        args = resume_workflow.call_args.args
        self.assertIsInstance(args[0], sync.Git)
        self.assertIsInstance(args[1], sync.Gh)
        self.assertEqual(args[3], Path("C:/repo/.github/upstream-sync.json"))
        self.assertEqual(
            args[4], Path("C:/repo/.git/upstream-sync-state.json")
        )

    @mock.patch.object(
        sync,
        "repository_root",
        side_effect=sync.GhError(
            ["pr", "create"], 7, "partial", "GitHub unavailable"
        ),
    )
    def test_checked_github_errors_are_reported_concisely(self, repository_root):
        error_output = io.StringIO()
        with mock.patch("sys.stderr", error_output):
            result = sync.main([])
        self.assertEqual(result, 1)
        self.assertEqual(
            error_output.getvalue(),
            "error: gh pr create failed: GitHub unavailable\n",
        )

    def test_help_exits_successfully_and_describes_upstream_selection(self):
        output = io.StringIO()

        with mock.patch("sys.stdout", output):
            with self.assertRaises(SystemExit) as caught:
                sync.main(["--help"])

        self.assertEqual(caught.exception.code, 0)
        self.assertIn(
            "Select and cherry-pick commits from zeronsh/comet",
            output.getvalue(),
        )
        help_text = output.getvalue().lower()
        self.assertIn("clean worktree", help_text)
        self.assertIn("attached branch", help_text)
        self.assertIn("fixed upstream remote", help_text)
        self.assertIn("refuses a collision", help_text)
        self.assertIn("2, 1,4, or 2-5", help_text)
        self.assertIn("confirmation", help_text)
        self.assertIn("sync/upstream-yyyy-mm-dd", help_text)
        self.assertIn("committed ledger", help_text)
        self.assertIn("deferred", help_text)
        self.assertIn("gh auth login", help_text)
        self.assertIn("pushes", help_text)
        self.assertIn("draft pull request", help_text)
        self.assertIn("--resume", help_text)
        self.assertIn("bookkeeping-only", help_text)
        self.assertIn("never merges", help_text)

    @mock.patch.object(sync, "repository_root", side_effect=sync.SyncError("not a repo"))
    def test_sync_errors_are_reported_concisely_to_stderr(self, repository_root):
        error_output = io.StringIO()

        with mock.patch("sys.stderr", error_output):
            result = sync.main([])

        self.assertEqual(result, 1)
        self.assertEqual(error_output.getvalue(), "error: not a repo\n")

    @mock.patch.object(
        sync,
        "repository_root",
        side_effect=sync.GitError(
            ["fetch", "--prune", "upstream", "main"],
            7,
            "partial output",
            "fatal: example\n    hint: retry\twith care\n",
        ),
    )
    def test_checked_git_errors_are_reported_concisely_to_stderr(
        self, repository_root
    ):
        error_output = io.StringIO()

        with mock.patch("sys.stderr", error_output):
            result = sync.main([])

        self.assertEqual(result, 1)
        self.assertEqual(
            error_output.getvalue(),
            "error: git fetch --prune upstream main failed: "
            "fatal: example hint: retry with care\n",
        )

    @mock.patch.object(
        sync,
        "repository_root",
        side_effect=sync.GitError(["fetch"], 7, "", ""),
    )
    def test_checked_git_errors_fall_back_to_exit_status(self, repository_root):
        error_output = io.StringIO()

        with mock.patch("sys.stderr", error_output):
            result = sync.main([])

        self.assertEqual(result, 1)
        self.assertEqual(
            error_output.getvalue(),
            "error: git fetch failed: exit status 7\n",
        )


class GitAdapterTests(unittest.TestCase):
    @mock.patch.object(sync.subprocess, "run")
    def test_run_preserves_failed_command_details(self, run):
        run.return_value = subprocess.CompletedProcess(
            ["git", "fetch"], 7, "partial", "fatal: example"
        )

        with self.assertRaises(sync.GitError) as caught:
            sync.Git().run("fetch")

        error = caught.exception
        self.assertEqual(error.args_list, ["fetch"])
        self.assertEqual(error.returncode, 7)
        self.assertEqual(error.stdout, "partial")
        self.assertEqual(error.stderr, "fatal: example")
        run.assert_called_once_with(
            ["git", "fetch"],
            cwd=None,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    @mock.patch.object(sync.subprocess, "run", side_effect=FileNotFoundError)
    def test_run_reports_missing_git(self, run):
        with self.assertRaisesRegex(
            sync.SyncError, "Git is not installed or is not on PATH"
        ):
            sync.Git(Path("C:/repo")).run("status")


class GitHubAdapterTests(unittest.TestCase):
    @mock.patch.object(sync.subprocess, "run")
    def test_run_preserves_failed_command_details(self, run):
        run.return_value = subprocess.CompletedProcess(
            ["gh", "auth", "status"], 7, "partial", "not logged in"
        )

        with self.assertRaises(sync.GhError) as caught:
            sync.Gh().run("auth", "status")

        error = caught.exception
        self.assertEqual(error.args_list, ["auth", "status"])
        self.assertEqual(error.returncode, 7)
        self.assertEqual(error.stdout, "partial")
        self.assertEqual(error.stderr, "not logged in")
        run.assert_called_once_with(
            ["gh", "auth", "status"],
            cwd=None,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    @mock.patch.object(sync.subprocess, "run", side_effect=FileNotFoundError)
    def test_run_reports_missing_github_cli(self, run):
        with self.assertRaisesRegex(
            sync.SyncError, "GitHub CLI is not installed or is not on PATH"
        ):
            sync.Gh(Path("C:/repo")).run("auth", "status")

    def test_github_preflight_checks_authentication(self):
        gh = ScriptedGh({("auth", "status"): ""})
        sync.validate_github(gh)
        self.assertEqual(gh.commands, [(('auth', 'status'), True)])

    def test_github_preflight_explains_failed_authentication(self):
        failure = subprocess.CompletedProcess(
            ["gh", "auth", "status"], 1, "", "not logged in"
        )
        gh = ScriptedGh({("auth", "status"): failure})
        with self.assertRaisesRegex(sync.SyncError, "gh auth login"):
            sync.validate_github(gh)

    def test_origin_preflight_accepts_configured_remote(self):
        git = ScriptedGit(
            {("remote", "get-url", "origin"): "https://github.com/matty/comet\n"}
        )
        sync.validate_origin(git)
        self.assertEqual(
            git.commands, [(('remote', 'get-url', 'origin'), False)]
        )

    def test_origin_preflight_rejects_missing_remote(self):
        missing = subprocess.CompletedProcess(
            ["git", "remote", "get-url", "origin"], 2, "", "missing"
        )
        git = ScriptedGit({("remote", "get-url", "origin"): missing})
        with self.assertRaisesRegex(sync.SyncError, "origin"):
            sync.validate_origin(git)


class PullRequestTests(unittest.TestCase):
    def sample_run(self):
        return sync.SyncRun(
            "sync/upstream-2026-08-05",
            "sync",
            "2026-08-05",
            "main",
            "sync/upstream-2026-08-05",
            (
                sync.RunDecision(
                    "a" * 40,
                    "Applied fix",
                    "implemented",
                    "Cherry-picked by upstream sync helper.",
                    "b" * 40,
                ),
                sync.RunDecision(
                    "c" * 40,
                    "Hosted fix",
                    "not-applicable",
                    "Hosted service was removed.",
                ),
                sync.RunDecision(
                    "d" * 40,
                    "Later fix",
                    "deferred",
                    "Deferred during interactive review.",
                ),
            ),
        )

    def test_pr_body_groups_all_outcomes(self):
        title, body = sync.format_pr(self.sample_run())
        self.assertEqual(title, "Sync upstream commits (2026-08-05)")
        self.assertIn("## Implemented", body)
        self.assertIn("`aaaaaaa` → `bbbbbbb`", body)
        self.assertIn("## Not applicable", body)
        self.assertIn("Hosted service was removed.", body)
        self.assertIn("## Deferred", body)

    def test_find_existing_pr_returns_none_for_empty_result(self):
        command = (
            "pr", "list", "--state", "open", "--base", "main",
            "--head", "sync/upstream-2026-08-05", "--json", "url",
        )
        gh = ScriptedGh({command: "[]\n"})
        self.assertIsNone(
            sync.find_existing_pr(
                gh, "sync/upstream-2026-08-05", "main"
            )
        )

    def test_find_existing_pr_returns_single_url(self):
        command = (
            "pr", "list", "--state", "open", "--base", "main",
            "--head", "sync/upstream-2026-08-05", "--json", "url",
        )
        gh = ScriptedGh({command: '[{"url":"https://example/pr/1"}]\n'})
        self.assertEqual(
            sync.find_existing_pr(
                gh, "sync/upstream-2026-08-05", "main"
            ),
            "https://example/pr/1",
        )

    def test_find_existing_pr_rejects_malformed_or_multiple_results(self):
        command = (
            "pr", "list", "--state", "open", "--base", "main",
            "--head", "sync/upstream-2026-08-05", "--json", "url",
        )
        for response in ("not json", '[{"url":"one"},{"url":"two"}]'):
            with self.subTest(response=response):
                gh = ScriptedGh({command: response})
                with self.assertRaises(sync.SyncError):
                    sync.find_existing_pr(
                        gh, "sync/upstream-2026-08-05", "main"
                    )

    def test_create_pr_is_draft_against_original_target(self):
        run = self.sample_run()
        title, body = sync.format_pr(run)
        command = (
            "pr", "create", "--draft", "--base", "main", "--head",
            run.sync_branch, "--title", title, "--body", body,
        )
        gh = ScriptedGh({command: "https://example/pr/2\n"})
        self.assertEqual(
            sync.create_draft_pr(gh, run, title, body),
            "https://example/pr/2",
        )
        self.assertEqual(gh.commands, [(command, True)])


class PendingStateTests(unittest.TestCase):
    def commit(self, character, subject):
        return sync.Commit(
            character * 40,
            character * 7,
            "2026-08-05",
            "Author",
            subject,
        )

    def pending(self, phase="prepared"):
        selected = (
            self.commit("a", "First selected"),
            self.commit("b", "Second selected"),
        )
        not_selected = self.commit("c", "Not selected")
        return sync.PendingRun(
            1,
            phase,
            "main",
            "sync/upstream-2026-08-05",
            "sync/upstream-2026-08-05",
            "2026-08-05",
            selected,
            (
                sync.RunDecision(
                    not_selected.oid,
                    not_selected.subject,
                    "deferred",
                    "Deferred during interactive review.",
                ),
            ),
            {},
            considered=(selected[1], not_selected, selected[0]),
        )

    def round_trip(self, pending):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "pending.json"
            sync.write_pending(path, pending)
            first = path.read_text(encoding="utf-8")
            loaded = sync.load_pending(path)
            sync.write_pending(path, loaded)
            self.assertEqual(path.read_text(encoding="utf-8"), first)
            return loaded

    def test_pending_state_round_trips_each_phase(self):
        prepared = self.pending()
        picking = replace(prepared, phase="cherry-picking")
        ledger_committed = replace(
            prepared,
            phase="ledger-committed",
            local_commits={"a" * 40: "d" * 40, "b" * 40: "e" * 40},
        )
        pushed = replace(ledger_committed, phase="pushed")

        for pending in (prepared, picking, ledger_committed, pushed):
            with self.subTest(phase=pending.phase):
                self.assertEqual(self.round_trip(pending), pending)

    def invalid_document(self, mutate):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "pending.json"
            sync.write_pending(path, self.pending())
            document = json.loads(path.read_text(encoding="utf-8"))
            mutate(document)
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaises(sync.SyncError):
                sync.load_pending(path)

    def test_rejects_unknown_phase(self):
        self.invalid_document(lambda document: document.update(phase="unknown"))

    def test_rejects_more_local_commits_than_selected(self):
        def mutate(document):
            document["phase"] = "ledger-committed"
            document["local_commits"] = {
                "a" * 40: "d" * 40,
                "b" * 40: "e" * 40,
                "f" * 40: "1" * 40,
            }

        self.invalid_document(mutate)

    def test_rejects_local_commits_outside_chronological_prefix(self):
        def mutate(document):
            document["phase"] = "cherry-picking"
            document["local_commits"] = {"b" * 40: "e" * 40}

        self.invalid_document(mutate)

    def test_rejects_active_commit_that_is_not_next(self):
        def mutate(document):
            document["phase"] = "cherry-picking"
            document["active_upstream_sha"] = "b" * 40
            document["pre_pick_head"] = "f" * 40

        self.invalid_document(mutate)

    def test_pending_state_path_resolves_relative_git_path(self):
        git = ScriptedGit(
            {
                ("rev-parse", "--git-path", "upstream-sync-state.json"):
                    ".git/upstream-sync-state.json\n"
            }
        )
        git.cwd = Path("C:/repo")
        self.assertEqual(
            sync.pending_state_path(git),
            Path("C:/repo/.git/upstream-sync-state.json"),
        )

    def test_record_pick_start_and_success_capture_exact_heads(self):
        pending = replace(self.pending(), phase="cherry-picking")
        started = sync.record_pick_start(
            pending, pending.selected[0], "d" * 40
        )
        self.assertEqual(started.active_upstream_sha, "a" * 40)
        self.assertEqual(started.pre_pick_head, "d" * 40)
        completed = sync.record_pick_success(started, "e" * 40)
        self.assertEqual(completed.local_commits, {"a" * 40: "e" * 40})
        self.assertIsNone(completed.active_upstream_sha)

    def test_resume_records_manually_continued_cherry_pick(self):
        pending = replace(self.pending(), phase="cherry-picking")
        pending = sync.record_pick_start(
            pending, pending.selected[0], "d" * 40
        )
        updated = sync.verify_resumed_pick(pending, current_head="e" * 40)
        self.assertEqual(updated.local_commits["a" * 40], "e" * 40)
        self.assertIsNone(updated.active_upstream_sha)

    def test_resume_rejects_unfinished_cherry_pick(self):
        result = subprocess.CompletedProcess(
            ["git", "rev-parse"], 0, "c" * 40, ""
        )
        git = ScriptedGit(
            {("rev-parse", "-q", "--verify", "CHERRY_PICK_HEAD"): result}
        )
        with self.assertRaisesRegex(sync.SyncError, "cherry-pick --continue"):
            sync.ensure_cherry_pick_resolved(git)

    def test_resume_detects_aborted_cherry_pick(self):
        pending = replace(self.pending(), phase="cherry-picking")
        pending = sync.record_pick_start(
            pending, pending.selected[0], "d" * 40
        )
        with self.assertRaises(sync.SyncCancelled):
            sync.verify_resumed_pick(pending, current_head="d" * 40)

    def test_clear_pending_is_safe_when_file_is_absent(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "pending.json"
            sync.clear_pending(path)
            path.write_text("pending", encoding="utf-8")
            sync.clear_pending(path)
            self.assertFalse(path.exists())


class RepositoryTests(unittest.TestCase):
    def test_repository_root_returns_resolved_git_toplevel(self):
        git = ScriptedGit({("rev-parse", "--show-toplevel"): "C:/dev/comet\n"})

        self.assertEqual(sync.repository_root(git), Path("C:/dev/comet"))

    def test_validate_repository_rejects_dirty_worktree(self):
        git = ScriptedGit({("status", "--porcelain"): "?? local.txt\n"})
        with self.assertRaisesRegex(sync.SyncError, "clean worktree"):
            sync.validate_repository(git)

    def test_validate_repository_rejects_detached_head(self):
        git = ScriptedGit({
            ("status", "--porcelain"): "",
            ("symbolic-ref", "--quiet", "--short", "HEAD"): sync.GitError(
                ["symbolic-ref", "--quiet", "--short", "HEAD"], 1, "", ""
            ),
        })
        with self.assertRaisesRegex(sync.SyncError, "detached HEAD"):
            sync.validate_repository(git)

    def test_validate_repository_returns_exact_attached_branch_name(self):
        git = ScriptedGit({
            ("status", "--porcelain"): "",
            ("symbolic-ref", "--quiet", "--short", "HEAD"): "feature/exact-name\n",
        })

        self.assertEqual(sync.validate_repository(git), "feature/exact-name")


class UpstreamTests(unittest.TestCase):
    def test_missing_remote_adds_expected_upstream(self):
        get_url = ("remote", "get-url", "upstream")
        add = (
            "remote",
            "add",
            "upstream",
            "https://github.com/zeronsh/comet.git",
        )
        git = ScriptedGit({
            get_url: subprocess.CompletedProcess(get_url, 2, "", "missing"),
            add: "",
        })

        sync.ensure_upstream(git)

        self.assertEqual(git.commands, [(get_url, False), (add, True)])

    def test_matching_remote_causes_no_mutation(self):
        get_url = ("remote", "get-url", "upstream")
        for url in (
            "https://github.com/zeronsh/comet.git\n",
            "git@github.com:zeronsh/comet.git\n",
        ):
            with self.subTest(url=url):
                git = ScriptedGit({get_url: url})

                sync.ensure_upstream(git)

                self.assertEqual(git.commands, [(get_url, False)])

    def test_conflicting_remote_reports_url_and_rename_guidance(self):
        get_url = ("remote", "get-url", "upstream")
        found = "https://github.com/example/comet.git"
        git = ScriptedGit({get_url: found + "\n"})

        with self.assertRaises(sync.SyncError) as caught:
            sync.ensure_upstream(git)

        self.assertIn("upstream", str(caught.exception))
        self.assertIn(found, str(caught.exception))
        self.assertIn(
            "git remote rename upstream <new-name>",
            str(caught.exception),
        )


class DiscoveryTests(unittest.TestCase):
    LOG_COMMAND = (
        "log",
        "--right-only",
        "--cherry-pick",
        "--topo-order",
        "--format=%H%x00%h%x00%cs%x00%an%x00%s",
        "feature/current...upstream/main",
    )

    def test_discovery_uses_expected_log_and_parses_newest_first(self):
        output = (
            "b" * 40 + "\x00bbbbbbb\x002026-08-04\x00Bob\x00Newest\n"
            + "a" * 40 + "\x00aaaaaaa\x002026-08-03\x00Alice\x00Older\n"
        )
        git = ScriptedGit({self.LOG_COMMAND: output})

        commits = sync.discover_commits(git, "feature/current")

        self.assertEqual(git.commands, [(self.LOG_COMMAND, True)])
        self.assertEqual(
            commits,
            [
                sync.Commit("b" * 40, "bbbbbbb", "2026-08-04", "Bob", "Newest"),
                sync.Commit("a" * 40, "aaaaaaa", "2026-08-03", "Alice", "Older"),
            ],
        )

    def test_empty_discovery_returns_empty_list(self):
        git = ScriptedGit({self.LOG_COMMAND: ""})

        self.assertEqual(sync.discover_commits(git, "feature/current"), [])

    def test_discovery_preserves_non_ascii_author_and_subject(self):
        output = (
            "a" * 40
            + "\x00aaaaaaa\x002026-08-04\x00Zoë 王\x00Répare le café ☕\n"
        )
        git = ScriptedGit({self.LOG_COMMAND: output})

        self.assertEqual(
            sync.discover_commits(git, "feature/current"),
            [
                sync.Commit(
                    "a" * 40,
                    "aaaaaaa",
                    "2026-08-04",
                    "Zoë 王",
                    "Répare le café ☕",
                )
            ],
        )

    def test_malformed_discovery_data_is_rejected(self):
        git = ScriptedGit({self.LOG_COMMAND: "one\x00two\n"})

        with self.assertRaisesRegex(sync.SyncError, "malformed commit data"):
            sync.discover_commits(git, "feature/current")

    def test_existing_branches_returns_local_branch_names(self):
        command = (
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/",
        )
        git = ScriptedGit({command: "main\nfeature/current\n\n"})

        self.assertEqual(sync.existing_branches(git), {"main", "feature/current"})


class SelectionTests(unittest.TestCase):
    def test_blank_selection_is_allowed_only_when_requested(self):
        self.assertEqual(sync.parse_selection("", DISPLAYED, allow_empty=True), [])
        with self.assertRaisesRegex(ValueError, "Select at least one"):
            sync.parse_selection("", DISPLAYED)

    def test_parse_single_selection(self):
        self.assertEqual(
            [commit.oid for commit in sync.parse_selection("2", DISPLAYED)],
            ["c3"],
        )

    def test_parse_list_and_range_returns_chronological_order(self):
        selected = sync.parse_selection("1,3-4", DISPLAYED)
        self.assertEqual([commit.oid for commit in selected], ["c1", "c2", "c4"])

    def test_parse_selection_removes_duplicates(self):
        selected = sync.parse_selection("1,1,1-2", DISPLAYED)
        self.assertEqual([commit.oid for commit in selected], ["c3", "c4"])

    def test_parse_selection_rejects_invalid_input(self):
        for text in ("", "0", "5", "x", "1--2", "3-1"):
            with self.subTest(text=text):
                with self.assertRaisesRegex(ValueError, ".+"):
                    sync.parse_selection(text, DISPLAYED)


class NamingAndUrlTests(unittest.TestCase):
    def test_next_branch_name_uses_unsuffixed_name_first(self):
        self.assertEqual(
            sync.next_branch_name(set(), date(2026, 8, 4)),
            "sync/upstream-2026-08-04",
        )

    def test_next_branch_name_uses_first_available_suffix(self):
        existing = {"sync/upstream-2026-08-04", "sync/upstream-2026-08-04-2"}
        self.assertEqual(
            sync.next_branch_name(existing, date(2026, 8, 4)),
            "sync/upstream-2026-08-04-3",
        )

    def test_next_branch_name_fills_suffix_gap(self):
        existing = {"sync/upstream-2026-08-04", "sync/upstream-2026-08-04-3"}
        self.assertEqual(
            sync.next_branch_name(existing, date(2026, 8, 4)),
            "sync/upstream-2026-08-04-2",
        )

    def test_normalize_github_url_canonicalizes_supported_forms(self):
        expected = "github.com/zeronsh/comet"
        for url in (
            "https://github.com/ZeronSH/Comet.git/",
            "git@github.com:zeronsh/comet.git",
            "ssh://git@github.com/zeronsh/comet",
        ):
            with self.subTest(url=url):
                self.assertEqual(sync.normalize_github_url(url), expected)

    def test_expected_upstream_accepts_https_ssh_and_trailing_git(self):
        self.assertTrue(sync.is_expected_upstream("https://github.com/zeronsh/comet.git"))
        self.assertTrue(sync.is_expected_upstream("git@github.com:zeronsh/comet.git"))
        self.assertTrue(sync.is_expected_upstream("ssh://git@github.com/zeronsh/comet"))

    def test_expected_upstream_rejects_another_repository(self):
        self.assertFalse(sync.is_expected_upstream("https://github.com/example/comet.git"))


if __name__ == "__main__":
    unittest.main()
