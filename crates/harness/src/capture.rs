mod approval;
mod checklist;
mod corpus;
mod filesystem;
mod recording;
mod sanitize;
mod surface;
#[cfg(test)]
mod test_support;
mod types;

pub use approval::{
    approval_marker_command, approval_on_request_prompt, claude_approval_prompt,
    codex_approval_prompt,
};
pub use checklist::{claude_checklist_prompt, claude_checklist_resume_prompt};
pub use corpus::{CorpusError, selected_payload, selected_payloads, validate_corpus};
pub use recording::record;
pub use sanitize::{SanitizationError, SanitizationReport, sanitize_dir};
pub use surface::{
    Direction, FieldObservation, FrameRef, JsonType, SurfaceError, ValueSample, observe_corpus,
};
pub use types::{
    CaptureConfig, CaptureEvent, CaptureOperation, CaptureScenario, Channel,
    ClaudeCaptureOperation, ClaudeRunScript, CodexCaptureOperation, CodexRunScript,
    CommandSnapshot, PlatformMetadata, Provider, RawCapture, RedactionRoots, StdioMode,
};

pub(crate) use types::LaunchDescriptor;
