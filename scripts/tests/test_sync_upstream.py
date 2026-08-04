import importlib.util
import subprocess
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
