//! Payload assertions over promoted frames.
//!
//! Break caught: a test can name a frame that exists, sits on the right channel
//! and parses cleanly, while holding nothing like the evidence the test
//! describes. Adding three selectors one sequence off once produced a fully
//! green suite. Sequence numbers alone are not evidence; these tests assert what
//! the frames actually contain.

use comet_harness::capture::{
    Channel, corpus_frame, corpus_frame_where, corpus_root, frames, promoted_scenarios,
};
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

/// `remoteControl/status/changed` lands between initialize and the
/// `model/list` reply, so id-based matching in
/// `crates/harness/src/codex/discovery.rs` must skip notifications rather
/// than take the next frame. The `model/list` reply's `id` must equal its
/// own request's `id` — the join, restored on the stage-6 re-capture; the
/// pre-promotion corpus lost it at recording time, not at sanitizing (commit
/// history has the finding).
///
/// The initialize reply stays pinned to sequence 2: it is the direct
/// synchronous response to the initialize request at sequence 1, and nothing
/// unsolicited can land before the very first reply. The notification and
/// the `model/list` request/reply are found by predicate instead, since
/// their own sequence numbers shift between otherwise identical runs.
#[test]
fn codex_discovery_interleaves_a_notification_before_the_model_list_reply() {
    let initialize = payload(CODEX_MODEL_DISCOVERY, 2);
    let notification = corpus_frame_where(
        CODEX_MODEL_DISCOVERY,
        "a remoteControl/status/changed notification",
        |value| value["method"] == "remoteControl/status/changed",
    );
    let model_list_request = corpus_frame_where(
        CODEX_MODEL_DISCOVERY,
        "a request whose method is model/list",
        |value| value["method"] == "model/list",
    );
    let model_list_reply = corpus_frame_where(
        CODEX_MODEL_DISCOVERY,
        "a reply whose result carries a model list (result.data is an array)",
        |value| value["result"]["data"].is_array(),
    );

    assert_eq!(
        corpus_frame(CODEX_MODEL_DISCOVERY, 2).channel,
        comet_harness::capture::Channel::Stdout
    );
    assert!(
        !initialize["id"].is_null(),
        "initialize must carry a request id"
    );
    assert!(initialize["result"].is_object());

    assert_eq!(notification.value["method"], "remoteControl/status/changed");

    assert!(
        !model_list_reply.value["id"].is_null(),
        "model/list must carry a request id"
    );
    assert_ne!(
        initialize["id"], model_list_reply.value["id"],
        "initialize and model/list must be answers to two different requests"
    );
    assert_eq!(
        model_list_request.value["id"], model_list_reply.value["id"],
        "the model/list reply must join back to the model/list request's own id"
    );
    assert!(model_list_reply.value["result"]["data"].is_array());

    assert!(
        notification.sequence < model_list_reply.sequence,
        "the notification must be interleaved before the model/list reply: \
         notification at {}, model/list reply at {}",
        notification.sequence,
        model_list_reply.sequence
    );
}

/// Every JSON-RPC request id in a promoted Codex capture has a reply
/// carrying the same id on the other channel — the property that would have
/// caught the join loss the test above records.
#[test]
fn every_codex_request_id_has_a_reply_on_the_other_channel() {
    let corpus_root = corpus_root();
    let scenarios = promoted_scenarios(&corpus_root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", corpus_root.display()));
    let mut checked = 0u64;

    for scenario in scenarios
        .iter()
        .filter(|scenario| scenario.provider == "codex")
    {
        checked += 1;
        let mut requests = Vec::new();
        let mut replies = std::collections::HashSet::new();
        let events = frames(&scenario.directory)
            .unwrap_or_else(|error| panic!("{}: events.jsonl unreadable: {error}", scenario.label));
        for event in events {
            let Some(payload) = event["payload"].as_str() else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            let Some(id) = payload.get("id").filter(|id| !id.is_null()) else {
                continue;
            };
            let channel = event["channel"].as_str().unwrap_or_default().to_owned();
            if payload.get("method").is_some() {
                requests.push((
                    channel,
                    id.clone(),
                    event["sequence"].as_u64().unwrap_or_default(),
                ));
            } else if payload.get("result").is_some() || payload.get("error").is_some() {
                replies.insert((channel, id.clone()));
            }
        }

        for (channel, id, sequence) in requests {
            let other = if channel == "stdin" {
                "stdout"
            } else {
                "stdin"
            };
            assert!(
                replies.contains(&(other.to_owned(), id.clone())),
                "{} frame {sequence}: request id {id} on {channel} has no reply on {other}",
                scenario.label
            );
        }
    }

    assert!(
        checked > 0,
        "found no codex scenario under {} -- corpus walk is broken, not just empty",
        corpus_root.display()
    );
}

/// Codex acknowledged the bounded steer request before completing the same
/// active turn, and the reply's `turnId` joins the request's `expectedTurnId`.
///
/// `steer`'s own capture puts `remoteControl/status/changed` at sequence 5,
/// well before any of these three frames — but that position is exactly what
/// varies capture to capture, so every frame recorded after it, not only the
/// notification itself, is at risk of a shifted sequence on a re-capture.
/// All three are found by predicate rather than pinned.
#[test]
fn codex_acknowledges_a_steer_before_completing_the_same_turn() {
    let request = corpus_frame_where(CODEX_STEER, "method == turn/steer", |value| {
        value["method"] == "turn/steer"
    });
    let reply = corpus_frame_where(
        CODEX_STEER,
        "a reply whose result carries turnId (the steer acknowledgement)",
        |value| value["result"]["turnId"].is_string(),
    );
    let completion = corpus_frame_where(CODEX_STEER, "method == turn/completed", |value| {
        value["method"] == "turn/completed"
    });

    assert_eq!(request.value["method"], "turn/steer");
    assert_eq!(request.channel, Channel::Stdin);
    assert!(
        request.value["params"]["expectedTurnId"].is_string(),
        "the steer request must name a turn: {}",
        request.value
    );
    assert_eq!(reply.value["id"], request.value["id"]);
    assert!(reply.value["result"].is_object());
    assert_eq!(
        reply.value["result"]["turnId"],
        request.value["params"]["expectedTurnId"]
    );
    assert_eq!(completion.value["method"], "turn/completed");
    assert_eq!(
        completion.value["params"]["turn"]["id"],
        request.value["params"]["expectedTurnId"]
    );

    assert!(
        request.sequence < reply.sequence,
        "the steer reply must follow its own request: request at {}, reply at {}",
        request.sequence,
        reply.sequence
    );
    assert!(
        reply.sequence < completion.sequence,
        "the steer must be acknowledged before the turn completes: reply at {}, completion at {}",
        reply.sequence,
        completion.sequence
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
/// D73 closed at the stage-6 promotion: `.message.content[].input.taskId` is
/// redacted now, so the join is proven through `.tool_use_result.task.id`
/// (TaskCreate's own result, still allowed) instead of the input.
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
        result["tool_use_result"]["taskId"], first_created["tool_use_result"]["task"]["id"],
        "the update result must name the task TaskCreate assigned: {result}"
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
/// D73 closed at the stage-6 promotion, redacting `.message.content[].input.taskId`.
/// This scenario has no TaskCreate frame to cross-check the literal
/// `.tool_use_result.taskId` against (the task predates the resume), so the
/// join is proven the other way this corpus still can: two separate update
/// calls in the same resumed run carry the same redacted `input.taskId`
/// placeholder, meaning they name the same task.
#[test]
fn a_resumed_run_updates_a_task_it_never_created() {
    let init = payload(CHECKLIST_RESUME, 2);
    let first_call = payload(CHECKLIST_RESUME, 50);
    let second_call = payload(CHECKLIST_RESUME, 58);
    let result = payload(CHECKLIST_RESUME, 55);

    assert_eq!(init["subtype"], "init");
    for key in ["tasks", "todos", "plan"] {
        assert!(
            init.get(key).is_none(),
            "a resumed init restates no task list, but carried {key}: {init}"
        );
    }
    assert_eq!(
        first_call["message"]["content"][0]["input"]["taskId"],
        second_call["message"]["content"][0]["input"]["taskId"],
        "two update calls in the same resumed run must name the same task: \
         first={first_call} second={second_call}"
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
