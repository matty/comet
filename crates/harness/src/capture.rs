//! Recording, sanitizing and reading provider captures.
//!
//! Test tooling. Nothing here is on Comet's runtime path — the launch types
//! that are live in [`crate::launch`], which this module consumes rather than
//! owns.

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
pub use corpus::{Frame, corpus_frame, frame};
pub use recording::record;
pub use sanitize::{SanitizationError, SanitizationReport, sanitize_dir};
pub use surface::{
    Direction, FieldObservation, FrameRef, SurfaceError, observe_corpus, observed_field_lines,
};
pub use types::{
    CaptureConfig, CaptureEvent, CaptureOperation, CaptureScenario, Channel,
    ClaudeCaptureOperation, ClaudeRunScript, CodexCaptureOperation, CodexRunScript,
    CommandSnapshot, PlatformMetadata, Provider, RawCapture, RedactionRoots,
};
