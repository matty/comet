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

fn main() {
    check_permission_mode();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let first = read_line(&mut stdin);

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
        r#"{"type":"result","subtype":"success","result":"done!","errors":[],"usage":{"input_tokens":10,"output_tokens":20},"session_id":"sess-1","total_cost_usd":0.01}"#,
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
