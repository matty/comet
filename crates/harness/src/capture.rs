mod approval;
mod corpus;
mod recording;
mod sanitize;
#[cfg(test)]
mod test_support;
mod types;

pub use approval::{
    approval_marker_command, approval_on_request_prompt, claude_approval_prompt,
    codex_approval_prompt,
};
pub use corpus::{CorpusError, selected_payload, validate_corpus};
pub use recording::record;
pub use sanitize::{SanitizationError, SanitizationReport, sanitize_dir};
pub use types::{
    CaptureConfig, CaptureEvent, CaptureOperation, CaptureScenario, Channel,
    ClaudeCaptureOperation, ClaudeRunScript, CodexCaptureOperation, CodexRunScript,
    CommandSnapshot, PlatformMetadata, Provider, RawCapture, RedactionRoots, StdioMode,
};

pub(crate) use types::LaunchDescriptor;
