# Debounced Nightly Releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish immutable Linux, macOS, and Windows GitHub prereleases after `main` has been quiet for 30 minutes, with no Cloudflare deployment or R2 publication.

**Architecture:** A single push/manual GitHub Actions workflow owns debounce, version derivation, platform builds, artifact collection, and GitHub Release creation. Workflow-level concurrency cancels the whole pending/building pipeline on a newer commit. PyYAML semantic tests inspect the parsed workflow structure, while actionlint and local build checks provide GitHub Actions and packaging verification.

**Tech Stack:** GitHub Actions YAML, Bash, PowerShell, Python `unittest`, Cargo/Rust, `softprops/action-gh-release@v3`.

## Global Constraints

- Automated releases trigger only from pushes to `main`.
- Push-triggered releases wait for 30 minutes of repository silence.
- Manual dispatches bypass the quiet period but still build the current `main` HEAD.
- The version format is `<base>-nightly.<UTC YYYYMMDD>.<github.run_number>.g<7-character commit SHA>`.
- Git tags prefix that version with `v`.
- Linux x86_64, Linux aarch64, and Windows x86_64 are required release targets.
- macOS Apple Silicon remains allowed to fail without blocking publication.
- Releases are immutable GitHub prereleases and are never marked latest.
- GitHub Actions must contain no Cloudflare, R2, or Wrangler publication logic.
- `edge/`, updater code, and non-workflow distribution code remain unchanged.

## File Structure

- Create `scripts/tests/test_release_workflow.py`: static regression checks for the release workflow contract.
- Modify `.github/workflows/release.yml`: debounce, version, cross-platform builds, and GitHub Release publication.
- Delete `.github/workflows/deploy.yml`: retire Cloudflare Worker deployment automation.
- Modify `dist/README.md`: document Windows packaging and debounced GitHub nightly releases.

---

### Task 1: Implement the Debounced Cross-Platform Release Pipeline

**Files:**
- Create: `scripts/tests/test_release_workflow.py`
- Modify: `.github/workflows/release.yml`
- Delete: `.github/workflows/deploy.yml`
- Test: `scripts/tests/test_release_workflow.py`

**Interfaces:**
- Consumes: repository-relative workflow paths, `Cargo.toml` workspace version, `github.run_number`, current `origin/main` SHA, and the Linux/macOS packaging scripts.
- Produces: `ReleaseWorkflowTests`; prepare-job outputs `version`, `tag`, and `source_sha`; Linux/macOS archives; `comet-<version>-windows-x86_64.zip`; immutable GitHub prerelease.

- [ ] **Step 1: Write the failing regression test**

Create `scripts/tests/test_release_workflow.py`. Workflow YAML is the
human-approved configuration exception to strict TDD; test its parsed semantics
rather than grepping source text:

```python
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

    def test_pushes_to_main_and_manual_dispatch_trigger_releases(self):
        events = self.workflow["on"]
        self.assertEqual(events["push"]["branches"], ["main"])
        self.assertIn("workflow_dispatch", events)
        self.assertNotIn("schedule", events)

    def test_new_push_cancels_and_restarts_the_quiet_period(self):
        self.assertEqual(
            self.workflow["concurrency"],
            {"group": "nightly-release", "cancel-in-progress": "true"},
        )
        steps = self.workflow["jobs"]["prepare"]["steps"]
        quiet = next(step for step in steps if step.get("name") == "wait for 30 minutes of silence")
        self.assertEqual(quiet["if"], "github.event_name == 'push'")
        self.assertEqual(quiet["run"], "sleep 1800")

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


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails against the old workflows**

Run:

```powershell
python -m unittest scripts.tests.test_release_workflow -v
```

Expected: FAIL because `deploy.yml` still exists and `release.yml` still uses tag triggers, lacks Windows, and contains R2 publication.

- [ ] **Step 3: Replace the trigger, permissions, and debounce setup**

Start `.github/workflows/release.yml` with:

```yaml
name: nightly release

on:
  push:
    branches: [main]
  workflow_dispatch:

permissions:
  contents: write

concurrency:
  group: nightly-release
  cancel-in-progress: true

jobs:
  prepare:
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.release.outputs.version }}
      tag: ${{ steps.release.outputs.tag }}
      source_sha: ${{ steps.release.outputs.source_sha }}
    steps:
      - name: wait for 30 minutes of silence
        if: github.event_name == 'push'
        run: sleep 1800
      - uses: actions/checkout@v6
        with:
          ref: main
          fetch-depth: 0
      - name: derive immutable nightly version
        id: release
        shell: bash
        run: |
          git fetch origin main
          source_sha="$(git rev-parse origin/main)"
          if [ "${{ github.event_name }}" = push ] && [ "$source_sha" != "$GITHUB_SHA" ]; then
            echo "main advanced from $GITHUB_SHA to $source_sha" >&2
            exit 1
          fi
          base="$(sed -n '0,/^version = /s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"
          test -n "$base"
          version="$base-nightly.$(date -u +%Y%m%d).${{ github.run_number }}.g${source_sha:0:7}"
          echo "version=$version" >> "$GITHUB_OUTPUT"
          echo "tag=v$version" >> "$GITHUB_OUTPUT"
          echo "source_sha=$source_sha" >> "$GITHUB_OUTPUT"
```

The workflow-level concurrency behavior is supported by GitHub's current workflow syntax, and `cancel-in-progress: true` cancels both the wait and any active platform builds when a newer push starts.

- [ ] **Step 4: Add required Linux builds using the prepared SHA and version**

Add a `linux` matrix job for `ubuntu-24.04` and `ubuntu-24.04-arm`. Checkout `needs.prepare.outputs.source_sha`, install the existing GPUI packages, use `Swatinem/rust-cache@v2`, replace only the first root `version = "..."` line with Python, run `scripts/package-linux.sh`, and upload `target/package/*.tar.gz` with `actions/upload-artifact@v4` and `if-no-files-found: error`.

Use this version-replacement step in Unix jobs:

```yaml
      - name: set nightly workspace version
        env:
          VERSION: ${{ needs.prepare.outputs.version }}
        run: |
          python3 - "$VERSION" <<'PY'
          from pathlib import Path
          import re
          import sys

          path = Path("Cargo.toml")
          text = path.read_text()
          updated, count = re.subn(
              r'^version = "[^"]+"',
              f'version = "{sys.argv[1]}"',
              text,
              count=1,
              flags=re.MULTILINE,
          )
          if count != 1:
              raise SystemExit("workspace version was not found exactly once")
          path.write_text(updated)
          PY
```

- [ ] **Step 5: Retain the optional macOS build**

Add a `macos` job needing `prepare`, checking out the prepared SHA, applying the same version-replacement step, running `scripts/package-macos.sh`, and uploading both DMG and app tarball patterns. Keep `continue-on-error: true` on the job.

- [ ] **Step 6: Add the required Windows archive**

Add a `windows` job needing `prepare`, running on `windows-latest`, and use:

```yaml
      - uses: actions/checkout@v6
        with:
          ref: ${{ needs.prepare.outputs.source_sha }}
      - name: set nightly workspace version
        shell: pwsh
        env:
          VERSION: ${{ needs.prepare.outputs.version }}
        run: |
          $content = Get-Content -Raw Cargo.toml
          $updated = [regex]::Replace($content, '(?m)^version = "[^"]+"', "version = `"$env:VERSION`"", 1)
          if ($updated -eq $content) { throw "workspace version was not replaced" }
          Set-Content -Path Cargo.toml -Value $updated -NoNewline
      - name: rust toolchain (rust-toolchain.toml)
        run: rustup show active-toolchain
      - uses: Swatinem/rust-cache@v2
      - name: build and package
        shell: pwsh
        env:
          VERSION: ${{ needs.prepare.outputs.version }}
        run: |
          cargo build --release -p comet
          $stage = Join-Path $env:RUNNER_TEMP "comet-$env:VERSION-windows-x86_64"
          New-Item -ItemType Directory -Force $stage | Out-Null
          Copy-Item target/release/comet.exe, README.md, LICENSE $stage
          New-Item -ItemType Directory -Force target/package | Out-Null
          Compress-Archive -Path "$stage/*" -DestinationPath "target/package/comet-$env:VERSION-windows-x86_64.zip"
      - uses: actions/upload-artifact@v4
        with:
          name: windows-x86_64
          path: target/package/*.zip
          if-no-files-found: error
```

- [ ] **Step 7: Replace R2 publication with immutable GitHub prerelease publication**

Add a `publish` job needing all build jobs. Download artifacts with `merge-multiple: true`, require the two Linux tarballs and Windows zip for the prepared version, reject an existing tag with `git ls-remote --exit-code --tags origin "refs/tags/$TAG"`, and then publish:

```yaml
      - uses: softprops/action-gh-release@v3
        with:
          tag_name: ${{ needs.prepare.outputs.tag }}
          name: Comet ${{ needs.prepare.outputs.version }}
          target_commitish: ${{ needs.prepare.outputs.source_sha }}
          prerelease: true
          make_latest: false
          generate_release_notes: true
          body: Built from `${{ needs.prepare.outputs.source_sha }}` on `main`.
          files: dist/*
          fail_on_unmatched_files: true
```

`softprops/action-gh-release@v3` is the maintained Node 24 release line and supports `tag_name`, `target_commitish`, generated notes, prerelease status, latest-release control, asset globs, and unmatched-file failure.

- [ ] **Step 8: Delete obsolete deployment automation**

Delete `.github/workflows/deploy.yml`. Do not modify `edge/` or updater files.

- [ ] **Step 9: Run the contract and syntax checks**

Run:

```powershell
python -m unittest scripts.tests.test_release_workflow -v
python -c "from pathlib import Path; import yaml; [yaml.safe_load(path.read_text()) for path in Path('.github/workflows').glob('*.yml')]; print('workflow YAML parsed')"
actionlint .github/workflows/release.yml
git diff --check
```

Expected: all unit tests pass, PyYAML prints `workflow YAML parsed`, actionlint exits 0, and `git diff --check` is silent.

- [ ] **Step 10: Commit the workflow implementation**

```powershell
git add scripts/tests/test_release_workflow.py .github/workflows/release.yml .github/workflows/deploy.yml
git commit -m "ci: publish debounced cross-platform nightly releases"
```

---

### Task 2: Document and Smoke-Test Windows Packaging

**Files:**
- Modify: `dist/README.md`
- Test: `scripts/tests/test_release_workflow.py`

**Interfaces:**
- Consumes: nightly workflow behavior and Windows archive layout from Task 2.
- Produces: contributor-facing release documentation and local Windows build evidence.

- [ ] **Step 1: Update packaging documentation**

Add a `Windows` section showing `cargo build --release -p comet` and the `comet-<version>-windows-x86_64.zip` layout (`comet.exe`, `README.md`, `LICENSE`). Replace the macOS statement that CI runs on tags with a `Nightly GitHub releases` section documenting push-to-`main`, the 30-minute resettable quiet period, manual bypass, immutable version example, GitHub prerelease status, and GitHub-only artifact publication.

- [ ] **Step 2: Run the focused regression tests**

```powershell
python -m unittest scripts.tests.test_release_workflow -v
```

Expected: all workflow contract tests pass.

- [ ] **Step 3: Build the Windows binary locally**

Run:

```powershell
cargo build --release -p comet
Test-Path target/release/comet.exe
```

Expected: Cargo exits 0 and `Test-Path` prints `True`. Do not modify the local workspace version for this smoke test.

- [ ] **Step 4: Verify the final repository state**

```powershell
python -m unittest discover -s scripts/tests -p 'test_*.py' -v
python -c "from pathlib import Path; import yaml; [yaml.safe_load(path.read_text()) for path in Path('.github/workflows').glob('*.yml')]; print('workflow YAML parsed')"
actionlint .github/workflows/release.yml
rg -n -i "cloudflare|wrangler|r2 object|manifest\.json" .github/workflows
git diff --check
git status --short
```

Expected: all script tests pass; YAML parses; `rg` finds no obsolete cloud publication references and exits 1; diff check is silent; status lists only the intended documentation change before commit.

- [ ] **Step 5: Commit the documentation**

```powershell
git add dist/README.md
git commit -m "docs: describe GitHub nightly packages"
```
