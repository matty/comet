import importlib.util
import unittest
from datetime import date
from pathlib import Path


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
