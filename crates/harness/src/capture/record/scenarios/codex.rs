use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use comet_proto::{ApprovalDecision, ReasoningLevel, RunRequest, RuntimeMode};
use serde_json::{Value, json};

use crate::capture::record::provider::CaptureProvider;
use crate::capture::record::providers::codex::{CodexProvider, rpc_request};
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::{Session, protocol_stopped};
use crate::launch::LaunchDescriptor;

/// SPAWN for every Codex discovery row (`model-discovery` and its
/// `-neutral-cwd`/`-project-cwd`/`-logged-out` aliases): the same launch,
/// varying only by which `cwd`/`codex_home` the row's `ScenarioInput`
/// carries.
pub(in crate::capture::record) fn model_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let home = input
        .codex_home
        .clone()
        .or_else(crate::codex::discovery::codex_home)
        .ok_or_else(|| {
            anyhow!("Codex home could not be found. Pass --codex-home and try again.")
        })?;
    let home = crate::capture::record::session::absolute_from_parent(home)?;
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::codex::discovery::discovery_launch(
        executable, &home, &cwd,
    ))
}

/// The handshake, then the cursor-paginated `model/list` loop. Per the
/// amendment "the scenario body calls the handshake; the recorder does
/// not" — `record_generic` no longer calls `P::handshake` for any scenario,
/// so every Codex body (discovery here; run bodies from Task 5 on) opens
/// with it directly, since Codex's app-server protocol genuinely requires
/// `initialize`/`initialized` before any request. The pagination loop itself
/// is `recording.rs:488-506`'s loop, moved unchanged.
pub(in crate::capture::record) async fn model_discovery(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let mut cursor: Option<String> = None;
    for _ in 0..20_u64 {
        let id = session.provider.next_id();
        session
            .send(&codex_model_list_line(id, cursor.as_deref()))
            .await?;
        let reply = session
            .wait_for("JSON-RPC reply", |value| {
                (value["id"].as_u64() == Some(id)).then(|| value.clone())
            })
            .await?;
        cursor = reply["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return Ok(());
        }
    }
    bail!("Codex returned too many model pages. Update the CLI or retry the capture later.")
}

fn codex_model_list_line(id: u64, cursor: Option<&str>) -> String {
    let mut params = serde_json::Map::new();
    if let Some(cursor) = cursor {
        params.insert("cursor".into(), cursor.into());
    }
    rpc_request(id, "model/list", Value::Object(params))
}

// Below this point: `fresh-text`, `resume`, `steer`, `interruption`,
// `approval` and `approval-on-request`. Every one of these is a registered
// row in `super::SCENARIOS` and reachable through `record()`.

/// The model every non-discovery Codex scenario runs against: cheap and
/// fast, mirroring `record/scenarios/claude.rs`'s `CHEAP_MODEL`. Ported from
/// `comet-provider-capture.rs`'s `cheap_codex_request` — decision "the
/// scenario owns its prompt" moves the choice out of the binary along with
/// the prompt text itself.
const CHEAP_MODEL: &str = "gpt-5.6-luna";

/// The `RunRequest` every non-discovery Codex scenario in this file starts
/// from: the cheap model, low reasoning, and the caller's cwd (or a neutral
/// temp directory). Always run through `crate::codex::normalize_run_request`
/// here — exactly where `recording.rs`'s `RecordingSession::start` used to
/// apply it before anything else touched the request — so every caller
/// (launch builder and scenario body alike) sees the same normalized value
/// and the linked-worktree sandbox escalation (D13) can never disagree
/// between the two.
fn cheap_codex_request(prompt: &str, input: &ScenarioInput, mode: RuntimeMode) -> RunRequest {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    let request = RunRequest {
        prompt: prompt.into(),
        model: Some(CHEAP_MODEL.into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(mode)
    };
    crate::codex::normalize_run_request(request)
}

/// Start a brand-new thread and return its id. Shared by every scenario here
/// except `resume`, which needs `thread/resume` instead — see
/// [`resume_thread`].
async fn start_thread(
    session: &mut Session<CodexProvider>,
    request: &RunRequest,
) -> anyhow::Result<String> {
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "thread/start",
            crate::codex::thread_start_params(request),
        ))
        .await?;
    let reply = session
        .wait_for("JSON-RPC reply", |value| {
            (value["id"].as_u64() == Some(id)).then(|| value.clone())
        })
        .await?;
    let thread_id = reply["result"]["thread"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if thread_id.is_empty() {
        return protocol_stopped("Codex", "thread identifier");
    }
    Ok(thread_id)
}

/// Resume the thread named by `request.resume` — never a fresh one. If
/// Codex's reply carries an error, that IS the capture: no fallback to
/// `thread/start`, which would silently mislabel a rejected resume as if it
/// had actually resumed. Ported from `recording.rs`'s deleted `codex_run`
/// resume branch (`thread_reply.get("error").is_some() && method ==
/// "thread/resume"` bail) — the one piece of that branch that was driving a
/// real promise about the evidence, not merely validating a frame's shape,
/// so it survives the port.
async fn resume_thread(
    session: &mut Session<CodexProvider>,
    request: &RunRequest,
) -> anyhow::Result<String> {
    let resume_id = request.resume.as_deref().unwrap_or_default();
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "thread/resume",
            crate::codex::thread_resume_params(request, resume_id),
        ))
        .await?;
    let reply = session
        .wait_for("JSON-RPC reply", |value| {
            (value["id"].as_u64() == Some(id)).then(|| value.clone())
        })
        .await?;
    if reply.get("error").is_some() {
        bail!("Codex rejected the requested thread resume; no fresh-thread fallback was recorded.");
    }
    let thread_id = reply["result"]["thread"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if thread_id.is_empty() {
        return protocol_stopped("Codex", "thread identifier");
    }
    Ok(thread_id)
}

/// Start the turn every scenario here opens with: the request's own prompt,
/// via the production `turn/start` builder. No reply wait — `recording.rs`'s
/// `codex_run` never waited for `turn/start`'s own reply either; the frame
/// loop that follows picks up `turn/started` and everything after it.
async fn start_turn(
    session: &mut Session<CodexProvider>,
    request: &RunRequest,
    thread_id: &str,
) -> anyhow::Result<()> {
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "turn/start",
            crate::codex::turn_start_params(request, thread_id, &request.prompt),
        ))
        .await
}

/// Pump frames until Codex's own `turn/started` notification confirms the
/// turn is genuinely under way, returning the turn id it carries.
///
/// Ported from `recording.rs`'s deleted `codex_run`, whose `active_turn`
/// tracking gated the steer/interrupt send on `active_turn.is_some()` — i.e.
/// on having already observed this exact notification. Acting before it
/// arrives would record a race against Codex's own turn bookkeeping, not a
/// steer or an interruption. Driving, not validating: nothing here inspects
/// anything else about the frame the way the deleted code's per-script
/// terminal-frame bail did.
async fn wait_for_turn_started(session: &mut Session<CodexProvider>) -> anyhow::Result<String> {
    session
        .wait_for("a turn/started notification", |frame| {
            (frame["method"] == "turn/started").then(|| {
                frame["params"]["turn"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
        })
        .await
}

/// The `RunRequest` `record.rs`'s `derive_launch` calls exactly once per
/// `fresh-text` recording: it builds `fresh-text`'s launch from this value,
/// then hands the SAME value to `fresh_text` below (via `Session::request`),
/// so the recorded argv and the recorded wire line can never describe two
/// different requests — see `record/scenarios.rs`'s `ScenarioLaunch` for the
/// hazard this closes.
pub(in crate::capture::record) fn fresh_text_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    Ok(cheap_codex_request(
        "Reply with the single word capture.",
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// A plain text turn: start a fresh thread, start the turn, wait for
/// whichever terminal frame Codex sends. No bail on the terminal frame's
/// type — see `recording.rs`'s `codex_run` doc comment on why that check is
/// deleted, not ported. Reads the request `record.rs` already built for the
/// launch (`Session::request`) rather than rebuilding it — see
/// `fresh_text_request`'s own doc comment.
pub(in crate::capture::record) async fn fresh_text(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("fresh-text is a Run scenario and always carries a request");
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    session.wait_for_turn_end().await
}

/// Unlike Claude, a Codex resume never reaches the CLI as a launch argument;
/// the thread id lives entirely on the wire (`thread/resume`, built by
/// `resume_thread`) — `crate::codex::run_launch` never reads
/// `request.resume`. Same one-call-per-recording contract as
/// `fresh_text_request` above.
pub(in crate::capture::record) fn resume_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let resume_id = input
        .resume_id
        .clone()
        .ok_or_else(|| anyhow!("The resume scenario needs a --resume-id."))?;
    let mut request = cheap_codex_request(
        "Reply with the single word resumed.",
        input,
        RuntimeMode::AutoAcceptEdits,
    );
    request.resume = Some(resume_id);
    Ok(request)
}

pub(in crate::capture::record) async fn resume(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("resume is a Run scenario and always carries a request");
    let thread_id = resume_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    session.wait_for_turn_end().await
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::capture::record) fn steer_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    Ok(cheap_codex_request(
        "Begin a short response, then accept the follow-up instruction.",
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// The exact text `steer` sends as its `turn/steer` message. A named
/// constant — like `record/scenarios/claude.rs`'s `CLAUDE_APPROVAL_COMMAND`
/// — so the driving code and its own test can't drift into two separate
/// string literals.
const STEER_MESSAGE: &str = "Capture steering message.";

/// Start a fresh thread and turn, wait for the turn to be genuinely under
/// way (see [`wait_for_turn_started`]'s doc comment on why that gate is not
/// optional), then send the production `turn/steer` params and wait for the
/// turn to end. No reply wait on the steer itself and no bail on the
/// terminal frame's type — `recording.rs`'s deleted `codex_run` did both,
/// and both were the "frame check that aborts" class this stage's design
/// removes (§3.2): a Codex rejection of the steer, or a turn that failed or
/// aborted instead of completing under it, is itself the evidence a capture
/// exists to record, not a reason to discard it.
pub(in crate::capture::record) async fn steer(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("steer is a Run scenario and always carries a request");
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    let turn_id = wait_for_turn_started(session).await?;
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "turn/steer",
            crate::codex::turn_steer_params(&thread_id, &turn_id, STEER_MESSAGE),
        ))
        .await?;
    session.wait_for_turn_end().await
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::capture::record) fn interruption_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    Ok(cheap_codex_request(
        "Count upward slowly and keep working until interrupted.",
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// Same shape as [`steer`]: wait for the turn to be genuinely under way, then
/// send the production `turn/interrupt` params and wait for the turn to end.
/// No bail on the terminal frame being `turn/aborted` specifically —
/// `recording.rs`'s deleted `codex_run` required exactly that, which is the
/// same removed validator class `steer`'s doc comment explains.
pub(in crate::capture::record) async fn interruption(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("interruption is a Run scenario and always carries a request");
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    let turn_id = wait_for_turn_started(session).await?;
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "turn/interrupt",
            crate::codex::turn_interrupt_params(&thread_id, &turn_id),
        ))
        .await?;
    session.wait_for_turn_end().await
}

/// The exact command the `approval` prompt below asks Codex to run three times, mirroring
/// `record/scenarios/claude.rs`'s `CLAUDE_APPROVAL_COMMAND` — one constant so the prompt text and
/// the instruction it gives can't drift apart as two separate literals.
const CODEX_APPROVAL_COMMAND: &str = "echo capture";

/// Prompt for `approval`: ask Codex to run a fixed, harmless command three times, so the capture
/// records three real command-execution approval decisions in one turn. Moved here, unchanged in
/// substance, from the deleted `capture/approval/common.rs::codex_approval_prompt` — decision "the
/// scenario owns its prompt", completing for Codex the move Task 4 already made for Claude's
/// `claude_approval_prompt`.
fn codex_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(crate::capture::safety::APPROVAL_MARKER_NAME);
    format!(
        "Run the exact command {} three separate times, then add exactly one file at {} \
         containing exactly capture followed by one newline.",
        serde_json::to_string(CODEX_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::capture::record) fn approval_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_codex_request(
        &codex_approval_prompt(&cwd),
        input,
        RuntimeMode::ApprovalRequired,
    ))
}

/// Shared by `approval_on_request_request` and `approval_on_request`'s own
/// grant-time recheck (below): both need `input.approval_target`, and
/// duplicating the "needs an --approval-target" error text in two places
/// would let them drift.
fn require_approval_target(input: &ScenarioInput) -> anyhow::Result<PathBuf> {
    input
        .approval_target
        .clone()
        .ok_or_else(|| anyhow!("The approval-on-request scenario needs an --approval-target."))
}

/// Prompt for `approval-on-request`: ask Codex to run the platform-specific marker-writing
/// command `crate::capture::safety::approval_marker_command` builds, once, so the wire records a
/// real `item/commandExecution/requestApproval` round trip for exactly the command the fence's
/// grant-time recheck expects. Moved here, unchanged in substance, from the deleted
/// `capture/approval/common.rs::approval_on_request_prompt` — same rationale as
/// `codex_approval_prompt` above.
fn approval_on_request_prompt(target: &Path) -> String {
    format!(
        "Run this exact command once and report success: {}",
        crate::capture::safety::approval_marker_command(target)
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::capture::record) fn approval_on_request_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    // `validate_on_request_preflight`'s doc comment explains why the deleted `recording.rs`
    // assertion against `normalize_run_request`'s cwd-dependent sandbox escalation is not ported:
    // the fence's own `repository_root` check makes that escalation unreachable for this
    // scenario's cwd. This narrower debug assertion restores the other half of what that
    // assertion covered — `RunRequest::for_session`'s `RuntimeMode -> SandboxLevel` mapping
    // itself changing underneath this scenario, independent of any particular cwd.
    // `comet_proto::agent::tests::for_session_pairs_the_sandbox_with_the_mode` already guards the
    // mapping directly; this is defense in depth for the one caller whose safety depends on it.
    debug_assert_eq!(
        RunRequest::for_session(RuntimeMode::AutoAcceptEdits).sandbox,
        comet_proto::SandboxLevel::WorkspaceWrite,
        "approval-on-request depends on AutoAcceptEdits mapping to workspace-write; Codex's \
         on-request approval path is unreachable under any other sandbox"
    );
    let target = require_approval_target(input)?;
    Ok(cheap_codex_request(
        &approval_on_request_prompt(&target),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// Recognizes any Codex server request whose method ends in
/// `/requestApproval` — `item/fileChange/requestApproval` for `approval`,
/// `item/commandExecution/requestApproval` for `approval_on_request`, and
/// (unreachable by any capture so far, but not excluded on purpose)
/// `item/permissions/requestApproval` — and returns its JSON-RPC id,
/// unmodified, to echo back in the reply. Every other frame — item lifecycle
/// notifications, `turn/started`, anything else — returns `None` and is
/// simply left unanswered.
///
/// This is the Codex counterpart of `record/scenarios/claude.rs`'s
/// `pending_approval`: noticing that a frame is (or is not) an approval
/// request is driving, not validating, so nothing here checks the item type,
/// the command text, the request order, or the shape of the surrounding
/// transcript the way the deleted `approval/codex.rs` validators
/// (`validate_codex_approval_request`, `validate_codex_on_request_approval`,
/// `codex_approval_ids`, and everything upstream of them) did. Shared by both
/// scenario bodies below because recognizing "this is an approval request"
/// does not depend on which of the two scenarios is running.
///
/// Answering *every* recognized method unconditionally — rather than only
/// the one each scenario expects — is deliberate, not an oversight: an
/// approval left unanswered blocks the turn until the recorder's own hard
/// timeout kills it, destroying a paid-for capture. Per design §3.2 that is
/// the exact outcome a driver must avoid; auto-answering (subject to the
/// grant-time recheck in `answer_every_approval` below) is the safe failure.
fn pending_approval(frame: &Value) -> Option<Value> {
    let method = frame["method"].as_str()?;
    if !method.ends_with("/requestApproval") {
        return None;
    }
    let id = frame.get("id")?;
    (!id.is_null()).then(|| id.clone())
}

/// The reply for one approval request: the JSON-RPC envelope every scenario
/// in this file hand-builds (same as `rpc_request` for a request line), but
/// the `"decision"` literal itself — the one piece production owns and the
/// one piece that can silently drift — routed through the real
/// `codex::approval::decision_literal` (`crates/harness/src/codex/approval.rs:256`),
/// the same function `codex/mod.rs:1343`'s `handle_server_request` calls for
/// a real "allow"/"decline" reply. `codex::approval` is declared
/// `pub(crate)` (`codex/mod.rs:29`, a bare `pub(crate) mod approval;` with
/// no comment of its own) specifically so this reply builder can reach it.
fn decision_response(id: Value, decision: &ApprovalDecision) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"decision": crate::codex::approval::decision_literal(decision)},
    })
    .to_string()
}

/// Pump frames, answering every approval request `pending_approval`
/// recognizes, until the provider's own terminal turn notification ends the
/// loop. Shared by `approval` and `approval_on_request`, which differ only in
/// which request they start and which `recheck` they pass (see each
/// function's own doc comment).
///
/// No count, no order, no item-type check, no command-text check: the
/// deleted `CodexApprovalState`/`CodexOnRequestState` and the validators that
/// threaded them (`observe_codex_approval_*`, `validate_codex_approval_*`,
/// `validate_on_request_item`, `validate_codex_on_request_approval`)
/// enforced an exact "reviewed" contract — a fixed count and order of command
/// executions, a single bounded file-change, one sandbox failure followed by
/// exactly one retry — and bailed on any deviation, discarding a real,
/// paid-for capture. Per design §3.2, only driving survives here: notice a
/// request, answer it, keep going, and stop only when the provider itself
/// says the turn is over.
///
/// `recheck` is the one exception, and it is not a frame validator: it
/// re-verifies the pre-spawn fence's environment guarantee — the cwd or
/// approval target the fence validated before spawn — still holds at the
/// exact moment a write is about to be granted, closing the TOCTOU window
/// between spawn and this grant. `recheck` never reads the frame; it reads
/// the filesystem. On failure this declines the write instead of aborting
/// the capture: no unsafe write is granted, and the tape survives.
async fn answer_every_approval(
    session: &mut Session<CodexProvider>,
    mut recheck: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    loop {
        let Some(frame) = session.next_frame().await? else {
            return protocol_stopped("Codex", "an approval request or a turn end");
        };
        if CodexProvider::turn_complete(&frame) {
            return Ok(());
        }
        if let Some(id) = pending_approval(&frame) {
            let decision = match recheck() {
                Ok(()) => ApprovalDecision::Allow,
                Err(err) => ApprovalDecision::Deny {
                    message: err.to_string(),
                },
            };
            session.send(&decision_response(id, &decision)).await?;
        }
    }
}

/// Start a fresh thread and turn under `RuntimeMode::ApprovalRequired`, then
/// answer every `item/fileChange/requestApproval` request the model makes
/// until the turn ends. Ported from `recording.rs`'s deleted `codex_run`
/// `CodexRunScript::Approval` arm, minus every validator listed on
/// `answer_every_approval`'s doc comment.
///
/// The grant-time recheck re-verifies the cwd's identity against
/// `session.fence.approval_cwd_identity` — the value `record::codex_fence`
/// records before spawn — with `require_marker_absent: true`, matching what
/// the deleted `codex_run` checked immediately before accepting. Unlike that
/// deleted code, a mismatch here declines the grant instead of aborting the
/// whole capture.
pub(in crate::capture::record) async fn approval(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("approval is a Run scenario and always carries a request");
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    let cwd = PathBuf::from(&request.cwd);
    let expected_cwd_identity = session.fence.approval_cwd_identity.clone();
    answer_every_approval(session, move || {
        crate::capture::safety::validate_ordinary_approval_cwd(
            &cwd,
            expected_cwd_identity.as_ref(),
            true,
        )
        .map(|_identity| ())
    })
    .await
}

/// Same shape as [`approval`], differing only in which request it starts
/// (`approval_on_request_request`, built from `input.approval_target` rather
/// than `input.cwd`), which approval method Codex ends up asking about
/// (`item/commandExecution/requestApproval` rather than
/// `item/fileChange/requestApproval`) — `answer_every_approval` does not need
/// to know which one it is answering — and which grant-time recheck it
/// passes: `require_empty_approval_target` against
/// `session.fence.approval_target_identity`, matching what the deleted
/// `codex_run` checked immediately before accepting an on-request approval.
/// Ported from `recording.rs`'s deleted `codex_run` `CodexRunScript::ApprovalOnRequest`
/// arm, minus every validator listed on `answer_every_approval`'s doc
/// comment.
pub(in crate::capture::record) async fn approval_on_request(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("approval-on-request is a Run scenario and always carries a request");
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    let target = require_approval_target(input)?;
    let expected_target_identity = session.fence.approval_target_identity.clone();
    answer_every_approval(session, move || {
        crate::capture::safety::require_empty_approval_target(
            &target,
            expected_target_identity.as_ref(),
        )
        .map(|_identity| ())
    })
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::capture::record::scenarios::ScenarioLaunch;
    use crate::capture::record::session::FenceOutcome;
    use crate::capture::test_support::{
        absolute_program, channel_payloads, config, contract_request, fixture_path,
    };
    use crate::capture::types::{Channel, CommandSnapshot};
    use crate::launch::StdioMode;

    /// Starts a run-scenario `Session` exactly the way `record.rs`'s
    /// `derive_launch`/`record_generic` now do: one `RunRequest`, used to
    /// build both the launch and `Session::request` — never rebuilt by the
    /// scenario body. Shared by every test below that drives a real spawn.
    async fn start_codex_run_session(
        scenario_name: &'static str,
        executable: PathBuf,
        raw_root: &Path,
        request: RunRequest,
    ) -> Session<CodexProvider> {
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config(scenario_name, executable, "codex", raw_root);
        Session::start(
            CodexProvider::new(),
            &cfg,
            launch,
            FenceOutcome::none(),
            Some(request),
        )
        .await
        .unwrap()
    }

    #[test]
    fn codex_capture_uses_the_run_command_builder() {
        let request = contract_request();
        let exe = absolute_program("codex");
        let launch = crate::codex::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(snapshot.args, ["app-server"]);
        assert_eq!(snapshot.cwd.as_deref(), Some(request.cwd.as_str()));
        assert!(snapshot.configured_env.is_empty(), "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0);
    }

    /// Break caught: the `codex/fresh-text` row stops naming `fresh_text_request`, so
    /// `record.rs`'s `derive_launch` would build its launch from the wrong request builder (or find
    /// none at all).
    #[test]
    fn fresh_text_row_is_wired_to_fresh_text_request() {
        let input = ScenarioInput::default();
        let spec = crate::capture::record::scenarios::scenario("codex", "fresh-text").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/fresh-text must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            fresh_text_request(&input).unwrap()
        );
    }

    /// Break caught: same as `fresh_text_row_is_wired_to_fresh_text_request`, for `resume`.
    #[test]
    fn resume_row_is_wired_to_resume_request() {
        let input = ScenarioInput {
            resume_id: Some("resume-abc".into()),
            ..ScenarioInput::default()
        };
        let spec = crate::capture::record::scenarios::scenario("codex", "resume").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/resume must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            resume_request(&input).unwrap()
        );
    }

    /// Break caught: same as `fresh_text_row_is_wired_to_fresh_text_request`, for `steer`.
    #[test]
    fn steer_row_is_wired_to_steer_request() {
        let input = ScenarioInput::default();
        let spec = crate::capture::record::scenarios::scenario("codex", "steer").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/steer must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            steer_request(&input).unwrap()
        );
    }

    /// Break caught: same as `fresh_text_row_is_wired_to_fresh_text_request`, for
    /// `interruption`.
    #[test]
    fn interruption_row_is_wired_to_interruption_request() {
        let input = ScenarioInput::default();
        let spec = crate::capture::record::scenarios::scenario("codex", "interruption").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/interruption must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            interruption_request(&input).unwrap()
        );
    }

    /// Break caught: a Codex run driver skips a handshake stage, loses the concrete run scenario,
    /// or waits forever after the provider's terminal turn notification.
    ///
    /// Ported from `recording.rs`, renamed from `..._records_the_explicit_script` — `CodexRunScript`
    /// no longer names what runs, the scenario functions do. `fresh_text`'s real prompt ("Reply with
    /// the single word capture.") now has its own branch in `fake_codex.rs` (`simple_completed`,
    /// additive alongside the pre-existing `scenario:capture-fresh` test marker, same rationale as
    /// the `steer`/`interrupt` matches below), so this drives a genuine modelled `turn/completed`
    /// transcript rather than the fixture's generic `fail_turn` fallback — the pin below fails
    /// loudly if that dispatch match ever stops matching and the fallback quietly took over.
    #[tokio::test]
    async fn recorder_codex_run_records_the_explicit_scenario() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let input = ScenarioInput::default();
        let request = fresh_text_request(&input).unwrap();
        let mut session =
            start_codex_run_session("fresh-text", executable, raw.path(), request).await;

        fresh_text(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdin = channel_payloads(&capture, Channel::Stdin);
        let methods: Vec<_> = stdin
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|line| line["method"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            methods,
            ["initialize", "initialized", "thread/start", "turn/start"]
        );
        // Pinned to `simple_completed`'s exact terminal frame, per its own literal in
        // `fake_codex.rs` — `fail_turn`'s fallback emits a *different* terminal id ("t-bad") and
        // method ("turn/failed"), so a fallthrough (the prompt match silently stopping) fails this
        // assertion instead of satisfying it by coincidence.
        let stdout = channel_payloads(&capture, Channel::Stdout);
        assert!(
            stdout.contains(&r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#),
            "the fake-codex simple_completed() branch must have run, not the unknown-scenario \
             fallthrough: {stdout:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Ported from `recording.rs` (name kept). Now end-to-end: drives `steer` and `interruption`
    /// against `fake-codex`'s real `steer`/`interrupt` branches — reachable through the ported
    /// scenarios' own production prompts, per the additive match `fake_codex.rs` gained alongside
    /// the neutral-recorder stage's port of these scenarios — and asserts the exact
    /// `turn/steer`/`turn/interrupt` line each one puts on the
    /// wire matches `crate::codex::turn_steer_params`/`turn_interrupt_params` computed
    /// independently. The pre-port version only checked those two production functions against
    /// themselves, never against anything `codex_run` actually sent.
    ///
    /// Break caught: `steer`/`interruption` stop calling those production helpers and hand-build
    /// the JSON-RPC params inline instead.
    #[tokio::test]
    async fn capture_steer_and_interrupt_params_match_production_helpers() {
        let input = ScenarioInput::default();

        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let request = steer_request(&input).unwrap();
        let mut session = start_codex_run_session("steer", executable, raw.path(), request).await;
        steer(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();
        let stdin: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let steer_line = stdin
            .iter()
            .find(|line| line["method"] == "turn/steer")
            .expect("a turn/steer line was sent");
        assert_eq!(
            steer_line["params"],
            crate::codex::turn_steer_params("th-1", "t-1", STEER_MESSAGE)
        );
        assert_eq!(capture.exit_code, Some(0));

        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let request = interruption_request(&input).unwrap();
        let mut session =
            start_codex_run_session("interruption", executable, raw.path(), request).await;
        interruption(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();
        let stdin: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let interrupt_line = stdin
            .iter()
            .find(|line| line["method"] == "turn/interrupt")
            .expect("a turn/interrupt line was sent");
        assert_eq!(
            interrupt_line["params"],
            crate::codex::turn_interrupt_params("th-1", "t-1")
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Break caught: capture skips the production request normalization that works around
    /// Codex's malformed workspace-write mount for linked slash-branch worktrees.
    ///
    /// Ported from `recording.rs`. Builds its own `RunRequest` directly rather than through
    /// `fresh_text_request`, because this test needs `sandbox`/`model_options` control
    /// `ScenarioInput` does not expose — `crate::codex::normalize_run_request` is applied here at
    /// exactly the point every `*_request` builder in this file applies it internally, and the
    /// driving below reuses the same `start_thread`/`start_turn` helpers every scenario body does.
    #[tokio::test]
    async fn recorder_codex_run_preserves_production_linked_worktree_parameters() {
        let raw = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}", admin.path().display()),
        )
        .unwrap();
        std::fs::write(
            admin.path().join("HEAD"),
            "ref: refs/heads/feature/capture\n",
        )
        .unwrap();
        let mut request = RunRequest {
            prompt: "scenario:capture-fresh".into(),
            model: Some("gpt-5.6-luna".into()),
            reasoning: Some(ReasoningLevel::Low),
            cwd: worktree.path().display().to_string(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        request
            .model_options
            .insert("serviceTier".into(), json!("fast"));
        let provider_request = crate::codex::normalize_run_request(request.clone());

        let executable = fixture_path("fake-codex");
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config("codex-linked-worktree", executable, "codex", raw.path());
        let mut session = Session::start(
            CodexProvider::new(),
            &cfg,
            launch,
            FenceOutcome::none(),
            None,
        )
        .await
        .unwrap();

        CodexProvider::handshake(&mut session, &ScenarioInput::default())
            .await
            .unwrap();
        let thread_id = start_thread(&mut session, &provider_request).await.unwrap();
        start_turn(&mut session, &provider_request, &thread_id)
            .await
            .unwrap();
        session.wait_for_turn_end().await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdin: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let thread = stdin
            .iter()
            .find(|line| line["method"] == "thread/start")
            .unwrap();
        let expected_thread = json!({
            "cwd": worktree.path().display().to_string(),
            "approvalPolicy": "untrusted",
            "sandbox": "danger-full-access",
            "approvalsReviewer": "user",
            "model": "gpt-5.6-luna",
            "serviceTier": "fast",
        });
        assert_eq!(thread["params"], expected_thread);
        assert_eq!(
            crate::codex::thread_start_params(&provider_request),
            expected_thread
        );
        assert_eq!(
            crate::codex::thread_resume_params(&provider_request, "resume-thread"),
            json!({
                "cwd": worktree.path().display().to_string(),
                "approvalPolicy": "untrusted",
                "sandbox": "danger-full-access",
                "approvalsReviewer": "user",
                "model": "gpt-5.6-luna",
                "serviceTier": "fast",
                "threadId": "resume-thread",
            })
        );
        let turn = stdin
            .iter()
            .find(|line| line["method"] == "turn/start")
            .unwrap();
        let expected_turn = json!({
            "threadId": "th-1",
            "input": [{"type": "text", "text": "scenario:capture-fresh"}],
            "approvalPolicy": "untrusted",
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "summary": "auto",
            "model": "gpt-5.6-luna",
            "effort": "low",
            "serviceTier": "fast",
        });
        assert_eq!(turn["params"], expected_turn);
        assert_eq!(
            crate::codex::turn_start_params(&provider_request, "th-1", "scenario:capture-fresh"),
            expected_turn
        );
    }

    /// Break caught: `resume` falls through to `thread/start` when Codex rejects the requested
    /// thread — silently mislabeling a fresh thread as a resumed one.
    #[tokio::test]
    async fn codex_resume_never_falls_back_to_a_fresh_thread() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let input = ScenarioInput {
            resume_id: Some("resume-fail".into()),
            ..ScenarioInput::default()
        };
        let request = resume_request(&input).unwrap();
        let mut session = start_codex_run_session("resume", executable, raw.path(), request).await;

        let error = resume(&mut session, &input).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("rejected the requested thread resume")
        );
    }

    /// Break caught: `resume` hand-builds the `thread/resume` params instead of calling the
    /// production `crate::codex::thread_resume_params`, or passes the wrong id.
    /// `codex_resume_never_falls_back_to_a_fresh_thread` above only drives the rejection branch
    /// (`resume_thread`'s bail on an error reply) and never inspects what was actually sent; this
    /// covers the success path, the same gap `recorder_codex_run_preserves_production_linked_worktree_parameters`
    /// leaves for `thread/start`/`turn/start` closed by driving the real function rather than
    /// asserting a production helper against itself.
    #[tokio::test]
    async fn resume_sends_the_production_thread_resume_params() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let input = ScenarioInput {
            resume_id: Some("resume-success".into()),
            ..ScenarioInput::default()
        };
        let request = resume_request(&input).unwrap();
        let mut session =
            start_codex_run_session("codex-resume-success", executable, raw.path(), request).await;

        resume(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdin: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let resume_line = stdin
            .iter()
            .find(|line| line["method"] == "thread/resume")
            .expect("a thread/resume line was sent");
        assert_eq!(
            resume_line["params"],
            crate::codex::thread_resume_params(&resume_request(&input).unwrap(), "resume-success")
        );
    }

    /// Break caught: same as `fresh_text_row_is_wired_to_fresh_text_request`, for `approval`.
    #[test]
    fn approval_row_is_wired_to_approval_request() {
        let input = ScenarioInput::default();
        let spec = crate::capture::record::scenarios::scenario("codex", "approval").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/approval must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            approval_request(&input).unwrap()
        );
    }

    /// Break caught: same as `fresh_text_row_is_wired_to_fresh_text_request`, for
    /// `approval-on-request`.
    #[test]
    fn approval_on_request_row_is_wired_to_approval_on_request_request() {
        let input = ScenarioInput {
            approval_target: Some(std::path::PathBuf::from("target-dir")),
            ..ScenarioInput::default()
        };
        let spec =
            crate::capture::record::scenarios::scenario("codex", "approval-on-request").unwrap();
        let ScenarioLaunch::Run(build_request) = spec.launch else {
            panic!("codex/approval-on-request must be a Run scenario");
        };
        assert_eq!(
            build_request(&input).unwrap(),
            approval_on_request_request(&input).unwrap()
        );
    }

    /// Ported from `comet-provider-capture.rs`'s own test module, where it covered
    /// `approval_on_request_prompt` through the crate's public re-export
    /// (`comet_harness::capture::approval_on_request_prompt`) back when the binary built prompts
    /// itself. Task 7's table refactor left that re-export with no production caller, and Task 8
    /// dropped it along with `approval_marker_command`'s two prompt-building siblings — this
    /// scenario module is the function's home now, so the coverage moves here rather than being
    /// lost with the re-export.
    ///
    /// Break caught: a future edit to `approval_marker_command`'s Windows quoting drops the
    /// doubled single-quote escape (`replace('\'', "''")`) or the Unix branch's shell-escape
    /// (`replace('\'', "'\\''")`), letting an apostrophe in the target path break out of the
    /// quoted `-LiteralPath`/argument and inject a second command.
    #[test]
    fn on_request_command_quotes_a_target_with_spaces_and_quotes() {
        let target = std::path::PathBuf::from(if cfg!(windows) {
            r"C:\capture targets\O'Brien"
        } else {
            "/capture targets/O'Brien"
        });
        let prompt = approval_on_request_prompt(&target);
        assert!(prompt.contains("approval-marker.txt"));
        if cfg!(windows) {
            assert!(
                prompt
                    .contains("-LiteralPath 'C:\\capture targets\\O''Brien\\approval-marker.txt'")
            );
            assert!(!prompt.contains("cmd.exe /C"));
        } else {
            assert!(prompt.contains("'/capture targets/O'\\''Brien/approval-marker.txt'"));
        }
    }

    /// Parses every stdin line as JSON and splits it into the `turn/start` request (used to pin
    /// prompt/mode/model/target) and the approval decisions (anything carrying
    /// `result.decision`), in send order. Shared by every test below so the parsing itself can't
    /// drift between them.
    fn turn_start_and_decisions(capture: &crate::capture::RawCapture) -> (Value, Vec<Value>) {
        let stdin: Vec<Value> = channel_payloads(capture, Channel::Stdin)
            .into_iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let turn_start = stdin
            .iter()
            .find(|line| line["method"] == "turn/start")
            .cloned()
            .expect("a turn/start line was sent");
        let decisions = stdin
            .into_iter()
            .filter(|line| {
                line.get("result")
                    .is_some_and(|r| r.get("decision").is_some())
            })
            .collect();
        (turn_start, decisions)
    }

    /// The validator deletion, proven: a fake Codex that raises two
    /// `item/fileChange/requestApproval` requests in a row — neither checked for count, order or
    /// item shape — must have both answered with an `accept` reply carrying that request's own
    /// id. The deleted `CodexApprovalState` and the validators that threaded it would have
    /// bailed the instant anything but the single reviewed file-change request appeared;
    /// `pending_approval` has no such bookkeeping, so it just keeps answering.
    ///
    /// Also pins the two things nothing else in this file's tests catch (`approval_row_is_wired_
    /// to_approval_request` builds its "expected" from the same `approval_request` call, so it
    /// cannot tell a wrong runtime mode from a right one — see this test's own falsification note
    /// in the task report):
    /// - `RuntimeMode::ApprovalRequired` reaching the wire as `"approvalPolicy":"untrusted"` —
    ///   checked against a literal, not by calling `approval_request` again, so a production
    ///   regression to any other mode can't satisfy its own assertion.
    /// - the grant-time cwd recheck passing (not declining) when the cwd never changed, using a
    ///   real `DirectoryIdentity` computed the same way `record::codex_fence`'s pre-spawn check
    ///   would.
    ///
    /// Dispatched in `fake_codex.rs` on a substring of the real `codex_approval_prompt` text —
    /// same rationale as the `steer`/`interrupt` branches matching their scenarios' real prompts.
    #[tokio::test]
    async fn codex_approval_scenario_answers_every_request_it_sees() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_identity =
            crate::capture::safety::validate_ordinary_approval_cwd(cwd.path(), None, false)
                .unwrap();
        let input = ScenarioInput {
            cwd: Some(cwd.path().into()),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let request = approval_request(&input).unwrap();
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config("approval", executable, "codex", raw.path());
        let fence = FenceOutcome {
            approval_cwd_identity: Some(cwd_identity),
            ..FenceOutcome::none()
        };
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, fence, Some(request))
            .await
            .unwrap();
        // Bounded, like `record/scenarios/claude.rs`'s equivalent test: the fixture reads a
        // reply off stdin before emitting its next request, so a driver that stops answering
        // does not error, it blocks forever.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approval(&mut session, &input),
        )
        .await
        .expect(
            "approval must answer every request instead of leaving the fixture blocked on a reply",
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let (turn_start, decisions) = turn_start_and_decisions(&capture);
        let expected_request = approval_request(&input).unwrap();
        assert_eq!(
            turn_start["params"],
            crate::codex::turn_start_params(&expected_request, "th-1", &expected_request.prompt)
        );
        assert_eq!(
            turn_start["params"]["approvalPolicy"], "untrusted",
            "ApprovalRequired must reach the wire as the untrusted policy, or Codex never asks \
             and the capture is worthless: {turn_start:?}"
        );
        assert_eq!(
            decisions
                .iter()
                .map(|reply| reply["id"].clone())
                .collect::<Vec<_>>(),
            vec![json!(0), json!(1)],
            "both approval requests must be answered, in the order they arrived: {decisions:?}"
        );
        assert!(
            decisions
                .iter()
                .all(|reply| reply["result"]["decision"] == "accept"),
            "the grant-time cwd recheck must pass for a cwd that never changed: {decisions:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// The grant-time recheck, proven: when `session.fence.approval_cwd_identity` names a
    /// *different* directory than the one this capture actually runs against — simulating the
    /// cwd having been swapped out from under the fence between pre-spawn validation and this
    /// grant — every approval must be declined, not accepted, and the capture must still run to
    /// completion. This is strictly better than the deleted `codex_run`'s behavior, which
    /// `bail!`ed on exactly this mismatch and discarded the whole recording.
    #[tokio::test]
    async fn codex_approval_declines_a_grant_when_the_cwd_identity_no_longer_matches() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mismatched_identity =
            crate::capture::safety::validate_ordinary_approval_cwd(elsewhere.path(), None, false)
                .unwrap();
        let input = ScenarioInput {
            cwd: Some(cwd.path().into()),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let request = approval_request(&input).unwrap();
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config(
            "codex-approval-cwd-mismatch",
            executable,
            "codex",
            raw.path(),
        );
        let fence = FenceOutcome {
            approval_cwd_identity: Some(mismatched_identity),
            ..FenceOutcome::none()
        };
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, fence, Some(request))
            .await
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approval(&mut session, &input),
        )
        .await
        .unwrap()
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let (_turn_start, decisions) = turn_start_and_decisions(&capture);
        assert_eq!(
            decisions.len(),
            2,
            "both requests must still be answered: {decisions:?}"
        );
        assert!(
            decisions
                .iter()
                .all(|reply| reply["result"]["decision"] == "decline"),
            "a cwd identity mismatch must decline the write, not grant it: {decisions:?}"
        );
        assert_eq!(
            capture.exit_code,
            Some(0),
            "the recording must survive a declined grant, not be discarded"
        );
    }

    /// Same shape as `codex_approval_scenario_answers_every_request_it_sees`, for
    /// `approval_on_request`. The prompt-text substring check pins `input.approval_target`
    /// specifically: `input.cwd` (defaulted to the system temp dir here) and `approval_target`
    /// (an isolated tempdir) are never equal, so a scenario that read the wrong field would send
    /// a prompt without the target's own path in it.
    #[tokio::test]
    async fn codex_on_request_approval_scenario_answers_every_request_it_sees() {
        let raw = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let target_identity =
            crate::capture::safety::require_empty_approval_target(target.path(), None).unwrap();
        let input = ScenarioInput {
            approval_target: Some(target.path().into()),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let request = approval_on_request_request(&input).unwrap();
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config("approval-on-request", executable, "codex", raw.path());
        let fence = FenceOutcome {
            approval_target: Some(target.path().into()),
            approval_target_identity: Some(target_identity),
            ..FenceOutcome::none()
        };
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, fence, Some(request))
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approval_on_request(&mut session, &input),
        )
        .await
        .expect("approval_on_request must answer every request instead of leaving the fixture blocked on a reply")
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let (turn_start, decisions) = turn_start_and_decisions(&capture);
        assert_eq!(
            turn_start["params"],
            crate::codex::turn_start_params(
                &approval_on_request_request(&input).unwrap(),
                "th-1",
                &approval_on_request_request(&input).unwrap().prompt
            )
        );
        assert!(
            turn_start["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&target.path().display().to_string()),
            "the sent prompt must carry input.approval_target's own path, not input.cwd's: {turn_start:?}"
        );
        assert_eq!(
            decisions
                .iter()
                .map(|reply| reply["id"].clone())
                .collect::<Vec<_>>(),
            vec![json!(0), json!(1)],
            "both approval requests must be answered, in the order they arrived: {decisions:?}"
        );
        assert!(
            decisions
                .iter()
                .all(|reply| reply["result"]["decision"] == "accept"),
            "the grant-time target recheck must pass for a target that stayed empty: {decisions:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Same shape as `codex_approval_declines_a_grant_when_the_cwd_identity_no_longer_matches`,
    /// for `approval_on_request`'s `require_empty_approval_target` recheck.
    #[tokio::test]
    async fn codex_on_request_approval_declines_a_grant_when_the_target_identity_no_longer_matches()
    {
        let raw = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mismatched_identity =
            crate::capture::safety::require_empty_approval_target(elsewhere.path(), None).unwrap();
        let input = ScenarioInput {
            approval_target: Some(target.path().into()),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let request = approval_on_request_request(&input).unwrap();
        let launch = crate::codex::run_launch(&executable, &request);
        let cfg = config(
            "codex-approval-on-request-target-mismatch",
            executable,
            "codex",
            raw.path(),
        );
        let fence = FenceOutcome {
            approval_target: Some(target.path().into()),
            approval_target_identity: Some(mismatched_identity),
            ..FenceOutcome::none()
        };
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, fence, Some(request))
            .await
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approval_on_request(&mut session, &input),
        )
        .await
        .unwrap()
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let (_turn_start, decisions) = turn_start_and_decisions(&capture);
        assert_eq!(
            decisions.len(),
            2,
            "both requests must still be answered: {decisions:?}"
        );
        assert!(
            decisions
                .iter()
                .all(|reply| reply["result"]["decision"] == "decline"),
            "a target identity mismatch must decline the write, not grant it: {decisions:?}"
        );
        assert_eq!(
            capture.exit_code,
            Some(0),
            "the recording must survive a declined grant, not be discarded"
        );
    }

    /// The Codex counterpart of `record/scenarios/claude.rs`'s
    /// `every_claude_run_rows_declared_mode_matches_its_request_builder` — see that test's own
    /// doc comment for why this needs its own check independent of
    /// `comet-provider-capture.rs::scenario_names_own_their_runtime_modes`, which reads only
    /// `spec.runtime_mode`. Codex `approval` is the sharpest case here: `codex_fence` selects the
    /// trusted-PowerShell/cwd-identity fence off `spec.runtime_mode ==
    /// Some(RuntimeMode::ApprovalRequired)`, but the wire mode Codex actually receives comes from
    /// `approval_request`'s own `cheap_codex_request` call — a drift here would leave the fence
    /// running for a turn that (if the mode drifted to `AutoAcceptEdits`) Codex would never ask an
    /// approval for at all, exactly the failure `approval_on_request`'s own hardcoded
    /// `AutoAcceptEdits` documents on the table row.
    #[test]
    fn every_codex_run_rows_declared_mode_matches_its_request_builder() {
        let plain = ScenarioInput::default();
        let with_resume = ScenarioInput {
            resume_id: Some("resume-abc".into()),
            ..ScenarioInput::default()
        };
        let with_target = ScenarioInput {
            approval_target: Some(PathBuf::from("target-dir")),
            ..ScenarioInput::default()
        };
        let cases = [
            (
                "fresh-text",
                fresh_text_request(&plain).unwrap().runtime_mode,
            ),
            ("approval", approval_request(&plain).unwrap().runtime_mode),
            (
                "approval-on-request",
                approval_on_request_request(&with_target)
                    .unwrap()
                    .runtime_mode,
            ),
            ("resume", resume_request(&with_resume).unwrap().runtime_mode),
            ("steer", steer_request(&plain).unwrap().runtime_mode),
            (
                "interruption",
                interruption_request(&plain).unwrap().runtime_mode,
            ),
        ];
        for (name, mode) in cases {
            let spec = crate::capture::record::scenarios::scenario("codex", name)
                .unwrap_or_else(|| panic!("missing codex/{name}"));
            assert_eq!(
                spec.runtime_mode,
                Some(mode),
                "codex/{name}: table says {:?}, request builder says {mode:?}",
                spec.runtime_mode
            );
        }

        // Coverage, not just correctness — same reasoning as
        // `record/scenarios/claude.rs`'s `every_claude_run_rows_declared_mode_matches_its_request_builder`:
        // `cases` above must name every codex row that declares a runtime_mode, or a 13th run row
        // escapes both this test's loop (vacuously) and
        // `comet-provider-capture.rs::scenario_names_own_their_runtime_modes`.
        let covered: std::collections::BTreeSet<&str> =
            cases.iter().map(|(name, _)| *name).collect();
        let expected: std::collections::BTreeSet<&str> =
            crate::capture::record::scenarios::SCENARIOS
                .iter()
                .filter(|spec| {
                    spec.provider == crate::capture::Provider::Codex && spec.runtime_mode.is_some()
                })
                .map(|spec| spec.name)
                .collect();
        assert_eq!(
            covered, expected,
            "every codex row with Some(runtime_mode) must have a case in this test's list"
        );
    }
}
