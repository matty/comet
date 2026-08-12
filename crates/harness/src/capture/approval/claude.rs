use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, bail};
use serde_json::{Value, json};

use super::common::{APPROVAL_MARKER_CONTENT, APPROVAL_MARKER_NAME, CLAUDE_APPROVAL_COMMAND};

#[derive(Default)]
pub(in crate::capture) struct ClaudeApprovalState {
    request_ids: BTreeSet<String>,
    pub(in crate::capture) bash_tool_id: Option<String>,
    pub(in crate::capture) bash_succeeded: bool,
    write_tool_id: Option<String>,
    write_input: Option<Value>,
    pub(in crate::capture) write_approved: bool,
}

fn validate_claude_marker_input(input: &Value, cwd: &Path) -> anyhow::Result<()> {
    let expected_path = cwd.join(APPROVAL_MARKER_NAME);
    let expected = json!({
        "file_path": expected_path.display().to_string(),
        "content": APPROVAL_MARKER_CONTENT,
    });
    if input != &expected {
        bail!("Claude Write approval request did not match the exact bounded marker.");
    }
    let canonical_cwd = std::fs::canonicalize(cwd)
        .map_err(|_| anyhow!("Claude Write approval request cwd could not be validated."))?;
    let canonical_parent = expected_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| {
            anyhow!("Claude Write approval request marker parent could not be validated.")
        })?;
    if canonical_parent != canonical_cwd {
        bail!("Claude Write approval request escaped the configured cwd.");
    }
    Ok(())
}

fn strict_claude_approval_block(
    message: &crate::claude::wire::MessageFrame,
    is_candidate: impl Fn(&Value) -> bool,
) -> anyhow::Result<Option<crate::claude::wire::ContentBlock>> {
    let Some(raw_blocks) = message.message.content.as_array() else {
        if is_candidate(&message.message.content) {
            bail!("Claude approval capture observed malformed approval message content.");
        }
        return Ok(None);
    };
    if !raw_blocks.iter().any(&is_candidate) {
        return Ok(None);
    }
    if raw_blocks.len() != 1 {
        bail!("Claude approval capture observed extra approval message content.");
    }
    serde_json::from_value(raw_blocks[0].clone())
        .map(Some)
        .map_err(|_| anyhow!("Claude approval capture observed a malformed approval block."))
}

pub(in crate::capture) fn observe_claude_approval_frame(
    frame: &crate::claude::wire::Frame,
    cwd: &Path,
    state: &mut ClaudeApprovalState,
) -> anyhow::Result<Option<(String, Value)>> {
    use crate::claude::wire::Frame;

    match frame {
        Frame::Assistant(message) => {
            let Some(block) = strict_claude_approval_block(message, |block| {
                matches!(block["name"].as_str(), Some("Bash" | "Write"))
                    || (block["type"] == "tool_use"
                        && (block["input"].get("command").is_some()
                            || block["input"].get("file_path").is_some()))
            })?
            else {
                return Ok(None);
            };
            if block.kind != "tool_use" || (block.name != "Bash" && block.name != "Write") {
                bail!("Claude approval capture observed a malformed bounded tool use.");
            }
            if message.parent_tool_use_id.is_some()
                || message.message.role != "assistant"
                || block.id.trim().is_empty()
            {
                bail!("Claude approval capture observed a malformed bounded tool use.");
            }
            if block.name == "Bash" {
                if block.input != json!({"command": CLAUDE_APPROVAL_COMMAND}) {
                    bail!("Claude approval capture observed an unexpected Bash command.");
                }
                match state.bash_tool_id.as_deref() {
                    Some(id) if id != block.id => {
                        bail!("Claude approval capture observed duplicate Bash tool uses.")
                    }
                    None => state.bash_tool_id = Some(block.id.clone()),
                    _ => {}
                }
            } else {
                if !state.bash_succeeded {
                    bail!("Claude approval capture observed Write before successful Bash.");
                }
                validate_claude_marker_input(&block.input, cwd)?;
                match state.write_tool_id.as_deref() {
                    Some(_) => {
                        bail!("Claude approval capture observed duplicate Write tool uses.")
                    }
                    None => {
                        state.write_tool_id = Some(block.id.clone());
                        state.write_input = Some(block.input.clone());
                    }
                }
            }
        }
        Frame::User(message) => {
            let Some(bash_id) = state.bash_tool_id.as_deref() else {
                return Ok(None);
            };
            let Some(block) = strict_claude_approval_block(message, |block| {
                block["tool_use_id"] == bash_id
                    || (block["type"] == "tool_result" && block["tool_use_id"] == bash_id)
            })?
            else {
                return Ok(None);
            };
            if message.parent_tool_use_id.is_some()
                || message.message.role != "user"
                || block.kind != "tool_result"
                || block.tool_use_id != bash_id
                || block.is_error != Some(false)
            {
                bail!("Claude approval capture did not observe a successful Bash result.");
            }
            state.bash_succeeded = true;
        }
        Frame::ControlRequest(control) => {
            if control.request_id.trim().is_empty() {
                bail!("Claude approval request had no nonempty request identifier.");
            }
            if !state.request_ids.insert(control.request_id.clone()) {
                bail!("Claude approval request repeated a request identifier.");
            }
            if control.request.subtype != "can_use_tool"
                || control.request.tool_name != "Write"
                || !state.bash_succeeded
                || state.write_approved
                || state.write_tool_id.as_deref() != Some(control.request.tool_use_id.as_str())
                || state.write_input.as_ref() != Some(&control.request.input)
            {
                bail!("Claude approval request used an unexpected tool or order.");
            }
            validate_claude_marker_input(&control.request.input, cwd)?;
            state.write_approved = true;
            return Ok(Some((
                control.request_id.clone(),
                control.request.input.clone(),
            )));
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use comet_proto::{RunRequest, RuntimeMode};

    use crate::capture::test_support::{channel_payloads, config, fixture_path};
    use crate::capture::{
        CaptureOperation, Channel, ClaudeCaptureOperation, ClaudeRunScript, failed_session_stdin,
        record,
    };

    #[tokio::test]
    async fn claude_approval_requires_observed_bash_then_one_write_approval() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "claude-approval",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let replies: Vec<serde_json::Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 1);
        assert!(replies.iter().all(|reply| {
            reply["response"]["response"]["behavior"] == "allow"
                && reply["response"]["response"]["updatedInput"].is_object()
        }));
    }

    #[tokio::test]
    async fn claude_approval_rejects_destructive_requests_before_replying() {
        for prompt in [
            "scenario:capture-approval-destructive-command",
            "scenario:capture-approval-destructive-write",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "claude-approval-adversarial",
                fixture_path("fake-claude"),
                CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                    request,
                    script: ClaudeRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert_eq!(
                stdin.len(),
                1,
                "an unsafe request received an allow response: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    #[tokio::test]
    async fn claude_approval_rejects_deviations_from_the_observed_safe_contract() {
        for (scenario, expected_replies) in [
            ("missing-bash", 0),
            ("write-before-bash", 0),
            ("failed-bash", 0),
            ("wrong-bash", 0),
            ("duplicate-bash", 0),
            ("bash-control-response", 0),
            ("bash-malformed-extra", 0),
            ("bash-leading-text", 0),
            ("bash-trailing-text", 0),
            ("write-malformed-extra", 0),
            ("write-leading-text", 0),
            ("write-trailing-text", 0),
            ("user-malformed-extra", 0),
            ("user-leading-text", 0),
            ("user-trailing-text", 0),
            ("malformed-candidate", 0),
            ("missing-write", 0),
            ("duplicate-write", 1),
            ("missing-request-id", 0),
            ("duplicate-request-id", 1),
            ("extra-tool", 0),
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-{scenario}"),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "claude-approval-deviation",
                fixture_path("fake-claude"),
                CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                    request,
                    script: ClaudeRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            let expected_error = match scenario {
                "bash-malformed-extra"
                | "bash-leading-text"
                | "bash-trailing-text"
                | "write-malformed-extra"
                | "write-leading-text"
                | "write-trailing-text"
                | "user-malformed-extra"
                | "user-leading-text"
                | "user-trailing-text" => "extra approval message content",
                "malformed-candidate" => "malformed approval block",
                _ => "Claude approval",
            };
            assert!(error.contains(expected_error), "{scenario}: {error}");
            assert_eq!(
                stdin.len().saturating_sub(1),
                expected_replies,
                "{scenario} received an unsafe response: {stdin:?}"
            );
        }
    }

    #[tokio::test]
    async fn claude_approval_tolerates_a_repeated_snapshot_of_the_same_bash_tool() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-bash-snapshot-duplicate".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "claude-approval-snapshot",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        assert_eq!(
            channel_payloads(&capture, Channel::Stdin).len(),
            2,
            "only the Write request receives a reply"
        );
    }
}
