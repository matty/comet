//! Recording, sanitizing and reading provider captures.
//!
//! Test tooling: nothing here is on Comet's runtime path. The launch types
//! that *do* sit on it live in [`crate::launch`], which this module consumes
//! rather than owns.
//!
//! The module is nonetheless unconditionally `pub`, not `cfg(test)`, so it
//! compiles into the binary. The recorder inside it (`record/`) is now
//! provider-neutral; moving `capture/` behind its own crate boundary is a
//! later stage's scope, not blocked on anything left here — the `pub(super)`
//! visibility used throughout `record/` is what keeps that extraction
//! mechanical when it happens.

mod allowlist;
mod corpus;
mod filesystem;
mod record;
mod safety;
mod sanitize;
mod sheet;
mod surface;
#[cfg(test)]
mod test_support;
mod types;

pub use allowlist::{allows, allows_prefix, named_kind};
pub use corpus::{
    FoundFrame, Frame, PromotedScenario, corpus_frame, corpus_frame_where, corpus_root, frame,
    frames, promoted_scenarios,
};
pub use record::{Requirements, SCENARIOS, ScenarioSpec, record, scenario};
pub use sanitize::{
    NovelPath, SanitizationError, SanitizationReport, render_novel_paths_report, sanitize_dir,
};
pub use sheet::{SheetScenario, render_sheet};
pub use surface::{
    Direction, FieldObservation, FrameRef, MAP_PATHS, SurfaceError, VOCABULARY_PATHS, Vocabulary,
    observe_surface,
};
pub use types::{
    CaptureConfig, CaptureEvent, Channel, CommandSnapshot, PlatformMetadata, Provider, RawCapture,
    RedactionRoots,
};
