use super::support::*;

use std::path::Path;

use comet_harness::capture::{CorpusError, validate_corpus};
use serde_json::Value;

/// Break caught: a promoted token-free selector can exist in the index while its reviewed
/// manifest, frame, reciprocal consumer, or placeholder accounting is invalid.
#[test]
fn corpus_promoted_token_free_claims_are_valid() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    let promoted: std::collections::BTreeSet<&str> = index["claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|claim| {
            claim["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|evidence| {
                    !evidence["manifest"]
                        .as_str()
                        .unwrap()
                        .contains("pending-observed-version")
                })
        })
        .map(|claim| claim["id"].as_str().unwrap())
        .collect();
    assert!(
        !promoted.is_empty(),
        "every claim reads as pending; the promoted filter is inverted"
    );

    let errors = validate_corpus(&corpus_root);
    assert!(
        errors.iter().all(|error| matches!(
            error,
            CorpusError::MissingManifest { claim_id, manifest }
                if !promoted.contains(claim_id.as_str())
                    && manifest.contains("pending-observed-version")
        )),
        "a promoted token-free claim is invalid: {errors:#?}"
    );
}

/// Break caught: either ordering claim selects only the final reply, reverses the observed
/// neighbors, or points at a cwd variant instead of the reviewed base observation.
#[test]
fn corpus_inventory_pins_the_observed_codex_discovery_order() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    for claim_id in [
        "codex-model-notification-order",
        "codex-model-source-notification-order",
    ] {
        let claim = index["claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|claim| claim["id"] == claim_id)
            .unwrap();
        assert_eq!(
            claim["evidence"],
            serde_json::json!([{
                "manifest": "codex/0.147.0/model-discovery/manifest.json",
                "frames": [
                    {"sequence": 2, "channel": "stdout"},
                    {"sequence": 3, "channel": "stdout"},
                    {"sequence": 6, "channel": "stdout"}
                ]
            }]),
            "wrong observed selector for {claim_id}"
        );
        if claim_id == "codex-model-source-notification-order" {
            assert_eq!(
                claim["consumer"], "crates/harness/src/codex/discovery.rs:reply_to",
                "ordering rationale must name the actual reply matcher"
            );
        }
    }
}

/// Break caught: selector numbers alone do not prove that the promoted neighboring frames are
/// the initialize response, unsolicited remote-control notification, and model-list response.
#[test]
fn corpus_promoted_codex_ordering_frames_have_the_observed_payloads() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let events: Vec<Value> =
        std::fs::read_to_string(corpus_root.join("codex/0.147.0/model-discovery/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

    for claim_id in [
        "codex-model-notification-order",
        "codex-model-source-notification-order",
    ] {
        let payload = |sequence| {
            let event = events
                .iter()
                .find(|event| event["sequence"] == sequence)
                .unwrap_or_else(|| panic!("{claim_id}: missing promoted frame {sequence}"));
            assert_eq!(event["channel"], "stdout", "{claim_id}: frame {sequence}");
            serde_json::from_str::<Value>(event["payload"].as_str().unwrap()).unwrap()
        };
        let initialize = payload(2);
        let notification = payload(3);
        let model_list = payload(6);

        assert_eq!(initialize["id"], 1, "{claim_id}");
        assert!(initialize["result"].is_object(), "{claim_id}");
        assert_eq!(
            notification["method"], "remoteControl/status/changed",
            "{claim_id}"
        );
        assert_eq!(model_list["id"], 2, "{claim_id}");
        assert!(model_list["result"]["data"].is_array(), "{claim_id}");
    }
}

/// Break caught: a valid three-frame selector can point at unrelated neighboring notifications
/// without proving the steer request, its matching success reply, and the same turn's completion.
#[test]
fn corpus_promoted_codex_steer_frames_have_the_observed_payloads() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let steer = claim_payloads(&corpus_root, "codex-steer-reply-before-completion");

    assert_eq!(steer.len(), 3);
    assert_eq!(steer[0].0, "stdin");
    assert_eq!(steer[0].1["method"], "turn/steer");
    assert_eq!(steer[1].0, "stdout");
    assert_eq!(steer[1].1["id"], steer[0].1["id"]);
    assert!(steer[1].1["result"].is_object());
    assert_eq!(
        steer[1].1["result"]["turnId"],
        steer[0].1["params"]["expectedTurnId"]
    );
    assert_eq!(steer[2].0, "stdout");
    assert_eq!(steer[2].1["method"], "turn/completed");
    assert_eq!(
        steer[2].1["params"]["turn"]["id"],
        steer[0].1["params"]["expectedTurnId"]
    );
}

/// Break caught: a claim's frame selectors can drift onto neighbouring frames
/// and every structural check still passes.
///
/// Adding these three claims with a selector off by one sequence produced a
/// FULLY GREEN `--test capture_corpus` — the validator proves a selected frame
/// exists, is on a declared channel and accounts for its placeholders, but
/// nothing proves it holds the evidence the claim describes. Without a payload
/// assertion a claim is decoration, which is the same weakness a claimless
/// entry has while looking stronger.
#[test]
fn corpus_promoted_claude_checklist_frames_have_the_observed_payloads() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");

    // TaskCreate: the assigned id is on the RESULT and on no input.
    let create = claim_payloads(&corpus_root, "claude-task-create-result-shape");
    assert_eq!(create.len(), 2);
    let call = &create[0].1["message"]["content"][0];
    assert_eq!(call["name"], "TaskCreate");
    assert!(
        call["input"].get("taskId").is_none() && call["input"].get("id").is_none(),
        "the create call must carry no id of its own: {call}"
    );
    assert_eq!(create[1].1["tool_use_result"]["task"]["id"], "1");
    assert!(
        create[1].1["tool_use_result"]["task"]["subject"].is_string(),
        "the result echoes the subject"
    );

    // TaskUpdate: statusChange is on the result, activeForm only on the input.
    let update = claim_payloads(&corpus_root, "claude-task-update-status-transition");
    assert_eq!(update.len(), 2);
    let input = &update[0].1["message"]["content"][0]["input"];
    assert_eq!(input["taskId"], "1");
    assert!(
        input["activeForm"].is_string(),
        "activeForm rides the input: {input}"
    );
    let result = &update[1].1["tool_use_result"];
    assert!(
        result.get("activeForm").is_none(),
        "and never the result: {result}"
    );
    assert_eq!(result["statusChange"]["from"], "pending");
    assert_eq!(result["statusChange"]["to"], "in_progress");

    // A resumed process: nothing restated, and the first task frame moves an
    // id this process never created.
    let resumed = claim_payloads(&corpus_root, "claude-resumed-run-updates-an-uncreated-task");
    assert_eq!(resumed.len(), 3);
    let init = &resumed[0].1;
    assert_eq!(init["subtype"], "init");
    for key in ["tasks", "todos", "plan"] {
        assert!(
            init.get(key).is_none(),
            "a resumed init restates no task list, but carried {key}: {init}"
        );
    }
    assert_eq!(
        resumed[1].1["message"]["content"][0]["input"]["taskId"],
        "2"
    );
    assert_eq!(resumed[2].1["tool_use_result"]["taskId"], "2");
    assert_eq!(
        resumed[2].1["tool_use_result"]["statusChange"]["from"],
        "pending"
    );
}
