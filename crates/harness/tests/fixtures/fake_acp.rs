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
//! | `wedge-with-child\|<path>` | streams one update, spawns a real OS grandchild, records both this process's and the grandchild's pid to `<path>`, then goes silent and deaf exactly like `ignore-cancel` — D133's falsification that a cancelled ACP turn reaps the whole provider-owned tree, not just this fixture's own pid (the ACP analogue of `fake_codex.rs`'s `wedge_with_child`) |
//! | `exit-now`      | writes to stderr and exits mid-turn |
//! | `refusal`       | settles with `stopReason: "refusal"` |
//! | `cancel`        | settles with `stopReason: "cancelled"` |
//! | `complete-notification-only` | sends the completion notification (see [`PROMPT_COMPLETE_METHOD`]), then never answers the prompt request — poses the hang upstream reports, where the RPC response alone hangs |
//! | `complete-both` | sends the completion notification immediately, then the ordinary RPC reply after a deliberate delay — poses the healthy case both signals exist for |
//! | `complete-both-usage` | identical to `complete-both`, except the RPC reply follows after a realistic few-ms gap (not `complete-both`'s exaggerated 200ms) and carries Grok's real usage shape (`_meta.inputTokens`/`outputTokens`) — poses the healthy turn Finding 1's fix exists for: usage that lives only in the reply, which the notification-settle path must still recover |
//! | `complete-race-drain` | the notification, then an extra `agent_message_chunk` 50ms later, then the reply 10ms after that — so the chunk lands from the reader task strictly BETWEEN the two settle signals, during the harvest's own wait rather than before it. Poses the window D121 covers: only a drain placed AFTER the harvest, not the ones before it, can pick this up on the turn it belongs to. Unlike `complete-both-usage` the reply carries no token counts; this scenario is about frame ordering, not usage |
//! | `complete-response-only` | the ordinary RPC reply alone, no notification — this is the plain `end_turn` path below; named here because it is the deliberate MIRROR of `complete-notification-only`: an agent without the extension, which must still settle |
//! | `replay-stale` | replays the PREVIOUS turn's already-consumed promptId as a bogus early completion (`stopReason: "refusal"`) of THIS turn, before streaming this turn's own real content and completing for real — poses the cross-turn staleness the consumed-promptId dedup exists to reject |
//! | `complete-foreign-session` | sends the completion notification naming a DIFFERENT `sessionId` (`stopReason: "refusal"`, no promptId), before streaming this turn's own real content and completing for real, correctly scoped — poses the foreign-session mismatch D114 covers: a completion signal about a different session is not evidence about this one |
//! | `request-permission` | sends `session/request_permission` mid-turn with all four ACP option kinds, then — once answered — echoes `"chosen:<optionId or cancelled>"` as a text chunk and settles `end_turn`. Lets a test observe which option the client picked without reading raw wire bytes. |
//! | `request-permission-unrecognized` | identical, but the options carry no kind this build recognizes (`vendor_custom` only) — poses the protocol-drift case, which a correct client answers `cancelled` on its own, never touching the approval bridge |
//! | `request-permission-edit` | identical, but `toolCall.kind` is `"edit"` with a `diff` content block and the options are Hermes' real two-option edit shape (`allow_once`/`reject_once`, no `allow_always` at all) — poses the shape `AllowForSession`'s narrow-to-`allow_once` fallback exists for |
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
use std::time::Duration;

use serde_json::{Value, json};

/// Grok's vendor completion notification (`acp::session`'s own copy carries
/// the full rationale, and the note on why the repo-wide hosted-authority
/// guard exempts this name rather than each site obfuscating it).
const PROMPT_COMPLETE_METHOD: &str = "_x.ai/session/prompt_complete";

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

/// D133: strip `HANDLE_FLAG_INHERIT` from this process's own stdio handles so
/// a child THIS fixture spawns cannot silently inherit a duplicate of the
/// pipe connecting it to the real test harness — see the call site in
/// `spawn_grandchild_and_record` for why that matters. Identical to
/// `fake_codex.rs`'s own copy of this function (D46); unix needs no
/// equivalent for the same reason that one's doc comment gives.
#[cfg(windows)]
fn disable_stdio_inheritance() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        // SAFETY: clears one flag on a handle this process already owns and
        // keeps using exactly as before; this only changes what a future
        // child of ours can inherit.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

#[cfg(not(windows))]
fn disable_stdio_inheritance() {}

/// D133: spawn a real OS grandchild and record both this process's and the
/// grandchild's pid to `path`, newline-separated — the ACP analogue of
/// `fake_codex.rs`'s `wedge_with_child`. Re-execs THIS binary under
/// `--sleep-forever` rather than spawning a second helper: proving a leaked
/// provider-owned descendant needs no second `[[bin]]` target.
fn spawn_grandchild_and_record(path: &str) {
    disable_stdio_inheritance();

    let exe = std::env::current_exe().expect("current exe for grandchild re-exec");
    let grandchild = std::process::Command::new(&exe)
        .arg("--sleep-forever")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn grandchild");
    std::fs::write(
        path,
        format!("{}\n{}\n", std::process::id(), grandchild.id()),
    )
    .expect("record parent/grandchild pids");
    // Deliberately not waited on: an orphaned, still-running grandchild is
    // exactly the shape a real provider's shell or MCP-server child takes
    // when this fixture (its immediate parent) is torn down.
    drop(grandchild);
}

/// One outstanding `session/request_permission` this fixture sent, and what
/// to do once the client answers it: complete `prompt_id` (the
/// `session/prompt` this request interrupted) with `end_turn`, after echoing
/// which option — or `cancelled` — the client picked.
struct PendingPermission {
    request_id: Value,
    prompt_id: Value,
    session_id: String,
}

fn main() {
    // D133: this fixture re-execs itself under this flag to play the
    // "grandchild" role for `wedge-with-child` below — the same binary, so
    // proving a leaked descendant needs no second `[[bin]]` target. It never
    // touches stdio, so it cannot be mistaken for the real protocol child.
    // Mirrors `fake_codex.rs`'s identical trick for the same reason (D46).
    if std::env::args().any(|a| a == "--sleep-forever") {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
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
    let mut last_cwd = String::new();
    let mut last_model_id: Option<String> = None;
    let mut last_config: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut last_load_session_id: Option<String> = None;
    // A `session/request_permission` this fixture sent and is still waiting
    // on. `request-permission[-unrecognized]` sets it; the bare-response
    // branch below clears it once the client answers.
    let mut pending_permission: Option<PendingPermission> = None;

    loop {
        let line = read_line(&mut stdin);
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame["id"].clone();

        // **A bare JSON-RPC response has no `method` at all** — this is the
        // client's answer to a request THIS fixture sent, not a call FROM the
        // client. It must be recognized before the method dispatch below:
        // falling into that match's catch-all would misread it as an
        // unsupported method call and answer it with a second, nonsensical
        // `-32601` under the same id.
        if frame["method"].is_null()
            && (frame.get("result").is_some() || frame.get("error").is_some())
        {
            if let Some(pending) = &pending_permission
                && pending.request_id == id
            {
                let chosen = match frame["result"]["outcome"]["outcome"].as_str() {
                    Some("selected") => frame["result"]["outcome"]["optionId"]
                        .as_str()
                        .unwrap_or("<missing optionId>")
                        .to_owned(),
                    _ => "cancelled".to_owned(),
                };
                let prompt_id = pending.prompt_id.clone();
                let session_id = pending.session_id.clone();
                pending_permission = None;
                pending_prompt = None;
                emit(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": format!("chosen:{chosen}")},
                        },
                    },
                }));
                emit(&json!({
                    "jsonrpc": "2.0", "id": prompt_id,
                    "result": {"stopReason": "end_turn"},
                }));
            }
            continue;
        }

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
                // A `cwd` containing `needs-setup` poses the signed-out/
                // unconfigured `session/new` failure `acp_turn.rs`'s
                // `a_session_new_failure_reaches_the_caller_through_the_
                // open_failure_mapper` needs: real evidence this is the
                // exact wire shape Grok answers with when signed out
                // (`grok::map_open_failure`'s own doc comment), reachable
                // only by actually driving `AcpSession::open` end to end
                // against a real child process, not by calling a mapper
                // function directly.
                let cwd = frame["params"]["cwd"].as_str().unwrap_or_default();
                // Kept for `session/set_model` below: the setter's own params
                // carry no cwd, so the trigger has to be remembered from the
                // call that did.
                last_cwd = cwd.to_owned();
                if cwd.contains("needs-setup") {
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {
                            "code": -32000,
                            "message": "Authentication required",
                            "data": "no auth method id provided",
                        },
                    }));
                    continue;
                }
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
                    &mut pending_permission,
                )
            }
            "session/set_model" => {
                // A `cwd` containing `refuse-set-model` poses the agent that
                // opens a session and then rejects the model Comet selected —
                // Grok's real shape when the picker's curated or cached id is
                // not on the account's live list (`grok.rs`'s own doc names
                // the wire text). Drives D119's path end to end: the raw
                // message below must NOT be what the caller reports.
                if last_cwd.contains("refuse-set-model") {
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {
                            "code": -32602,
                            "message": "Invalid params: unknown model id",
                        },
                    }));
                    continue;
                }
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
                if session_id.contains("needs-setup-load") {
                    // The same signed-out shape `session/new`'s `needs-setup`
                    // trigger answers with, above -- poses a signed-out user
                    // reopening an EXISTING resumable chat, the case
                    // `open_or_resume`'s own doc comment names: Grok
                    // advertises `loadSession: true`, so this path is real,
                    // not hypothetical, and must not lose the sign-in
                    // guidance to the generic "could not resume" fallback.
                    emit(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {
                            "code": -32000,
                            "message": "Authentication required",
                            "data": "no auth method id provided",
                        },
                    }));
                } else if session_id.contains("reject-load") {
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
    pending_permission: &mut Option<PendingPermission>,
) -> Option<Value> {
    let session_id = frame["params"]["sessionId"]
        .as_str()
        .unwrap_or("fake-session-1");
    let text = prompt_text(frame);

    if text.contains("request-permission") {
        let unrecognized = text.contains("request-permission-unrecognized");
        // Hermes' own edit-approval shape (`_build_permission_tool_call`,
        // `acp_adapter/edit_approval.py`, source-read): `kind: "edit"`, a
        // `diff` content block, and exactly TWO options — no `allow_always`
        // at all. This is the shape `AllowForSession`'s narrowing exists
        // for: the four-kind mode below never exercised it, which is how
        // the bug it fixes got through review.
        let edit = text.contains("request-permission-edit");
        // The four ACP option kinds, one real optionId each, so a test can
        // tell which was picked from the echoed `chosen:<id>` text. `execute`
        // + `rawInput.command` is Hermes' own shape for a dangerous-command
        // approval (`acp_adapter/permissions.py`, source-read — see
        // `crate::acp::approval`'s module doc).
        let options = if unrecognized {
            json!([
                {"optionId": "vendor-1", "kind": "vendor_custom", "name": "Do it anyway"},
            ])
        } else if edit {
            json!([
                {"optionId": "opt-allow-once", "kind": "allow_once", "name": "Allow edit"},
                {"optionId": "opt-reject-once", "kind": "reject_once", "name": "Deny"},
            ])
        } else {
            json!([
                {"optionId": "opt-allow-once", "kind": "allow_once", "name": "Allow once"},
                {"optionId": "opt-allow-always", "kind": "allow_always", "name": "Allow always"},
                {"optionId": "opt-reject-once", "kind": "reject_once", "name": "Reject once"},
                {"optionId": "opt-reject-always", "kind": "reject_always", "name": "Reject always"},
            ])
        };
        let tool_call = if edit {
            json!({
                "toolCallId": format!("call-{id}"),
                "kind": "edit",
                "content": [{
                    "type": "diff",
                    "path": "/tmp/edited.txt",
                    "oldText": "old",
                    "newText": "new",
                }],
            })
        } else {
            json!({
                "toolCallId": format!("call-{id}"),
                "kind": "execute",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "$ rm -rf /tmp/x"},
                }],
                "rawInput": {"command": "rm -rf /tmp/x"},
            })
        };
        let request_id = format!("perm-{id}");
        emit(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": options,
            },
        }));
        *pending_permission = Some(PendingPermission {
            request_id: Value::String(request_id),
            prompt_id: id.clone(),
            session_id: session_id.to_owned(),
        });
        // Left unanswered here, same as `starve`/`drop-reply`: the reply to
        // THIS `session/prompt` is sent from the bare-response branch in
        // `main`, once the client answers the permission request.
        return Some(id.clone());
    }

    let update = |payload: Value| {
        emit(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": payload},
        }));
    };

    if let Some(path) = text.strip_prefix("wedge-with-child|") {
        // D133: prove a provider-owned GRANDCHILD is gone after cancellation,
        // not just this fixture's own pid — see the doc-table entry above and
        // `spawn_grandchild_and_record`'s own comment. Silent and deaf after
        // this, exactly like `ignore-cancel` below: the client's own bounded
        // give-up is the only thing that can end this turn.
        spawn_grandchild_and_record(path);
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "working"},
        }));
        return None;
    }

    if text.contains("announcement-notices") {
        for (method, message) in [
            ("_x.ai/settings/update", "First fixture announcement"),
            ("_x.ai/announcements/update", "Updated fixture announcement"),
        ] {
            emit(&json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": {"announcements": [{
                    "id": "fixture-release",
                    "title": "Fixture release",
                    "message": message,
                    "severity": "warning",
                }]},
            }));
        }
    }

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

    if text.contains("complete-both-usage") {
        // The healthy case Finding 1's fix exists for, with a REALISTIC gap
        // rather than `complete-both`'s deliberately exaggerated 200ms
        // (checked first because "complete-both-usage" also contains the
        // substring "complete-both"): the notification fires immediately,
        // the ordinary reply follows a few ms later — short enough to land
        // inside `POST_NOTIFICATION_REPLY_BOUND` — carrying Grok's real
        // usage shape in `_meta` (`inputTokens`/`outputTokens`, the same
        // field names `grok::usage` reads) so a test can prove the
        // notification-settle path recovers it from the reply instead of
        // dropping that already-in-flight future un-polled.
        complete("end_turn");
        *last_prompt_id = Some(prompt_id.clone());
        std::thread::sleep(std::time::Duration::from_millis(5));
        emit(&json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "stopReason": "end_turn",
                "_meta": {"promptId": prompt_id, "inputTokens": 111, "outputTokens": 22},
            },
        }));
        return None;
    }

    if text.contains("complete-race-drain") {
        // The exact window D121 covers, and `complete-both-usage` cannot pose
        // it: the notification settles the turn, but a SECOND frame arrives
        // from the reader task while the harvest below is still polling
        // `reply`, not before it — so only a drain placed AFTER that poll
        // returns can pick it up on THIS turn. Left undrained (the bug this
        // scenario pins), it would sit in `incoming` until the NEXT turn's
        // own leading drain instead (`docs/debt/README.md`'s D121).
        //
        // **The 50ms gap is the whole scenario, and it was 2ms first.** At
        // 2ms the client is routinely descheduled long enough that its
        // notification-arm drain — the one that runs BEFORE the harvest —
        // has not executed yet when this chunk hits the pipe, so that drain
        // scoops it and the test passes with the post-harvest drain deleted.
        // Measured on a loaded debug build: 3 of 7 runs passed against the
        // broken code. 50ms buys the margin, and both gaps together stay far
        // inside `POST_NOTIFICATION_REPLY_BOUND` (250ms) so the harvest still
        // receives the reply rather than timing out. Do not shrink them.
        complete("end_turn");
        *last_prompt_id = Some(prompt_id.clone());
        std::thread::sleep(std::time::Duration::from_millis(50));
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "raced-in"},
        }));
        std::thread::sleep(std::time::Duration::from_millis(10));
        emit(&json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "stopReason": "end_turn",
                "_meta": {"promptId": prompt_id},
            },
        }));
        return None;
    }

    if text.contains("complete-both") {
        // The healthy case both signals exist for, made deterministic rather
        // than raced: the notification fires immediately, the ordinary reply
        // follows after a delay comfortably longer than
        // `POST_NOTIFICATION_REPLY_BOUND` (`acp/session.rs`), so the client's
        // own harvest of that reply always times out rather than actually
        // receiving it — this scenario is about proving the notification
        // arm decided the turn and stayed bounded, not about what a
        // received reply would have added.
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
        std::thread::sleep(std::time::Duration::from_millis(600));
        emit(&json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"stopReason": "end_turn", "_meta": {"promptId": prompt_id}},
        }));
        return None;
    }

    if text.contains("complete-foreign-session") {
        // D114: the completion notification's `sessionId` naming a DIFFERENT
        // session is not evidence about THIS turn — the guard in
        // `acp/session.rs` must reject it before it ever looks at the
        // promptId. Posed the same way `replay-stale` poses cross-turn
        // staleness: a notification that, if the guard were missing, would
        // settle this turn immediately and WRONGLY (`stopReason: "refusal"`,
        // no promptId to dedup on so `is_none_or` alone would let it
        // through), followed after a delay by this turn's own real content
        // and its own correctly-scoped completion. A client that checks the
        // guard waits past the foreign notification for that real
        // completion; one that doesn't settles instantly, `Errored`, with
        // "second" never streamed.
        emit(&json!({
            "jsonrpc": "2.0",
            "method": PROMPT_COMPLETE_METHOD,
            "params": {
                "sessionId": format!("{session_id}-foreign"),
                "promptId": Value::Null,
                "stopReason": "refusal",
                "agentResult": Value::Null,
            },
        }));
        std::thread::sleep(std::time::Duration::from_millis(200));
        update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "second"},
        }));
        complete("end_turn");
        *last_prompt_id = Some(prompt_id);
        return Some(id.clone());
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
