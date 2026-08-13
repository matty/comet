//! Prompts for the checklist capture scenarios.
//!
//! Both are written the way the approval prompts are: every tool call named,
//! every input spelled out, and a fixed closing word. A capture is evidence
//! only if the same instruction produces the same frames on a re-run, and a
//! prompt that leaves the model room to plan its own approach produces a
//! different shape each time.
//!
//! The subjects are deliberately content-free ("Alpha step"). They are model
//! prose and the sanitizer replaces them with placeholders, so a claim can
//! prove the field was CARRIED but never what it said — writing something
//! meaningful there would only invite a reader to assert the text.

/// Create two tasks, then drive the first through both transitions.
///
/// Opens with `ToolSearch` because the task tools are *deferred* on at least
/// one machine — captured 2026-08-13, where the model reached them through
/// `{"query":"select:TaskCreate,TaskUpdate","total_deferred_tools":45}`. On an
/// installation that lists them eagerly the search is a harmless extra frame;
/// without it, on one that does not, the run produces no checklist at all.
pub fn claude_checklist_prompt() -> String {
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

/// Continue the SAME list from a second process.
///
/// Task 2 was created by the first process, so a run driven by this prompt
/// updates an id it has never seen — the case the whole scenario exists to
/// record. It deliberately does not create anything: a `TaskCreate` here would
/// give the resumed process a subject of its own and destroy the evidence.
pub fn claude_checklist_resume_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskUpdate","max_results":5}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"in_progress","activeForm":"Working the second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"completed"}. "#,
        r#"Do not create any task. Do nothing else, and reply with the single word resumed."#,
    )
    .to_owned()
}
