import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from datetime import date
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


class WorkflowGit:
    def __init__(self, commits=DISPLAYED, existing=None, cherry_pick_failures=None):
        self.commits = commits
        self.existing = existing or set()
        self.cherry_pick_failures = cherry_pick_failures or set()
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
            returncode = 1 if args[1] in self.cherry_pick_failures else 0
            result = subprocess.CompletedProcess(args, returncode, "", "conflict")
            if check and returncode:
                raise sync.GitError(list(args), returncode, "", "conflict")
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

    def test_cherry_pick_failure_prints_recovery_and_stops_later_commits(self):
        git = WorkflowGit(cherry_pick_failures={"c3"})

        result, output, _ = self.run_workflow(git, ["1-3", "y"])

        self.assertEqual(result, 1)
        self.assertIn("git cherry-pick --continue", output)
        self.assertIn("git cherry-pick --abort", output)
        cherry_picks = [
            args for args, _ in git.commands if args[:1] == ("cherry-pick",)
        ]
        self.assertEqual(cherry_picks, [("cherry-pick", "c2"), ("cherry-pick", "c3")])


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

    @mock.patch.object(sync, "repository_root", side_effect=sync.SyncError("not a repo"))
    def test_sync_errors_are_reported_concisely_to_stderr(self, repository_root):
        error_output = io.StringIO()

        with mock.patch("sys.stderr", error_output):
            result = sync.main([])

        self.assertEqual(result, 1)
        self.assertEqual(error_output.getvalue(), "error: not a repo\n")


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

    def test_conflicting_remote_reports_name_and_found_url(self):
        get_url = ("remote", "get-url", "upstream")
        found = "https://github.com/example/comet.git"
        git = ScriptedGit({get_url: found + "\n"})

        with self.assertRaises(sync.SyncError) as caught:
            sync.ensure_upstream(git)

        self.assertIn("upstream", str(caught.exception))
        self.assertIn(found, str(caught.exception))


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
    def test_next_branch_name_uses_first_available_suffix(self):
        existing = {"sync/upstream-2026-08-04", "sync/upstream-2026-08-04-2"}
        self.assertEqual(
            sync.next_branch_name(existing, date(2026, 8, 4)),
            "sync/upstream-2026-08-04-3",
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
