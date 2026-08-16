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

fn payload(scenario: &str, sequence: u64) -> Value {
    serde_json::from_str(&corpus_frame(scenario, sequence).payload)
        .unwrap_or_else(|error| panic!("{scenario} frame {sequence} is not JSON: {error}"))
}

/// After initialize succeeds, `remoteControl/status/changed` is interleaved
/// before the `model/list` response — which is why ID-based reply matching in
/// `crates/harness/src/codex/discovery.rs` must skip notifications rather than
/// take the next frame.
///
/// The literal request ids (`1`, `2`) were incidental to that claim, and
/// `.id` is not on `codex.txt` (the allowlist-sanitizer stage), so the
/// archive now holds placeholders there. What actually matters — that
/// `initialize` and `model/list` are two DIFFERENT replies, not the same
/// frame read twice — survives as an inequality instead of an equality.
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
