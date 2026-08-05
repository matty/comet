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
            base_oid = git("rev-parse", "main", cwd=fork).stdout.strip()

            (source / "first.txt").write_text("first\n", encoding="utf-8")
            git("add", "first.txt", cwd=source)
            git("commit", "-m", "First upstream change", cwd=source)
            (source / "second.txt").write_text("second\n", encoding="utf-8")
            git("add", "second.txt", cwd=source)
            git("commit", "-m", "Second upstream change", cwd=source)
            git("push", "origin", "main", cwd=source)

            git("remote", "add", "upstream", str(upstream), cwd=fork)
            offline_entrypoint = """
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("sync_upstream_offline", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
expected_upstream = module.is_expected_upstream
module.is_expected_upstream = lambda url: (
    expected_upstream(url) or url == sys.argv[2]
)
raise SystemExit(module.main([]))
"""
            result = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    offline_entrypoint,
                    str(SCRIPT_PATH),
                    str(upstream),
                ],
                cwd=fork,
                input="1,2\nyes\n",
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            eligible_output = result.stdout.split(
                "Selected commits (oldest first):", 1
            )[0]
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
                ["Second upstream change", "First upstream change"],
            )
            self.assertEqual(git("status", "--porcelain", cwd=fork).stdout, "")
            self.assertEqual(git("rev-parse", "main", cwd=fork).stdout.strip(), base_oid)


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


class WorkflowGit:
    def __init__(self, commits=DISPLAYED, existing=None, cherry_pick_failures=None):
        self.commits = commits
        self.existing = existing or set()
        self.cherry_pick_failures = cherry_pick_failures or {}
        self.commands = []

    def run(self, *args, check=True):
        self.commands.append((args, check))
        if args == ("status", "--porcelain"):
            stdout = ""
        elif args == ("symbolic-ref", "--quiet", "--short", "HEAD"):
            stdout = "feature/current\n"
        elif args == ("remote", "get-url", "upstream"):
            stdout = "https://github.com/zeronsh/comet.git\n"
        elif args == ("fetch", "--prune", "upstream", "main"):
            stdout = ""
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
            stdout = "".join(f"{branch}\n" for branch in sorted(self.existing))
        elif args[:2] == ("switch", "-c"):
            stdout = ""
        elif args[:1] == ("cherry-pick",):
            result = self.cherry_pick_failures.get(
                args[1], subprocess.CompletedProcess(args, 0, "", "")
            )
            if check and result.returncode:
                raise sync.GitError(
                    list(args), result.returncode, result.stdout, result.stderr
                )
            return result
        else:
            raise AssertionError(f"Unexpected Git command: {args!r}")
        return subprocess.CompletedProcess(args, 0, stdout, "")


class WorkflowTests(unittest.TestCase):
    def run_workflow(self, git, answers):
        prompts = []
        answer_iter = iter(answers)

        def input_fn(prompt):
            prompts.append(prompt)
            return next(answer_iter)

        output = io.StringIO()
        result = sync.run_workflow(git, input_fn, output, date(2026, 8, 4))
        return result, output.getvalue(), prompts

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

    def test_no_commits_fetches_and_exits_without_creating_branch(self):
        git = WorkflowGit(commits=[])

        result, output, prompts = self.run_workflow(git, [])

        self.assertEqual(result, 0)
        self.assertIn("already aligned", output)
        self.assertEqual(prompts, [])
        commands = [args for args, _ in git.commands]
        self.assertIn(("fetch", "--prune", "upstream", "main"), commands)
        self.assertFalse(any(args[:2] == ("switch", "-c") for args in commands))

    def test_invalid_selection_prints_error_and_prompts_again(self):
        git = WorkflowGit()

        result, output, prompts = self.run_workflow(git, ["nope", "2", "n"])

        self.assertEqual(result, 0)
        self.assertIn("Invalid selection: nope", output)
        selection_prompts = [prompt for prompt in prompts if "Select" in prompt]
        self.assertEqual(len(selection_prompts), 2)

    def test_declined_confirmation_does_not_mutate_repository(self):
        git = WorkflowGit()

        result, _, _ = self.run_workflow(git, ["2", "n"])

        self.assertEqual(result, 0)
        commands = [args for args, _ in git.commands]
        self.assertFalse(any(args[:2] == ("switch", "-c") for args in commands))
        self.assertFalse(any(args[:1] == ("cherry-pick",) for args in commands))

    def test_confirmed_selection_creates_unique_branch_and_cherry_picks_oldest_first(self):
        git = WorkflowGit(
            existing={"sync/upstream-2026-08-04"},
        )

        result, output, _ = self.run_workflow(git, ["1,3-4", "yes"])

        self.assertEqual(result, 0)
        state_changes = [
            args
            for args, _ in git.commands
            if args[:2] == ("switch", "-c") or args[:1] == ("cherry-pick",)
        ]
        self.assertEqual(
            state_changes,
            [
                ("switch", "-c", "sync/upstream-2026-08-04-2"),
                ("cherry-pick", "c1"),
                ("cherry-pick", "c2"),
                ("cherry-pick", "c4"),
            ],
        )
        self.assertIn("Selected commits (oldest first):", output)
        self.assertIn("git diff feature/current...HEAD", output)
        self.assertIn("git log --oneline feature/current..HEAD", output)
        self.assertIn("git switch feature/current", output)
        self.assertIn("git merge --ff-only sync/upstream-2026-08-04-2", output)

    def test_cherry_pick_failure_surfaces_git_detail_and_stops_later_commits(self):
        failure = subprocess.CompletedProcess(
            ("cherry-pick", "c3"),
            1,
            "",
            "error: could not apply c3... Third\n"
            "hint: after resolving the conflicts, mark the corrected paths\n",
        )
        git = WorkflowGit(cherry_pick_failures={"c3": failure})

        result, output, _ = self.run_workflow(git, ["1-3", "y"])

        self.assertEqual(result, 1)
        self.assertIn(
            "Cherry-pick stopped at c3 (Third): "
            "error: could not apply c3... Third hint: after resolving the "
            "conflicts, mark the corrected paths",
            output,
        )
        self.assertIn("Inspect the repository with git status.", output)
        self.assertIn("If conflicts or cherry-pick state are present", output)
        self.assertIn("git cherry-pick --continue", output)
        self.assertIn("git cherry-pick --abort", output)
        cherry_picks = [
            args for args, _ in git.commands if args[:1] == ("cherry-pick",)
        ]
        self.assertEqual(cherry_picks, [("cherry-pick", "c2"), ("cherry-pick", "c3")])


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
        self.assertIn("resolve conflicts manually", help_text)
        self.assertIn("prints integration commands", help_text)
        self.assertIn("never merges or pushes", help_text)

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
                    "c" * 40,
                    "Not selected",
                    "deferred",
                    "Deferred during interactive review.",
                ),
            ),
            {},
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
