//! Payload assertions over promoted frames.
//!
//! Break caught: a test can name a frame that exists, sits on the right channel
//! and parses cleanly, while holding nothing like the evidence the test
//! describes. Adding three selectors one sequence off once produced a fully
//! green suite. Sequence numbers alone are not evidence; these tests assert what
//! the frames actually contain.

use comet_harness::capture::{Channel, corpus_frame};
use serde_json::Value;

const CODEX_MODEL_DISCOVERY: &str = "codex/0.147.0/model-discovery";
const CODEX_STEER: &str = "codex/0.147.0/steer";
const CHECKLIST: &str = "claude/2.1.229/checklist";
const CHECKLIST_RESUME: &str = "claude/2.1.229/checklist-resume";
const SUBAGENT: &str = "claude/2.1.229/subagent";

fn payload(scenario: &str, sequence: u64) -> Value {
    serde_json::from_str(&corpus_frame(scenario, sequence).payload)
        .unwrap_or_else(|error| panic!("{scenario} frame {sequence} is not JSON: {error}"))
}

/// After initialize succeeds, `remoteControl/status/changed` is interleaved
/// before the `model/list` response — which is why ID-based reply matching in
/// `crates/harness/src/codex/discovery.rs` must skip notifications rather than
/// take the next frame.
///
/// This is an inequality rather than the equality it should be, and the
/// reason recorded here until 2026-08-16 was **wrong**. It blamed the
/// allowlist: `.id` is not on `codex.txt`, so the archive holds placeholders
/// there, and the join was said to be unrecoverable.
///
/// Redaction does not lose the join. Equal values share a placeholder by
/// design, and `steer` demonstrates it — its `initialize` request and reply
/// both read `<V1>`. This scenario reads `<V1>` on the request and `<V2>` on
/// the reply, which means the two ids **differed before sanitizing**: the
/// recording lost the join, not the sanitizer.
///
/// Six of the seven committed Codex scenarios have zero request-to-reply id
/// joins; only `steer` (recorded in a later change) has all four. The current
/// recorder is not affected — a live `model-discovery` taken on 2026-08-16
/// joins `id=1` and `id=2` across stdin and stdout correctly.
///
/// So this assertion is weaker than the evidence should support, and the fix
/// is a re-capture rather than an edit here. **Restore the equality when
/// stage 6 re-records Codex**, and add the property test that would have
/// caught it: every request id in a Codex capture appears on both channels.
#[test]
fn codex_discovery_interleaves_a_notification_before_the_model_list_reply() {
    let initialize = payload(CODEX_MODEL_DISCOVERY, 2);
    let notification = payload(CODEX_MODEL_DISCOVERY, 3);
    let model_list = payload(CODEX_MODEL_DISCOVERY, 6);

    assert_eq!(
        corpus_frame(CODEX_MODEL_DISCOVERY, 2).channel,
        comet_harness::capture::Channel::Stdout
    );
    assert!(
        !initialize["id"].is_null(),
        "initialize must carry a request id"
    );
    assert!(initialize["result"].is_object());

    assert_eq!(notification["method"], "remoteControl/status/changed");

    assert!(
        !model_list["id"].is_null(),
        "model/list must carry a request id"
    );
    assert_ne!(
        initialize["id"], model_list["id"],
        "initialize and model/list must be answers to two different requests"
    );
    assert!(model_list["result"]["data"].is_array());
}

/// Codex acknowledged the bounded steer request before completing the same
/// active turn, and the reply's `turnId` joins the request's `expectedTurnId`.
#[test]
fn codex_acknowledges_a_steer_before_completing_the_same_turn() {
    let request = payload(CODEX_STEER, 19);
    let reply = payload(CODEX_STEER, 20);
    let completion = payload(CODEX_STEER, 56);

    assert_eq!(request["method"], "turn/steer");
    assert_eq!(corpus_frame(CODEX_STEER, 19).channel, Channel::Stdin);
    assert!(
        request["params"]["expectedTurnId"].is_string(),
        "the steer request must name a turn: {request}"
    );
    assert_eq!(reply["id"], request["id"]);
    assert!(reply["result"].is_object());
    assert_eq!(
        reply["result"]["turnId"],
        request["params"]["expectedTurnId"]
    );
    assert_eq!(completion["method"], "turn/completed");
    assert_eq!(
        completion["params"]["turn"]["id"],
        request["params"]["expectedTurnId"]
    );
}

/// `TaskCreate`'s `tool_use_result` carries the assigned task id and its
/// subject; the id appears nowhere on the tool input, so a decode reading only
/// the input cannot key the item.
#[test]
fn task_create_puts_the_assigned_id_only_on_the_result() {
    let call = payload(CHECKLIST, 55);
    let result = payload(CHECKLIST, 64);

    let tool_call = &call["message"]["content"][0];
    assert_eq!(tool_call["name"], "TaskCreate");
    assert!(
        tool_call["input"].get("taskId").is_none() && tool_call["input"].get("id").is_none(),
        "the create call must carry no id of its own: {tool_call}"
    );
    assert_eq!(result["tool_use_result"]["task"]["id"], "1");
    assert!(
        result["tool_use_result"]["task"]["subject"].is_string(),
        "the result echoes the subject"
    );
}

/// `TaskUpdate`'s `tool_use_result` reports an explicit `statusChange`
/// `{from,to}`, while `activeForm` appears only on the tool input, so neither
/// frame alone describes the change.
///
/// `.message.content[].input.taskId` is one of D73's seven tool-argument
/// union paths (`docs/debt/D73-tool-argument-union-paths.md`): it is
/// allowlisted whole, so its literal value is published as-is rather than
/// replaced by a placeholder. This assertion no longer depends on that —
/// it checks the *join* (the update call names the task `TaskCreate`
/// assigned, and that differs from the corpus's other task) rather than the
/// literal string, so the seven lines can be dropped from `claude.txt` at
/// the next promotion without breaking this test.
#[test]
fn task_update_splits_status_change_and_active_form_across_two_frames() {
    let first_created = payload(CHECKLIST, 64);
    let second_created = payload(CHECKLIST, 65);
    let call = payload(CHECKLIST, 88);
    let result = payload(CHECKLIST, 93);

    assert_ne!(
        first_created["tool_use_result"]["task"]["id"],
        second_created["tool_use_result"]["task"]["id"],
        "the corpus's two created tasks must carry distinct ids, or the equality \
         check below would pass for the wrong reason"
    );

    let input = &call["message"]["content"][0]["input"];
    assert_eq!(
        input["taskId"], first_created["tool_use_result"]["task"]["id"],
        "the update call must name the task TaskCreate assigned: {input}"
    );
    assert!(
        input["activeForm"].is_string(),
        "activeForm rides the input: {input}"
    );

    let result = &result["tool_use_result"];
    assert!(
        result.get("activeForm").is_none(),
        "and never the result: {result}"
    );
    assert_eq!(result["statusChange"]["from"], "pending");
    assert_eq!(result["statusChange"]["to"], "in_progress");
}

/// A resumed Claude process restates no task list at init, and its first task
/// frame updates an id it never created — so a per-run accumulator receives a
/// status change for an unknown item.
///
/// Same D73 caveat as the test above: `.message.content[].input.taskId` and
/// `.tool_use_result.taskId` are both union paths whose literal is published
/// as-is, not a placeholder. This asserts the join instead — the update call
/// and its own result must name the same task — so dropping the seven
/// allowlist lines at the next promotion (turning the value into a
/// placeholder) leaves this test unaffected.
#[test]
fn a_resumed_run_updates_a_task_it_never_created() {
    let init = payload(CHECKLIST_RESUME, 2);
    let call = payload(CHECKLIST_RESUME, 50);
    let result = payload(CHECKLIST_RESUME, 55);

    assert_eq!(init["subtype"], "init");
    for key in ["tasks", "todos", "plan"] {
        assert!(
            init.get(key).is_none(),
            "a resumed init restates no task list, but carried {key}: {init}"
        );
    }
    assert_eq!(
        call["message"]["content"][0]["input"]["taskId"], result["tool_use_result"]["taskId"],
        "the update call and its result must name the same task: call={call} result={result}"
    );
    assert_eq!(result["tool_use_result"]["statusChange"]["from"], "pending");
}

/// The `Agent` tool_use that spawns a subagent and its `task_started` reading
/// join on one id: the tool_use's own `id` is the system frame's
/// `tool_use_id`. `fake_claude.rs`'s `happy()` fixture hand-types this same
/// join with readable stand-ins (`"sub-1"` for both); this proves the join
/// the fixture claims to mirror actually holds on the genuine bytes, not just
/// on an author's guess at their shape.
#[test]
fn subagent_tool_use_joins_its_own_task_started() {
    let spawn = payload(SUBAGENT, 115);
    let started = payload(SUBAGENT, 116);

    let tool_use = &spawn["message"]["content"][0];
    assert_eq!(tool_use["name"], "Agent");
    assert!(
        tool_use["input"]["subagent_type"].is_string(),
        "the spawn call must carry a subagent_type: {tool_use}"
    );
    assert_eq!(started["subtype"], "task_started");
    assert_eq!(
        tool_use["id"], started["tool_use_id"],
        "task_started must name the tool_use_id the Agent call was assigned: \
         spawn={tool_use} started={started}"
    );
}

/// `task_progress` and both terminal `task_notification` readings carry a
/// `usage` object with `total_tokens`/`tool_uses`/`duration_ms` — the field
/// set `fake_claude.rs`'s hand-typed literals reproduce with real numbers.
/// The literal numbers themselves are not checked here (nor loadable at all:
/// none of `.usage.total_tokens`/`.usage.tool_uses`/`.usage.duration_ms`
/// under a `task_progress`/`task_notification` frame is on
/// `capture/allowlist/claude.txt`, so the real values are `<Vn>`
/// placeholders in this corpus) — this proves the *shape* the fixture claims
/// against the genuine bytes, which is what is actually checkable.
#[test]
fn subagent_progress_and_notification_carry_a_usage_object() {
    let progress = payload(SUBAGENT, 121);
    let notification = payload(SUBAGENT, 125);

    assert_eq!(progress["subtype"], "task_progress");
    for key in ["total_tokens", "tool_uses", "duration_ms"] {
        assert!(
            progress["usage"].get(key).is_some(),
            "task_progress usage missing {key}: {progress}"
        );
    }
    assert!(
        progress["last_tool_name"].is_string(),
        "task_progress must name the tool last run: {progress}"
    );

    assert_eq!(notification["subtype"], "task_notification");
    for key in ["total_tokens", "tool_uses", "duration_ms"] {
        assert!(
            notification["usage"].get(key).is_some(),
            "task_notification usage missing {key}: {notification}"
        );
    }
    assert_eq!(notification["status"], "completed");
}

/// A SendMessage-resumed subagent invocation reuses the FIRST invocation's
/// `task_id` under a brand new `tool_use_id` — exactly the shape
/// `fake_claude.rs`'s `happy()` fixture exercises with its own second
/// `task_started` (same `"sub-1-task"`, new `"sub-2"`), which is what proves
/// `normalize.rs`'s `subagent_progress.remove(&f.task_id)` on `task_started`
/// through a real spawn: without it, the resumed terminal reading would be
/// compared against the first invocation's already-terminal one and dropped
/// as redundant even though the summary differs. This test is the corpus-side
/// half of that claim — that the real provider actually resumes this way, not
/// just that the fixture's hand-typed replay of it decodes correctly.
#[test]
fn a_resumed_subagent_task_started_reuses_the_task_id_under_a_new_tool_use_id() {
    let first_started = payload(SUBAGENT, 116);
    let resumed_started = payload(SUBAGENT, 174);
    let resumed_updated = payload(SUBAGENT, 201);
    let resumed_notification = payload(SUBAGENT, 202);

    assert_eq!(first_started["subtype"], "task_started");
    assert_eq!(resumed_started["subtype"], "task_started");
    assert_eq!(
        first_started["task_id"], resumed_started["task_id"],
        "a resumed invocation must reuse the first invocation's task_id: \
         first={first_started} resumed={resumed_started}"
    );
    assert_ne!(
        first_started["tool_use_id"], resumed_started["tool_use_id"],
        "a resumed invocation must NOT reuse the first tool_use_id, or this \
         test would pass without the resumption ever happening: \
         first={first_started} resumed={resumed_started}"
    );

    assert_eq!(resumed_updated["subtype"], "task_updated");
    assert_eq!(resumed_updated["task_id"], first_started["task_id"]);

    assert_eq!(resumed_notification["subtype"], "task_notification");
    assert_eq!(resumed_notification["task_id"], first_started["task_id"]);
    assert_eq!(
        resumed_notification["tool_use_id"], resumed_started["tool_use_id"],
        "the resumed notification must name the RESUMED tool_use_id, not the \
         first invocation's: notification={resumed_notification} \
         resumed_started={resumed_started}"
    );
}
