//! Fake Claude Code CLI for comet-harness tests.
//!
//! Reads the first stream-json user line from stdin, picks a scenario from the
//! prompt text, and plays a scripted stream-json transcript on stdout —
//! including control-channel round-trips read back from stdin. Driven by
//! crates/harness/tests/claude.rs.
//!
//! Rust rather than `#!/bin/sh` because Windows cannot spawn a shell script:
//! the harness hands the path straight to `CreateProcess`, which rejects a
//! non-PE image with "%1 is not a valid Win32 application" (os error 193).

use std::io::{BufRead, StdinLock, Write};
use std::process::exit;
use std::time::Duration;

/// One transcript line. Rust's stdout is line-buffered even on a pipe, so each
/// line reaches the harness before the fixture blocks on its next read.
fn emit(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// `read -r line || exit 1`: EOF is a hard failure, not an empty line.
fn read_line(stdin: &mut StdinLock<'_>) -> String {
    let mut buf = String::new();
    match stdin.read_line(&mut buf) {
        Ok(0) | Err(_) => exit(1),
        Ok(_) => buf.trim_end_matches(['\r', '\n']).to_string(),
    }
}

/// The last `"content":"…"` value on the line, mirroring the greedy sed the
/// shell fixture used. A line without one is passed through unchanged.
fn last_content(line: &str) -> String {
    const KEY: &str = "\"content\":\"";
    match line.rfind(KEY) {
        Some(at) => {
            let rest = &line[at + KEY.len()..];
            match rest.find('"') {
                Some(end) => rest[..end].to_string(),
                None => line.to_string(),
            }
        }
        None => line.to_string(),
    }
}

/// The values the installed Claude Code 2.1.226 accepts for
/// `--permission-mode`: the six choices its own `--help` advertises, plus the
/// unadvertised `default` alias comet keeps for older CLIs (see
/// `crates/harness/src/claude/mod.rs`). Every other argument is ignored —
/// this fixture checks only that the one flag the adapter derives from
/// `RunRequest.runtime_mode` is a value a real `claude` binary would accept,
/// so every scenario that spawns this binary gets that check for free
/// instead of no scenario checking it at all.
const VALID_PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
    "default",
];

fn check_permission_mode() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pos) = args.iter().position(|a| a == "--permission-mode") else {
        return;
    };
    let Some(value) = args.get(pos + 1) else {
        return;
    };
    if !VALID_PERMISSION_MODES.contains(&value.as_str()) {
        eprintln!(
            "fake-claude: --permission-mode {value:?} is not a value the real CLI accepts \
             (expected one of {VALID_PERMISSION_MODES:?})"
        );
        exit(1);
    }
}

/// Shaped exactly like Claude Code 2.1.227's answer, including the double
/// nesting and `resolvedModel` — see the 2026-08-11 initialize capture. Two
/// models are enough to prove the merge; the full five are pinned in
/// `claude/discovery.rs`'s unit test.
const INITIALIZE_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"comet-discovery-1","response":{"commands":[],"agents":[],"output_style":"default","available_output_styles":["default"],"models":[{"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet","description":"Sonnet 5","supportsEffort":true,"supportedEffortLevels":["low","high"]},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku","description":"Haiku 4.5"}],"account":{"email":"user@example.test","organization":"Example","subscriptionType":"Claude Max","apiProvider":"firstParty"},"pid":1234,"current_permission_mode":"acceptEdits"}}}"#;

/// The initialize reply for a **command** discovery: the same frame, with a
/// `commands` array whose entries report what this child was actually handed.
///
/// The child is the only thing that can say which directory it was started in
/// and which arguments reached it, and a test that cannot see those cannot tell
/// a right answer from a plausible one. Slice 2.3 learned this the expensive
/// way — a login check read one `CODEX_HOME` while the child used another, and
/// every test passed — so the echo is the fixture's whole job here.
fn command_reply() -> String {
    let args: Vec<String> = std::env::args().collect();
    let bare = args.iter().any(|a| a == "--bare");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<none>".into());
    let escaped = cwd.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"comet-discovery-1","response":{{"commands":[{{"name":"cwd-echo","description":"{escaped}","argumentHint":""}},{{"name":"bare-echo","description":"{bare}","argumentHint":""}},{{"name":"review","description":"Review the diff.","argumentHint":"[--fix]","aliases":["cr"]}}],"agents":[],"models":[],"account":{{}}}}}}}}"#
    )
}

fn main() {
    check_permission_mode();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let first = read_line(&mut stdin);

    // A discovery session sends no prompt: its first line is the control
    // request. Answering it here is what gives `models()` a real spawn and a
    // real round-trip to be tested against.
    //
    // Model discovery and command discovery send the SAME request and differ
    // only in their arguments, so the fixture splits on `--bare` exactly as the
    // adapter does: with it, the caller wants models; without it, commands.
    if first.contains("control_request") {
        if std::env::args().any(|a| a == "--bare") {
            emit(INITIALIZE_REPLY);
        } else {
            emit(&command_reply());
        }
        // The adapter closes stdin to end the session; a real CLI exits 0.
        exit(0);
    }

    if first.contains("scenario:happy") {
        happy();
    } else if first.contains("scenario:askuser") {
        askuser(&mut stdin);
    } else if first.contains("scenario:steer") {
        steer(&mut stdin);
    } else if first.contains("scenario:interrupt") {
        interrupt();
    } else if first.contains("scenario:error") {
        error();
    } else if first.contains("scenario:notices") {
        notices();
    } else if first.contains("scenario:diagnostics") {
        diagnostics();
    } else if first.contains("scenario:approval") {
        approval(&mut stdin);
    } else {
        emit(
            r#"{"type":"result","subtype":"error_during_execution","errors":["unknown scenario"],"usage":{"input_tokens":0,"output_tokens":0},"session_id":"sess-x"}"#,
        );
    }
}

fn happy() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}"#,
    );
    // Re-emitted init mid-run (background-task wakeup): must be deduped.
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}"#,
    );
    emit(
        r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"pondering"}}}"#,
    );
    emit(
        r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}"#,
    );
    // Subagent frames (parent_tool_use_id set): all filtered.
    emit(
        r#"{"type":"stream_event","parent_tool_use_id":"sub-1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"SUBAGENT"}}}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_use","id":"sub-tool","name":"Bash","input":{"command":"echo sub"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_result","tool_use_id":"sub-tool","is_error":false}]}}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"ls -la"}},{"type":"tool_use","id":"tool-2","name":"mcp__linear__search","input":{"q":"bug"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false},{"type":"tool_result","tool_use_id":"tool-2","is_error":true}]}}"#,
    );
    // Informational rate-limit status: stays quiet.
    emit(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#);
    emit(
        // Shaped like a real `result` frame, not a minimal one: the cache
        // fields carry almost the whole prompt and `modelUsage` is the only
        // place the context window appears. A fixture without them let the
        // adapter report single-digit prompts for months.
        r#"{"type":"result","subtype":"success","result":"done!","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":34932,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-1","total_cost_usd":0.01}"#,
    );
}

fn askuser(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-ask"}"#,
    );
    // A plain tool permission request: must round-trip through approval and
    // be allowed.
    emit(
        r#"{"type":"control_request","request_id":"cr-0","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
    );
    let resp0 = read_line(stdin);
    if !(resp0.contains(r#""request_id":"cr-0""#) && resp0.contains(r#""behavior":"allow""#)) {
        emit(
            r#"{"type":"result","subtype":"error_during_execution","errors":["bash tool was not allowed"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}"#,
        );
        return;
    }
    // AskUserQuestion: must be intercepted and answered via updatedInput.answers.
    emit(
        r#"{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false}]}}}"#,
    );
    let resp1 = read_line(stdin);
    if !resp1.contains(r#""behavior":"allow""#) {
        emit(
            r#"{"type":"result","subtype":"error_during_execution","errors":["AskUserQuestion was denied"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}"#,
        );
    } else if resp1.contains(r#""Pick one":"B""#) {
        emit(
            r#"{"type":"result","subtype":"success","result":"answered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}"#,
        );
    } else {
        emit(
            r#"{"type":"result","subtype":"error_during_execution","errors":["answers missing from updatedInput"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}"#,
        );
    }
}

fn steer(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-steer"}"#,
    );
    emit(
        r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"first"}}}"#,
    );
    // The queued steering user line, applied at "the step boundary" (here: now).
    let content = last_content(&read_line(stdin));
    emit(&format!(
        r#"{{"type":"stream_event","parent_tool_use_id":null,"event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"steered:{content}"}}}}}}"#
    ));
    emit(
        r#"{"type":"result","subtype":"success","result":"steered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-steer"}"#,
    );
}

fn interrupt() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-int"}"#,
    );
    // Wedge without reading stdin — forces the kill escalation path.
    std::thread::sleep(Duration::from_secs(30));
}

fn error() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-err"}"#,
    );
    // Terse assistant-level error code with no content.
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[]},"error":"rate_limit"}"#,
    );
    // Hard-rejected claude.ai usage window.
    emit(
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#,
    );
    // Result error with an EMPTY errors array: needs fallback wording.
    emit(
        r#"{"type":"result","subtype":"error_max_turns","errors":[],"usage":{"input_tokens":1,"output_tokens":2},"session_id":"sess-err"}"#,
    );
}

fn notices() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto","pre_tokens":68000,"post_tokens":12000},"session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"model_refusal_fallback","trigger":"refusal","direction":"sticky","original_model":"claude-fable-5","fallback_model":"claude-haiku-4-5","content":"refused","session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,"retry_delay_ms":2000,"error_status":529,"session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":3,"retry_delay_ms":4000,"error_status":null,"session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"informational","content":"Consider running /doctor to fix your settings.","level":"suggestion","session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"notification","key":"usage-warning","text":"You have used half of your weekly limit.","priority":"low","session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour"}}"#,
    );
    // A system subtype nobody claimed: must vanish quietly (Frame::Other) —
    // the interlock point where slice 0b.2's diagnostics will pick it up.
    emit(
        r#"{"type":"system","subtype":"someFutureSubtype","content":"???","session_id":"sess-n"}"#,
    );
    emit(
        r#"{"type":"result","subtype":"success","result":"done","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-n"}"#,
    );
}

/// The Write's target, JSON-escaped for the frame below. Absolute, because the
/// adapter reads a relative path as an operation it cannot determine — there is
/// no cwd on the request to resolve one against. Under a directory that does
/// not exist, so the adapter's existence check answers "nothing there" and the
/// frame stays a create. Kept in step with `write_target` in tests/claude.rs.
const WRITE_TARGET_JSON: &str = if cfg!(windows) {
    r"C:\\comet-fake-fixture\\a.txt"
} else {
    "/comet-fake-fixture/a.txt"
};

/// Requests permission for a Write, then reports what it was told. The
/// `control_request` frame shape is copied from a live capture (2026-08-10,
/// CLI 2.1.226), with only the path swapped for one this machine can be
/// trusted not to have. The reply is echoed as a `stream_event` text delta
/// rather than a full `assistant` message: `normalize.rs`'s `Frame::Assistant`
/// arm only turns `tool_use` blocks into events and drops a bare text block, so
/// only the streamed form actually reaches the test as a `TextDelta`.
fn approval(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Write"],"cwd":"/tmp","session_id":"sess-1"}"#,
    );
    emit(
        &r#"{"type":"control_request","request_id":"fake-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"__PATH__","content":"hi\n"},"description":"a.txt","tool_use_id":"toolu_fake"}}"#
            .replace("__PATH__", WRITE_TARGET_JSON),
    );
    let reply = read_line(stdin);
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("a control response");
    let behavior = reply["response"]["response"]["behavior"]
        .as_str()
        .unwrap_or("none");
    emit(&format!(
        r#"{{"type":"stream_event","parent_tool_use_id":null,"event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"told: {behavior}"}}}}}}"#
    ));
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-1"}"#,
    );
}

fn diagnostics() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-d"}"#,
    );
    // Sink 5: a non-JSON stdout line → the "unparseable" Malformed sentinel.
    emit("claude: warming up (not json)");
    // Ignored tier — every one of these is routine on a healthy session
    // (capture-confirmed) and must produce NOTHING.
    emit(r#"{"type":"system","subtype":"status","status":"requesting","session_id":"sess-d"}"#);
    emit(r#"{"type":"system","subtype":"thinking_tokens","tokens":123,"session_id":"sess-d"}"#);
    emit(
        r#"{"type":"system","subtype":"hook_started","hook":"SessionStart","session_id":"sess-d"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"hook_response","hook":"SessionStart","session_id":"sess-d"}"#,
    );
    emit(r#"{"type":"tool_progress","tool_use_id":"t1","progress":0.5,"session_id":"sess-d"}"#);
    // Unknown tier: an unclaimed system subtype and an unknown top-level type.
    emit(
        r#"{"type":"system","subtype":"someFutureSubtype","payload":"do-not-carry","session_id":"sess-d"}"#,
    );
    emit(r#"{"type":"mystery_frame","secret":"do-not-carry","session_id":"sess-d"}"#);
    // Sink 3: an unclaimed control request (counted, not answered — the fake
    // does not wait for a reply).
    emit(
        r#"{"type":"control_request","request_id":"cr-9","request":{"subtype":"request_user_dialog","dialog":{"kind":"someDialog"}}}"#,
    );
    emit(
        r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}}"#,
    );
    emit(
        r#"{"type":"result","subtype":"success","result":"done","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-d"}"#,
    );
}
