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
- Session/workspace and device-relay Durable Objects, bindings, migrations, and
  all related tests/scripts.
- Runtime attachment R2 binding and code.
- `jose`, `loro-crdt`, `loro-protocol`, `loro-adaptors`, and `loro-websocket`
  dependencies.
- The legacy custom-domain route at `edge.comet.zeron.sh`; the retained routes
  are only `comet.zeron.sh/install.sh` and `comet.zeron.sh/releases/*`.

## Verification

- `cargo test -p comet-update` — 7 passed, including release independence and
  unreachable-distribution status behavior.
- `cargo test -p comet --bin comet` — 14 passed.
- `npm test --prefix edge` — 7 passed; only `distribution.test.ts` remains.
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
- Edge source/config scans found no WorkOS, auth mode, Durable Object, runtime
  blob, Loro, or removed edge URL references.

## Residual risks and scope notes

- `npm ci` reports three high-severity advisories in the retained build/test
  tool dependency tree. No removed runtime dependency remains; remediation
  requires upstream-compatible Wrangler/Vitest dependency updates.
- The repository's out-of-scope iOS prototype still documents and implements
  the removed hosted API. The approved product scope for this cutover is Desktop
  and TUI only; Task 14 should keep that limitation explicit or remove/archive
  the stale iOS material.
