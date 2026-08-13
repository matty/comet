mod claude;
mod codex;
mod common;

pub(super) use claude::{ClaudeApprovalState, observe_claude_approval_frame};
pub(super) use codex::{
    CodexApprovalState, CodexOnRequestState, observe_codex_approval_file_item,
    observe_codex_approval_routine_item, observe_codex_approval_turn_started,
    validate_codex_approval_command_event, validate_codex_approval_lifecycle,
    validate_codex_approval_request, validate_codex_on_request_approval, validate_on_request_item,
};
pub(super) use common::{
    DirectoryIdentity, FileIdentity, repository_root, require_empty_approval_target,
    resolve_trusted_powershell, validate_on_request_preflight, validate_ordinary_approval_cwd,
    validate_ordinary_approval_marker,
};
pub use common::{
    approval_marker_command, approval_on_request_prompt, claude_approval_prompt,
    codex_approval_prompt,
};
