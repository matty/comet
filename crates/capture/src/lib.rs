//! Recording, sanitizing and reading provider captures.
//!
//! Test tooling: nothing here is on Comet's runtime path, and this crate
//! answers "does production ever reach it" structurally rather than by
//! convention — `apps/comet` does not depend on `comet-capture` at all, so
//! the recorder, sanitizer, corpus reader and sheet generator cannot link
//! into `comet.exe` regardless of what any module here is marked `pub`.
//! `comet-harness` depends on this crate only as a **dev-dependency**
//! (`#[cfg(test)]` unit-test reads of a promoted corpus frame — its
//! `fake-claude` [[bin]] fixture is the one thing that *cannot* use it, since
//! Cargo never links a bin target's dev-dependencies), and this crate depends
//! on `comet-harness` normally, to build launches from its production types
//! (`comet_harness::launch::LaunchDescriptor` and friends). D87 stage 7.
//!
//! Formerly a `pub mod capture;` inside `comet-harness` itself; extracted so
//! the compile graph proves the boundary instead of a doc comment asserting
//! it. See `docs/debt/D87-capture-stays-coupled-to-harness.md`.

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
    EXPLORATORY_MARKER_FILENAME, FoundFrame, Frame, PromotedScenario, corpus_frame,
    corpus_frame_where, corpus_root, exploratory_root, frame, frames, promoted_scenarios,
};
pub use record::SESSION_ID_FILE;
pub use record::{Requirements, SCENARIOS, ScenarioSpec, record, scenario};
pub use sanitize::{
    NovelPath, PATH_ROOT_PLACEHOLDERS, SanitizationError, SanitizationReport, is_placeholder_token,
    render_escaped_paths_report, render_novel_paths_report, sanitize_dir,
};
pub use sheet::{SheetScenario, render_sheet};
pub use surface::{
    Direction, FieldObservation, FrameRef, MAP_PATHS, MapPath, SurfaceError, SuspectedMap,
    VOCABULARY_PATHS, Vocabulary, escape_path_segment, is_identifier_shaped, is_map_path,
    is_named_map_child, observe_surface, suspected_map,
};
pub use types::{
    CaptureConfig, CaptureEvent, Channel, CommandSnapshot, PlatformMetadata, Provider, RawCapture,
    RedactionRoots, corpus_provider_name,
};
