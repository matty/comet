use std::path::Path;

use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};

use crate::capture::record::provider::CaptureProvider;
use crate::capture::record::providers::claude::ClaudeProvider;
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;
use crate::launch::LaunchDescriptor;

/// SPAWN for `model-discovery` and its `-neutral-cwd`/`-project-cwd`
/// aliases: the bare initialize launch.
pub(in crate::capture::record) fn model_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::claude::discovery::model_discovery_launch(
        executable, &cwd,
    ))
}

/// SPAWN for `command-discovery`: the non-bare initialize launch — the
/// entire reason this scenario needs its own launch fn rather than reusing
/// `model_discovery_launch`. Recording command discovery with `--bare`
/// would be silently wrong, which is exactly the drift a first draft of
/// this seam let happen when `launch` lived on the provider trait instead
/// of the scenario row.
pub(in crate::capture::record) fn command_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::claude::commands::command_discovery_launch(
        executable, &cwd,
    ))
}

/// Model discovery's entire drive IS the handshake — the body calls it
/// itself (per the amendment "the scenario body calls the handshake; the
/// recorder does not": `record_generic` no longer calls `P::handshake` for
/// any scenario). Kept as its own named function, rather than a bare call
/// inlined elsewhere, because it is a named row in [`super::SCENARIOS`].
pub(in crate::capture::record) async fn model_discovery(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    ClaudeProvider::handshake(session, input).await
}

/// Command discovery's driving is identical to model discovery's — the
/// difference between the two scenarios is entirely in which launch builder
/// `record()` selects (`command_discovery_launch` vs `model_discovery_launch`
/// picks `--bare` or not), never in what happens after spawn.
pub(in crate::capture::record) async fn command_discovery(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    ClaudeProvider::handshake(session, input).await
}

// Below this point: `fresh-text`, `resume` and `attachment`. None of the
// three is reachable from production code yet — the SCENARIOS table only
// gains their rows in Task 7 — so every item down to the test module carries
// `#[allow(dead_code)]`, exercised meanwhile only by this file's own tests.
// Same shape as `ScenarioSpec`'s own unread fields in `scenarios.rs`.

/// The model every non-discovery Claude scenario runs against: cheap and
/// fast, because a capture's whole point is the wire *shape*, not which
/// model answered. Ported from `comet-provider-capture.rs`'s
/// `cheap_claude_request` — decision "the scenario owns its prompt" moves
/// the choice out of the binary along with the prompt text itself.
#[allow(dead_code)]
const CHEAP_MODEL: &str = "claude-haiku-4-5-20251001";

/// The `RunRequest` every non-discovery Claude scenario in this file starts
/// from: the cheap model, low reasoning, and the caller's cwd (or a neutral
/// temp directory, exactly like the discovery launches above). Each
/// scenario's own function fills in its fixed prompt and whatever else it
/// needs (`resume`, `attachments`).
#[allow(dead_code)]
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
#[allow(dead_code)]
async fn claude_user_line(request: &RunRequest, require_image: bool) -> anyhow::Result<String> {
    let images = crate::claude::load_image_blocks(&request.attachments).await;
    if require_image && images.is_empty() {
        anyhow::bail!(
            "The selected attachment could not be inlined. Use a supported image under 5 MiB and retry."
        );
    }
    Ok(crate::claude::wire::user_message_line_with_images(
        &request.prompt,
        &images,
    ))
}

// `fresh_text_launch` and `fresh_text` (and the matching pairs for `resume`
// and `attachment` below) each call this a second time rather than sharing
// one computed value — the launch-on-the-row design (see the amendment on
// `ScenarioSpec::launch`) means the launch and the body are built by two
// separate calls with no shared state between them. The two calls cannot
// diverge only because this stays a pure function of `input`: a future field
// that reads something else (the clock, an env var, a counter) would make
// the launch and the wire line disagree about what request was sent.
#[allow(dead_code)]
fn fresh_text_request(input: &ScenarioInput) -> RunRequest {
    cheap_claude_request(
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
    Ok(crate::claude::run_launch(
        executable,
        &fresh_text_request(input),
    ))
}

/// A plain text turn: write the user line through the production helper,
/// then wait for the terminal result. No handshake — a Claude run never
/// sends one; see `provider.rs`'s doc comment.
#[allow(dead_code)]
pub(in crate::capture::record) async fn fresh_text(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    let line = claude_user_line(&fresh_text_request(input), false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

// Called once by `resume_launch`, once by `resume` — see the note on
// `fresh_text_request` above; the same "stays pure, so the two calls can't
// disagree" reasoning applies here.
#[allow(dead_code)]
fn resume_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
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

/// SPAWN for `resume`: the id reaches the CLI as `--resume=<id>` on the
/// launch (`crate::claude::run_launch` reads `request.resume`), never as
/// part of the wire line `resume`'s body sends.
#[allow(dead_code)]
pub(in crate::capture::record) fn resume_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::claude::run_launch(
        executable,
        &resume_request(input)?,
    ))
}

#[allow(dead_code)]
pub(in crate::capture::record) async fn resume(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    let line = claude_user_line(&resume_request(input)?, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

// Called once by `attachment_launch`, once by `attachment` — see the note on
// `fresh_text_request` above.
#[allow(dead_code)]
fn attachment_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
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

#[allow(dead_code)]
pub(in crate::capture::record) fn attachment_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::claude::run_launch(
        executable,
        &attachment_request(input)?,
    ))
}

/// The one scenario whose wire line must carry an inlined image ahead of the
/// text — `require_image: true` is what makes `claude_user_line` fail loudly
/// instead of silently recording a text-only capture when the attachment
/// could not be inlined.
#[allow(dead_code)]
pub(in crate::capture::record) async fn attachment(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    let line = claude_user_line(&attachment_request(input)?, true).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

/// Prompt for `checklist`: create two tasks, then drive the first through
/// both transitions. Moved here, unchanged in substance, from the deleted
/// `capture/checklist.rs` — decision "the scenario owns its prompt" moves
/// prompt text out of the binary and into the scenario body that sends it.
///
/// Opens with `ToolSearch` because the task tools are *deferred* on at least
/// one machine — captured 2026-08-13, where the model reached them through
/// `{"query":"select:TaskCreate,TaskUpdate","total_deferred_tools":45}`. On an
/// installation that lists them eagerly the search is a harmless extra frame;
/// without it, on one that does not, the run produces no checklist at all.
#[allow(dead_code)]
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
#[allow(dead_code)]
fn claude_checklist_resume_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskUpdate","max_results":5}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"in_progress","activeForm":"Working the second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"completed"}. "#,
        r#"Do not create any task. Do nothing else, and reply with the single word resumed."#,
    )
    .to_owned()
}

// Called once by `checklist_launch`, once by `checklist` — see the note on
// `fresh_text_request` above; the same "stays pure, so the two calls can't
// disagree" reasoning applies here.
#[allow(dead_code)]
fn checklist_request(input: &ScenarioInput) -> RunRequest {
    cheap_claude_request(
        &claude_checklist_prompt(),
        input,
        RuntimeMode::AutoAcceptEdits,
    )
}

/// SPAWN for `checklist`: an ordinary run launch, built from the same request
/// `checklist` replays as its wire line.
#[allow(dead_code)]
pub(in crate::capture::record) fn checklist_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::claude::run_launch(
        executable,
        &checklist_request(input),
    ))
}

/// Send the checklist prompt and wait for the turn to end. Nothing here
/// inspects what the model actually did with the task tools — no
/// created/updated task-id accounting, no bail on a mutation count the
/// prompt asked for but the model did not produce.
///
/// That accounting used to live in `recording.rs`'s `claude_run` (the
/// `created`/`updated` `BTreeSet`s and their `bail!`s, requiring at least 2
/// distinct confirmed creates and 1 confirmed update here, and for
/// `checklist-resume` at least 1 confirmed update to an id it had not itself
/// created) and is deleted by this task, closing `docs/debt/` D61. Per
/// design §3.2
/// (`2026-08-14-provider-capture-simplification-design.md`): a pre-spawn
/// guard protects the machine; a frame check that aborts protects only a
/// scenario's tidiness, and does it by destroying evidence already paid for
/// in tokens. A model that ignores this prompt and creates no task does not
/// produce a failed capture — it produces a recording of a model ignoring
/// instructions, which is itself evidence of the CLI's real behavior under
/// this prompt, and the deleted guard threw it away along with the tokens
/// that paid for it.
#[allow(dead_code)]
pub(in crate::capture::record) async fn checklist(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    let line = claude_user_line(&checklist_request(input), false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

// Called once by `checklist_resume_launch`, once by `checklist_resume` — see
// the note on `fresh_text_request` above.
#[allow(dead_code)]
fn checklist_resume_request(input: &ScenarioInput) -> anyhow::Result<RunRequest> {
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

/// SPAWN for `checklist-resume`: the id reaches the CLI as `--resume=<id>` on
/// the launch, never as part of the wire line the body sends — same split as
/// `resume_launch` above.
#[allow(dead_code)]
pub(in crate::capture::record) fn checklist_resume_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    Ok(crate::claude::run_launch(
        executable,
        &checklist_resume_request(input)?,
    ))
}

/// No session-identity check against `input.resume_id` — the abort-on-mismatch
/// class this stage's design removes (§3.2), same as `resume` above. The old
/// `recording.rs::claude_run` checked `value["session_id"] == request.resume`
/// before returning; a Claude bug that returned the wrong session id would
/// still be worth recording, not a reason to throw the capture away.
#[allow(dead_code)]
pub(in crate::capture::record) async fn checklist_resume(
    session: &mut Session<ClaudeProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    let line = claude_user_line(&checklist_resume_request(input)?, false).await?;
    session.send(&line).await?;
    session.wait_for_turn_end().await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::*;
    use crate::capture::record::session::FenceOutcome;
    use crate::capture::test_support::{
        absolute_program, channel_payloads, config, contract_request, fixture_path,
    };
    use crate::capture::types::{
        CaptureOperation, Channel, ClaudeCaptureOperation, ClaudeRunScript, CommandSnapshot,
    };
    use crate::launch::StdioMode;

    #[test]
    fn claude_capture_uses_the_run_command_builder() {
        let request = contract_request();
        let exe = absolute_program("claude");
        let launch = crate::claude::run_launch(&exe, &request);
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

    /// Break caught: `fresh_text_launch` stops calling the production `crate::claude::run_launch`
    /// and hand-builds (or hand-edits) a `LaunchDescriptor` instead — the same class of hole Task
    /// 1's `command_discovery_launch` bypass would have shipped had it gone unnoticed.
    /// `claude_capture_uses_the_run_command_builder` above only proves `run_launch` itself is
    /// right; it never calls `fresh_text_launch`, so it cannot catch this.
    #[test]
    fn fresh_text_launch_uses_the_production_run_launch() {
        let exe = absolute_program("claude");
        let input = ScenarioInput::default();

        let launch = fresh_text_launch(&input, &exe).unwrap();
        let expected = crate::claude::run_launch(&exe, &fresh_text_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `resume`.
    #[test]
    fn resume_launch_uses_the_production_run_launch() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };

        let launch = resume_launch(&input, &exe).unwrap();
        let expected = crate::claude::run_launch(&exe, &resume_request(&input).unwrap());

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `attachment`.
    #[test]
    fn attachment_launch_uses_the_production_run_launch() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            attachment: Some(PathBuf::from("tiny.png")),
            ..ScenarioInput::default()
        };

        let launch = attachment_launch(&input, &exe).unwrap();
        let expected = crate::claude::run_launch(&exe, &attachment_request(&input).unwrap());

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: `resume_request` stops setting `request.resume`, so the launch silently
    /// starts a fresh session under the `resume` scenario name instead of resuming one — a
    /// mislabeled capture the parity test above cannot catch, because it builds "expected" from
    /// the same `resume_request` call. This is a pre-spawn check on the built launch descriptor,
    /// not a frame check, so it is not the class of check this stage's design removes.
    #[test]
    fn resume_launch_passes_the_resume_id_as_a_launch_argument() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };

        let launch = resume_launch(&input, &exe).unwrap();
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
        let launch = fresh_text_launch(&input, &executable).unwrap();
        let cfg = config(
            "claude-fresh-text",
            executable,
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request: fresh_text_request(&input),
                script: ClaudeRunScript::FreshText,
            }),
            raw.path(),
        );
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

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
        let production_images = crate::claude::load_image_blocks(&request.attachments).await;
        assert_eq!(
            claude_user_line(&request, true).await.unwrap(),
            crate::claude::wire::user_message_line_with_images(&request.prompt, &production_images)
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
        let launch = attachment_launch(&input, &executable).unwrap();
        let cfg = config(
            "claude-attachment",
            executable,
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request: attachment_request(&input).unwrap(),
                script: ClaudeRunScript::Attachment,
            }),
            raw.path(),
        );
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

        attachment(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let first: serde_json::Value =
            serde_json::from_str(channel_payloads(&capture, Channel::Stdin)[0]).unwrap();
        assert_eq!(first["message"]["content"][0]["type"], "image");
        assert_eq!(first["message"]["content"][1]["type"], "text");
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for `checklist`.
    #[test]
    fn checklist_launch_uses_the_production_run_launch() {
        let exe = absolute_program("claude");
        let input = ScenarioInput::default();

        let launch = checklist_launch(&input, &exe).unwrap();
        let expected = crate::claude::run_launch(&exe, &checklist_request(&input));

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
    }

    /// Break caught: same as `fresh_text_launch_uses_the_production_run_launch`, for
    /// `checklist-resume`.
    #[test]
    fn checklist_resume_launch_uses_the_production_run_launch() {
        let exe = absolute_program("claude");
        let input = ScenarioInput {
            resume_id: Some("session-abc".into()),
            ..ScenarioInput::default()
        };

        let launch = checklist_resume_launch(&input, &exe).unwrap();
        let expected = crate::claude::run_launch(&exe, &checklist_resume_request(&input).unwrap());

        assert_eq!(
            CommandSnapshot::from_launch(&launch),
            CommandSnapshot::from_launch(&expected)
        );
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

        let launch = checklist_resume_launch(&input, &exe).unwrap();
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
        let launch = checklist_launch(&input, &executable).unwrap();
        let cfg = config(
            "claude-checklist",
            executable,
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request: checklist_request(&input),
                script: ClaudeRunScript::Checklist,
            }),
            raw.path(),
        );
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none())
            .await
            .unwrap();

        checklist(&mut session, &input).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();

        let stdout = channel_payloads(&capture, Channel::Stdout);
        // The fixture is selected by matching a substring of the real
        // `claude_checklist_prompt()` text (`fake_claude.rs`'s
        // `"TaskCreate exactly twice"` branch), not a `scenario:` tag. If
        // that prompt is ever reworded, the match silently fails and
        // dispatch falls through to the fixture's generic
        // `error_during_execution` reply — which also has no `TaskCreate`
        // call, also carries a `result` frame, and also exits 0, so every
        // assertion below would keep passing while asserting nothing about
        // `checklist_no_tasks()` at all. `sess-checklist-no-tasks` exists
        // only in that scenario, so this fails loudly instead.
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
}
