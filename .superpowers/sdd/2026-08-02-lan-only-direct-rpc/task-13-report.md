# Task 13 report: release distribution is the only online edge

## Outcome

The retained Cloudflare Worker is distribution-only. It has one R2 binding and
serves only `GET`/`HEAD /install.sh` and `GET`/`HEAD /releases/*`; every former
health, authentication, organization/workspace, session, device-relay, and
attachment route returns `404`. The deployed Worker name remains
`comet-native-edge` intentionally so the next deployment replaces the existing
authoritative Worker instead of leaving it deployed under the old name.

The updater now owns `DEFAULT_RELEASES_URL` (`https://comet.zeron.sh`) and
`releases_url_from_env()`. Only `COMET_RELEASES_URL` can override it. The CLI,
desktop boot config, and engine config all consume that release-specific
resolver; removed edge/auth variables are not consulted. An unreachable release
endpoint updates only `UpdateStatus.error`; it does not prevent updater creation
or local/LAN engine assembly.

Release metadata is additionally pinned to the canonical fork identity
`matty/comet`. The release workflow writes `$GITHUB_REPOSITORY` into every
manifest, and both the Rust updater and shell installer reject missing or
mismatched repository identity. `COMET_RELEASES_URL` mirrors remain supported,
but must mirror a manifest identifying the same fork. A provenance failure is
terminal for that check; the updater no longer falls back to unprovenanced
`latest.txt` metadata. The workspace currently has no Cargo repository metadata,
so the expected fork is an explicit release-policy constant.

The installer no longer checks `session.json` or instructs users to run removed
login commands. A systemd-capable install starts the local headless service
immediately.

## TDD evidence

- Rust RED: the release resolver tests failed to compile because
  `DEFAULT_RELEASES_URL` and `releases_url_from` did not exist.
- Worker RED: after adding a shell-module test double, all seven distribution
  assertions failed: `/health` still returned `200`, removed runtime routes and
  disallowed methods returned `401`, rather than `404`.
- GREEN: the resolver moved into `comet-update`, and the Worker was reduced to
  its two read-only distribution route families.
- The existing non-fatal update architecture was locked down with an explicit
  unreachable-loopback regression that checks the resulting `UpdateStatus`.

## Removed hosted surface

- WorkOS/JWT auth modules and secrets.
- Session/workspace and device-relay Durable Object implementations, bindings,
  and runtime tests/scripts. Wrangler retains the original `v1` migration only
  as deployment history and adds `v2-distribution-only` to delete both classes.
- Runtime attachment R2 binding and code.
- `jose`, `loro-crdt`, `loro-protocol`, `loro-adaptors`, and `loro-websocket`
  dependencies.
- The legacy custom-domain route at `edge.comet.zeron.sh`; the retained routes
  are only `comet.zeron.sh/install.sh` and `comet.zeron.sh/releases/*`.

## Verification

- `cargo test -p comet-update` — 15 passed, including release independence and
  unreachable-distribution status behavior.
- `cargo test -p comet --bin comet` — 14 passed.
- `npm test --prefix edge` — 7 Worker assertions plus the real-installer
  provenance harness passed.
- `npm run typecheck --prefix edge` — passed.
- `npm run build --prefix edge` — dry-run passed; upload exposes only the
  `RELEASES` R2 binding.
- `cargo fmt --all -- --check` — passed after formatting.
- `cargo clippy -p comet-update --all-targets -- -A
  clippy::collapsible-if -D warnings` — passed. Strict Clippy without the narrow
  allow remains blocked by one pre-existing `collapsible_if` in the unchanged
  update loop.
- Fresh `npm ci` followed by `npm ls --all --depth=0` shows only the Cloudflare
  types, TypeScript, Vitest, and Wrangler direct dependencies.
- `bash -n edge/src/install.sh` and `git diff --check` passed.
- Edge source/config scans found no WorkOS/auth mode, active Durable Object or
  runtime blob bindings, Loro runtime, or removed edge URL references; the only
  class names retained are migration history and deletion tombstones.

Provenance follow-up verification:

- Rust RED failed because the provenance parser did not exist; GREEN covers
  accepted canonical manifests and rejection of both missing and wrong
  repositories. An HTTP-boundary regression also proves a provenance failure
  makes one manifest request and never probes `latest.txt`.
- The installer RED executed the real `install.sh` against controlled manifests
  and showed it continued into artifact download/tar. GREEN rejects missing and
  wrong repository identities before choosing a version or downloading an
  artifact. This harness is part of `npm test`.

Review fix round 1 adds defense at every distribution boundary:

- The Worker deployment preserves the original DO migration record, applies one
  uniquely tagged deletion migration for `SessionRoom` and `DeviceRoom`, deploys
  that distribution-only version first, then lists secrets and deletes the
  obsolete `WORKOS_API_KEY` only when present. List/auth/parse/delete failures
  fail the workflow; an absent secret succeeds without a blanket error waiver.
  An executable config/workflow check also proves only the two distribution
  routes and `RELEASES` binding remain.
- The installer uses a real JSON parser: Python decoding rejects duplicate keys
  and a tested preflight gives a clear dependency error when Python 3 is absent.
  Parsed repository, dotted-numeric version, selected artifact,
  and 64-hex checksum are validated before any path interpolation or download.
  Attack regressions cover duplicate keys, escaped JSON identity, whitespace,
  path traversal, missing metadata, malformed checksums, and checksum mismatch.
- Both Rust staging paths validate selected-artifact metadata before creating a
  staging directory. Downloaded bytes must match the manifest SHA-256 before any
  unpack or swap. Fresh Rust and shell stages persist a provenance/checksum
  marker; existing headless/mac/shell destinations are reusable only when that
  marker exactly matches current canonical metadata. Unverified headless/shell
  installs fail closed, while the macOS staging cache is discarded and restaged.
- Release publication runs only in `matty/comet` and writes that literal identity,
  preventing a fork workflow from populating the canonical bucket under its own
  repository identity.

Review fix round 2 tightened lifecycle and reuse semantics:

- The jq fallback was removed in favor of one explicitly documented Python 3
  dependency. The executable installer harness hides Python and proves the
  installer fails before artifact selection, eliminating untested differences
  between JSON parsers while retaining strict duplicate-key handling.
- Secret cleanup now occurs after the distribution-only deploy. Wrangler secret
  listing and JSON parsing must succeed; deletion is conditional on the key's
  presence and deletion failures propagate. The workflow check asserts ordering
  and rejects blanket `continue-on-error` or `|| true` handling.
- Existing headless, macOS-cache, and shell stages now require an exact
  repository/version/artifact/checksum marker. Tests cover missing metadata,
  missing and mismatched markers, secure verified reuse, and macOS restaging.

## Residual risks and scope notes

- `npm ci` reports three high-severity advisories in the retained build/test
  tool dependency tree. No removed runtime dependency remains; remediation
  requires upstream-compatible Wrangler/Vitest dependency updates.
- The repository's out-of-scope iOS prototype still documents and implements
  the removed hosted API. The approved product scope for this cutover is Desktop
  and TUI only; Task 14 should keep that limitation explicit or remove/archive
  the stale iOS material.
- Backward compatibility is intentionally stricter: legacy release endpoints
  that provide only `latest.txt`, or manifests without `repository`, no longer
  update or install. Operators of intentional mirrors must republish a current
  manifest with `repository: "matty/comet"`, complete selected-artifact metadata,
  and a valid SHA-256. Shell installation also now requires Python 3 plus
  `sha256sum` or `shasum`, and fails closed when those tools are unavailable.
  Existing installs/staging caches created before verification markers were
  introduced are not silently reused; headless/shell users must remove the
  explicitly reported unverified version, while macOS update caches restage.
