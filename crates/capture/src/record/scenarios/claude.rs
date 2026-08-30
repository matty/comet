use std::path::{Path, PathBuf};

use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};
use serde_json::{Value, json};

use crate::record::provider::CaptureProvider;
use crate::record::providers::claude::ClaudeProvider;
use crate::record::scenarios::ScenarioInput;
use crate::record::session::{Session, protocol_stopped};
use crate::safety::{APPROVAL_MARKER_CONTENT, APPROVAL_MARKER_NAME};
use comet_harness::launch::LaunchDescriptor;

/// SPAWN for `model-discovery` and its `-neutral-cwd`/`-project-cwd`
/// aliases: the bare initialize launch.
pub(in crate::record) fn model_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(comet_harness::claude::discovery::model_discovery_launch(
        executable, &cwd,
    ))
}

/// SPAWN for `command-discovery`: the non-bare initialize launch — the
/// entire reason this scenario needs its own launch fn rather than reusing
/// `model_discovery_launch`. Recording command discovery with `--bare`
/// would be silently wrong, which is exactly the drift a first draft of
/// this seam let happen when `launch` lived on the provider trait instead
/// of the scenario row.
pub(in crate::record) fn command_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(comet_harness::claude::commands::command_discovery_launch(
        executable, &cwd,
    ))
}

/// Model discovery's entire drive IS the handshake — the body calls it
/// itself (per the amendment "the scenario body calls the handshake; the
/// recorder does not": `record_generic` no longer calls `P::handshake` for
/// any scenario). Kept as its own named function, rather than a bare call
/// inlined elsewhere, because it is a named row in [`super::SCENARIOS`].
pub(in crate::record) async fn model_discovery(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    ClaudeProvider::handshake(session, input).await
}

/// Command discovery's driving is identical to model discovery's — the
/// difference between the two scenarios is entirely in which launch builder
/// `record()` selects (`command_discovery_launch` vs `model_discovery_launch`
/// picks `--bare` or not), never in what happens after spawn.
pub(in crate::record) async fn command_discovery(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    ClaudeProvider::handshake(session, input).await
}

// Below this point: `fresh-text`, `resume`, `attachment`, `checklist`,
// `checklist-resume` and `approval`. Every one of these is a registered row
// in `super::SCENARIOS` and reachable through `record()`.

/// The model every non-discovery Claude scenario runs against: cheap and
/// fast, because a capture's whole point is the wire *shape*, not which
/// model answered. Ported from `comet-provider-capture.rs`'s
/// `cheap_claude_request` — decision "the scenario owns its prompt" moves
/// the choice out of the binary along with the prompt text itself.
const CHEAP_MODEL: &str = "claude-haiku-4-5-20251001";

/// The `RunRequest` every non-discovery Claude scenario in this file starts
/// from: the cheap model, low reasoning, and the caller's cwd (or a neutral
/// temp directory, exactly like the discovery launches above). Each
/// scenario's own function fills in its fixed prompt and whatever else it
/// needs (`resume`, `attachments`).
fn cheap_claude_request(prompt: &str, input: &ScenarioInput, mode: RuntimeMode) -> RunRequest {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    RunRequest {
        prompt: prompt.into(),
        model: Some(CHEAP_MODEL.into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(mode)
    }
}

/// The wire line for a scenario's first turn: the prompt, with any
/// attachments in `request.attachments` inlined as image blocks by the
/// production image helper. Ported, unchanged in substance, from
/// `recording.rs`'s `claude_user_line` — `script: ClaudeRunScript` is
/// replaced by `require_image`, the one distinction that helper actually
/// made: `attachment` is the only scenario whose whole point is recording an
/// inlined image, so it is the only one that fails loudly when the load
/// produced none.
async fn claude_user_line(request: &RunRequest, require_image: bool) -> anyhow::Result<String> {
    let images = comet_harness::claude::load_image_blocks(&request.attachments).await;
    if require_image && images.is_empty() {
        anyhow::bail!(
            "The selected attachment could not be inlined. Use a supported image under 5 MiB and retry."
        );
    }
    Ok(comet_harness::claude::wire::user_message_line_with_images(
        &request.prompt,
        &images,
    ))
}

/// The `RunRequest` `record.rs`'s `derive_launch` calls exactly once per
/// `fresh-text` recording: it builds `fresh-text`'s launch from this value,
/// then hands the SAME value to `fresh_text` below (via `Session::request`),
/// so the recorded argv and the recorded wire line can never describe two
/// different requests — see `ScenarioLaunch`'s own doc comment for the hazard
/// this closes.
pub(in crate::record) fn fresh_text_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    Ok(cheap_claude_request(
        "Reply with the single word capture.",
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// A plain text turn: write the user line through the production helper,
/// then wait for the terminal result. No handshake — a Claude run never
/// sends one; see `provider.rs`'s doc comment. Reads the request `record.rs`
/// already built for the launch (`Session::request`) rather than rebuilding
/// it — see `fresh_text_request`'s own doc comment.
pub(in crate::record) async fn fresh_text(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("fresh-text is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// The id reaches the CLI as `--resume=<id>` on the launch
/// (`comet_harness::claude::run_launch` reads `request.resume`), never as part of the
/// wire line `resume`'s body sends — same one-call-per-recording contract as
/// `fresh_text_request` above.
pub(in crate::record) fn resume_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let resume_id = input
        .resume_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("The resume scenario needs a --resume-id."))?;
    let mut request = cheap_claude_request(
        "Reply with the single word resumed.",
        input,
        RuntimeMode::AutoAcceptEdits,
    );
    request.resume = Some(resume_id);
    Ok(request)
}

pub(in crate::record) async fn resume(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("resume is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn attachment_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let attachment = input
        .attachment
        .clone()
        .ok_or_else(|| anyhow::anyhow!("The attachment scenario needs an image path."))?;
    let mut request = cheap_claude_request(
        "Describe the attached image in one short sentence.",
        input,
        RuntimeMode::AutoAcceptEdits,
    );
    request.attachments.push(attachment.display().to_string());
    Ok(request)
}

/// The one scenario whose wire line must carry an inlined image ahead of the
/// text — `require_image: true` is what makes `claude_user_line` fail loudly
/// instead of silently recording a text-only capture when the attachment
/// could not be inlined.
pub(in crate::record) async fn attachment(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("attachment is a Run scenario and always carries a request");
    let line = claude_user_line(&request, true).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// Same one-call-per-recording contract as `fresh_text_request` above. The prompt is
/// deliberately identical to `fresh_text_request`'s — `auto`/`full-access` exist to record what
/// mode configuration reaches the wire, not to exercise interesting agent behaviour, and holding
/// the prompt fixed makes the mode the only variable a reader diffing this scenario against
/// `fresh-text`/`full-access` needs to account for.
pub(in crate::record) fn auto_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    Ok(cheap_claude_request(
        "Reply with the single word capture.",
        input,
        RuntimeMode::Auto,
    ))
}

/// Same shape as `fresh_text` above: a plain text turn, no approval handling needed — the trivial
/// prompt never triggers a tool call, so it does not matter that `Auto` would otherwise still let
/// Claude self-review some calls.
pub(in crate::record) async fn auto(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("auto is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// Same one-call-per-recording contract as `fresh_text_request` above, and the same "identical
/// prompt" reasoning as `auto_request`.
pub(in crate::record) fn full_access_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    Ok(cheap_claude_request(
        "Reply with the single word capture.",
        input,
        RuntimeMode::FullAccess,
    ))
}

/// Same shape as `fresh_text`/`auto` above. `FullAccess` disables the sandbox and skips
/// permissions entirely (`bypassPermissions` + `--dangerously-skip-permissions`), so there is no
/// `can_use_tool` round trip to answer even if the trivial prompt did call a tool — this scenario
/// exists to record that the mode reaches the wire, not to exercise what an unsandboxed tool call
/// looks like.
pub(in crate::record) async fn full_access(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("full-access is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// Prompt for `checklist`: create two tasks, then drive the first through both transitions.
///
/// Opens with `ToolSearch` because the task tools are *deferred* on at least one machine —
/// without it first, on an installation that doesn't list them eagerly, the run produces no
/// checklist at all. On one that does, the search is just a harmless extra frame.
fn claude_checklist_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskCreate,TaskUpdate","max_results":5}. "#,
        r#"Then use TaskCreate exactly twice, first with input {"subject":"Alpha step","description":"The first step"} "#,
        r#"and then with input {"subject":"Beta step","description":"The second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"1","status":"in_progress","activeForm":"Working the first step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"1","status":"completed"}. "#,
        r#"Do nothing else, and reply with the single word capture."#,
    )
    .to_owned()
}

/// Prompt for `checklist-resume`: continue the SAME list from a second
/// process. Task 2 was created by the first process, so a run driven by this
/// prompt updates an id it has never seen — the case the whole scenario
/// exists to record. It deliberately does not create anything: a `TaskCreate`
/// here would give the resumed process a subject of its own and destroy the
/// evidence.
fn claude_checklist_resume_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskUpdate","max_results":5}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"in_progress","activeForm":"Working the second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"completed"}. "#,
        r#"Do not create any task. Do nothing else, and reply with the single word resumed."#,
    )
    .to_owned()
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn checklist_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    Ok(cheap_claude_request(
        &claude_checklist_prompt(),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// Send the checklist prompt and wait for the turn to end. Nothing here inspects what the model
/// did with the task tools — no created/updated task-id accounting, no bail on a mutation count
/// the prompt asked for but the model didn't produce (that accounting lived in `recording.rs`'s
/// `claude_run`; deleted here, closing `docs/debt/` D61).
///
/// Per design §3.2: a pre-spawn guard protects the machine; a frame check that aborts protects
/// only a scenario's tidiness, by destroying evidence already paid for in tokens. A model that
/// ignores this prompt and creates no task doesn't produce a failed capture — it produces a
/// recording of the CLI's real behavior under this prompt, which is evidence in itself.
pub(in crate::record) async fn checklist(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("checklist is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// The id reaches the CLI as `--resume=<id>` on the launch, never as part of
/// the wire line the body sends — same split as `resume_request` above, and
/// the same one-call-per-recording contract as `fresh_text_request`.
pub(in crate::record) fn checklist_resume_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let resume_id = input
        .resume_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("The checklist-resume scenario needs a --resume-id."))?;
    let mut request = cheap_claude_request(
        &claude_checklist_resume_prompt(),
        input,
        RuntimeMode::AutoAcceptEdits,
    );
    request.resume = Some(resume_id);
    Ok(request)
}

/// No session-identity check against `input.resume_id` — the abort-on-mismatch
/// class this stage's design removes (§3.2), same as `resume` above. The old
/// `recording.rs::claude_run` checked `value["session_id"] == request.resume`
/// before returning; a Claude bug that returned the wrong session id would
/// still be worth recording, not a reason to throw the capture away.
pub(in crate::record) async fn checklist_resume(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("checklist-resume is a Run scenario and always carries a request");
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// The exact Bash command the `approval` prompt below asks Claude to run
/// once. Read by two things that must not drift apart: `claude_approval_prompt`
/// (the instruction text) and `claude_marker_grant` below (the grant-time
/// check that a Bash request is the one thing this scenario expects to run).
const CLAUDE_APPROVAL_COMMAND: &str = "printf capture";

/// Prompt for `approval`: ask for one Bash approval, then one Write approval, so the capture
/// records a real `can_use_tool` round trip for each.
///
/// `APPROVAL_MARKER_NAME`/`APPROVAL_MARKER_CONTENT` stay defined in `crate::safety` rather
/// than moving here: `validate_ordinary_approval_cwd`'s marker-absence check and Codex's
/// `codex_approval_prompt` both read the same constants for their own marker paths — only each
/// provider's prompt-building function moved, not the shared name/content both prompts and the
/// fence agree on.
fn claude_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    format!(
        "Use Bash exactly once with input {{\"command\":{}}}. Wait for it to finish successfully. Then use Write exactly once with input {{\"file_path\":{},\"content\":{}}}.",
        serde_json::to_string(CLAUDE_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
        serde_json::to_string(APPROVAL_MARKER_CONTENT).expect("static content serializes"),
    )
}

/// The file `edit` seeds and asks Claude to change, and the two halves of the
/// change.
///
/// **Every value is fixed and boring on purpose.** The point of the scenario is
/// the SHAPE of an `Edit` tool_use frame — `file_path`, `old_string`,
/// `new_string` — and a prompt that leaves the model any latitude spends a
/// turn's tokens on a capture that may not contain the frame at all.
const EDIT_TARGET_NAME: &str = "capture-edit-target.txt";
const EDIT_ORIGINAL: &str = "one
before
three
";
const EDIT_OLD_STRING: &str = "before";
const EDIT_NEW_STRING: &str = "after";

/// The prompt for [`edit`], naming the tools and their exact inputs the same way
/// `claude_approval_prompt` does.
///
/// **The Read is not padding — Claude Code refuses to edit a file it has not
/// read.** Measured, not assumed: the first recording of this scenario
/// (2026-08-30, 2.1.251) said "do not read the file first", and the CLI's own
/// Edit tool answered that the file had to be read before writing to it. The
/// turn still produced a real `Edit` tool_use frame — which is what the corpus
/// is for — but its result was that error and the file was never changed, so
/// nothing downstream of a SUCCESSFUL edit (the result payload, the diff a card
/// renders) was recorded. Asking for the Read costs one extra tool call and
/// buys the whole path.
fn claude_edit_prompt(cwd: &Path) -> String {
    let target = cwd.join(EDIT_TARGET_NAME);
    let path = serde_json::to_string(&target.display().to_string()).expect("path serializes");
    format!(
        "Use Read once with input {{\"file_path\":{path}}}, then use Edit exactly once with input {{\"file_path\":{path},\"old_string\":{},\"new_string\":{}}}. Use no other tool.",
        serde_json::to_string(EDIT_OLD_STRING).expect("static string serializes"),
        serde_json::to_string(EDIT_NEW_STRING).expect("static string serializes"),
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn edit_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_edit_prompt(&cwd),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// A turn that edits an existing file, for the one tool whose decode has never
/// met a real payload.
///
/// **Why this scenario exists at all**: `claude_tool_coverage.rs` measures that
/// `Edit` appears nowhere in the promoted corpus, at any path, while
/// `ToolCall::EditFile` reads `file_path`/`old_string`/`new_string` from it —
/// and two open rows (D17, D18) reason about Edit's approval card from typings
/// because there is no frame to read. One capture settles all three.
///
/// **The target file is written HERE, not in the request builder.** A request
/// builder that touched the filesystem would run its side effect during
/// `--help`-shaped dry runs and argument validation, where nothing has agreed
/// to spend anything or write anywhere. The body runs only once a session is
/// genuinely spawned, which is also the first moment the file needs to exist.
///
/// `AutoAcceptEdits` rather than `ApprovalRequired`: the frame this scenario is
/// for is the `tool_use`, and routing it through an approval round-trip would
/// capture the approval surface `approval` already covers while adding a way
/// for the turn to end without ever emitting the edit.
pub(in crate::record) async fn edit(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("edit is a Run scenario and always carries a request");
    let target = PathBuf::from(&request.cwd).join(EDIT_TARGET_NAME);
    std::fs::write(&target, EDIT_ORIGINAL).map_err(|error| {
        anyhow::anyhow!(
            "edit target {} could not be seeded: {error}",
            target.display()
        )
    })?;

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// D132, live: an `Edit` whose `old_string` is empty and whose `new_string` is non-empty, on a
/// path that has never existed.
///
/// **Every value is fixed and boring on purpose**, same rationale as [`EDIT_TARGET_NAME`] above.
/// This is the question D132's own row names: older Claude Code documented an `Edit` with an
/// empty `old_string` as CREATING the file, and `claude/2.1.251/edit` — an ordinary edit on an
/// existing, already-read file — cannot speak to it at all. This scenario targets a path nothing
/// has ever written, so whatever the CLI does with it is the missing evidence.
const EDIT_CREATE_TARGET_NAME: &str = "capture-edit-create-target.txt";
const EDIT_CREATE_NEW_STRING: &str = "created by edit\n";

/// The prompt for [`edit_create`]. Deliberately no `Read` step: the whole point of the scenario
/// is a path nothing has read because nothing has ever written it, and asking for one would
/// either fail for an unrelated reason (no file to read) or, if the CLI tolerates it, blur the
/// case under test with `edit`'s already-answered one.
fn claude_edit_create_prompt(cwd: &Path) -> String {
    let target = cwd.join(EDIT_CREATE_TARGET_NAME);
    let path = serde_json::to_string(&target.display().to_string()).expect("path serializes");
    format!(
        "The file {path} does not exist yet. Use Edit exactly once with input {{\"file_path\":{path},\"old_string\":\"\",\"new_string\":{}}}. Do not use Read, Write, or any other tool.",
        serde_json::to_string(EDIT_CREATE_NEW_STRING).expect("static string serializes"),
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn edit_create_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_edit_create_prompt(&cwd),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// D132's whole question, live: does Claude's `Edit` tool create a file when `old_string` is
/// empty and the path does not exist, refuse the call, or something else. Whatever happens —
/// a successful create, a `tool_use_result` error, or the model declining the exact call it was
/// told to make — is the frame this scenario exists to record. Nothing here bends the outcome
/// toward a nicer answer.
///
/// **No file is seeded.** Unlike [`edit`], the target's absence IS the scenario.
pub(in crate::record) async fn edit_create(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("edit-create is a Run scenario and always carries a request");
    let target = PathBuf::from(&request.cwd).join(EDIT_CREATE_TARGET_NAME);
    if target.exists() {
        anyhow::bail!(
            "edit-create target {} already exists; the scenario needs a path that has never \
             been written",
            target.display()
        );
    }

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// D17's degenerate case, live: `old_string` absent and `new_string` empty, on a file that DOES
/// exist and has been read. `claude/approval.rs`'s `(None, true)` arm reads this as
/// `FileOperation::Unknown` today — decided from the typed contract alone, never from a captured
/// frame. This settles what Claude's own `Edit` tool does when asked for it: refuse the call as a
/// no-op, accept it as a genuine (empty) edit, or something else.
const EDIT_NOOP_TARGET_NAME: &str = "capture-edit-noop-target.txt";
const EDIT_NOOP_ORIGINAL: &str = "left untouched\n";

/// The prompt for [`edit_noop`]. Reads first, exactly like [`claude_edit_prompt`] — the file
/// exists, so the same "Claude refuses to edit a file it has not read" contract applies here.
/// The `Edit` call's input deliberately omits the `old_string` key rather than sending an empty
/// one: it is the `(None, true)` arm this scenario is for, not `(Some(""), true)`, which
/// `approval.rs`'s own comment says reads identically but which `edit_noop` is not testing.
fn claude_edit_noop_prompt(cwd: &Path) -> String {
    let target = cwd.join(EDIT_NOOP_TARGET_NAME);
    let path = serde_json::to_string(&target.display().to_string()).expect("path serializes");
    format!(
        "Use Read once with input {{\"file_path\":{path}}}, then use Edit exactly once with \
         input {{\"file_path\":{path},\"new_string\":\"\"}}. Do not include an old_string key in \
         the Edit input. Use no other tool."
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn edit_noop_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_edit_noop_prompt(&cwd),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// D17's degenerate case, live — see [`claude_edit_noop_prompt`] for what is being asked and why.
pub(in crate::record) async fn edit_noop(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("edit-noop is a Run scenario and always carries a request");
    let target = PathBuf::from(&request.cwd).join(EDIT_NOOP_TARGET_NAME);
    std::fs::write(&target, EDIT_NOOP_ORIGINAL).map_err(|error| {
        anyhow::anyhow!(
            "edit-noop target {} could not be seeded: {error}",
            target.display()
        )
    })?;

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// D18, live: a `Write` that overwrites a file that already has different content. The row asks
/// whether the result frame gives a fix the same `originalFile`/`structuredPatch[]` material
/// `claude/2.1.251/edit` settled for `Edit`, or nothing of the kind — which would mean a fix has
/// to read the replaced file itself rather than lean on the result payload.
const WRITE_OVERWRITE_TARGET_NAME: &str = "capture-write-overwrite-target.txt";
const WRITE_OVERWRITE_ORIGINAL: &str = "old line one\nold line two\n";
const WRITE_OVERWRITE_NEW_CONTENT: &str = "brand new content\n";

/// The prompt for [`write_overwrite`]. Reads first, on the same precedent as
/// [`claude_edit_prompt`]: whether `Write` shares `Edit`'s "must read first" contract for a file
/// that already exists is itself unmeasured, and the Read costs one extra tool call against the
/// risk of burning the whole recording on a refusal the way the first `edit` attempt did.
fn claude_write_overwrite_prompt(cwd: &Path) -> String {
    let target = cwd.join(WRITE_OVERWRITE_TARGET_NAME);
    let path = serde_json::to_string(&target.display().to_string()).expect("path serializes");
    format!(
        "Use Read once with input {{\"file_path\":{path}}}, then use Write exactly once with \
         input {{\"file_path\":{path},\"content\":{}}}. Use no other tool.",
        serde_json::to_string(WRITE_OVERWRITE_NEW_CONTENT).expect("static string serializes"),
    )
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn write_overwrite_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_write_overwrite_prompt(&cwd),
        input,
        RuntimeMode::AutoAcceptEdits,
    ))
}

/// D18, live — see [`claude_write_overwrite_prompt`] for what is being asked and why.
pub(in crate::record) async fn write_overwrite(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("write-overwrite is a Run scenario and always carries a request");
    let target = PathBuf::from(&request.cwd).join(WRITE_OVERWRITE_TARGET_NAME);
    std::fs::write(&target, WRITE_OVERWRITE_ORIGINAL).map_err(|error| {
        anyhow::anyhow!(
            "write-overwrite target {} could not be seeded: {error}",
            target.display()
        )
    })?;

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// The reply for one `can_use_tool` request answered by an `edit-*-approval` scenario: allow
/// (echoing the request's own input back unmodified, exactly as [`decision_response`] does for
/// `approval`) when `grant` accepts it, deny with its error text otherwise.
///
/// A free function rather than a copy of [`decision_response`] inlined per scenario: the two
/// `edit-*-approval` bodies below need different accept rules (Read+Edit vs. Edit alone) but the
/// same "allow the exact expected input, decline and keep recording on anything else" shape
/// `claude_marker_grant` established for `approval`.
fn approval_mode_decision(request_id: &str, input: &Value, grant: anyhow::Result<()>) -> String {
    let response = match grant {
        Ok(()) => comet_harness::claude::wire::allow_response(input.clone()),
        Err(err) => comet_harness::claude::wire::deny_response(err.to_string()),
    };
    comet_harness::claude::wire::control_response_line(request_id, response)
}

/// D132, under real permission gating: does Claude's `can_use_tool` control channel raise a
/// request for an `Edit` whose `old_string` is empty and whose `new_string` is non-empty,
/// against a path that has never existed — the same input [`edit_create`] recorded under
/// `AutoAcceptEdits`, which never asks permission at all and so cannot show whether Comet's
/// `approval.rs` decode ever meets this shape on the wire in the first place.
///
/// Only the exact expected `Edit` input is granted; anything else is declined and recording
/// continues, the same defensive shape [`claude_marker_grant`] uses for `approval`.
fn edit_create_approval_grant(tool_name: &str, input: &Value, target: &Path) -> anyhow::Result<()> {
    if tool_name == "Edit" {
        // `replace_all: false` rides along even though the prompt never mentions it — the SDK's
        // Edit tool schema defaults it, and `edit_create`'s own raw capture shows the same key
        // on the wire for identical input. Matching without it here declined a genuinely correct
        // request twice on the first recording attempt before this fix.
        let expected = json!({
            "file_path": target.display().to_string(),
            "old_string": "",
            "new_string": EDIT_CREATE_NEW_STRING,
            "replace_all": false,
        });
        if input == &expected {
            return Ok(());
        }
        anyhow::bail!("edit-create-approval request was not the expected Edit input.");
    }
    anyhow::bail!("edit-create-approval request used an unexpected tool.");
}

/// Same one-call-per-recording contract as `fresh_text_request` above, under `ApprovalRequired`
/// rather than `edit_create_request`'s `AutoAcceptEdits` — the whole point of this row.
pub(in crate::record) fn edit_create_approval_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_edit_create_prompt(&cwd),
        input,
        RuntimeMode::ApprovalRequired,
    ))
}

/// D132's ordering question, live: send the same `edit-create` prompt, but this time under
/// `ApprovalRequired` so a real `can_use_tool` round trip is the only way the `Edit` call can
/// reach the filesystem at all. Whether that request ever arrives — and if it does, what its
/// `input` looks like on the wire — is the frame this scenario exists to record.
pub(in crate::record) async fn edit_create_approval(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("edit-create-approval is a Run scenario and always carries a request");
    let cwd = PathBuf::from(&request.cwd);
    let target = cwd.join(EDIT_CREATE_TARGET_NAME);
    if target.exists() {
        anyhow::bail!(
            "edit-create-approval target {} already exists; the scenario needs a path that has \
             never been written",
            target.display()
        );
    }

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    loop {
        let Some(frame) = session.next_frame().await? else {
            return protocol_stopped("Claude", "an approval request or a turn end");
        };
        if ClaudeProvider::turn_complete(&frame) {
            return Ok(());
        }
        if let Some((request_id, tool_name, tool_input)) = pending_approval(&frame) {
            let grant = edit_create_approval_grant(&tool_name, &tool_input, &target);
            session
                .send(&approval_mode_decision(&request_id, &tool_input, grant))
                .await?;
        }
    }
}

/// D17's degenerate case, under real permission gating: does `can_use_tool` fire for an `Edit`
/// whose `old_string` is absent and `new_string` empty at all, or does Claude's own tool-input
/// validation reject the call before it ever reaches Comet's approval hook. [`edit_noop`]
/// recorded what the tool does once it runs under `AutoAcceptEdits`, which never asks
/// permission and so cannot answer this ordering question either way.
///
/// Grants the expected `Read` (needed to satisfy Claude's "must read before Edit" contract,
/// same as [`edit_noop`]) and the expected degenerate `Edit`; declines anything else.
fn edit_noop_approval_grant(tool_name: &str, input: &Value, target: &Path) -> anyhow::Result<()> {
    match tool_name {
        "Read" => {
            let expected = json!({"file_path": target.display().to_string()});
            if input == &expected {
                return Ok(());
            }
            anyhow::bail!("edit-noop-approval Read request did not match the expected file.");
        }
        "Edit" => {
            let expected = json!({
                "file_path": target.display().to_string(),
                "new_string": "",
            });
            if input == &expected {
                return Ok(());
            }
            anyhow::bail!(
                "edit-noop-approval Edit request did not match the expected degenerate input."
            );
        }
        _ => anyhow::bail!("edit-noop-approval request used an unexpected tool."),
    }
}

/// Same one-call-per-recording contract as `fresh_text_request` above, under `ApprovalRequired`
/// rather than `edit_noop_request`'s `AutoAcceptEdits` — the whole point of this row.
pub(in crate::record) fn edit_noop_approval_request(
    input: &ScenarioInput,
) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_edit_noop_prompt(&cwd),
        input,
        RuntimeMode::ApprovalRequired,
    ))
}

/// D17's ordering question, live — see [`edit_noop_approval_grant`] for what is being asked and
/// why. Seeds the same target [`edit_noop`] does, because the `Read` this prompt asks for first
/// needs a real file to read.
pub(in crate::record) async fn edit_noop_approval(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("edit-noop-approval is a Run scenario and always carries a request");
    let target = PathBuf::from(&request.cwd).join(EDIT_NOOP_TARGET_NAME);
    std::fs::write(&target, EDIT_NOOP_ORIGINAL).map_err(|error| {
        anyhow::anyhow!(
            "edit-noop-approval target {} could not be seeded: {error}",
            target.display()
        )
    })?;

    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    loop {
        let Some(frame) = session.next_frame().await? else {
            return protocol_stopped("Claude", "an approval request or a turn end");
        };
        if ClaudeProvider::turn_complete(&frame) {
            return Ok(());
        }
        if let Some((request_id, tool_name, tool_input)) = pending_approval(&frame) {
            let grant = edit_noop_approval_grant(&tool_name, &tool_input, &target);
            session
                .send(&approval_mode_decision(&request_id, &tool_input, grant))
                .await?;
        }
    }
}

/// Same one-call-per-recording contract as `fresh_text_request` above.
pub(in crate::record) fn approval_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(cheap_claude_request(
        &claude_approval_prompt(&cwd),
        input,
        RuntimeMode::ApprovalRequired,
    ))
}

/// Recognizes a Claude `can_use_tool` control request and returns its request id, the tool it
/// names, and the input it asks to run. Every other frame returns `None` and is left unanswered.
///
/// A `can_use_tool` request with a missing/non-string `tool_name` is still recognized here (with
/// an empty tool name standing in), not folded into the `None` bucket — it's a real request the
/// CLI is blocked on a reply for. An empty tool name matches neither `claude_marker_grant`'s
/// `"Bash"` nor `"Write"` arm, so it lands in that function's catch-all and gets declined.
/// Before this, a missing/non-string `tool_name` returned `None`, the request went unanswered,
/// and the CLI blocked forever — burning the run to the recorder timeout and losing the capture.
///
/// The surviving half of the deleted `observe_claude_approval_frame`: noticing a frame IS an
/// approval request is driving, not validating, so nothing here checks request order, id
/// uniqueness, or transcript shape. Deciding whether the tool/input actually gets granted is a
/// separate question, answered at grant time by `claude_marker_grant` below.
fn pending_approval(frame: &Value) -> Option<(String, String, Value)> {
    if frame["type"] != "control_request" || frame["request"]["subtype"] != "can_use_tool" {
        return None;
    }
    let request_id = frame["request_id"].as_str()?.to_owned();
    let tool_name = frame["request"]["tool_name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Some((request_id, tool_name, frame["request"]["input"].clone()))
}

/// The one check that survives from the deleted validators, run at GRANT TIME rather than as a
/// frame check that aborts: the request must be exactly the marker Bash command, or exactly the
/// marker Write into the scenario's own cwd. Everything else is refused.
///
/// Codex's precedent, applied to Claude: this protects the machine, not evidence tidiness, so
/// design §3.2 doesn't reach it — but per that precedent it DECLINES an unrecognized request
/// rather than aborting the capture. Claude has no pre-spawn fence to recheck an identity against,
/// so unlike Codex's grant-time rechecks this recomputes, fresh, the same bounded shape the
/// deleted `validate_claude_marker_input` checked: the Write's input must equal
/// `{file_path: <cwd>/capture-marker.txt, content: "capture\n"}` exactly.
///
/// What actually stops a `../` or symlinked `file_path` from escaping `cwd` is that byte-for-byte
/// equality in the `Write` arm below — `file_path` must match exactly before the canonicalize
/// comparison that follows it ever runs. That comparison is inherited from the deleted code for
/// behavioural continuity but is inert: `expected_path.parent()` is always `cwd` itself, so it
/// compares one canonicalized path to itself, not a second line of defense.
fn claude_marker_grant(tool_name: &str, input: &Value, cwd: &Path) -> anyhow::Result<()> {
    if tool_name == "Bash" {
        if input == &json!({"command": CLAUDE_APPROVAL_COMMAND}) {
            return Ok(());
        }
        anyhow::bail!("Claude approval request was not the expected marker command.");
    }
    if tool_name == "Write" {
        let expected_path = cwd.join(APPROVAL_MARKER_NAME);
        let expected = json!({
            "file_path": expected_path.display().to_string(),
            "content": APPROVAL_MARKER_CONTENT,
        });
        if input != &expected {
            anyhow::bail!("Claude approval request was not the expected marker write.");
        }
        let canonical_cwd = std::fs::canonicalize(cwd)
            .map_err(|_| anyhow::anyhow!("Claude approval capture cwd could not be validated."))?;
        let canonical_parent = expected_path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .ok_or_else(|| {
                anyhow::anyhow!("Claude approval request marker parent could not be validated.")
            })?;
        if canonical_parent != canonical_cwd {
            anyhow::bail!("Claude approval request escaped the configured cwd.");
        }
        return Ok(());
    }
    anyhow::bail!("Claude approval request used an unexpected tool.");
}

/// The reply for one `can_use_tool` request: allow — echoing the request's
/// own input back unmodified, exactly as Comet's own approval driver
/// (`claude/mod.rs`) does for a real "always allow" decision — when
/// `claude_marker_grant` accepts it, deny with its error text otherwise.
/// Built through the production response shape,
/// `comet_harness::claude::wire::{control_response_line, allow_response,
/// deny_response}`.
fn decision_response(request_id: &str, tool_name: &str, input: &Value, cwd: &Path) -> String {
    let response = match claude_marker_grant(tool_name, input, cwd) {
        Ok(()) => comet_harness::claude::wire::allow_response(input.clone()),
        Err(err) => comet_harness::claude::wire::deny_response(err.to_string()),
    };
    comet_harness::claude::wire::control_response_line(request_id, response)
}

/// Send the approval prompt, then answer every `can_use_tool` request the model makes until the
/// turn ends.
///
/// No count, order, or request-id bookkeeping — the deleted validators enforced an exact "one
/// Bash, then one bounded Write" contract and bailed, discarding a real, paid-for capture, on any
/// deviation.
///
/// `claude_marker_grant` above is the one check that DOES survive, because it runs before a
/// write is granted, not after one already happened. This matters live: Claude's
/// `--permission-prompt-tool stdio` asks this driver to approve or deny EVERY tool call, and this
/// provider has no pre-spawn fence at all — replying `allow` unconditionally would let a live
/// `approval` capture grant an arbitrary Write or Bash against an arbitrary operator cwd. A
/// mismatch here DECLINES the grant and keeps recording — never `bail!`, which would throw away
/// tokens already spent.
pub(in crate::record) async fn approval(
    session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let request = session
        .request
        .clone()
        .expect("approval is a Run scenario and always carries a request");
    let cwd = PathBuf::from(&request.cwd);
    let line = claude_user_line(&request, false).await?;
    session.send(&line).await?;
    loop {
        let Some(frame) = session.next_frame().await? else {
            return protocol_stopped("Claude", "an approval request or a turn end");
        };
        if ClaudeProvider::turn_complete(&frame) {
            return Ok(());
        }
        if let Some((request_id, tool_name, tool_input)) = pending_approval(&frame) {
            session
                .send(&decision_response(
                    &request_id,
                    &tool_name,
                    &tool_input,
                    &cwd,
                ))
                .await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::*;
    use crate::record::session::FenceOutcome;
    use crate::test_support::{
        absolute_program, channel_payloads, config, contract_request, fixture_path,
    };
    use crate::types::{Channel, CommandSnapshot};
    use comet_harness::launch::StdioMode;

    #[test]
    fn claude_capture_uses_the_run_command_builder() {
        let request = contract_request();
        let exe = absolute_program("claude");
        let launch = comet_harness::claude::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(
            &snapshot.args[..18],
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--model",
                "claude-sonnet-5[1m]",
                "--effort",
                "xhigh",
                "--permission-mode",
                "bypassPermissions",
                "--dangerously-skip-permissions",
                "--resume=session-to-resume",
                "--settings",
            ]
        );
        assert_eq!(snapshot.args.len(), 19);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot.args[18]).unwrap(),
            json!({"alwaysThinkingEnabled": true, "fastMode": true})
        );
        assert_eq!(snapshot.cwd.as_deref(), Some(request.cwd.as_str()));
        assert!(snapshot.configured_env.is_empty(), "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0);
    }

    /// Starts a run-scenario `Session` exactly the way `record.rs`'s
    /// `derive_launch`/`record_generic` now do: one `RunRequest`, used to
    /// build both the launch and `Session::request` — never rebuilt by the
    /// scenario body. Shared by every test below that drives a real spawn.
    async fn start_claude_run_session(
        scenario_name: &'static str,
        executable: PathBuf,
        raw_root: &Path,
        request: RunRequest,
    ) -> Session<ClaudeProvider> {
        let launch = comet_harness::claude::run_launch(&executable, &request);
        let cfg = config(scenario_name, executable, "claude", raw_root);
        Session::start(
            ClaudeProvider,
            &cfg,
            launch,
            FenceOutcome::none(),
            Some(request),
        )
        .await
        .unwrap()
    }

    /// Break caught: `resume_request` stops setting `request.resume`, so the launch silently
    /// starts a fresh session under the `resume` scenario name instead of resuming one. This is a
    /// pre-spawn check on the built launch descriptor, not a frame check, so it is not the class
    /// of check this stage's design removes.
    #[test]
    fn resume_launch_passes_the_resume_id_as_a_launch_argument() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };

        let launch = comet_harness::claude::run_launch(&exe, &resume_request(&input).unwrap());
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert!(
            snapshot
                .args
                .iter()
                .any(|arg| arg == "--resume=session-abc"),
            "resume launch must carry --resume=<id>: {:?}",
            snapshot.args
        );
    }

    /// Break caught: a Claude run driver invents its own initial wire line instead of recording
    /// the exact provider-specific user message it writes through the production run launch.
    ///
    /// `fresh_text`'s real prompt ("Reply with the single word capture.") matches no branch in
    /// `fake_claude.rs`, so this exercises the fixture's generic `error_during_execution` result,
    /// not a modelled success transcript — the assertions below only need a single stdin write and
    /// *a* terminal `result` frame, both of which that fallback still produces. Noted here so the
    /// next reader does not assume this proves a modelled multi-frame run; it does not.
    #[tokio::test]
    async fn recorder_claude_run_records_the_exact_initial_write() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let request = fresh_text_request(&input).unwrap();
        let mut session =
            start_claude_run_session("fresh-text", executable, raw.path(), request).await;

        fresh_text(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let writes = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(writes[0]).unwrap(),
            json!({
                "type": "user",
                "message": {"role": "user", "content": "Reply with the single word capture."},
                "parent_tool_use_id": null,
            })
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    #[tokio::test]
    async fn capture_attachment_line_uses_the_production_image_helpers() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("tiny.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let input = ScenarioInput {
            attachment: Some(image),
            ..ScenarioInput::default()
        };
        let request = attachment_request(&input).unwrap();
        let production_images =
            comet_harness::claude::load_image_blocks(&request.attachments).await;
        assert_eq!(
            claude_user_line(&request, true).await.unwrap(),
            comet_harness::claude::wire::user_message_line_with_images(
                &request.prompt,
                &production_images
            )
        );
    }

    #[tokio::test]
    async fn claude_attachment_capture_requires_inline_image_before_text() {
        let raw = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let image = files.path().join("tiny.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let input = ScenarioInput {
            attachment: Some(image),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-claude");
        let request = attachment_request(&input).unwrap();
        let mut session =
            start_claude_run_session("attachment", executable, raw.path(), request).await;

        attachment(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let first: serde_json::Value =
            serde_json::from_str(channel_payloads(&capture, Channel::Stdin)[0]).unwrap();
        assert_eq!(first["message"]["content"][0]["type"], "image");
        assert_eq!(first["message"]["content"][1]["type"], "text");
    }

    /// Break caught: same as `resume_launch_passes_the_resume_id_as_a_launch_argument`, for
    /// `checklist-resume` — a mislabeled capture that silently starts a fresh session under the
    /// `checklist-resume` name instead of resuming one.
    #[test]
    fn checklist_resume_launch_passes_the_resume_id_as_a_launch_argument() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };

        let launch =
            comet_harness::claude::run_launch(&exe, &checklist_resume_request(&input).unwrap());
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert!(
            snapshot
                .args
                .iter()
                .any(|arg| arg == "--resume=session-abc"),
            "checklist-resume launch must carry --resume=<id>: {:?}",
            snapshot.args
        );
    }

    /// The evidence guard's removal, proven: a fake Claude that answers the real checklist
    /// prompt with plain text and never calls `TaskCreate` at all must still produce a
    /// successful capture holding every frame. The deleted `recording.rs` guard bailed here —
    /// "Claude checklist capture created 0 task(s) and updated 0; needed 2 distinct creates and
    /// at least 1 update" — because it inspected what the model did with the prompt instead of
    /// only recording it. See `checklist`'s own doc comment for why that inspection is gone.
    #[tokio::test]
    async fn checklist_capture_records_a_run_that_created_no_tasks() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let request = checklist_request(&input).unwrap();
        let mut session =
            start_claude_run_session("checklist", executable, raw.path(), request).await;

        checklist(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdout = channel_payloads(&capture, Channel::Stdout);
        // Dispatched by substring match on the real prompt text, not a `scenario:` tag — if the
        // prompt is ever reworded, dispatch silently falls through to the fixture's generic
        // `error_during_execution` reply, which also has no TaskCreate call, also carries a
        // result frame, and also exits 0, so every assertion below would keep passing while
        // testing nothing. `sess-checklist-no-tasks` only exists in the real branch, so this
        // fails loudly instead.
        assert!(
            stdout
                .iter()
                .any(|line| line.contains("sess-checklist-no-tasks")),
            "the fake-claude checklist_no_tasks() branch must have run, not the \
             error_during_execution fallthrough: {stdout:?}"
        );
        // The init frame's advertised tool list still names `TaskCreate` (the
        // CLI offers it whether or not the model uses it) — the check is for
        // an actual `tool_use` call, not the substring, so it does not
        // false-positive on that list.
        assert!(
            !stdout
                .iter()
                .any(|line| line.contains(r#""name":"TaskCreate""#)),
            "fixture must reply without ever calling TaskCreate: {stdout:?}"
        );
        assert!(
            stdout
                .iter()
                .any(|line| line.contains(r#""type":"result""#)),
            "capture must still hold a terminal frame: {stdout:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Unit-level coverage of `claude_marker_grant` itself, independent of any process spawn:
    /// the expected marker Bash command and the expected marker Write into `cwd` are the only two
    /// requests it accepts; a different command, a Write to a different path, a Write whose
    /// content does not match, and any other tool are all refused.
    #[test]
    fn claude_marker_grant_accepts_only_the_expected_marker_command_or_write() {
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            claude_marker_grant(
                "Bash",
                &json!({"command": CLAUDE_APPROVAL_COMMAND}),
                cwd.path()
            )
            .is_ok()
        );
        assert!(
            claude_marker_grant("Bash", &json!({"command": "echo pwned"}), cwd.path()).is_err()
        );
        let expected_write = json!({
            "file_path": cwd.path().join(APPROVAL_MARKER_NAME).display().to_string(),
            "content": APPROVAL_MARKER_CONTENT,
        });
        assert!(claude_marker_grant("Write", &expected_write, cwd.path()).is_ok());
        let wrong_path = json!({
            "file_path": cwd.path().join("unexpected.txt").display().to_string(),
            "content": APPROVAL_MARKER_CONTENT,
        });
        assert!(claude_marker_grant("Write", &wrong_path, cwd.path()).is_err());
        let wrong_content = json!({
            "file_path": cwd.path().join(APPROVAL_MARKER_NAME).display().to_string(),
            "content": "not the marker\n",
        });
        assert!(claude_marker_grant("Write", &wrong_content, cwd.path()).is_err());
        assert!(
            claude_marker_grant("Read", &json!({"file_path": "whatever"}), cwd.path()).is_err()
        );
    }

    /// The validator deletion, proven end to end: a fake Claude raising three `can_use_tool`
    /// requests in a row (uncounted, unordered) must have every one answered — `pending_approval`
    /// has no bookkeeping to bail on, unlike the deleted validators.
    ///
    /// Also proves the grant/decline split: the first two requests are the marker Bash command
    /// and the marker Write into this scenario's cwd, so both must be GRANTED; the third is a
    /// Write to an unrequested file, so it must be DECLINED — and, unlike the deleted validators
    /// (which `bail!`ed and discarded the whole capture on this kind of mismatch), the run must
    /// still reach its terminal frame.
    #[tokio::test]
    async fn claude_approval_scenario_answers_every_request_it_sees() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let request = approval_request(&input).unwrap();
        let mut session =
            start_claude_run_session("approval", executable, raw.path(), request).await;

        // Bounded, unlike the other scenario tests in this file: the fixture below
        // (`approval_three_requests`) reads a reply off stdin before emitting its next line, so a
        // driver that stops answering does not error, it blocks forever. Wrapping the call keeps
        // that failure a fast, legible timeout instead of a hung test process.
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

        let stdin = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(
            stdin.len(),
            4,
            "one user line, then one reply per request: {stdin:?}"
        );
        let replies: Vec<Value> = stdin[1..]
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            replies
                .iter()
                .map(|reply| reply["response"]["request_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["approval-req-1", "approval-req-2", "approval-req-3"],
            "every approval request must be answered, in the order they arrived: {replies:?}"
        );
        let behaviors: Vec<&str> = replies
            .iter()
            .map(|reply| reply["response"]["response"]["behavior"].as_str().unwrap())
            .collect();
        assert_eq!(
            behaviors,
            ["allow", "allow", "deny"],
            "the two expected marker requests must be granted and the unexpected write declined: \
             {replies:?}"
        );
        assert!(
            replies[..2]
                .iter()
                .all(|reply| reply["response"]["response"]["updatedInput"].is_object()),
            "a granted reply must echo the request's own input back: {replies:?}"
        );
        assert!(
            replies[2]["response"]["response"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "a declined reply must carry a message: {replies:?}"
        );
        assert_eq!(
            capture.exit_code,
            Some(0),
            "the recording must survive a declined grant, not be discarded"
        );
    }

    /// Unit-level coverage of the bug itself: a `can_use_tool` frame whose `tool_name` is
    /// missing entirely, or present but not a string, used to make `pending_approval` return
    /// `None` — the same bucket as a frame that was never an approval request at all — which
    /// left the request unanswered. This asserts the frame is still recognized (`Some`, not
    /// `None`) and that feeding its output through `decision_response` produces a decline, not
    /// a hang.
    #[test]
    fn pending_approval_folds_a_missing_or_non_string_tool_name_into_a_decline() {
        let missing = json!({
            "type": "control_request",
            "request_id": "cr-missing",
            "request": {"subtype": "can_use_tool", "input": {"command": "echo hi"}},
        });
        let (request_id, tool_name, input) = pending_approval(&missing)
            .expect("a can_use_tool frame with a request_id must be recognized even without a usable tool_name");
        assert_eq!(request_id, "cr-missing");
        let cwd = tempfile::tempdir().unwrap();
        let reply = decision_response(&request_id, &tool_name, &input, cwd.path());
        assert!(
            reply.contains(r#""behavior":"deny""#),
            "a missing tool_name must be declined, not silently dropped: {reply}"
        );

        let non_string = json!({
            "type": "control_request",
            "request_id": "cr-non-string",
            "request": {"subtype": "can_use_tool", "tool_name": 7, "input": {}},
        });
        let (request_id, tool_name, input) = pending_approval(&non_string).expect(
            "a non-string tool_name must also be recognized, not treated as no request at all",
        );
        let reply = decision_response(&request_id, &tool_name, &input, cwd.path());
        assert!(
            reply.contains(r#""behavior":"deny""#),
            "a non-string tool_name must be declined, not silently dropped: {reply}"
        );
    }

    /// End-to-end proof of the same bug against a real spawned fake-claude: before the fix,
    /// `pending_approval` returning `None` for a `tool_name`-less request means the fixture's
    /// `read_line` never gets a reply and the capture hangs to the timeout instead of reaching
    /// its terminal frame. Drives a custom wire line (not the production `approval_request`
    /// prompt) to reach fake-claude's dedicated branch, through the same recognize-then-answer
    /// loop `approval` itself runs.
    #[tokio::test]
    async fn claude_approval_declines_a_request_missing_tool_name_instead_of_hanging() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let launch =
            comet_harness::claude::run_launch(&executable, &approval_request(&input).unwrap());
        let cfg = config("approval", executable, "claude", raw.path());
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None)
            .await
            .unwrap();

        let request = RunRequest {
            prompt: "scenario:approval-missing-tool-name".into(),
            model: Some(CHEAP_MODEL.into()),
            reasoning: Some(ReasoningLevel::Low),
            cwd: std::env::temp_dir().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let cwd = PathBuf::from(&request.cwd);
        let line = claude_user_line(&request, false).await.unwrap();
        session.send(&line).await.unwrap();

        // Bounded for the same reason as `claude_approval_scenario_answers_every_request_it_sees`
        // above: the fixture blocks on a reply before it emits its next line, so a driver that
        // stops answering hangs the test process rather than failing fast.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = session
                    .next_frame()
                    .await
                    .unwrap()
                    .expect("a reply or a turn end");
                if ClaudeProvider::turn_complete(&frame) {
                    return;
                }
                if let Some((request_id, tool_name, tool_input)) = pending_approval(&frame) {
                    session
                        .send(&decision_response(
                            &request_id,
                            &tool_name,
                            &tool_input,
                            &cwd,
                        ))
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .expect(
            "a missing tool_name must still be answered, not leave the fixture blocked on a \
             reply forever",
        );

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdin = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(
            stdin.len(),
            2,
            "one user line, then one decline reply: {stdin:?}"
        );
        let reply: Value = serde_json::from_str(stdin[1]).unwrap();
        assert_eq!(
            reply["response"]["response"]["behavior"], "deny",
            "the missing-tool_name request must be declined: {reply:?}"
        );
        assert_eq!(
            capture.exit_code,
            Some(0),
            "the run must still reach its terminal frame instead of hanging to the recorder timeout"
        );
    }

    /// The table's `runtime_mode` and the body's own `RunRequest.runtime_mode` are two separate
    /// homes for one fact — nothing but this loop proves they still agree;
    /// `comet-provider-capture.rs::scenario_names_own_their_runtime_modes` reads only
    /// `spec.runtime_mode` and can't see the two drift apart.
    ///
    /// Break caught: a `*_request` builder's `RuntimeMode` literal goes stale while the table's
    /// `runtime_mode` field doesn't — the row would still select the right fence and print the
    /// right `--help` text while sending the wrong mode on the wire.
    #[test]
    fn every_claude_run_rows_declared_mode_matches_its_request_builder() {
        let plain = ScenarioInput::default();
        let with_resume = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };
        let with_attachment = ScenarioInput {
            attachment: Some(PathBuf::from("tiny.png")),
            ..ScenarioInput::default()
        };
        let cases = [
            (
                "fresh-text",
                fresh_text_request(&plain).unwrap().runtime_mode,
            ),
            ("approval", approval_request(&plain).unwrap().runtime_mode),
            ("edit", edit_request(&plain).unwrap().runtime_mode),
            (
                "edit-create",
                edit_create_request(&plain).unwrap().runtime_mode,
            ),
            (
                "edit-create-approval",
                edit_create_approval_request(&plain).unwrap().runtime_mode,
            ),
            ("edit-noop", edit_noop_request(&plain).unwrap().runtime_mode),
            (
                "edit-noop-approval",
                edit_noop_approval_request(&plain).unwrap().runtime_mode,
            ),
            (
                "write-overwrite",
                write_overwrite_request(&plain).unwrap().runtime_mode,
            ),
            ("resume", resume_request(&with_resume).unwrap().runtime_mode),
            (
                "attachment",
                attachment_request(&with_attachment).unwrap().runtime_mode,
            ),
            ("checklist", checklist_request(&plain).unwrap().runtime_mode),
            (
                "checklist-resume",
                checklist_resume_request(&with_resume).unwrap().runtime_mode,
            ),
            ("auto", auto_request(&plain).unwrap().runtime_mode),
            (
                "full-access",
                full_access_request(&plain).unwrap().runtime_mode,
            ),
        ];
        for (name, mode) in cases {
            let spec = crate::record::scenarios::scenario("claude", name)
                .unwrap_or_else(|| panic!("missing claude/{name}"));
            assert_eq!(
                spec.runtime_mode,
                Some(mode),
                "claude/{name}: table says {:?}, request builder says {mode:?}",
                spec.runtime_mode
            );
        }

        // Coverage, not just correctness: `cases` above must name every claude row that declares
        // a runtime_mode, not merely the ones someone remembered to add. A 13th run row with no
        // entry here would pass the loop above vacuously and escape both this test and
        // `comet-provider-capture.rs::scenario_names_own_their_runtime_modes`, which is the exact
        // "second unsynchronized copy" shape this test exists to catch, one level up.
        let covered: std::collections::BTreeSet<&str> =
            cases.iter().map(|(name, _)| *name).collect();
        let expected: std::collections::BTreeSet<&str> = crate::record::scenarios::SCENARIOS
            .iter()
            .filter(|spec| spec.provider == crate::Provider::Claude && spec.runtime_mode.is_some())
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            covered, expected,
            "every claude row with Some(runtime_mode) must have a case in this test's list"
        );
    }

    /// `Auto` must reach the wire as Claude's `auto` permission mode, and must NOT carry
    /// `--dangerously-skip-permissions` — same pin contract as
    /// `record/scenarios/codex.rs`'s `codex_auto_scenario_pins_the_auto_review_reviewer_on_the_wire`,
    /// applied to Claude's launch-argument wire instead of a JSON-RPC params object.
    #[test]
    fn claude_auto_scenario_pins_the_auto_permission_mode() {
        let request = auto_request(&ScenarioInput::default()).unwrap();
        let exe = absolute_program("claude");
        let launch = comet_harness::claude::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);
        assert!(
            snapshot
                .args
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "auto"]),
            "Auto must reach the wire as the auto permission mode: {:?}",
            snapshot.args
        );
        assert!(
            !snapshot
                .args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "Auto must not skip permissions: {:?}",
            snapshot.args
        );
    }

    /// `FullAccess` must reach the wire as Claude's `bypassPermissions` permission mode plus
    /// `--dangerously-skip-permissions` — same pin contract as the `Auto` test above.
    #[test]
    fn claude_full_access_scenario_pins_bypass_permissions_and_the_skip_flag() {
        let request = full_access_request(&ScenarioInput::default()).unwrap();
        let exe = absolute_program("claude");
        let launch = comet_harness::claude::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);
        assert!(
            snapshot
                .args
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "bypassPermissions"]),
            "FullAccess must reach the wire as the bypassPermissions permission mode: {:?}",
            snapshot.args
        );
        assert!(
            snapshot
                .args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "FullAccess must skip permissions on the launch: {:?}",
            snapshot.args
        );
    }
}
