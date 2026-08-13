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
//!
//! Corpus-backed contracts: `claude-model-fixture-shape`,
//! `claude-routine-frame-fixture`, and `claude-approval-fixture-shape`.

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

/// Exceed any ordinary pipe capacity before answering discovery. A caller that
/// pipes stderr without draining it blocks here and never reads the reply.
fn fill_stderr() {
    let chunk = [b'x'; 8192];
    let mut stderr = std::io::stderr().lock();
    for _ in 0..128 {
        stderr.write_all(&chunk).expect("write discovery stderr");
    }
    stderr.flush().expect("flush discovery stderr");
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
/// nesting and `resolvedModel` pinned by `claude-model-fixture-shape`. Two
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
    if !std::env::args().any(|arg| arg == "--permission-mode") {
        fill_stderr();
    }
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

    if first.contains("scenario:attachment") {
        emit(
            r#"{"type":"result","subtype":"success","result":"image","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-attachment"}"#,
        );
    } else if first.contains("scenario:happy") {
        happy();
    } else if first.contains("scenario:subagent-failed") {
        subagent_failed();
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
    } else if first.contains("scenario:capture-approval-destructive-command") {
        capture_destructive_command(&mut stdin);
    } else if first.contains("scenario:capture-approval-unexpected-second") {
        capture_unexpected_second(&mut stdin);
    } else if first.contains("scenario:capture-approval-write-before-bash") {
        capture_write_before_bash(&mut stdin);
    } else if first.contains("scenario:capture-approval-missing-bash") {
        capture_missing_bash(&mut stdin);
    } else if first.contains("scenario:capture-approval-failed-bash") {
        capture_failed_bash(&mut stdin);
    } else if first.contains("scenario:capture-approval-wrong-bash") {
        capture_wrong_bash(&mut stdin);
    } else if first.contains("scenario:capture-approval-duplicate-bash") {
        capture_duplicate_bash(&mut stdin);
    } else if first.contains("scenario:capture-approval-bash-snapshot-duplicate") {
        capture_bash_snapshot_duplicate(&mut stdin);
    } else if first.contains("scenario:capture-approval-bash-control-response") {
        capture_bash_control_response(&mut stdin);
    } else if first.contains("scenario:capture-approval-bash-malformed-extra") {
        capture_bash_content_deviation(&mut stdin, "malformed-extra");
    } else if first.contains("scenario:capture-approval-bash-leading-text") {
        capture_bash_content_deviation(&mut stdin, "leading-text");
    } else if first.contains("scenario:capture-approval-bash-trailing-text") {
        capture_bash_content_deviation(&mut stdin, "trailing-text");
    } else if first.contains("scenario:capture-approval-write-malformed-extra") {
        capture_write_content_deviation(&mut stdin, "malformed-extra");
    } else if first.contains("scenario:capture-approval-write-leading-text") {
        capture_write_content_deviation(&mut stdin, "leading-text");
    } else if first.contains("scenario:capture-approval-write-trailing-text") {
        capture_write_content_deviation(&mut stdin, "trailing-text");
    } else if first.contains("scenario:capture-approval-user-malformed-extra") {
        capture_user_content_deviation(&mut stdin, "malformed-extra");
    } else if first.contains("scenario:capture-approval-user-leading-text") {
        capture_user_content_deviation(&mut stdin, "leading-text");
    } else if first.contains("scenario:capture-approval-user-trailing-text") {
        capture_user_content_deviation(&mut stdin, "trailing-text");
    } else if first.contains("scenario:capture-approval-malformed-candidate") {
        capture_malformed_candidate(&mut stdin);
    } else if first.contains("scenario:capture-approval-missing-write") {
        capture_missing_write();
    } else if first.contains("scenario:capture-approval-duplicate-write") {
        capture_duplicate_write(&mut stdin);
    } else if first.contains("scenario:capture-approval-missing-request-id") {
        capture_missing_request_id(&mut stdin);
    } else if first.contains("scenario:capture-approval-duplicate-request-id") {
        capture_duplicate_request_id(&mut stdin);
    } else if first.contains("scenario:capture-approval-extra-tool") {
        capture_extra_tool(&mut stdin);
    } else if first.contains("scenario:capture-approval-destructive-write") {
        capture_destructive_write(&mut stdin);
    } else if first.contains("scenario:capture-approval") {
        capture_approval(&mut stdin);
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
    // Spawns the subagent below. Realism, not one of the five system/task_*
    // frames itself (those start at the next `emit`) — a subagent block with
    // no call that spawned it is an incoherent sequence. Shaped from
    // run2-claude-subagent.jsonl:97: `description`/`subagent_type`/`prompt`/
    // `run_in_background`/`caller` plus the four numeric `usage` fields it
    // carries are copied verbatim; the fake fixture's own convention is
    // readable ids, not the capture's opaque hex/toolu_ ones, so "sub-1" /
    // "sub-1-task" stand in for the real tool_use_id / task_id. Decodes as
    // ToolCall::Unknown{name:"Agent"}: nothing in this slice teaches
    // decode_tool_use a dedicated Agent arm.
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"sub-1","name":"Agent","input":{"description":"Read README and report first heading","subagent_type":"general-purpose","prompt":"Read the README.md file in the current directory and report what the first heading is.","run_in_background":false},"caller":{"type":"direct"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1496,"cache_read_input_tokens":34676,"output_tokens":3}}}"#,
    );
    // Shaped from run2-claude-subagent.jsonl:98.
    emit(
        r#"{"type":"system","subtype":"task_started","task_id":"sub-1-task","tool_use_id":"sub-1","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Read the README.md file in the current directory and report what the first heading is."}"#,
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
    // Shaped from run2-claude-subagent.jsonl:103.
    emit(
        r#"{"type":"system","subtype":"task_progress","task_id":"sub-1-task","tool_use_id":"sub-1","description":"Reading README.md","subagent_type":"general-purpose","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906},"last_tool_name":"Read"}"#,
    );
    // Shaped from run2-claude-subagent.jsonl:106 — a PARTIAL patch, status
    // only, exactly like the real one.
    emit(
        r#"{"type":"system","subtype":"task_updated","task_id":"sub-1-task","patch":{"status":"completed","end_time":1786581776304}}"#,
    );
    // Shaped from run2-claude-subagent.jsonl:107 — the frame carrying the
    // answer and the terminal usage totals.
    emit(
        r#"{"type":"system","subtype":"task_notification","task_id":"sub-1-task","tool_use_id":"sub-1","status":"completed","output_file":"C:\\tmp\\sub-1-task.output","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#,
    );
    // A `SendMessage`-resumed agent: the SAME task_id under a NEW
    // tool_use_id — the fifth distinct system/task_* shape (capture:150,
    // 168, 169). This is what exercises `normalize.rs`'s
    // `subagent_progress.remove(&f.task_id)` on `task_started` through a
    // real spawn: without it, this second terminal reading would be
    // compared against the first invocation's already-terminal one and
    // dropped as redundant, even though the summary differs.
    emit(
        r#"{"type":"system","subtype":"task_started","task_id":"sub-1-task","tool_use_id":"sub-2","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"What was the first heading you found?"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"task_updated","task_id":"sub-1-task","patch":{"status":"completed","end_time":1786581781670}}"#,
    );
    emit(
        r#"{"type":"system","subtype":"task_notification","task_id":"sub-1-task","tool_use_id":"sub-2","status":"completed","output_file":"C:\\tmp\\sub-1-task-2.output","summary":"The first heading is **Sandbox**.","usage":{"total_tokens":19111,"tool_uses":0,"duration_ms":2186}}"#,
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

/// A subagent whose terminal reading is `"failed"`, not `"completed"`. No
/// capture has ever recorded this — every run in
/// `run2-claude-subagent.jsonl` ends `status: "completed"` — so
/// `normalize.rs`'s `Failed`/`Cancelled` arms were written by hand against
/// `.agents/rules/optional-wire-fields.md`'s rule and never exercised through
/// a real spawn. This scenario is that exercise: same lifecycle shape as
/// `happy()`'s subagent (task_started → child work → terminal reading), with
/// the status strings changed to `"failed"`. The child's own tool result is
/// also marked `is_error` for realism only — it carries `parent_tool_use_id`
/// and is filtered before normalization, so it does not feed either
/// assertion below.
fn subagent_failed() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-subfail"}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"sub-1","name":"Agent","input":{"description":"Run the release check","subagent_type":"general-purpose","prompt":"Run scripts/check.sh and report the result.","run_in_background":false},"caller":{"type":"direct"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1200,"cache_read_input_tokens":30000,"output_tokens":3}}}"#,
    );
    emit(
        r#"{"type":"system","subtype":"task_started","task_id":"sub-1-task","tool_use_id":"sub-1","description":"Run the release check","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Run scripts/check.sh and report the result."}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_use","id":"sub-tool","name":"Bash","input":{"command":"scripts/check.sh"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_result","tool_use_id":"sub-tool","content":"check.sh: exit 1","is_error":true}]}}"#,
    );
    // The partial patch, same shape as the completed case (line 106 of the
    // capture) with the status string swapped.
    emit(
        r#"{"type":"system","subtype":"task_updated","task_id":"sub-1-task","patch":{"status":"failed","end_time":1786581776304}}"#,
    );
    emit(
        r#"{"type":"system","subtype":"task_notification","task_id":"sub-1-task","tool_use_id":"sub-1","status":"failed","output_file":"C:\\tmp\\sub-1-task.output","summary":"check.sh exited 1","usage":{"total_tokens":8120,"tool_uses":1,"duration_ms":1830}}"#,
    );
    emit(
        // Same non-flat shape as `happy()`'s own result frame (`:311-315`) —
        // reintroducing the flat `{input_tokens, output_tokens}`-only shape
        // in a new scenario is exactly what this slice's finding (both fake
        // CLIs emitted unrealistically flat frames) exists to stop, even
        // though nothing here asserts on it yet.
        r#"{"type":"result","subtype":"success","result":"the release check failed","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-subfail","total_cost_usd":0.01}"#,
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
/// `control_request` frame shape follows `claude-approval-fixture-shape`, with
/// only the path swapped for one this machine can be
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

fn capture_approval(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("fake-write", "toolu_write");
    expect_write_allow(stdin, "fake-write");
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-1"}"#,
    );
}

fn emit_bash_neighborhood(id: &str, command: &str, is_error: bool, output: &str) {
    emit(
        &serde_json::json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"role":"assistant","content": [{
                "type": "tool_use",
                "id": id,
                "name": "Bash",
                "input": {"command": command},
            }]},
        })
        .to_string(),
    );
    emit(
        &serde_json::json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role":"user","content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": output,
                "is_error": is_error,
            }]},
        })
        .to_string(),
    );
}

fn emit_write_request(request_id: &str, tool_use_id: &str) {
    let marker = std::env::current_dir()
        .expect("fixture cwd")
        .join("capture-marker.txt")
        .display()
        .to_string();
    let input = serde_json::json!({"file_path": marker, "content": "capture\n"});
    emit(
        &serde_json::json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"role":"assistant","content": [{
                "type": "tool_use",
                "id": tool_use_id,
                "name": "Write",
                "input": input,
            }]},
        })
        .to_string(),
    );
    emit(
        &serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": input,
                "description": "capture-marker.txt",
                "tool_use_id": tool_use_id,
            },
        })
        .to_string(),
    );
}

fn expect_write_allow(stdin: &mut StdinLock<'_>, request_id: &str) {
    let write: serde_json::Value =
        serde_json::from_str(&read_line(stdin)).expect("a Write control response");
    if write["response"]["response"]["behavior"] != "allow"
        || write["response"]["request_id"] != request_id
    {
        exit(1);
    }
}

fn capture_destructive_command(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"control_request","request_id":"bad-bash","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"},"description":"destructive","tool_use_id":"toolu_bad"}}"#,
    );
    let _ = read_line(stdin);
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-bad"}"#,
    );
}

fn capture_destructive_write(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_bad","name":"Write","input":{"file_path":"/outside-cwd/destructive.txt","content":"overwrite"}}]}}"#,
    );
    emit(
        r#"{"type":"control_request","request_id":"bad-write","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/outside-cwd/destructive.txt","content":"overwrite"},"description":"destructive","tool_use_id":"toolu_bad"}}"#,
    );
    let _ = read_line(stdin);
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-bad"}"#,
    );
}

fn capture_unexpected_second(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("good-write", "toolu_write");
    expect_write_allow(stdin, "good-write");
    emit(
        r#"{"type":"control_request","request_id":"bad-read","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"capture-marker.txt"},"description":"unexpected","tool_use_id":"toolu_bad"}}"#,
    );
    let _ = read_line(stdin);
}

fn capture_write_before_bash(stdin: &mut StdinLock<'_>) {
    emit_write_request("early-write", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_missing_bash(stdin: &mut StdinLock<'_>) {
    emit_write_request("missing-bash-write", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_failed_bash(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", true, "capture");
    emit_write_request("failed-bash-write", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_wrong_bash(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf wrong", false, "wrong");
    emit_write_request("wrong-bash-write", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_duplicate_bash(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash-1", "printf capture", false, "capture");
    emit_bash_neighborhood("toolu_bash-2", "printf capture", false, "capture");
    emit_write_request("duplicate-bash-write", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_bash_snapshot_duplicate(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("snapshot-write", "toolu_write");
    expect_write_allow(stdin, "snapshot-write");
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-snapshot"}"#,
    );
}

fn capture_bash_control_response(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"printf capture"}}]}}"#,
    );
    emit(
        r#"{"type":"control_response","response":{"subtype":"success","request_id":"bash-request","response":{"behavior":"allow"}}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_bash","content":"capture","is_error":false}]}}"#,
    );
    emit_write_request("write-after-bash-response", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_bash_content_deviation(stdin: &mut StdinLock<'_>, shape: &str) {
    let tool = serde_json::json!({
        "type": "tool_use",
        "id": "toolu_bash",
        "name": "Bash",
        "input": {"command": "printf capture"},
    });
    let malformed = serde_json::json!({"type":"tool_use","name":42});
    let text = serde_json::json!({"type":"text","text":"provider prose"});
    let content = match shape {
        "malformed-extra" => vec![tool, malformed],
        "leading-text" => vec![text, tool],
        "trailing-text" => vec![tool, text],
        _ => unreachable!(),
    };
    emit(
        &serde_json::json!({
            "type":"assistant",
            "parent_tool_use_id":null,
            "message":{"role":"assistant","content":content},
        })
        .to_string(),
    );
    emit_write_request("write-after-bad-bash", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_user_content_deviation(stdin: &mut StdinLock<'_>, shape: &str) {
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"printf capture"}}]}}"#,
    );
    let result = serde_json::json!({
        "type":"tool_result",
        "tool_use_id":"toolu_bash",
        "content":"capture",
        "is_error":false,
    });
    let malformed =
        serde_json::json!({"type":"tool_result","tool_use_id":"toolu_bash","is_error":"false"});
    let text = serde_json::json!({"type":"text","text":"provider prose"});
    let content = match shape {
        "malformed-extra" => vec![result, malformed],
        "leading-text" => vec![text, result],
        "trailing-text" => vec![result, text],
        _ => unreachable!(),
    };
    emit(
        &serde_json::json!({
            "type":"user",
            "parent_tool_use_id":null,
            "message":{"role":"user","content":content},
        })
        .to_string(),
    );
    emit_write_request("write-after-bad-result", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_write_content_deviation(stdin: &mut StdinLock<'_>, shape: &str) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    let marker = std::env::current_dir()
        .expect("fixture cwd")
        .join("capture-marker.txt")
        .display()
        .to_string();
    let input = serde_json::json!({"file_path":marker,"content":"capture\n"});
    let tool = serde_json::json!({
        "type":"tool_use",
        "id":"toolu_write",
        "name":"Write",
        "input":input,
    });
    let malformed = serde_json::json!({"type":"tool_use","name":42});
    let text = serde_json::json!({"type":"text","text":"provider prose"});
    let content = match shape {
        "malformed-extra" => vec![tool, malformed],
        "leading-text" => vec![text, tool],
        "trailing-text" => vec![tool, text],
        _ => unreachable!(),
    };
    emit(
        &serde_json::json!({
            "type":"assistant",
            "parent_tool_use_id":null,
            "message":{"role":"assistant","content":content},
        })
        .to_string(),
    );
    emit(
        &serde_json::json!({
            "type":"control_request",
            "request_id":"write-after-bad-content",
            "request":{
                "subtype":"can_use_tool",
                "tool_name":"Write",
                "input":input,
                "tool_use_id":"toolu_write",
            },
        })
        .to_string(),
    );
    let _ = read_line(stdin);
}

fn capture_malformed_candidate(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":[],"name":"Bash","input":{"command":"printf capture"}}]}}"#,
    );
    emit_write_request("write-after-malformed-candidate", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_missing_write() {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-missing-write"}"#,
    );
}

fn capture_duplicate_write(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("write-1", "toolu_write-1");
    expect_write_allow(stdin, "write-1");
    emit_write_request("write-2", "toolu_write-1");
    let _ = read_line(stdin);
}

fn capture_missing_request_id(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_duplicate_request_id(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit_write_request("same-request", "toolu_write");
    expect_write_allow(stdin, "same-request");
    emit_write_request("same-request", "toolu_write");
    let _ = read_line(stdin);
}

fn capture_extra_tool(stdin: &mut StdinLock<'_>) {
    emit_bash_neighborhood("toolu_bash", "printf capture", false, "capture");
    emit(
        r#"{"type":"control_request","request_id":"extra-read","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"capture-marker.txt"},"description":"unexpected","tool_use_id":"toolu_extra"}}"#,
    );
    let _ = read_line(stdin);
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
    emit(
        r#"{"type":"system","subtype":"hook_started","hook":"SessionStart","session_id":"sess-d"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"hook_response","hook":"SessionStart","session_id":"sess-d"}"#,
    );
    emit(
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[],"session_id":"sess-d"}"#,
    );
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
