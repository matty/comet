use super::support::*;

use std::path::PathBuf;

use comet_harness::capture::{SanitizationError, sanitize_dir};
use serde_json::Value;

/// Break caught: skipping any semantic branch leaves captured identifiers, human-authored
/// content, or attachment bytes in reviewable evidence.
#[test]
fn sanitizer_replaces_semantic_values_with_typed_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "semantic-values",
        &[
            r#"{"type":"user","request_id":"claude-request-secret","session_id":"session-secret","message":{"role":"user","content":[{"type":"text","text":"my confidential prompt"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo-attachment-secret"}}]},"parent_tool_use_id":null}"#,
            r#"{"jsonrpc":"2.0","id":73,"method":"turn/start","params":{"threadId":"thread-secret","turnId":"turn-secret","input":[{"type":"text","text":"second user prompt"}]}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"itemId":"message-item-secret","delta":"codex assistant prose"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"private assistant answer"},{"type":"tool_use","id":"tool-secret","name":"Bash","input":{"command":"pwd"}}]}}"#,
            r#"{"level":"debug","message":"safe diagnostic for session-secret"}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "semantic-values")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    assert_eq!(payloads[0]["type"], "user");
    assert_eq!(payloads[0]["request_id"], "<CLAUDE_REQUEST_ID_1>");
    assert_eq!(payloads[0]["session_id"], "<SESSION_ID_1>");
    assert_eq!(
        payloads[0]["message"]["content"][0]["text"],
        "<USER_TEXT_1>"
    );
    assert_eq!(
        payloads[0]["message"]["content"][1]["source"]["data"],
        "<ATTACHMENT_BYTES_1>"
    );
    assert!(payloads[0]["parent_tool_use_id"].is_null());

    assert_eq!(payloads[1]["jsonrpc"], "2.0");
    assert_eq!(payloads[1]["method"], "turn/start");
    assert_eq!(payloads[1]["id"], "<CODEX_RPC_ID_1>");
    assert_eq!(payloads[1]["params"]["threadId"], "<THREAD_ID_1>");
    assert_eq!(payloads[1]["params"]["turnId"], "<TURN_ID_1>");
    assert_eq!(payloads[1]["params"]["input"][0]["text"], "<USER_TEXT_2>");

    assert_eq!(payloads[2]["method"], "item/agentMessage/delta");
    assert_eq!(payloads[2]["params"]["itemId"], "<TOOL_USE_ID_1>");
    assert_eq!(payloads[2]["params"]["delta"], "<ASSISTANT_PROSE_1>");

    assert_eq!(payloads[3]["type"], "assistant");
    assert_eq!(
        payloads[3]["message"]["content"][0]["text"],
        "<ASSISTANT_PROSE_2>"
    );
    assert_eq!(
        payloads[3]["message"]["content"][1]["id"],
        "<TOOL_USE_ID_2>"
    );
    assert_eq!(payloads[3]["message"]["content"][1]["name"], "Bash");
    assert_eq!(payloads[4]["message"], "<PROVIDER_PROSE_1>");

    let all_output = String::from_utf8(
        [
            report.events_bytes.as_slice(),
            report.manifest_bytes.as_slice(),
        ]
        .concat(),
    )
    .unwrap();
    for leaked in [
        "claude-request-secret",
        "session-secret",
        "my confidential prompt",
        "iVBORw0KGgo-attachment-secret",
        "thread-secret",
        "turn-secret",
        "second user prompt",
        "message-item-secret",
        "codex assistant prose",
        "private assistant answer",
        "tool-secret",
    ] {
        assert!(
            !all_output.contains(leaked),
            "sanitized output leaked {leaked}"
        );
    }
}

/// Break caught: assistant-prose inheritance rewrites a bounded tool input even though the same
/// object is repeated in the approval request and allow response, destroying the captured join.
#[test]
fn sanitizer_preserves_claude_tool_input_semantics_across_approval_frames() {
    let temp = tempfile::tempdir().unwrap();
    let input = serde_json::json!({
        "content": "capture\n",
        "file_path": r"C:\captured-cwd\capture-marker.txt",
        "metadata": {
            "session_id": "nested-session-secret",
            "note": r"C:\captured-cwd\private-note.txt"
        }
    });
    let events = [
        serde_json::json!({
            "type":"assistant",
            "message":{"role":"assistant","content":[
                {"type":"text","text":"private assistant answer"},
                {"type":"tool_use","id":"tool-write","name":"Write","input":input}
            ]}
        })
        .to_string(),
        serde_json::json!({
            "type":"control_request",
            "request_id":"request-write",
            "request":{"subtype":"can_use_tool","tool_name":"Write","tool_use_id":"tool-write","input":input}
        })
        .to_string(),
        serde_json::json!({
            "type":"control_response",
            "response":{"request_id":"request-write","response":{"behavior":"allow","updatedInput":input}}
        })
        .to_string(),
        serde_json::json!({
            "type":"assistant",
            "message":{"role":"assistant","content":[{
                "type":"other","input":{"content":"unrelated assistant prose"}
            }]}
        })
        .to_string(),
    ];
    let refs: Vec<_> = events.iter().map(String::as_str).collect();
    let raw = write_raw_capture(temp.path(), "claude-tool-input", &refs);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["cwd"] = Value::String(r"C:\captured-cwd".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-tool-input")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    let tool_input = &payloads[0]["message"]["content"][1]["input"];
    assert_eq!(tool_input, &payloads[1]["request"]["input"]);
    assert_eq!(
        tool_input,
        &payloads[2]["response"]["response"]["updatedInput"]
    );
    assert_eq!(tool_input["content"], "capture\n");
    assert_eq!(tool_input["file_path"], r"<CWD>\capture-marker.txt");
    assert_eq!(tool_input["metadata"]["session_id"], "<SESSION_ID_1>");
    assert_eq!(tool_input["metadata"]["note"], r"<CWD>\private-note.txt");
    assert_eq!(
        payloads[0]["message"]["content"][0]["text"],
        "<ASSISTANT_PROSE_1>"
    );
    assert_eq!(
        payloads[3]["message"]["content"][0]["input"]["content"],
        "<ASSISTANT_PROSE_2>"
    );

    let codex_raw = write_raw_capture(
        temp.path(),
        "codex-tool-like-input",
        &[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"content":"codex private prose"}}]}}"#,
        ],
    );
    let codex_path = codex_raw.join("capture.json");
    let mut codex_capture: Value =
        serde_json::from_slice(&std::fs::read(&codex_path).unwrap()).unwrap();
    codex_capture["provider"] = Value::String("codex".into());
    std::fs::write(
        &codex_path,
        serde_json::to_vec_pretty(&codex_capture).unwrap(),
    )
    .unwrap();
    let codex = sanitize_dir(
        &codex_raw,
        &staging_dir(temp.path(), "codex-tool-like-input"),
    )
    .unwrap();
    assert_eq!(
        sanitized_payloads(&codex.events_bytes)[0]["message"]["content"][0]["input"]["content"],
        "<ASSISTANT_PROSE_1>"
    );
}

/// Break caught: Claude discovery can embed locally authored command and agent descriptions,
/// including machine paths, even though those prose fields are irrelevant to corpus claims.
#[test]
fn sanitizer_replaces_discovery_descriptions_as_provider_prose() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "discovery-provider-prose",
        &[
            r#"{"type":"control_response","response":{"subtype":"success","response":{"commands":[{"name":"safe-command","description":"private command prose at D:\\private\\command.md","argumentHint":"<private-path>"},{"name":"no-argument-command","description":"second private command prose","argumentHint":""}],"agents":[{"name":"safe-agent","description":"private agent prose at /private/agent.md"}],"models":[{"value":"safe-model","description":"private model prose"}]}}}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "discovery-provider-prose")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    let reply = &payloads[0]["response"]["response"];

    for value in [
        &reply["agents"][0]["description"],
        &reply["commands"][0]["argumentHint"],
        &reply["commands"][0]["description"],
        &reply["models"][0]["description"],
    ] {
        assert!(
            value
                .as_str()
                .is_some_and(|value| value.starts_with("<PROVIDER_PROSE_")),
            "provider prose was not typed: {value}"
        );
    }
    assert_eq!(reply["commands"][1]["argumentHint"], "");
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["provider_prose"], 5);
}

/// Break caught: non-bare discovery publishes local hook output, identifiers, or paths even
/// though hook lifecycle frames are needed only as neighboring protocol evidence.
#[test]
fn sanitizer_replaces_hook_output_and_identifiers() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "hook-output",
        &[
            r#"{"type":"system","subtype":"hook_response","hook_id":"hook-private","uuid":"uuid-private","pid":4812,"session_id":"session-private","output":"local hook output D:\\private\\hook.txt","stdout":"local hook output D:\\private\\hook.txt","stderr":""}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "hook-output")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];

    assert_eq!(payload["hook_id"], "<TOOL_USE_ID_1>");
    let uuid = payload["uuid"].as_str().unwrap();
    let pid = payload["pid"].as_str().unwrap();
    assert!(uuid.starts_with("<MACHINE_ID_"));
    assert!(pid.starts_with("<MACHINE_ID_"));
    assert_ne!(uuid, pid);
    assert_eq!(payload["session_id"], "<SESSION_ID_1>");
    assert_eq!(payload["output"], "<PROVIDER_PROSE_1>");
    assert_eq!(payload["stdout"], "<PROVIDER_PROSE_1>");
}

/// Break caught: free-form provider stderr can reveal ambient account/auth configuration even
/// when it contains no credential value, and has no literal corpus consumer.
#[test]
fn sanitizer_replaces_free_form_provider_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "provider-stderr",
        &[r#"{"type":"control_response"}"#, "ambient account warning"],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "provider-stderr")).unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&report.events_bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[1]["payload"], "<PROVIDER_PROSE_1>");
}

/// Break caught: Codex initialize/model-list metadata can publish the local server name and
/// provider-authored catalog/Nux prose, including paths emitted by a logged-out CLI.
#[test]
fn sanitizer_replaces_codex_server_name_and_catalog_prose() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-discovery-prose",
        &[
            r#"{"method":"account/login/completed","params":{"serverName":"private-host","installationId":"private-installation"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"data":[{"id":"model","description":"private model prose","availabilityNux":{"message":"private Nux path D:\\private\\nux.md"}}]}}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-discovery-prose")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    let installation = payloads[0]["params"]["installationId"].as_str().unwrap();
    let server = payloads[0]["params"]["serverName"].as_str().unwrap();
    assert!(installation.starts_with("<MACHINE_ID_"));
    assert!(server.starts_with("<MACHINE_ID_"));
    assert_ne!(installation, server);
    let description = payloads[1]["result"]["data"][0]["description"]
        .as_str()
        .unwrap();
    let nux = payloads[1]["result"]["data"][0]["availabilityNux"]["message"]
        .as_str()
        .unwrap();
    assert!(description.starts_with("<PROVIDER_PROSE_"));
    assert!(nux.starts_with("<PROVIDER_PROSE_"));
    assert_ne!(description, nux);
}

/// Break caught: treating every bare `id` as structural lets actual Codex thread, turn, and item
/// objects retain provider-generated identifiers when those IDs are nested below named entities.
#[test]
fn sanitizer_redacts_nested_codex_thread_turn_and_every_item_id() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "nested-codex-ids",
        &[
            r#"{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"thread-secret"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"turn":{"id":"turn-secret"}}}"#,
            r#"{"method":"item/started","params":{"item":{"id":"message-secret","type":"agentMessage"}}}"#,
            r#"{"method":"item/completed","params":{"item":{"id":"todo-secret","type":"todoList","items":[]}}}"#,
            r#"{"method":"item/started","params":{"item":{"id":"compaction-secret","type":"contextCompaction"}}}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "nested-codex-ids")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["result"]["thread"]["id"], "<THREAD_ID_1>");
    assert_eq!(payloads[1]["result"]["turn"]["id"], "<TURN_ID_1>");
    assert_eq!(payloads[2]["params"]["item"]["id"], "<TOOL_USE_ID_1>");
    assert_eq!(payloads[3]["params"]["item"]["id"], "<TOOL_USE_ID_2>");
    assert_eq!(payloads[4]["params"]["item"]["id"], "<TOOL_USE_ID_3>");
    let output = String::from_utf8(report.events_bytes).unwrap();
    for leaked in [
        "thread-secret",
        "turn-secret",
        "message-secret",
        "todo-secret",
        "compaction-secret",
    ] {
        assert!(!output.contains(leaked));
    }
}

/// Break caught: `turn/steer` carries the active turn under `expectedTurnId`, so recognizing only
/// the ordinary `turnId` spelling publishes a provider-generated identifier and breaks the join.
#[test]
fn sanitizer_redacts_codex_steer_expected_turn_id() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-steer-turn-id",
        &[
            r#"{"method":"turn/started","params":{"turn":{"id":"turn-secret"}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"turn/steer","params":{"expectedTurnId":"turn-secret","input":[]}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-steer-turn-id")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    assert_eq!(payloads[0]["params"]["turn"]["id"], "<TURN_ID_1>");
    assert_eq!(payloads[1]["params"]["expectedTurnId"], "<TURN_ID_1>");
    assert!(
        !String::from_utf8(report.events_bytes)
            .unwrap()
            .contains("turn-secret")
    );
}

/// Break caught: Codex app-server replies can omit `jsonrpc`, so recognizing request IDs only on
/// objects with that field leaves the numeric reply ID raw and destroys the request/reply join.
#[test]
fn sanitizer_reuses_codex_rpc_ids_in_root_replies_without_jsonrpc() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-root-reply-id",
        &[
            r#"{"jsonrpc":"2.0","id":4,"method":"turn/steer","params":{}}"#,
            r#"{"id":4,"result":{"turnId":"turn-secret"}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-root-reply-id")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    assert_eq!(payloads[0]["id"], "<CODEX_RPC_ID_1>");
    assert_eq!(payloads[1]["id"], "<CODEX_RPC_ID_1>");
}

/// Break caught: steer input is user-authored turn input just like `turn/start` input, but a
/// method-specific context check leaves the bounded steering message in published evidence.
#[test]
fn sanitizer_redacts_codex_steer_input_as_user_text() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-steer-input",
        &[
            r#"{"jsonrpc":"2.0","id":4,"method":"turn/steer","params":{"input":[{"type":"text","text":"private steering message"}]}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-steer-input")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];

    assert_eq!(payload["params"]["input"][0]["text"], "<USER_TEXT_1>");
    assert!(
        !String::from_utf8(report.events_bytes)
            .unwrap()
            .contains("private steering message")
    );
}

/// Break caught: a completed Codex reasoning item stores assistant prose as strings inside its
/// `summary` array, where key-based scalar classification no longer has the `summary` field name.
#[test]
fn sanitizer_redacts_codex_reasoning_summary_array_prose() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-reasoning-summary",
        &[
            r#"{"method":"item/completed","params":{"item":{"id":"reasoning-id","type":"reasoning","summary":["private reasoning summary"],"content":[]}}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-reasoning-summary")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];

    assert_eq!(
        payload["params"]["item"]["summary"][0],
        "<ASSISTANT_PROSE_1>"
    );
    assert!(
        !String::from_utf8(report.events_bytes)
            .unwrap()
            .contains("private reasoning summary")
    );
}

/// Break caught: allowing a known local root through unchanged, or failing to replace it when
/// embedded in prose, exposes machine-specific paths while making regeneration non-portable.
#[test]
fn sanitizer_replaces_allowlisted_paths_in_values_and_embedded_text() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cwd = repo.join("working-dir");
    std::fs::create_dir_all(&cwd).unwrap();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap();
    let system_temp = std::env::temp_dir();
    let raw = write_raw_capture(
        temp.path(),
        "known-paths",
        &[&format!(
            r#"{{"type":"system","subtype":"init","cwd":{},"note":{},"repo_note":{},"home_note":{},"temp_note":{}}}"#,
            serde_json::to_string(&cwd).unwrap(),
            serde_json::to_string(&format!("opened {}\\src\\main.rs", cwd.display())).unwrap(),
            serde_json::to_string(&format!("repo {}\\Cargo.toml", repo.display())).unwrap(),
            serde_json::to_string(&format!("home {}\\profile", home.display())).unwrap(),
            serde_json::to_string(&format!("temp {}\\capture", system_temp.display())).unwrap()
        )],
    );
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["command"]["cwd"] = Value::String(cwd.display().to_string());
    capture["redaction_roots"] = serde_json::json!({
        "cwd": cwd,
        "repo": repo,
        "home": home,
        "temp": system_temp
    });
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "known-paths")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["cwd"], "<CWD>");
    assert_eq!(payloads[0]["note"], "opened <CWD>\\src\\main.rs");
    assert_eq!(payloads[0]["repo_note"], "repo <REPO>\\Cargo.toml");
    assert_eq!(payloads[0]["home_note"], "home <HOME>\\profile");
    assert_eq!(payloads[0]["temp_note"], "temp <TEMP>\\capture");
    assert!(
        !String::from_utf8(report.manifest_bytes)
            .unwrap()
            .contains(&cwd.display().to_string())
    );
}

/// Break caught: Claude encodes the cwd into the basename beneath `memory_paths`, so replacing
/// only the literal HOME prefix leaves machine-specific workspace identity in reviewed evidence.
#[test]
fn sanitizer_replaces_every_nonempty_claude_memory_path_as_typed_local_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let home = r"D:\capture-home";
    let cwd = r"C:\private workspace\project";
    let first_encoded_slug = "C--private-workspace-project";
    let second_encoded_slug = "Z--another-opaque-workspace";
    let payload = serde_json::json!({
        "type": "system",
        "subtype": "init",
        "memory_paths": {
            "auto": format!(r"{home}\.claude\projects\{first_encoded_slug}\memory\"),
            "nested": {
                "unknown_future_scope": format!(
                    r"{home}\.claude\projects\{second_encoded_slug}\memory\"
                )
            },
            "disabled": ""
        }
    })
    .to_string();
    let raw = write_raw_capture(temp.path(), "claude-memory-paths", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["cwd"] = Value::String(cwd.into());
    capture["redaction_roots"]["home"] = Value::String(home.into());
    capture["command"]["cwd"] = Value::String(cwd.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-memory-paths")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(
        payload["memory_paths"],
        serde_json::json!({
            "auto": "<CLAUDE_MEMORY_PATH_1>",
            "nested": {"unknown_future_scope": "<CLAUDE_MEMORY_PATH_2>"},
            "disabled": ""
        })
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_memory_path"], 2);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|definition| definition["kind"] == "claude_memory_path")
            .map(|definition| definition["placeholder"].as_str().unwrap())
            .eq(["<CLAUDE_MEMORY_PATH_1>", "<CLAUDE_MEMORY_PATH_2>"])
    );
    let sanitized = String::from_utf8(report.events_bytes).unwrap();
    for private_component in [home, cwd, first_encoded_slug, second_encoded_slug] {
        assert!(!sanitized.contains(private_component));
    }
}

/// KNOWN GAP, not yet fixed (slice 4.2 task 9, docs/debt row D58): a subagent `task_id` has no
/// `semantic_kind` rule under any key spelling -- `normalize_field("task_id")` is `"taskid"`,
/// which matches none of `"tooluseid" | "parenttooluseid" | "itemid"` or `"hookid"`, so it is
/// never typed, anywhere it appears, even under its own key. `task_notification` compounds this:
/// it also carries `output_file`, a path under the operator's temp dir whose middle segment is
/// the *same* hyphen-mangled cwd slug `memory_paths.auto` uses (colons and backslashes turned
/// into hyphens -- see `sanitizer_replaces_every_nonempty_claude_memory_path_as_typed_local_metadata`
/// above for the encoding), and `semantic_kind` has no rule keyed on `output_file` at all. Even a
/// `session_id` -- which IS typed correctly under its own key -- leaks again here, because there
/// is no value-based sweep that also replaces an already-typed identifier when it turns up
/// embedded, unkeyed, inside a different field's string. This test pins today's behavior so a
/// promotion built from a raw `task_notification` frame stays reviewable rather than silently
/// leaking. Found while sanitizing `claude/2.1.229/subagent` by hand: that promotion's own
/// `task_id` and `output_file` needed a manual pass this test proves the real tool lacks. When
/// closed, these assertions flip and the test should be rewritten to expect redaction.
#[test]
fn sanitizer_has_no_rule_for_task_id_or_output_file_mangled_cwd_slug_known_gap() {
    let temp = tempfile::tempdir().unwrap();
    let home = r"D:\capture-home";
    let cwd = r"C:\private workspace\project";
    let mangled_cwd_slug = "C--private-workspace-project";
    let session_id = "session-under-tasks";
    let task_id = "task-output-file";
    let payload = serde_json::json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": task_id,
        "status": "completed",
        "output_file": format!(
            r"{home}\AppData\Local\Temp\claude\{mangled_cwd_slug}\{session_id}\tasks\{task_id}.output"
        ),
        "session_id": session_id
    })
    .to_string();
    let raw = write_raw_capture(temp.path(), "output-file-gap", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["cwd"] = Value::String(cwd.into());
    capture["redaction_roots"]["home"] = Value::String(home.into());
    capture["command"]["cwd"] = Value::String(cwd.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "output-file-gap")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];

    // session_id IS typed correctly under its own key -- the gap is specific to task_id (never
    // typed anywhere) and to output_file's untouched embedded copy of both identifiers.
    assert_eq!(payload["session_id"], "<SESSION_ID_1>");

    // Known gap 1: task_id has no rule at all, so it survives literally under its own key too.
    assert_eq!(payload["task_id"], task_id);

    // Known gap 2: output_file itself is untouched by any rule, so the mangled cwd slug and both
    // identifiers survive verbatim inside it even though session_id is typed correctly elsewhere.
    let output_file = payload["output_file"].as_str().unwrap();
    assert!(
        output_file.contains(mangled_cwd_slug),
        "expected today's known gap (mangled cwd slug NOT redacted), got: {output_file}"
    );
    assert!(
        output_file.contains(task_id) && output_file.contains(session_id),
        "expected today's known gap (embedded ids NOT redacted), got: {output_file}"
    );
}

/// Break caught: Claude assistant message IDs are provider-generated correlators, and leaving
/// them literal exposes run identity even when session, request, and tool IDs are typed correctly.
#[test]
fn sanitizer_types_claude_assistant_message_ids_by_shape_and_reuses_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    let first_message_id = "opaque-provider-message-alpha";
    let second_message_id = "future-format-message-beta";
    let raw = write_raw_capture(
        temp.path(),
        "claude-message-ids",
        &[
            &serde_json::json!({
                "type": "stream_event",
                "request_id": "request-private",
                "session_id": "session-private",
                "event": {
                    "type": "message_start",
                    "message": {
                        "type": "message",
                        "role": "assistant",
                        "id": first_message_id,
                        "content": [{"type": "tool_use", "id": "tool-private"}]
                    }
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {"type": "message", "role": "assistant", "id": first_message_id}
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {"type": "message", "role": "assistant", "id": second_message_id}
            })
            .to_string(),
            r#"{"type":"assistant","message":{"type":"message","role":"assistant","id":null}}"#,
            r#"{"type":"metadata","id":"safe-unrelated-literal"}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-message-ids")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["event"]["message"]["id"],
        "<CLAUDE_MESSAGE_ID_1>"
    );
    assert_eq!(payloads[1]["message"]["id"], "<CLAUDE_MESSAGE_ID_1>");
    assert_eq!(payloads[2]["message"]["id"], "<CLAUDE_MESSAGE_ID_2>");
    assert_eq!(payloads[3]["message"]["id"], Value::Null);
    assert_eq!(payloads[4]["id"], "safe-unrelated-literal");
    assert_eq!(payloads[0]["request_id"], "<CLAUDE_REQUEST_ID_1>");
    assert_eq!(payloads[0]["session_id"], "<SESSION_ID_1>");
    assert_eq!(
        payloads[0]["event"]["message"]["content"][0]["id"],
        "<TOOL_USE_ID_1>"
    );

    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_message_id"], 3);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|definition| definition["kind"] == "claude_message_id")
            .map(|definition| definition["placeholder"].as_str().unwrap())
            .eq(["<CLAUDE_MESSAGE_ID_1>", "<CLAUDE_MESSAGE_ID_2>"])
    );

    let capture_path = raw.join("capture.json");
    let mut non_claude_capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    non_claude_capture["provider"] = Value::String("codex".into());
    std::fs::write(
        &capture_path,
        serde_json::to_vec_pretty(&non_claude_capture).unwrap(),
    )
    .unwrap();
    let non_claude = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "codex-message-shaped-object"),
    )
    .unwrap();
    assert_eq!(
        sanitized_payloads(&non_claude.events_bytes)[0]["event"]["message"]["id"],
        first_message_id,
        "a message-shaped object is not a Claude message ID outside Claude evidence"
    );
}

/// Break caught: the resume identifier is repeated in Claude's launch argv, outside event JSON,
/// and must reuse the exact typed session placeholder discovered from the captured wire frames.
#[test]
fn sanitizer_reuses_the_event_session_mapping_in_claude_resume_argv() {
    let temp = tempfile::tempdir().unwrap();
    let session_id = "opaque-session-to-resume";
    let raw = write_raw_capture(
        temp.path(),
        "claude-resume-argv",
        &[&serde_json::json!({
            "type": "result",
            "subtype": "success",
            "session_id": session_id
        })
        .to_string()],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--print", format!("--resume={session_id}")]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-resume-argv")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(
        manifest["command"]["args"],
        serde_json::json!(["--print", "--resume=<SESSION_ID_1>"])
    );
    assert_eq!(manifest["redaction_counts"]["session_id"], 2);
    assert!(
        !String::from_utf8(report.manifest_bytes)
            .unwrap()
            .contains(session_id)
    );
}

#[test]
fn sanitizer_rejects_a_claude_resume_argv_identifier_absent_from_events() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "claude-unmapped-resume-argv",
        &[r#"{"type":"result","subtype":"success","session_id":"captured-session"}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--resume=different-session"]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let output = staging_dir(temp.path(), "claude-unmapped-resume-argv");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::InvalidClaudeResumeCommand { .. }
    ));
    assert!(!error.to_string().contains("different-session"));
    assert!(!output.exists());
}

/// Break caught: recognizing only the happy-path prefix lets malformed, split, duplicate, absent,
/// or scenario-inappropriate resume arguments bypass the manifest's semantic-ID contract.
#[test]
fn sanitizer_enforces_the_exact_claude_resume_command_grammar() {
    let session_id = "captured-session";
    let cases = [
        ("missing", "resume", serde_json::json!(["--print"])),
        ("empty", "resume", serde_json::json!(["--resume="])),
        (
            "split",
            "resume",
            serde_json::json!(["--resume", session_id]),
        ),
        (
            "duplicate",
            "resume",
            serde_json::json!([
                format!("--resume={session_id}"),
                format!("--resume={session_id}")
            ]),
        ),
        (
            "malformed",
            "resume",
            serde_json::json!([format!("--resume-id={session_id}")]),
        ),
        (
            "mismatch",
            "resume",
            serde_json::json!(["--resume=another-session"]),
        ),
        (
            "unexpected-nonresume",
            "fresh-text",
            serde_json::json!([format!("--resume={session_id}")]),
        ),
    ];

    for (name, scenario, args) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            name,
            &[&serde_json::json!({
                "type": "result",
                "subtype": "success",
                "session_id": session_id
            })
            .to_string()],
        );
        let capture_path = raw.join("capture.json");
        let mut capture: Value =
            serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
        capture["scenario"] = Value::String(scenario.into());
        capture["command"]["args"] = args;
        std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        let output = staging_dir(temp.path(), name);

        let error = sanitize_dir(&raw, &output).unwrap_err();
        assert!(
            matches!(error, SanitizationError::InvalidClaudeResumeCommand { .. }),
            "{name}: {error}"
        );
        assert!(!error.to_string().contains(session_id), "{name}");
        assert!(!error.to_string().contains("another-session"), "{name}");
        assert!(!output.exists(), "{name}");
    }
}

#[test]
fn sanitizer_requires_the_claude_resume_command_to_map_the_sole_session_semantic() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "multiple-session-semantics",
        &[
            r#"{"type":"system","session_id":"first-session"}"#,
            r#"{"type":"result","subtype":"success","session_id":"second-session"}"#,
        ],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--resume=first-session"]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let output = staging_dir(temp.path(), "multiple-session-semantics");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::InvalidClaudeResumeCommand { .. }
    ));
    assert!(!error.to_string().contains("first-session"));
    assert!(!error.to_string().contains("second-session"));
    assert!(!output.exists());
}

/// Break caught: thinking signatures are opaque provider correlators repeated across stream and
/// assistant frames; they need their own structural type instead of prefix or secret heuristics.
#[test]
fn sanitizer_types_nonempty_claude_thinking_signatures_by_shape() {
    let temp = tempfile::tempdir().unwrap();
    let first_signature = "opaque-signature-alpha";
    let second_signature = "future-signature-format-beta";
    let raw = write_raw_capture(
        temp.path(),
        "claude-thinking-signatures",
        &[
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "signature_delta", "signature": first_signature}
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "private", "signature": first_signature},
                        {"type": "thinking", "thinking": "private", "signature": second_signature},
                        {"type": "thinking", "thinking": "private", "signature": ""}
                    ]
                }
            })
            .to_string(),
        ],
    );

    let report = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "claude-thinking-signatures"),
    )
    .unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["event"]["delta"]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_1>"
    );
    assert_eq!(
        payloads[1]["message"]["content"][0]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_1>"
    );
    assert_eq!(
        payloads[1]["message"]["content"][1]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_2>"
    );
    assert_eq!(payloads[1]["message"]["content"][2]["signature"], "");
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_thinking_signature"], 3);

    let capture_path = raw.join("capture.json");
    let mut codex_capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    codex_capture["provider"] = Value::String("codex".into());
    std::fs::write(
        &capture_path,
        serde_json::to_vec_pretty(&codex_capture).unwrap(),
    )
    .unwrap();
    let codex = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "codex-thinking-shaped-values"),
    )
    .unwrap();
    assert_eq!(
        sanitized_payloads(&codex.events_bytes)[0]["event"]["delta"]["signature"],
        first_signature
    );
}

/// Break caught: a one-character assistant delta used to be replaced as a substring in every
/// later JSON string, corrupting stable skill and slash-command metadata that happened to contain
/// the same character. Semantic redaction is structural and provider diagnostics are whole fields.
#[test]
fn sanitizer_does_not_apply_assistant_semantics_to_unrelated_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "claude-short-assistant-delta",
        &[
            &serde_json::json!({
                "type": "system",
                "subtype": "init",
                "skills": ["deep-research", "r"],
                "slash_commands": ["review", "r"]
            })
            .to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": "r"}
                }
            })
            .to_string(),
            &serde_json::json!({"level": "debug", "message": "r"}).to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta", "partial_json": "{\"skill\":\"r\"}"}
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta", "partial_json": "{\"skill\":\"r\"}"}
                }
            })
            .to_string(),
        ],
    );

    let report = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "claude-short-assistant-delta"),
    )
    .unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0],
        serde_json::json!({
            "type": "system",
            "subtype": "init",
            "skills": ["deep-research", "r"],
            "slash_commands": ["review", "r"]
        }),
        "stable init metadata must remain byte-for-byte semantically unchanged"
    );
    assert_eq!(payloads[1]["event"]["delta"]["text"], "<ASSISTANT_PROSE_1>");
    assert_eq!(payloads[2]["message"], "<PROVIDER_PROSE_1>");
    assert_eq!(
        payloads[3]["event"]["delta"]["partial_json"],
        "<ASSISTANT_PROSE_2>"
    );
    assert_eq!(
        payloads[4]["event"]["delta"]["partial_json"],
        "<ASSISTANT_PROSE_2>"
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["assistant_prose"], 3);
}

/// Break caught: isolated logged-out discovery cannot be sanitized because its explicitly
/// configured CODEX_HOME is not one of the raw evidence's captured redaction roots.
#[test]
fn sanitizer_replaces_the_captured_codex_home() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "codex-home", &[r#"{"level":"debug"}"#]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["codex_home"] = Value::String(r"D:\isolated-codex-home".into());
    capture["command"]["configured_env"]["CODEX_HOME"] =
        Value::String(r"D:\isolated-codex-home".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-home")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(
        manifest["command"]["configured_env"]["CODEX_HOME"],
        "<CODEX_HOME>"
    );
    assert_eq!(manifest["redaction_counts"]["codex_home_path"], 1);
}

#[test]
fn sanitizer_replaces_the_captured_approval_target() {
    let temp = tempfile::tempdir().unwrap();
    let target = r"D:\bounded approval target";
    let payload = format!(
        r#"{{"method":"item/commandExecution/requestApproval","params":{{"command":{}}}}}"#,
        serde_json::to_string(&format!("write {target}\\approval-marker.txt")).unwrap()
    );
    let raw = write_raw_capture(temp.path(), "approval-target", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["approval_target"] = Value::String(target.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "approval-target")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["params"]["command"],
        "write <APPROVAL_TARGET>\\approval-marker.txt"
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["approval_target_path"], 1);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["placeholder"] == "<APPROVAL_TARGET>"
                    && entry["kind"] == "approval_target_path"
            })
    );
}

/// Break caught: the reviewed Codex approval wrapper contains an absolute trusted executable
/// path, so successful raw evidence must sample that path as provenance before sanitization.
#[test]
fn sanitizer_replaces_the_captured_trusted_powershell_path() {
    let temp = tempfile::tempdir().unwrap();
    let executable = r"C:\Program Files\WindowsApps\PowerShell\pwsh.exe";
    let payload = serde_json::json!({
        "method": "item/started",
        "params": {
            "item": {
                "type": "commandExecution",
                "command": format!(r#""{executable}" -Command 'echo capture'"#)
            }
        }
    })
    .to_string();
    let raw = write_raw_capture(temp.path(), "trusted-powershell", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["trusted_powershell"] = Value::String(executable.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "trusted-powershell")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["params"]["item"]["command"],
        r#""<TRUSTED_POWERSHELL>" -Command 'echo capture'"#
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["trusted_powershell_path"], 1);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["placeholder"] == "<TRUSTED_POWERSHELL>"
                && entry["kind"] == "trusted_powershell_path")
    );
}

/// Break caught: global value replacement can turn a protocol discriminator into a placeholder
/// when its literal happens to equal a dynamic identifier.
#[test]
fn sanitizer_never_replaces_protocol_discriminators_on_value_collision() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "discriminator-collision",
        &[r#"{"type":"user","session_id":"user","message":{"role":"user","content":"prompt"}}"#],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "discriminator-collision")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(payload["type"], "user");
    assert_eq!(payload["message"]["role"], "user");
    assert_eq!(payload["session_id"], "<SESSION_ID_1>");
}

/// Break caught: local MCP configuration names are opaque machine metadata, but only in the
/// direct params of Codex's startup-status notification; ordinary protocol `name` fields remain.
#[test]
fn sanitizer_redacts_only_direct_codex_mcp_startup_server_names() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-mcp-server-name",
        &[
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"local-server-name","status":"starting","error":null}}"#,
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"local-server-name","status":"failed","failureReason":null}}"#,
            r#"{"method":"unrelated","params":{"name":"stable-protocol-name","status":"ready"}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-mcp-server-name")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["params"]["name"], "<CODEX_MCP_SERVER_NAME_1>");
    assert_eq!(payloads[1]["params"]["name"], "<CODEX_MCP_SERVER_NAME_1>");
    assert_eq!(payloads[0]["params"]["status"], "starting");
    assert!(payloads[0]["params"]["error"].is_null());
    assert_eq!(payloads[1]["params"]["status"], "failed");
    assert!(payloads[1]["params"]["failureReason"].is_null());
    assert_eq!(payloads[2]["params"]["name"], "stable-protocol-name");

    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["codex_mcp_server_name"], 2);
    let definitions = manifest["placeholders"].as_array().unwrap();
    assert_eq!(
        definitions
            .iter()
            .filter(|definition| definition["kind"] == "codex_mcp_server_name")
            .count(),
        1
    );

    for (name, provider, payload) in [
        (
            "claude-mcp-server-name",
            "claude",
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"local-server-name","status":"starting"}}"#,
        ),
        (
            "codex-wrong-method-server-name",
            "codex",
            r#"{"method":"unrelated","params":{"name":"stable-protocol-name","status":"ready"}}"#,
        ),
        (
            "codex-root-server-name",
            "codex",
            r#"{"method":"mcpServer/startupStatus/updated","name":"stable-protocol-name","params":{"status":"starting"}}"#,
        ),
        (
            "codex-nested-server-name",
            "codex",
            r#"{"method":"mcpServer/startupStatus/updated","params":{"detail":{"name":"stable-protocol-name"},"status":"starting"}}"#,
        ),
    ] {
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let path = raw.join("capture.json");
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        capture["provider"] = Value::String(provider.into());
        std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        let report = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap();
        assert_eq!(
            sanitized_payloads(&report.events_bytes)[0]
                .pointer(if name == "codex-root-server-name" {
                    "/name"
                } else if name == "codex-nested-server-name" {
                    "/params/detail/name"
                } else {
                    "/params/name"
                })
                .and_then(Value::as_str),
            Some(if name == "claude-mcp-server-name" {
                "local-server-name"
            } else {
                "stable-protocol-name"
            })
        );
    }
}

/// Break caught: Codex thread records embed the thread identifier in a local history path, so
/// replacing only the home prefix leaves a machine-local identifier suffix in reviewed evidence.
#[test]
fn sanitizer_redacts_direct_codex_thread_paths_as_typed_local_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-thread-path",
        &[
            r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread-secret","path":"C:\\Users\\private\\.codex\\sessions\\thread-secret","preview":""}}}"#,
            r#"{"method":"thread/started","params":{"thread":{"id":"thread-secret","path":"C:\\Users\\private\\.codex\\sessions\\thread-secret","preview":""}}}"#,
            r#"{"method":"unrelated","params":{"path":"stable/protocol/path"}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-thread-path")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["result"]["thread"]["path"],
        "<CODEX_THREAD_PATH_1>"
    );
    assert_eq!(
        payloads[1]["params"]["thread"]["path"],
        "<CODEX_THREAD_PATH_1>"
    );
    assert_eq!(payloads[2]["params"]["path"], "stable/protocol/path");
    assert!(
        !std::str::from_utf8(&report.events_bytes)
            .unwrap()
            .contains("thread-secret")
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["codex_thread_path"], 2);
}

/// Break caught: terminal Codex turns repeat item IDs inside `turn.items[]`; losing item context
/// at that array leaks the raw identifier and breaks joins with the preceding item notification.
#[test]
fn sanitizer_reuses_codex_item_ids_inside_terminal_turn_items() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-terminal-item-id",
        &[
            r#"{"method":"item/started","params":{"item":{"id":"item-secret","type":"agentMessage"}}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"thread-secret","turn":{"id":"turn-secret","items":[{"id":"item-secret","type":"agentMessage"}],"status":"completed"}}}"#,
        ],
    );
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-terminal-item-id")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["params"]["item"]["id"], "<TOOL_USE_ID_1>");
    assert_eq!(
        payloads[1]["params"]["turn"]["items"][0]["id"],
        "<TOOL_USE_ID_1>"
    );
    assert!(
        !std::str::from_utf8(&report.events_bytes)
            .unwrap()
            .contains("item-secret")
    );
}

/// Break caught: a resumed Codex thread carries historical `turns[]` with nested `items[]`;
/// dropping entity context at either array leaks provider identifiers from the thread snapshot.
#[test]
fn sanitizer_types_codex_historical_turn_and_item_ids_through_nested_arrays() {
    let temp = tempfile::tempdir().unwrap();
    let payload = r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread-secret","turns":[{"id":"turn-secret","items":[{"id":"item-one","type":"userMessage"},{"id":"item-two","type":"agentMessage"}]}]}},"echo":{"turnId":"turn-secret","itemId":"item-two"},"unrelated":{"turns":[{"id":"stable-turn"}],"items":[{"id":"stable-item"}]}}"#;
    let raw = write_raw_capture(temp.path(), "codex-historical-ids", &[payload]);
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-historical-ids")).unwrap();
    let got = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(got["result"]["thread"]["turns"][0]["id"], "<TURN_ID_1>");
    let first_item = got["result"]["thread"]["turns"][0]["items"][0]["id"]
        .as_str()
        .unwrap();
    let second_item = got["result"]["thread"]["turns"][0]["items"][1]["id"]
        .as_str()
        .unwrap();
    assert!(first_item.starts_with("<TOOL_USE_ID_"));
    assert!(second_item.starts_with("<TOOL_USE_ID_"));
    assert_ne!(first_item, second_item);
    assert_eq!(got["echo"]["turnId"], "<TURN_ID_1>");
    assert_eq!(got["echo"]["itemId"], second_item);
    assert_eq!(got["unrelated"]["turns"][0]["id"], "stable-turn");
    assert_eq!(got["unrelated"]["items"][0]["id"], "stable-item");

    let claude = write_raw_capture(temp.path(), "claude-historical-ids", &[payload]);
    let report = sanitize_dir(&claude, &staging_dir(temp.path(), "claude-historical-ids")).unwrap();
    let got = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(got["result"]["thread"]["turns"][0]["id"], "turn-secret");
    assert_eq!(
        got["result"]["thread"]["turns"][0]["items"][0]["id"],
        "item-one"
    );
}

/// Break caught: exact credential names miss provider-prefixed token families, and counter-name
/// exemptions accept opaque strings that are credentials disguised as usage metadata.
#[test]
fn sanitizer_classifies_credential_families_and_requires_numeric_counters() {
    for field in [
        "apiToken",
        "githubApiToken",
        "providerAccessToken",
        "oauthRefreshToken",
        "serviceClientSecret",
        "accountPrivateKey",
        "proxyAuthorization",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({field: "opaque-value"}).to_string();
        let raw = write_raw_capture(temp.path(), field, &[&payload]);
        assert!(matches!(
            sanitize_dir(&raw, &staging_dir(temp.path(), field)),
            Err(SanitizationError::SecretLikeField { .. })
        ));
    }

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "string-token-counter",
        &[r#"{"usage":{"input_tokens":"opaque-value"}}"#],
    );
    assert!(matches!(
        sanitize_dir(&raw, &staging_dir(temp.path(), "string-token-counter")),
        Err(SanitizationError::SecretLikeField { .. })
    ));

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "unrelated-token-field",
        &[r#"{"tokenizerMode":"provider-default"}"#],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "unrelated-token-field")).unwrap();
    assert_eq!(
        sanitized_payloads(&report.events_bytes)[0]["tokenizerMode"],
        "provider-default"
    );
}

/// Break caught: scanning for credentials before semantic replacement rejects safe redaction of
/// a token pasted as user-authored content, while skipping user replacement publishes it.
#[test]
fn sanitizer_replaces_secret_looking_user_text_before_the_fail_closed_scan() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "token-in-user-text",
        &[r#"{"type":"user","message":{"role":"user","content":"sk-proj-user-pasted-token"}}"#],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "token-in-user-text")).unwrap();
    assert_eq!(
        sanitized_payloads(&report.events_bytes)[0]["message"]["content"],
        "<USER_TEXT_1>"
    );
}
