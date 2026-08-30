//! Evidence for D71 (2): a live run that escalates to an approval and then
//! never settles must fail FAST, naming the approval — not park to the read
//! loop's own budget with a bare "did not settle" message.
//!
//! Deliberately not `#[ignore]`d and not against a real CLI: it drives
//! `support::recording_decliner` / `support::settle_or_report` directly with
//! an in-process double — a stream that never produces anything, standing in
//! for a run that stalls after the escalation — the same shape
//! `grok_live.rs`, `hermes_live.rs` and `acp_run_fidelity_grok_live.rs` wire
//! into `RunControls` for real, just without needing an authenticated CLI to
//! prove the mechanism. Runs in CI like any other test in this crate.

mod support;

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use comet_harness::HarnessError;
use comet_proto::{AgentEvent, ApprovalRequest};
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::BoxStream;

/// A run that escalates to an approval and then never settles must fail fast
/// naming the approval, not hang out the read loop's whole budget with a bare
/// "the turn settles rather than hanging" message that names nothing.
#[tokio::test]
async fn an_unanswered_approval_fails_fast_and_names_itself() {
    let (responder, watch) = support::recording_decliner();

    // The harness calling `RunControls::request_approval` mid-run -- the same
    // call `acp::session::handle_permission_request` and
    // `codex::handle_server_request` make when the agent asks for permission.
    // The receiver is a `oneshot::Receiver`, not a bare future, but clippy
    // reads any `let _ =` binding of one as `let_underscore_future`; drop it
    // explicitly instead to say plainly that nothing is meant to poll it.
    std::mem::drop(responder(ApprovalRequest::FileRead {
        path: "src/secret.rs".into(),
    }));

    // Stands in for a run that stalls after the escalation above and never
    // produces another event, `Done` included -- exactly the shape D71 (2)
    // describes: an approval nobody answers, then silence.
    let stream: BoxStream<'static, Result<AgentEvent, HarnessError>> =
        futures::stream::pending().boxed();

    let started = std::time::Instant::now();
    let result = AssertUnwindSafe(support::settle_or_report(
        stream,
        Duration::from_secs(120),
        &watch,
        |_event| {},
    ))
    .catch_unwind()
    .await;
    let elapsed = started.elapsed();

    let panic = result.expect_err("an unanswered approval must fail the read loop");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload is a string");

    assert!(
        message.contains("Read a file") && message.contains("src/secret.rs"),
        "the failure must name the pending approval, not just say the turn hung: {message}"
    );

    assert!(
        elapsed < Duration::from_secs(5),
        "must fail fast on the approval rather than wait out the 120s budget: {elapsed:?}"
    );
}

/// The companion case: nothing ever asks for an approval, and the stream
/// simply never settles either. The failure must still fire, at the budget --
/// this helper must not fail SILENTLY just because no approval was involved.
#[tokio::test]
async fn a_plain_hang_with_no_approval_still_fails_at_the_budget() {
    let (_responder, watch) = support::recording_decliner();

    let stream: BoxStream<'static, Result<AgentEvent, HarnessError>> =
        futures::stream::pending().boxed();

    let started = std::time::Instant::now();
    let result = AssertUnwindSafe(support::settle_or_report(
        stream,
        Duration::from_millis(200),
        &watch,
        |_event| {},
    ))
    .catch_unwind()
    .await;
    let elapsed = started.elapsed();

    let panic = result.expect_err("a stream that never settles must still fail");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload is a string");
    assert!(
        message.contains("the turn settles rather than hanging"),
        "no approval was involved, so the bare message must survive: {message}"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "must wait out the budget rather than fail early with nothing to report: {elapsed:?}"
    );
}
