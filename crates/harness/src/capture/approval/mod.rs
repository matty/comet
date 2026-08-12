mod claude;
mod codex;
mod common;

pub(super) use claude::{ClaudeApprovalState, observe_claude_approval_frame};
#[cfg(test)]
pub(super) use common::APPROVAL_MARKER_CONTENT;
#[cfg(windows)]
pub(super) use common::file_identity;
pub(super) use common::{
    APPROVAL_MARKER_ADD_DIFF, APPROVAL_MARKER_NAME, CODEX_APPROVAL_COMMAND, DirectoryIdentity,
    FileIdentity, repository_root, require_empty_approval_target, resolve_trusted_powershell,
    validate_on_request_preflight, validate_ordinary_approval_cwd,
    validate_ordinary_approval_marker,
};
pub use common::{
    approval_marker_command, approval_on_request_prompt, claude_approval_prompt,
    codex_approval_prompt,
};
#[cfg(all(test, windows))]
pub(super) use common::{
    canonical_protected_roots, select_trusted_powershell, windows_protected_roots,
};
