# Debounced Nightly Releases Design

## Goal

Replace Comet's tag-driven and Cloudflare-backed release automation with
immutable GitHub prereleases built from `main` after the branch has received no
new commits for 30 minutes. Add Windows x86_64 to the existing Linux and macOS
artifact set.

## Scope

- Rework `.github/workflows/release.yml` into the sole release workflow.
- Delete `.github/workflows/deploy.yml`.
- Remove Cloudflare credentials, Wrangler commands, R2 uploads, and release
  manifest publication from GitHub Actions.
- Add Windows x86_64 packaging and release artifacts.
- Update packaging documentation for the new GitHub-only nightly process.
- Leave `edge/`, the updater implementation, and other server or distribution
  code unchanged. Removing those components is outside this workflow-focused
  change.

## Trigger and quiet period

The release workflow runs on pushes to `main` and on `workflow_dispatch`.
Push-triggered runs enter a lightweight Ubuntu job that waits for 30 minutes.
The workflow uses one repository-wide concurrency group with
`cancel-in-progress: true`, so every subsequent push cancels the previous
pending or building run and starts a fresh quiet period.

Manual dispatches bypass the quiet period. Before either trigger proceeds to
the build matrix, the workflow verifies that the commit being built is still
the current `main` HEAD. This prevents an older run that escaped cancellation
from publishing after a newer commit lands.

Only one release pipeline may be active at a time. A push during compilation
also cancels that compilation and begins a new 30-minute quiet period; the
project therefore publishes only commits that remained current throughout the
entire release run.

## Version format

Each run derives the base version from `[workspace.package].version` in the
root `Cargo.toml` and constructs:

```text
<base>-nightly.<UTC YYYYMMDD>.<github.run_number>.g<7-character commit SHA>
```

For example:

```text
0.1.15-nightly.20260804.123.g30a642b
```

The Git tag adds a `v` prefix:

```text
v0.1.15-nightly.20260804.123.g30a642b
```

This is a valid SemVer prerelease, retains the product's base version, is
unique across automated and manual builds through `github.run_number`, and is
traceable to its source commit. The `g` prefix prevents an all-numeric short
SHA from being interpreted as a numeric SemVer identifier with a forbidden
leading zero.

The workflow temporarily replaces the workspace version in its checked-out
copy before packaging. The source repository is not modified or committed.
Consequently the compiled application version, package filenames, Git tag, and
GitHub Release title all use the same nightly version.

## Build and packaging jobs

After the quiet-period job emits the version and confirms the commit, the
workflow fans out into these jobs:

- Linux x86_64 on `ubuntu-24.04`, using `scripts/package-linux.sh`.
- Linux aarch64 on `ubuntu-24.04-arm`, using
  `scripts/package-linux.sh`.
- macOS Apple Silicon on `macos-latest`, using
  `scripts/package-macos.sh`.
- Windows x86_64 on `windows-latest`, building `comet.exe` in release mode and
  producing `comet-<version>-windows-x86_64.zip`.

The Windows archive contains `comet.exe`, `README.md`, and `LICENSE`. Its
packaging commands live directly in `release.yml` until another consumer needs
a reusable Windows packaging script.

Linux and Windows jobs are required. The macOS job retains its existing
allowed-failure behavior because its platform-specific paths are not yet known
to be reliable in CI. Each job uploads its packages as workflow artifacts with
`if-no-files-found: error`.

## GitHub Release publication

The publish job runs only in `matty/comet`, downloads all successful build
artifacts into one directory, and validates that required Linux and Windows
filenames contain the computed nightly version. It then creates an immutable
Git tag at the verified source commit and a GitHub prerelease with that tag.
The prerelease is explicitly excluded from GitHub's `latest` designation and
attaches every downloaded package.

The release body identifies the full source commit. GitHub-generated release
notes may be included, but the immutable tag and attached artifacts are the
canonical output. No manifest, mirror, installer endpoint, Worker, or R2
bucket is updated.

If a tag already exists, publication fails instead of replacing an earlier
release. The run number makes this exceptional and preserves the immutable
release guarantee.

## Failure and cancellation behavior

- A new push cancels any pending or building release and restarts the quiet
  period.
- A stale commit check exits without publishing.
- Any required Linux or Windows build/package failure blocks publication.
- A macOS failure is reported but does not block the other platforms.
- A missing required artifact blocks publication.
- A GitHub Release creation failure leaves build artifacts available on the
  workflow run for diagnosis.
- Manual runs follow the same validation and publishing rules but skip the
  30-minute wait.

## Documentation and validation

`dist/README.md` will describe the debounced nightly trigger, GitHub-only
publication, version format, and Windows archive. Obsolete text describing
tag-triggered CI will be removed.

Validation will cover:

- YAML parsing for every workflow that remains under `.github/workflows/`.
- Static assertions for the `main` push trigger, manual trigger, concurrency
  cancellation, 30-minute delay, version construction, Windows runner, and
  GitHub prerelease settings.
- Confirmation that `.github/workflows/deploy.yml` is absent.
- Confirmation that workflows contain no Cloudflare, R2, or Wrangler
  references.
- A local Windows release build and archive-layout smoke test where the current
  platform permits it.
