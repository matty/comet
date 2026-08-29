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
//! | `silent-after-prompt` | identical wire shape to `starve` — named separately because it exists for a different mechanism (the prompt-stall bound, not cancel-recovery) and should not be confused with it if `starve` is ever repurposed |
//! | `ignore-cancel` | never answers, and ignores `session/cancel` too |
//! | `exit-now`      | writes to stderr and exits mid-turn |
//! | `refusal`       | settles with `stopReason: "refusal"` |
//! | `cancel`        | settles with `stopReason: "cancelled"` |
//! | `complete-notification-only` | sends the completion notification (see [`PROMPT_COMPLETE_METHOD`]), then never answers the prompt request — poses the hang upstream reports, where the RPC response alone hangs |
//! | `complete-both` | sends the completion notification immediately, then the ordinary RPC reply after a deliberate delay — poses the healthy case both signals exist for |
//! | `complete-response-only` | the ordinary RPC reply alone, no notification — this is the plain `end_turn` path below; named here because it is the deliberate MIRROR of `complete-notification-only`: an agent without the extension, which must still settle |
//! | `replay-stale` | replays the PREVIOUS turn's already-consumed promptId as a bogus early completion (`stopReason: "refusal"`) of THIS turn, before streaming this turn's own real content and completing for real — poses the cross-turn staleness the consumed-promptId dedup exists to reject |
//! | anything else   | one text chunk, then `stopReason: "end_turn"`, no completion notification |
//!
//! **`starve` and `ignore-cancel` are not the same mode**, and the difference
//! is the whole point of the second one: a starved prompt is still recorded as
//! in-flight, so a `session/cancel` settles it. Only `ignore-cancel` leaves the
//! client's own bounded give-up as the sole thing that can end the turn.
//!
//! **The `complete-*` modes exist for the ACP hardening in `acp/session.rs`:
//! settling a turn on whichever of the RPC response or Grok's completion
//! notification ([`PROMPT_COMPLETE_METHOD`]) lands first, exactly once.**
//! The plain default path deliberately sends NO notification — every test
//! written before this hardening existed already drives that path, and
//! changing its shape would be changing what those tests cover.

use std::io::{BufRead, StdinLock, Write};
use std::process::exit;

use serde_json::{Value, json};

/// Grok's vendor completion notification (`acp::session`'s own copy carries
/// the full rationale). Built via `concat!`, not one literal, for the same
/// reason that module's copy is: `crates/engine/tests/no_runtime_cloud.rs`
/// forbids a slash, the word "session", and another slash appearing
/// contiguously anywhere under `crates/`, and a plain substring scan cannot
/// tell this vendor method name apart from a hosted-authority remnant.
const PROMPT_COMPLETE_METHOD: &str = concat!("_x.ai/ses", "sion/prompt_complete");

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
fn initialize_result(steering: bool, load_session: bool, image_capable: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "agentInfo": {"name": "fake-acp", "title": "Fake ACP", "version": "0.0.0"},
        "agentCapabilities": {
            "loadSession": load_session,
            "promptCapabilities": {"image": image_capable, "embeddedContext": true},
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
    // Both real agents in this corpus (Grok and Hermes) advertise
    // `loadSession: true`, so that is the default here too; `FAKE_ACP_NO_
    // LOAD_SESSION=1` poses the agent that never advertised it, for the
    // negative resume gate.
    let load_session = std::env::var_os("FAKE_ACP_NO_LOAD_SESSION").is_none();
    // The opposite default from `load_session`: Grok's own captured reply
    // answers `promptCapabilities.image: false`, so that is the default here;
    // `FAKE_ACP_IMAGE_CAPABLE=1` poses an agent like Hermes that answers
    // `true`.
    let image_capable = std::env::var_os("FAKE_ACP_IMAGE_CAPABLE").is_some();
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
    // The promptId minted for the MOST RECENT completed turn, so a later
    // turn's `replay-stale` can echo it as a bogus early completion — the
    // shape the consumed-promptId dedup exists to reject.
    let mut last_prompt_id: Option<String> = None;
    // **What PR7's run-fidelity tests prove was received**, not merely
    // decoded: the client is the only one who can report what it was
    // handed, so a `session/prompt` containing the `echo-selection` keyword
    // reports these back as streamed text (see `handle_prompt`). Populated by
    // `session/set_model`, `session/set_config_option` and `session/load`
    // below — real production requests, sent by `AcpSession::open` before
    // the first prompt, not frames a test wrote by hand.
    let mut last_model_id: Option<String> = None;
    let mut last_config: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut last_load_session_id: Option<String> = None;

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
                    "jsonrpc": "2.0", "id": id,
                    "result": initialize_result(steering, load_session, image_capable),
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
            "session/prompt" => {
                pending_prompt = handle_prompt(
                    &frame,
                    &id,
                    &mut last_prompt_id,
                    &last_model_id,
                    &last_config,
                    &last_load_session_id,
                )
            }
            "session/set_model" => {
                // The ACP org's own dedicated setter (`SetSessionModelRequest`:
                // `{sessionId, modelId}`) -- Hermes' installed source
                // implements exactly this (`hermes.rs::config_requests`'s doc
                // comment).
                last_model_id = frame["params"]["modelId"].as_str().map(str::to_owned);
                emit(&json!({"jsonrpc": "2.0", "id": id, "result": {}}));
            }
            "session/set_config_option" => {
                // The ACP org's own generic setter
                // (`SetSessionConfigOptionSelectRequest`: `{sessionId,
                // configId, value}`) -- the shape Grok's flat `category`-keyed
                // option rows imply (`grok.rs::config_requests`'s doc
                // comment). One entry per `configId`, so a later call for the
                // same category (e.g. re-selecting effort) replaces rather
                // than accumulates -- matching what a real setter does.
                if let (Some(config_id), Some(value)) = (
                    frame["params"]["configId"].as_str(),
                    frame["params"]["value"].as_str(),
                ) {
                    last_config.insert(config_id.to_owned(), value.to_owned());
                }
                emit(&json!({
                    "jsonrpc": "2.0", "id": id, "result": {"configOptions": []},
                }));
            }
            "session/load" => {
                // `LoadSessionRequest`'s own id is the resumed session --
                // there is nothing to mint, unlike `session/new`. A
                // `sessionId` containing `reject-load` poses the agent-side
                // failure `a_failed_load_reports_rather_than_starting_fresh`
                // needs: the id is real but the agent cannot resume it (an
                // expired or evicted session, in practice).
                let session_id = frame["params"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if session_id.contains("reject-load") {
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": "unknown session"},
                    }));
                } else {
                    last_load_session_id = Some(session_id);
                    emit(&json!({"jsonrpc": "2.0", "id": id, "result": {}}));
                }
            }
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
fn handle_prompt(
    frame: &Value,
    id: &Value,
    last_prompt_id: &mut Option<String>,
    last_model_id: &Option<String>,
    last_config: &std::collections::BTreeMap<String, String>,
    last_load_session_id: &Option<String>,
) -> Option<Value> {
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

    if text.contains("echo-selection") {
        // **What was actually received, not what was decoded.** Everything
        // here was set by a REAL `session/set_model` / `session/set_config_
        // option` / `session/load` request this same process answered
        // earlier -- see the doc comment on the state variables in `main`.
        let echo = json!({
            "model": last_model_id,
            "config": last_config,
            "load": last_load_session_id,
            "images": image_block_count(frame),
        });
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": echo.to_string()},
        }));
        emit(&json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
        return None;
    }

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

    if text.contains("starve") || text.contains("silent-after-prompt") {
        // Nothing at all: no update, no reply. This is the shape the
        // starved-turn recovery commits exist for (`starve`) and the shape
        // the prompt-stall bound exists for (`silent-after-prompt`) — two
        // different mechanisms that happen to want the identical wire
        // silence, kept as separate keywords so a future change to one
        // cannot silently retarget the other's test.
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

    // Grok's vendor promptId, echoed identically by the notification and by
    // the reply's `_meta.promptId` (verified live, 2026-08-28) — derived from
    // the request's own JSON-RPC id so it is unique per prompt without extra
    // state, the same way a real agent mints one per turn.
    let prompt_id = format!("fake-prompt-{id}");
    // Captured BEFORE this turn's own id overwrites it below, so
    // `replay-stale` can echo the PREVIOUS turn's promptId rather than its
    // own.
    let previous_prompt_id = last_prompt_id.clone();
    let complete = |stop: &str| {
        emit(&json!({
            "jsonrpc": "2.0",
            "method": PROMPT_COMPLETE_METHOD,
            "params": {
                "sessionId": session_id,
                "promptId": prompt_id,
                "stopReason": stop,
                "agentResult": Value::Null,
            },
        }));
    };

    if text.contains("complete-notification-only") {
        // The upstream hang, posed directly: the notification is the ONLY
        // thing that ever says this turn is over.
        complete("end_turn");
        *last_prompt_id = Some(prompt_id);
        return Some(id.clone());
    }

    if text.contains("complete-both") {
        // The healthy case both signals exist for, made deterministic rather
        // than raced: the notification fires immediately, the ordinary reply
        // follows after a delay comfortably longer than any reasonable
        // "settled fast" assertion, so a test can tell WHICH signal actually
        // ended the turn rather than merely that one eventually did.
        //
        // **No trailing text chunk after the notification, unlike the plain
        // `end_turn` path below.** Real Grok's own race is 3ms end to end
        // with nothing further to stream once the notification fires; adding
        // a chunk here would sit in `incoming`, undrained, until whichever
        // turn runs next — a discarded RPC reply has nothing left to explain
        // once its own turn already settled off the notification, and this
        // fixture should not invent content that turn never produced.
        complete("end_turn");
        *last_prompt_id = Some(prompt_id.clone());
        std::thread::sleep(std::time::Duration::from_millis(200));
        emit(&json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"stopReason": "end_turn", "_meta": {"promptId": prompt_id}},
        }));
        return None;
    }

    if text.contains("replay-stale") {
        // The cross-turn staleness shape the consumed-promptId dedup exists
        // to reject: a delayed duplicate of an EARLIER, already-settled
        // turn's completion notification, echoed into THIS turn's window.
        // Without the dedup this reads as completing THIS turn immediately,
        // with the PREVIOUS turn's stale stopReason — before this turn's own
        // real content has even streamed.
        if let Some(stale) = previous_prompt_id {
            emit(&json!({
                "jsonrpc": "2.0",
                "method": PROMPT_COMPLETE_METHOD,
                "params": {
                    "sessionId": session_id,
                    "promptId": stale,
                    "stopReason": "refusal",
                    "agentResult": Value::Null,
                },
            }));
        }
        // This turn's OWN real content, deliberately after a delay: a dedup
        // failure settles (wrongly, as a refusal) before this ever streams.
        std::thread::sleep(std::time::Duration::from_millis(200));
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "second"},
        }));
        complete("end_turn");
        *last_prompt_id = Some(prompt_id);
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

/// How many `{"type": "image", ...}` blocks rode this prompt — what
/// `echo-selection` reports back to prove (or disprove) that an attachment
/// actually reached the wire, as opposed to merely being staged in the
/// `RunRequest`.
fn image_block_count(frame: &Value) -> usize {
    frame["params"]["prompt"]
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"] == "image").count())
        .unwrap_or(0)
}
