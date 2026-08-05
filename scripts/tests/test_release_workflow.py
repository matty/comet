import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
RELEASE = WORKFLOWS / "release.yml"


class ReleaseWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = yaml.load(
            RELEASE.read_text(encoding="utf-8"), Loader=yaml.BaseLoader
        )

    def test_only_release_workflow_remains(self):
        self.assertEqual(
            [path.name for path in sorted(WORKFLOWS.glob("*.yml"))],
            ["release.yml"],
        )

    def test_schedule_and_manual_dispatch_trigger_releases(self):
        events = self.workflow["on"]
        self.assertEqual(events["schedule"], [{"cron": "0 */2 * * *"}])
        self.assertIn("workflow_dispatch", events)
        self.assertNotIn("push", events)

    def test_scheduled_runs_queue_instead_of_cancelling(self):
        self.assertEqual(
            self.workflow["concurrency"],
            {"group": "nightly-release", "cancel-in-progress": "false"},
        )

    def test_no_quiet_period_remains(self):
        steps = self.workflow["jobs"]["prepare"]["steps"]
        sleeps = [step for step in steps if "sleep" in step.get("run", "")]
        self.assertEqual(sleeps, [], "the debounce quiet period must be removed")

    def test_version_is_immutable_semver_prerelease(self):
        steps = self.workflow["jobs"]["prepare"]["steps"]
        derive = next(step for step in steps if step.get("id") == "release")["run"]
        self.assertIn("github.run_number", derive)
        self.assertIn("date -u +%Y%m%d", derive)
        self.assertIn("g${source_sha:0:7}", derive)
        self.assertIn("tag=v$version", derive)

    def test_all_platforms_are_packaged(self):
        jobs = self.workflow["jobs"]
        linux_runners = {
            row["runner"] for row in jobs["linux"]["strategy"]["matrix"]["include"]
        }
        self.assertEqual(linux_runners, {"ubuntu-24.04", "ubuntu-24.04-arm"})
        self.assertEqual(jobs["macos"]["runs-on"], "macos-latest")
        self.assertEqual(jobs["macos"]["continue-on-error"], "true")
        self.assertEqual(jobs["windows"]["runs-on"], "windows-latest")
        windows_package = next(
            step for step in jobs["windows"]["steps"] if step.get("name") == "build and package"
        )["run"]
        self.assertIn("Compress-Archive", windows_package)
        self.assertIn("windows-x86_64.zip", windows_package)

    def test_windows_version_step_only_changes_workspace_package_version(self):
        pwsh = shutil.which("pwsh")
        if pwsh is None:
            self.skipTest("PowerShell is required to exercise the Windows workflow step")

        windows_steps = self.workflow["jobs"]["windows"]["steps"]
        version_step = next(
            step
            for step in windows_steps
            if step.get("name") == "set nightly workspace version"
        )
        cargo_toml = """\
[workspace]
members = []

[workspace.package]
version = "0.1.15"

[workspace.dependencies.windows]
version = "0.61"
"""

        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "Cargo.toml"
            manifest.write_text(cargo_toml, encoding="utf-8")
            env = os.environ.copy()
            env["VERSION"] = "0.1.15-nightly.20260804.1.g8a4edce"
            result = subprocess.run(
                [pwsh, "-NoProfile", "-NonInteractive", "-Command", version_step["run"]],
                cwd=directory,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                manifest.read_text(encoding="utf-8"),
                cargo_toml.replace(
                    'version = "0.1.15"',
                    'version = "0.1.15-nightly.20260804.1.g8a4edce"',
                    1,
                ),
            )

    def test_publication_is_github_only_prerelease(self):
        publish = self.workflow["jobs"]["publish"]
        release = next(
            step for step in publish["steps"] if step.get("uses") == "softprops/action-gh-release@v3"
        )
        self.assertEqual(release["with"]["prerelease"], "true")
        self.assertEqual(release["with"]["make_latest"], "false")
        self.assertEqual(
            release["with"]["target_commitish"],
            "${{ needs.prepare.outputs.source_sha }}",
        )

    def test_publication_gate_is_cancellable_and_tolerates_failed_macos(self):
        publish = self.workflow["jobs"]["publish"]
        self.assertEqual(publish["needs"], ["prepare", "linux", "macos", "windows"])
        self.assertEqual(
            publish.get("if"),
            "${{ github.repository == 'matty/comet' && !cancelled() && "
            "needs.prepare.result == 'success' && "
            "needs.linux.result == 'success' && needs.windows.result == 'success' && "
            "(needs.macos.result == 'success' || needs.macos.result == 'failure') }}",
        )

    def test_only_canonical_repository_can_publish(self):
        condition = self.workflow["jobs"]["publish"].get("if", "")
        self.assertIn("github.repository == 'matty/comet'", condition)

    def test_publication_rechecks_main_immediately_before_release(self):
        steps = self.workflow["jobs"]["publish"]["steps"]
        release_index = next(
            index
            for index, step in enumerate(steps)
            if step.get("uses") == "softprops/action-gh-release@v3"
        )
        guard = steps[release_index - 1]
        self.assertEqual(guard.get("name"), "verify source and tag are immutable")
        self.assertEqual(
            guard.get("env"),
            {
                "SOURCE_SHA": "${{ needs.prepare.outputs.source_sha }}",
                "TAG": "${{ needs.prepare.outputs.tag }}",
            },
        )
        self.assertIn("git fetch origin main", guard.get("run", ""))
        self.assertIn(
            'current_sha="$(git rev-parse origin/main)"',
            guard.get("run", ""),
        )
        self.assertIn(
            'if [ "$current_sha" != "$SOURCE_SHA" ]; then',
            guard.get("run", ""),
        )


if __name__ == "__main__":
    unittest.main()
