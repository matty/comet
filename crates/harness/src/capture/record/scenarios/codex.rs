use std::path::Path;

use anyhow::{anyhow, bail};
use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};
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

// Below this point: `fresh-text`, `resume`, `steer` and `interruption`. None
// of the four is reachable from production code yet — the SCENARIOS table
// only gains their rows in Task 7 — so every item down to the test module
// carries `#[allow(dead_code)]`, exercised meanwhile only by this file's own
// tests. Same shape as `record/scenarios/claude.rs`'s equivalent section.

/// The model every non-discovery Codex scenario runs against: cheap and
/// fast, mirroring `record/scenarios/claude.rs`'s `CHEAP_MODEL`. Ported from
/// `comet-provider-capture.rs`'s `cheap_codex_request` — decision "the
/// scenario owns its prompt" moves the choice out of the binary along with
/// the prompt text itself.
#[allow(dead_code)]
const CHEAP_MODEL: &str = "gpt-5.6-luna";

/// The `RunRequest` every non-discovery Codex scenario in this file starts
/// from: the cheap model, low reasoning, and the caller's cwd (or a neutral
/// temp directory). Always run through `crate::codex::normalize_run_request`
/// here — exactly where `recording.rs`'s `RecordingSession::start` used to
/// apply it before anything else touched the request — so every caller
/// (launch builder and scenario body alike) sees the same normalized value
/// and the linked-worktree sandbox escalation (D13) can never disagree
/// between the two.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

// Called once by `fresh_text_launch`, once by `fresh_text` — see
// `record/scenarios/claude.rs`'s `fresh_text_request` doc comment for why
// this stays a pure function of `input`: the two calls cannot diverge only
// because nothing here reads anything but `input`.
#[allow(dead_code)]
fn fresh_text_request(input: &ScenarioInput) -> RunRequest {
    cheap_codex_request(
        "Reply with the single word capture.",
        input,
        RuntimeMode::AutoAcceptEdits,
    )
}

/// SPAWN for `fresh-text`: an ordinary run launch, built from the same
/// request `fresh_text` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn fresh_text_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(
        executable,
        &fresh_text_request(input),
    ))
}

/// A plain text turn: start a fresh thread, start the turn, wait for
/// whichever terminal frame Codex sends. No bail on the terminal frame's
/// type — see `recording.rs`'s `codex_run` doc comment on why that check is
/// deleted, not ported.
#[allow(dead_code)]
pub(in crate::capture::record) async fn fresh_text(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = fresh_text_request(input);
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    session.wait_for_turn_end().await
}

// Called once by `resume_launch`, once by `resume` — see the note on
// `fresh_text_request` above.
#[allow(dead_code)]
fn resume_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
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

/// SPAWN for `resume`: the same `app-server` launch as every other Codex
/// scenario — unlike Claude, a Codex resume never reaches the CLI as a
/// launch argument; the thread id lives entirely on the wire (`thread/resume`,
/// built by `resume_thread`). See `crate::codex::run_launch`, which never
/// reads `request.resume`.
#[allow(dead_code)]
pub(in crate::capture::record) fn resume_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(
        executable,
        &resume_request(input)?,
    ))
}

#[allow(dead_code)]
pub(in crate::capture::record) async fn resume(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = resume_request(input)?;
    let thread_id = resume_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    session.wait_for_turn_end().await
}

// Called once by `steer_launch`, once by `steer` — see the note on
// `fresh_text_request` above.
#[allow(dead_code)]
fn steer_request(input: &ScenarioInput) -> RunRequest {
    cheap_codex_request(
        "Begin a short response, then accept the follow-up instruction.",
        input,
        RuntimeMode::AutoAcceptEdits,
    )
}

/// SPAWN for `steer`: an ordinary run launch, built from the same request
/// `steer` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn steer_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(executable, &steer_request(input)))
}

/// The exact text `steer` sends as its `turn/steer` message. A named
/// constant — like `record/scenarios/claude.rs`'s `CLAUDE_APPROVAL_COMMAND`
/// — so the driving code and its own test can't drift into two separate
/// string literals.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(in crate::capture::record) async fn steer(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = steer_request(input);
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

// Called once by `interruption_launch`, once by `interruption` — see the
// note on `fresh_text_request` above.
#[allow(dead_code)]
fn interruption_request(input: &ScenarioInput) -> RunRequest {
    cheap_codex_request(
        "Count upward slowly and keep working until interrupted.",
        input,
        RuntimeMode::AutoAcceptEdits,
    )
}

/// SPAWN for `interruption`: an ordinary run launch, built from the same
/// request `interruption` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn interruption_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(
        executable,
        &interruption_request(input),
    ))
}

/// Same shape as [`steer`]: wait for the turn to be genuinely under way, then
/// send the production `turn/interrupt` params and wait for the turn to end.
/// No bail on the terminal frame being `turn/aborted` specifically —
/// `recording.rs`'s deleted `codex_run` required exactly that, which is the
/// same removed validator class `steer`'s doc comment explains.
#[allow(dead_code)]
pub(in crate::capture::record) async fn interruption(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = interruption_request(input);
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

// Called once by `approval_launch`, once by `approval` — see the note on
// `fresh_text_request` above.
#[allow(dead_code)]
fn approval_request(input: &ScenarioInput) -> RunRequest {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    cheap_codex_request(
        &crate::capture::approval::codex_approval_prompt(&cwd),
        input,
        RuntimeMode::ApprovalRequired,
    )
}

/// SPAWN for `approval`: an ordinary run launch, built from the same request
/// `approval` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn approval_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(
        executable,
        &approval_request(input),
    ))
}

// Called once by `approval_on_request_launch`, once by `approval_on_request`
// — see the note on `fresh_text_request` above.
#[allow(dead_code)]
fn approval_on_request_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let target = input
        .approval_target
        .clone()
        .ok_or_else(|| anyhow!("The approval-on-request scenario needs an --approval-target."))?;
    Ok(cheap_codex_request(
        &crate::capture::approval::approval_on_request_prompt(&target),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// SPAWN for `approval-on-request`: an ordinary run launch, built from the
/// same request `approval_on_request` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn approval_on_request_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::codex::run_launch(
        executable,
        &approval_on_request_request(input)?,
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
#[allow(dead_code)]
fn pending_approval(frame: &Value) -> Option<Value> {
    let method = frame["method"].as_str()?;
    if !method.ends_with("/requestApproval") {
        return None;
    }
    let id = frame.get("id")?;
    (!id.is_null()).then(|| id.clone())
}

/// The `accept` reply for one approval request.
///
/// Production builds this envelope in `codex::rpc::RpcClient::respond`
/// (`crates/harness/src/codex/rpc.rs:105-108`:
/// `json!({"jsonrpc":"2.0","id":id,"result":result})`), and maps a real
/// "allow" decision to the literal `"accept"` in
/// `codex::approval::decision_literal`
/// (`crates/harness/src/codex/approval.rs:256-265`:
/// `ApprovalDecision::Allow | ApprovalDecision::AllowForSession => "accept"`).
/// Neither is reachable from here: `codex/mod.rs` declares `mod approval;`
/// and `mod rpc;` with no visibility keyword, so both stay private to
/// `crate::codex` and its descendants — unlike `claude::wire`'s line
/// builders, which `record/scenarios/claude.rs`'s `approval` calls directly
/// (`control_response_line`/`allow_response` are `pub`). There is no
/// crate-external production helper to route this through, so the envelope
/// is hand-written here, pinned to both file:line above so a future reader
/// can check it hasn't drifted.
#[allow(dead_code)]
fn accept_response(id: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": {"decision": "accept"}}).to_string()
}

/// Pump frames, answering every approval request `pending_approval`
/// recognizes, until the provider's own terminal turn notification ends the
/// loop. Shared by `approval` and `approval_on_request`, which differ only in
/// which request they start (see each function's own doc comment).
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
#[allow(dead_code)]
async fn answer_every_approval(session: &mut Session<CodexProvider>) -> anyhow::Result<()> {
    loop {
        let Some(frame) = session.next_frame().await? else {
            return protocol_stopped("Codex", "an approval request or a turn end");
        };
        if CodexProvider::turn_complete(&frame) {
            return Ok(());
        }
        if let Some(id) = pending_approval(&frame) {
            session.send(&accept_response(id)).await?;
        }
    }
}

/// Start a fresh thread and turn under `RuntimeMode::ApprovalRequired`, then
/// answer every `item/fileChange/requestApproval` request the model makes
/// until the turn ends. Ported from `recording.rs`'s deleted `codex_run`
/// `CodexRunScript::Approval` arm, minus every validator listed on
/// `answer_every_approval`'s doc comment.
#[allow(dead_code)]
pub(in crate::capture::record) async fn approval(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = approval_request(input);
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    answer_every_approval(session).await
}

/// Same shape as [`approval`], differing only in which request it starts
/// (`approval_on_request_request`, built from `input.approval_target` rather
/// than `input.cwd`) and therefore which approval method Codex ends up
/// asking about (`item/commandExecution/requestApproval` rather than
/// `item/fileChange/requestApproval`) — `answer_every_approval` does not need
/// to know which one it is answering. Ported from `recording.rs`'s deleted
/// `codex_run` `CodexRunScript::ApprovalOnRequest` arm, minus every validator
/// listed on `answer_every_approval`'s doc comment, including the mid-loop
/// `require_empty_approval_target` recheck: decision #6 in the stage plan
/// scopes the surviving pre-spawn fence to checks that "run before spawn,"
/// and a recheck from inside this frame loop runs after it.
#[allow(dead_code)]
pub(in crate::capture::record) async fn approval_on_request(
    session: &mut Session<CodexProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    CodexProvider::handshake(session, input).await?;
    let request = approval_on_request_request(input)?;
    let thread_id = start_thread(session, &request).await?;
    start_turn(session, &request, &thread_id).await?;
    answer_every_approval(session).await
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::capture::record::session::FenceOutcome;
    use crate::capture::test_support::{
        absolute_program, channel_payloads, config, contract_request, fixture_path,
    };
    use crate::capture::types::{
        CaptureOperation, Channel, CodexCaptureOperation, CodexRunScript, CommandSnapshot,
    };
    use crate::launch::StdioMode;

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

    /// Break caught: `fresh_text_launch` stops calling the production `crate::codex::run_launch`
    /// and hand-builds (or hand-edits) a `LaunchDescriptor` instead.
    #[test]
    fn fresh_text_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput::default();

        let launch = fresh_text_launch(&input, &exe).unwrap();
        let expected = crate::codex::run_launch(&exe, &fresh_text_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `resume`.
    #[test]
    fn resume_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput {
            resume_id: Some("resume-abc".into()),
            ..ScenarioInput::default()
        };

        let launch = resume_launch(&input, &exe).unwrap();
        let expected = crate::codex::run_launch(&exe, &resume_request(&input).unwrap());

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `steer`.
    #[test]
    fn steer_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput::default();

        let launch = steer_launch(&input, &exe).unwrap();
        let expected = crate::codex::run_launch(&exe, &steer_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for
    /// `interruption`.
    #[test]
    fn interruption_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput::default();

        let launch = interruption_launch(&input, &exe).unwrap();
        let expected = crate::codex::run_launch(&exe, &interruption_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
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
        let launch = fresh_text_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-fresh-text",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: fresh_text_request(&input),
                script: CodexRunScript::FreshText,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

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
    /// this task — and asserts the exact `turn/steer`/`turn/interrupt` line each one puts on the
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
        let launch = steer_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-steer",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: steer_request(&input),
                script: CodexRunScript::Steer,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();
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
        let launch = interruption_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-interruption",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: interruption_request(&input),
                script: CodexRunScript::Interruption,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();
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
        let cfg = config(
            "codex-linked-worktree",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: request.clone(),
                script: CodexRunScript::FreshText,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
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
        let launch = resume_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-resume",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: resume_request(&input).unwrap(),
                script: CodexRunScript::Resume,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

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
        let launch = resume_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-resume-success",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: resume_request(&input).unwrap(),
                script: CodexRunScript::Resume,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

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

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `approval`.
    #[test]
    fn approval_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput::default();

        let launch = approval_launch(&input, &exe).unwrap();
        let expected = crate::codex::run_launch(&exe, &approval_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for
    /// `approval_on_request`.
    #[test]
    fn approval_on_request_launch_uses_the_production_run_launch() {
        let exe = absolute_program("codex");
        let input = ScenarioInput {
            approval_target: Some(std::path::PathBuf::from("target-dir")),
            ..ScenarioInput::default()
        };

        let launch = approval_on_request_launch(&input, &exe).unwrap();
        let expected =
            crate::codex::run_launch(&exe, &approval_on_request_request(&input).unwrap());

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// The validator deletion, proven: a fake Codex that raises two
    /// `.../requestApproval` requests in a row for each scenario — neither checked for count,
    /// order or item shape — must have both answered with an `accept` reply carrying that
    /// request's own id. The deleted `CodexApprovalState`/`CodexOnRequestState` and the
    /// validators that threaded them would have bailed the instant a second, unaccounted-for
    /// request id showed up (or, for `approval`, the instant anything but the single reviewed
    /// file-change request appeared at all); `pending_approval` has no such bookkeeping, so it
    /// just keeps answering.
    ///
    /// Parameterized over both scenarios: `approval` against `fake-codex`'s
    /// `capture_approval_two_requests` (dispatched on a substring of the real
    /// `codex_approval_prompt` text), `approval_on_request` against
    /// `capture_approval_on_request_two_requests` (dispatched on a substring of the real
    /// `approval_on_request_prompt` text) — both additive branches, same rationale as
    /// `fake_codex.rs`'s `steer`/`interrupt` branches matching `steer_request`/
    /// `interruption_request`'s real production text.
    #[tokio::test]
    async fn codex_approval_scenarios_answer_every_request_they_see() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let input = ScenarioInput::default();
        let launch = approval_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-approval",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: approval_request(&input),
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
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
        let decisions: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter(|line| line.contains("\"decision\""))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
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
            "every reply must accept: {decisions:?}"
        );
        assert_eq!(capture.exit_code, Some(0));

        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-codex");
        let input = ScenarioInput {
            approval_target: Some(std::path::PathBuf::from("target-dir")),
            ..ScenarioInput::default()
        };
        let launch = approval_on_request_launch(&input, &executable).unwrap();
        let cfg = config(
            "codex-approval-on-request",
            executable,
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: approval_on_request_request(&input).unwrap(),
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        let mut session = Session::start(CodexProvider::new(), &cfg, launch, FenceOutcome::none())
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
        let decisions: Vec<Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .filter(|line| line.contains("\"decision\""))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
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
            "every reply must accept: {decisions:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }
}
