//! Fake ACP agent for comet-harness tests.
//!
//! Speaks Agent Client Protocol v1 over stdio — newline-framed JSON-RPC 2.0 —
//! well enough to drive the session loop: `initialize`, `session/new`, then a
//! `session/prompt` whose behaviour is picked from the prompt text.
//!
//! Rust rather than upstream's `fake-acp.sh`, because Windows cannot spawn a
//! shell script: the harness hands the path straight to `CreateProcess`, which
//! rejects a non-PE image with "%1 is not a valid Win32 application" (os error
//! 193). The scenarios below are the ones the ACP hardening commits exist for,
//! and each is reachable by prompt text so one binary covers them all.
//!
//! | prompt contains | behaviour |
//! | --- | --- |
//! | `drop-reply`    | streams an update, then never answers the prompt request |
//! | `starve`        | answers nothing at all after `session/new` |
//! | `ignore-cancel` | never answers, and ignores `session/cancel` too |
//! | `exit-now`      | writes to stderr and exits mid-turn |
//! | `refusal`       | settles with `stopReason: "refusal"` |
//! | `cancel`        | settles with `stopReason: "cancelled"` |
//! | anything else   | one text chunk, then `stopReason: "end_turn"` |
//!
//! **`starve` and `ignore-cancel` are not the same mode**, and the difference
//! is the whole point of the second one: a starved prompt is still recorded as
//! in-flight, so a `session/cancel` settles it. Only `ignore-cancel` leaves the
//! client's own bounded give-up as the sole thing that can end the turn.

use std::io::{BufRead, StdinLock, Write};
use std::process::exit;

use serde_json::{Value, json};

/// One JSON-RPC line. Rust's stdout is line-buffered even on a pipe, so each
/// line reaches the harness before the fixture blocks on its next read.
fn emit(value: &Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// EOF is a hard failure, not an empty line — the harness closing stdin means
/// the test is over.
fn read_line(stdin: &mut StdinLock<'_>) -> String {
    let mut buf = String::new();
    match stdin.read_line(&mut buf) {
        Ok(0) | Err(_) => exit(0),
        Ok(_) => buf.trim_end_matches(['\r', '\n']).to_string(),
    }
}

/// The `initialize` reply, shaped after what the real adapters answered when
/// probed on 2026-08-28 (codex-acp 1.7.0, claude-agent-acp 0.70.0): the same
/// `protocolVersion` / `agentInfo` / `agentCapabilities` / `authMethods` /
/// `_meta.steering` surface, trimmed to what a test needs.
///
/// `authMethods` is deliberately EMPTY here. claude-agent-acp answers `[]`
/// while codex-acp answers two entries, and the empty case is the one a plan's
/// fixtures would never supply — so it is the one the fixture defaults to.
fn initialize_result(steering: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "agentInfo": {"name": "fake-acp", "title": "Fake ACP", "version": "0.0.0"},
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {"image": false, "embeddedContext": true},
            "sessionCapabilities": {"resume": {}, "list": {}},
        },
        "authMethods": [],
        "_meta": {"steering": {"supported": steering}},
    })
}

fn main() {
    // Opt out of the steering extension to exercise the degraded path Hermes
    // takes: no `_session/steering`, so steering falls back to turn boundaries.
    let steering = std::env::var_os("FAKE_ACP_NO_STEERING").is_none();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    // A vendor session-config surface on `session/new`, shaped from the grok
    // 1.0.5 capture (2026-08-28). Off by default: the fixture is
    // provider-neutral and most tests want the bare reply. `FAKE_ACP_SESSION_
    // CONFIG=grok` turns it on for the tests that exercise a config-carrying
    // agent.
    let session_config = match std::env::var("FAKE_ACP_SESSION_CONFIG").as_deref() {
        Ok("grok") => Some(json!({"options": [
            {"category": "model", "id": "fake-model", "label": "Fake Model", "selected": true},
            {"category": "model", "id": "fake-mini", "label": "Fake Mini", "selected": false},
            // Grok spells its EFFORT ladder `mode`, not `thought_level`.
            {"category": "mode", "id": "high", "label": "High Effort", "selected": true},
            {"category": "mode", "id": "low", "label": "Low Effort", "selected": false},
        ]})),
        _ => None,
    };
    let mut session_counter = 0_u64;
    // The id of a `session/prompt` deliberately left unanswered (`drop-reply`
    // and `starve`). A `session/cancel` settles exactly this one.
    let mut pending_prompt: Option<Value> = None;

    loop {
        let line = read_line(&mut stdin);
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame["id"].clone();
        let method = frame["method"].as_str().unwrap_or_default().to_owned();

        match method.as_str() {
            "initialize" => {
                emit(&json!({
                    "jsonrpc": "2.0", "id": id, "result": initialize_result(steering),
                }));
                // **An unsolicited notification, the way a real agent does.**
                // grok 1.0.5 emits a burst of `_x.ai/*` frames right after the
                // handshake, and that burst is load-bearing for the client: a
                // client that has dropped its `Incoming` receiver kills its own
                // reader task on the first one, and every later reply goes
                // unparsed. This fixture was silent until prompted, so it could
                // not reproduce that — and the bug shipped to the model picker.
                emit(&json!({
                    "jsonrpc": "2.0",
                    "method": "_fake/ready",
                    "params": {"note": "unsolicited; see the comment above"},
                }));
            }
            "session/new" => {
                session_counter += 1;
                let mut result = json!({"sessionId": format!("fake-session-{session_counter}")});
                if let Some(config) = session_config.clone() {
                    result["_meta"] = json!({"x.ai/sessionConfig": config});
                    // The deprecated surface, alongside the config one and
                    // DISAGREEING with it on purpose: it enumerates model x
                    // effort, which is what an agent that has both really
                    // sends. A decode that reads it as the model list gets
                    // four rows instead of two, and the test says so.
                    result["models"] = json!({
                        "currentModelId": "fake-model",
                        "availableModels": [
                            {"modelId": "fake-model", "name": "Fake Model",
                             "description": "the fixture's model"},
                            {"modelId": "fake-model-low", "name": "Fake Model (low)"},
                            {"modelId": "fake-mini", "name": "Fake Mini"},
                            {"modelId": "fake-mini-low", "name": "Fake Mini (low)"},
                        ],
                    });
                }
                emit(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            "session/prompt" => pending_prompt = handle_prompt(&frame, &id),
            "session/cancel" => {
                // A cancel is a NOTIFICATION in ACP: it carries no id of its
                // own and gets no reply. What settles is the in-flight
                // `session/prompt`, which is why the fixture has to remember
                // that id rather than read one out of the cancel's params --
                // an earlier version invented a `_promptId` param, which no
                // real client sends and which therefore tested nothing.
                if let Some(id) = pending_prompt.take() {
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"stopReason": "cancelled"},
                    }));
                }
            }
            _ => {
                if !id.is_null() {
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"},
                    }));
                }
            }
        }
    }
}

/// Returns the prompt's id when it is left UNANSWERED, so `session/cancel` can
/// settle it later. `None` means the turn was answered here and nothing is
/// outstanding.
fn handle_prompt(frame: &Value, id: &Value) -> Option<Value> {
    let session_id = frame["params"]["sessionId"]
        .as_str()
        .unwrap_or("fake-session-1");
    let text = prompt_text(frame);

    let update = |payload: Value| {
        emit(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": payload},
        }));
    };

    if text.contains("ignore-cancel") {
        // Silent AND deaf: nothing is recorded as in-flight, so a later
        // `session/cancel` finds nothing to settle and the turn is never
        // answered by any route.
        //
        // **This is the only mode that isolates the client's bounded give-up.**
        // `starve` looks like it should, but the fixture settles a starved
        // prompt when the cancel arrives — so a give-up pushed a day into the
        // future still let that test pass. An agent that ignores the cancel is
        // what leaves the client as the only thing that can end the turn.
        return None;
    }

    if text.contains("exit-now") {
        // Die mid-turn with a word on stderr, the way a real agent does when it
        // is killed or panics. The client must report that as an error it can
        // explain, not as a turn that quietly finished.
        eprintln!("fake-acp: simulated crash");
        let _ = std::io::stderr().flush();
        exit(3);
    }

    if text.contains("starve") {
        // Nothing at all: no update, no reply. This is the shape the
        // starved-turn recovery commits exist for, and the only honest way to
        // pose it is to say nothing.
        return Some(id.clone());
    }

    update(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": "working"},
    }));

    if text.contains("drop-reply") {
        // Streamed, then silent. The turn never settles on its own — the
        // dropped-reply settle is what has to notice.
        return Some(id.clone());
    }

    let stop = if text.contains("refusal") {
        "refusal"
    } else if text.contains("cancel") {
        "cancelled"
    } else {
        "end_turn"
    };

    if stop == "end_turn" {
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": " done"},
        }));
    }

    emit(&json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}));
    None
}

/// ACP carries the prompt as a content-block array, so the text is the
/// concatenation of every `text` block rather than a single field.
fn prompt_text(frame: &Value) -> String {
    frame["params"]["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
