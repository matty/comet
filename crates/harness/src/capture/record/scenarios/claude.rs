use std::path::Path;

use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};

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

/// Model discovery's entire drive is the handshake: `record()` already ran
/// it before calling this body, so there is nothing left to do here. Kept as
/// its own function (rather than folded away) because it is a named row in
/// [`super::SCENARIOS`], not because its body is nontrivial.
pub(in crate::capture::record) async fn model_discovery(
    _session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    Ok(())
}

/// Command discovery's driving is identical to model discovery's — the
/// difference between the two scenarios is entirely in which launch builder
/// `record()` selects (`command_discovery_launch` vs `model_discovery_launch`
/// picks `--bare` or not), never in what happens after spawn.
pub(in crate::capture::record) async fn command_discovery(
    _session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    Ok(())
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

#[cfg(test)]
mod tests {
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

    /// Break caught: a Claude run driver invents its own initial wire line instead of recording
    /// the exact provider-specific user message it writes through the production run launch.
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
}
