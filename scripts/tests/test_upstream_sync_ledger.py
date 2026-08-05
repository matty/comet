import importlib.util
import io
import json
from datetime import date
from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).parents[2]
SCRIPT_PATH = ROOT / "scripts" / "sync-upstream.py"
SPEC = importlib.util.spec_from_file_location("sync_upstream_ledger", SCRIPT_PATH)
sync = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = sync
SPEC.loader.exec_module(sync)


UPSTREAM = "a" * 40
LOCAL = "b" * 40


def valid_document():
    return {
        "schema_version": 1,
        "commits": {
            UPSTREAM: {
                "upstream_sha": UPSTREAM,
                "subject": "Implemented change",
                "outcome": "implemented",
                "decision_date": "2026-08-05",
                "note": "Implemented locally.",
                "local_commit": LOCAL,
            }
        },
        "runs": [
            {
                "run_id": "bootstrap-2026-08-05",
                "kind": "bootstrap",
                "date": "2026-08-05",
                "target_branch": "main",
                "sync_branch": None,
                "decisions": [
                    {
                        "upstream_sha": UPSTREAM,
                        "subject": "Implemented change",
                        "outcome": "implemented",
                        "note": "Implemented locally.",
                        "local_commit": LOCAL,
                    }
                ],
            }
        ],
    }


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
        self.assertTrue(
            all(ledger.commits[sha].outcome == "implemented" for sha in expected)
        )

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


class LedgerValidationTests(unittest.TestCase):
    def load_document(self, document):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "ledger.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            return sync.load_ledger(path)

    def assert_invalid(self, document, pattern):
        with self.assertRaisesRegex(sync.SyncError, pattern):
            self.load_document(document)

    def test_valid_document_round_trips_stably(self):
        ledger = self.load_document(valid_document())
        serialized = sync.serialize_ledger(ledger)
        self.assertEqual(serialized, sync.serialize_ledger(ledger))
        self.assertTrue(serialized.endswith("\n"))
        self.assertEqual(json.loads(serialized), valid_document())

    def test_rejects_unknown_schema_version(self):
        document = valid_document()
        document["schema_version"] = 2
        self.assert_invalid(document, "schema version")

    def test_rejects_short_upstream_sha(self):
        document = valid_document()
        entry = document["commits"].pop(UPSTREAM)
        entry["upstream_sha"] = "abc1234"
        document["commits"]["abc1234"] = entry
        self.assert_invalid(document, "full lowercase SHA")

    def test_rejects_key_and_embedded_sha_mismatch(self):
        document = valid_document()
        document["commits"][UPSTREAM]["upstream_sha"] = "c" * 40
        self.assert_invalid(document, "does not match")

    def test_rejects_unknown_outcome(self):
        document = valid_document()
        document["commits"][UPSTREAM]["outcome"] = "ignored"
        self.assert_invalid(document, "outcome")

    def test_rejects_missing_note(self):
        document = valid_document()
        document["commits"][UPSTREAM]["note"] = ""
        self.assert_invalid(document, "note")

    def test_rejects_local_commit_for_non_implemented_outcome(self):
        document = valid_document()
        document["commits"][UPSTREAM]["outcome"] = "deferred"
        self.assert_invalid(document, "local_commit")

    def test_implemented_entry_can_omit_local_reference(self):
        document = valid_document()
        document["commits"][UPSTREAM]["local_commit"] = None
        document["runs"][0]["decisions"][0]["local_commit"] = None
        ledger = self.load_document(document)
        self.assertIsNone(ledger.commits[UPSTREAM].local_commit)

    def test_rejects_duplicate_run_ids(self):
        document = valid_document()
        document["runs"].append(document["runs"][0].copy())
        self.assert_invalid(document, "Duplicate run ID")

    def test_rejects_bootstrap_run_with_sync_branch(self):
        document = valid_document()
        document["runs"][0]["sync_branch"] = "sync/upstream-2026-08-05"
        self.assert_invalid(document, "bootstrap")


class LedgerPolicyTests(unittest.TestCase):
    def commit(self, character, subject):
        return sync.Commit(
            character * 40,
            character * 7,
            "2026-08-05",
            "Author",
            subject,
        )

    def entry(self, commit, outcome, local_commit=None):
        return sync.LedgerEntry(
            commit.oid,
            commit.subject,
            outcome,
            "2026-08-05",
            f"{outcome} note",
            local_commit,
        )

    def test_filter_resolved_keeps_only_deferred_and_unknown(self):
        implemented = self.commit("a", "Implemented")
        not_applicable = self.commit("b", "Not applicable")
        deferred = self.commit("c", "Deferred")
        unknown = self.commit("d", "Unknown")
        ledger = sync.Ledger(
            1,
            {
                implemented.oid: self.entry(
                    implemented, "implemented", "e" * 40
                ),
                not_applicable.oid: self.entry(
                    not_applicable, "not-applicable"
                ),
                deferred.oid: self.entry(deferred, "deferred"),
            },
            (),
        )

        self.assertEqual(
            [
                commit.oid
                for commit in sync.filter_resolved(
                    [implemented, not_applicable, deferred, unknown], ledger
                )
            ],
            [deferred.oid, unknown.oid],
        )

    def test_unselected_defaults_to_deferred(self):
        commit = self.commit("a", "Later")
        decisions = sync.classify_unselected(
            [commit], set(), lambda prompt: "", io.StringIO()
        )
        self.assertEqual(decisions[0].outcome, "deferred")
        self.assertEqual(
            decisions[0].note, "Deferred during interactive review."
        )

    def test_not_applicable_requires_a_reason(self):
        commit = self.commit("a", "Hosted change")
        answers = iter(["n", "", "Hosted workspace service was removed."])
        output = io.StringIO()
        decisions = sync.classify_unselected(
            [commit], set(), lambda prompt: next(answers), output
        )
        self.assertEqual(decisions[0].outcome, "not-applicable")
        self.assertEqual(
            decisions[0].note, "Hosted workspace service was removed."
        )
        self.assertIn("Reason is required", output.getvalue())

    def test_selected_commits_are_not_classified(self):
        commit = self.commit("a", "Selected")

        def unexpected_prompt(prompt):
            self.fail(f"unexpected prompt: {prompt}")

        self.assertEqual(
            sync.classify_unselected(
                [commit], {commit.oid}, unexpected_prompt, io.StringIO()
            ),
            [],
        )

    def test_invalid_classification_is_retried(self):
        commit = self.commit("a", "Later")
        answers = iter(["x", "d"])
        output = io.StringIO()
        decisions = sync.classify_unselected(
            [commit], set(), lambda prompt: next(answers), output
        )
        self.assertEqual(decisions[0].outcome, "deferred")
        self.assertIn("Choose d or n", output.getvalue())

    def test_build_sync_run_preserves_exact_upstream_order(self):
        newest = self.commit("c", "Newest")
        middle = self.commit("b", "Middle")
        oldest = self.commit("a", "Oldest")
        classifications = [
            sync.RunDecision(
                newest.oid,
                newest.subject,
                "deferred",
                "Deferred during interactive review.",
            ),
            sync.RunDecision(
                oldest.oid,
                oldest.subject,
                "not-applicable",
                "Not used by this fork.",
            ),
        ]
        branch = "sync/upstream-2026-08-05-2"
        run = sync.build_sync_run(
            date(2026, 8, 5),
            "main",
            branch,
            [newest, middle, oldest],
            {middle.oid: "d" * 40},
            classifications,
        )
        self.assertEqual(run.run_id, branch)
        self.assertEqual(
            [decision.upstream_sha for decision in run.decisions],
            [oldest.oid, middle.oid, newest.oid],
        )
        self.assertEqual(run.decisions[1].outcome, "implemented")

    def test_apply_run_updates_current_state_without_mutating_history(self):
        commit = self.commit("a", "Reconsidered")
        original = sync.Ledger(
            1,
            {commit.oid: self.entry(commit, "deferred")},
            (),
        )
        run = sync.SyncRun(
            "sync/upstream-2026-08-05",
            "sync",
            "2026-08-05",
            "main",
            "sync/upstream-2026-08-05",
            (
                sync.RunDecision(
                    commit.oid,
                    commit.subject,
                    "not-applicable",
                    "Not used by this fork.",
                ),
            ),
        )
        updated = sync.apply_run(original, run)
        self.assertEqual(original.commits[commit.oid].outcome, "deferred")
        self.assertEqual(updated.commits[commit.oid].outcome, "not-applicable")
        self.assertEqual(updated.runs, (run,))

    def test_apply_run_rejects_duplicate_run_id(self):
        run = sync.SyncRun(
            "sync/upstream-2026-08-05",
            "sync",
            "2026-08-05",
            "main",
            "sync/upstream-2026-08-05",
            (
                sync.RunDecision(
                    "a" * 40,
                    "Change",
                    "deferred",
                    "Deferred during interactive review.",
                ),
            ),
        )
        with self.assertRaisesRegex(sync.SyncError, "Duplicate run ID"):
            sync.apply_run(sync.Ledger(1, {}, (run,)), run)


if __name__ == "__main__":
    unittest.main()
