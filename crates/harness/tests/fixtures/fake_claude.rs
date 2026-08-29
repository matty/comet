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

/// The genuine captured initialize reply,
/// `crates/capture/tests/corpus/claude/2.1.228/model-discovery` frame 2, loaded
/// byte-for-byte rather than hand-typed. Its seven curated
/// models (including the double nesting and `resolvedModel`) are pinned in
/// `claude/discovery.rs`'s own unit tests, which load this exact frame too;
/// `models_come_back_live_and_merged` below only checks that `sonnet` merges
/// onto `claude-sonnet-5` and that a purely-curated id survives, both of
/// which this real payload still exercises.
fn initialize_reply() -> String {
    corpus_frame_payload("claude/2.1.228/model-discovery", 2)
}

/// Read one frame's `payload` field straight out of the sibling `comet-capture`
/// crate's corpus, without depending on that crate.
///
/// This file compiles as a `[[bin]]` target (`fake-claude`, spawned elsewhere
/// via `CARGO_BIN_EXE_fake-claude`), and Cargo never links a `[[bin]]`
/// target's `[dev-dependencies]` — only `[dependencies]` — regardless of
/// build command; `comet-harness`'s dev-only dependency on `comet-capture`
/// (D87 stage 7, so production cannot reach capture machinery) is invisible
/// here. `frame` in `crates/capture/src/corpus.rs` anticipated exactly this
/// split: it takes an explicit corpus root so a caller outside its own crate
/// never needs the crate itself, only the convention its `corpus_frame`
/// wraps — this is that convention, reimplemented locally rather than
/// imported. Panics rather than returning a `Result`: every caller here is a
/// test fixture that would immediately unwrap.
///
/// **Keep this in step with that function.** It is a copy, and the compiler
/// cannot see that it is: if the corpus root, the `sequence`/`payload` field
/// names or the line framing change over there, this keeps building and dies
/// inside the spawned child, whose stderr no test reads — the harness suite
/// then reports a discovery mismatch that names nothing about the corpus.
fn corpus_frame_payload(scenario: &str, sequence: u64) -> String {
    let events_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("capture")
        .join("tests")
        .join("corpus")
        .join(scenario)
        .join("events.jsonl");
    let text = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", events_path.display()));
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|error| {
            panic!(
                "{} has an invalid event line: {error}",
                events_path.display()
            )
        });
        if event["sequence"].as_u64() != Some(sequence) {
            continue;
        }
        return event["payload"]
            .as_str()
            .unwrap_or_else(|| panic!("corpus {scenario} frame {sequence} has no payload"))
            .to_owned();
    }
    panic!("corpus {scenario} has no frame {sequence}");
}

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
            emit(&initialize_reply());
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
    } else if first.contains("TaskCreate exactly twice") {
        // The real `capture/record/scenarios/claude.rs` `checklist` prompt
        // (matched by a substring unique to it, not a `scenario:` tag —
        // this scenario proves the neutral recorder against the exact wire
        // line the production `checklist` scenario sends, not a stand-in).
        // A model that ignores the tool instructions entirely: the run the
        // deleted `recording.rs` evidence guard used to bail on.
        checklist_no_tasks();
    } else if first.contains("Use Bash exactly once with input") {
        // The real `capture/record/scenarios/claude.rs` `approval` prompt
        // (matched by a substring unique to it, same pattern as the
        // checklist branch above). Three `can_use_tool` requests, so a test
        // can prove the neutral recorder answers every request it sees, not
        // just the first.
        approval_three_requests(&mut stdin);
    } else if first.contains("scenario:approval-missing-tool-name") {
        // A dedicated, custom-prompted scenario (not one of the real
        // production prompts — the test that drives this builds its own wire
        // line) whose single `can_use_tool` request has no `tool_name` at
        // all. Before the fix this drove, `pending_approval` treated that as
        // "not an approval request", never replied, and this `read_line`
        // blocked forever waiting for an answer that never came — exactly
        // the hang the fix exists to prevent. Checked before the generic
        // `scenario:approval` branch below since this tag contains that
        // substring too.
        approval_missing_tool_name(&mut stdin);
    } else if first.contains("scenario:checklist") {
        checklist();
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
    } else if first.contains("scenario:capture-non-frame-tolerance") {
        capture_non_frame_tolerance();
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
    // no call that spawned it is an incoherent sequence.
    //
    // Structurally shaped from the genuine corpus at
    // `tests/corpus/claude/2.1.229/subagent`, frame 115 (an `Agent` tool_use
    // with `description`/`subagent_type`/`run_in_background`/`caller`, same
    // as here) — NOT loaded byte-for-byte, because `description`, `prompt`
    // and `subagent_type` are not on `capture/allowlist/claude.txt` and
    // survive there only as `<Vn>` placeholders; loading the real bytes would
    // turn every readable assertion below into a placeholder-equality check
    // with nothing left to catch a wrong string. A prior version of this
    // comment cited `run2-claude-subagent.jsonl`, a raw pre-sanitization
    // capture that was never committed to this repository; that citation was
    // unverifiable and has been corrected to name the real, checked-in
    // corpus instead. The fake fixture's own convention is readable ids, not
    // the capture's opaque hex/toolu_ ones, so "sub-1" / "sub-1-task" stand
    // in for the real tool_use_id / task_id. Decodes as
    // ToolCall::Unknown{name:"Agent"}: nothing in this slice teaches
    // decode_tool_use a dedicated Agent arm.
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"sub-1","name":"Agent","input":{"description":"Read README and report first heading","subagent_type":"general-purpose","prompt":"Read the README.md file in the current directory and report what the first heading is.","run_in_background":false},"caller":{"type":"direct"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1496,"cache_read_input_tokens":34676,"output_tokens":3}}}"#,
    );
    // Shaped like `tests/corpus/claude/2.1.229/subagent` frame 116 (same subtype, same
    // field set: description/subagent_type/task_type/prompt/task_id/
    // tool_use_id) — see the corrected-provenance note above the `Agent`
    // tool_use two frames up.
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
    // Shaped like `tests/corpus/claude/2.1.229/subagent` frame 121 — same field set
    // (description/last_tool_name/subagent_type/task_id/tool_use_id/usage
    // with total_tokens, tool_uses, duration_ms). The real frame's usage
    // numbers are `<Vn>` placeholders (not on the allowlist), which is why
    // this fixture keeps hand-picked literals rather than the corpus bytes:
    // `happy_path_normalizes_events_and_filters_subagents` asserts the exact
    // numbers below, and a placeholder can't be that assertion.
    emit(
        r#"{"type":"system","subtype":"task_progress","task_id":"sub-1-task","tool_use_id":"sub-1","description":"Reading README.md","subagent_type":"general-purpose","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906},"last_tool_name":"Read"}"#,
    );
    // Shaped like `tests/corpus/claude/2.1.229/subagent` frame 124 — a PARTIAL patch,
    // status only, exactly like the real one.
    emit(
        r#"{"type":"system","subtype":"task_updated","task_id":"sub-1-task","patch":{"status":"completed","end_time":1786581776304}}"#,
    );
    // Shaped like `tests/corpus/claude/2.1.229/subagent` frame 125 — the frame carrying
    // the answer and the terminal usage totals.
    emit(
        r#"{"type":"system","subtype":"task_notification","task_id":"sub-1-task","tool_use_id":"sub-1","status":"completed","output_file":"C:\\tmp\\sub-1-task.output","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#,
    );
    // A `SendMessage`-resumed agent: the SAME task_id under a NEW
    // tool_use_id — the fifth distinct system/task_* SHAPE this scenario
    // emits, not a fifth SUBTYPE: there are still only four claimed subtypes
    // (task_started/task_progress/task_updated/task_notification, see
    // normalize.rs's own doc comment on `normalize_subagent_task`); this is a
    // second, differently-shaped `task_started` reusing one of them. The real
    // corpus records exactly this resumption at frames 174 (second
    // `task_started`, same `task_id`, new `tool_use_id`), 201 (`task_updated`)
    // and 202 (`task_notification`) — proven directly against those bytes by
    // `a_resumed_subagent_task_started_reuses_the_task_id_under_a_new_tool_use_id`
    // in `capture_corpus/corpus_frames.rs`. This is what exercises
    // `normalize.rs`'s `subagent_progress.remove(&f.task_id)` on
    // `task_started` through a real spawn: without it, this second terminal
    // reading would be compared against the first invocation's already-
    // terminal one and dropped as redundant, even though the summary
    // differs.
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
/// capture has ever recorded this — every subagent lifecycle in the
/// committed corpus (`tests/corpus/claude/2.1.229/subagent`) ends
/// `status: "completed"` — so `normalize.rs`'s `Failed`/`Cancelled` arms
/// were written by hand against
/// `.agents/rules/optional-wire-fields.md`'s rule and never exercised through
/// a real spawn. This scenario is that exercise: same lifecycle shape as
/// `happy()`'s subagent (task_started → child work → terminal reading), with
/// the status strings changed to `"failed"`. The child's own tool result is
/// also marked `is_error` for realism only — it carries `parent_tool_use_id`
/// and is filtered before normalization, so it does not feed either
/// assertion below.
/// The task-tool sequence, shaped from the recorded run at
/// `tests/corpus/claude/2.1.229/checklist`.
///
/// Every `tool_use_result` here is the literal shape that capture recorded —
/// `{"task":{"id","subject"}}` on a create, `statusChange {from,to}` on an
/// update. The `activeForm` rides the tool INPUT and never the result, which
/// is the split the decode exists for; a fixture that put it on the result
/// would let a wrong decode pass.
///
/// Ends with a bare update for task `"9"`, which this session never created —
/// the resumed-run case (capture §7). Nothing in the run names its subject, so
/// the fold has to build a row from a status and an `activeForm` alone.
fn checklist() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5-20251001","tools":["Bash","TaskCreate","TaskUpdate"],"cwd":"/tmp","session_id":"sess-checklist"}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tc-1","name":"TaskCreate","input":{"subject":"Alpha step","description":"The first step"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tc-1","content":"Task #1 created successfully: Alpha step"}]},"tool_use_result":{"task":{"id":"1","subject":"Alpha step"}}}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tc-2","name":"TaskCreate","input":{"subject":"Beta step","description":"The second step"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tc-2","content":"Task #2 created successfully: Beta step"}]},"tool_use_result":{"task":{"id":"2","subject":"Beta step"}}}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu-1","name":"TaskUpdate","input":{"taskId":"1","status":"in_progress","activeForm":"Working the first step"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-1","content":"Updated task #1 activeForm, status"}]},"tool_use_result":{"success":true,"taskId":"1","updatedFields":["activeForm","status"],"statusChange":{"from":"pending","to":"in_progress"}}}"#,
    );
    // No `activeForm` on the completion, exactly as the wire sends it: the row
    // must keep the label the previous frame gave it.
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu-2","name":"TaskUpdate","input":{"taskId":"1","status":"completed"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-2","content":"Updated task #1 status"}]},"tool_use_result":{"success":true,"taskId":"1","updatedFields":["status"],"statusChange":{"from":"in_progress","to":"completed"}}}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"tool_use","id":"tu-3","name":"TaskUpdate","input":{"taskId":"9","status":"in_progress","activeForm":"Working an inherited step"}}]}}"#,
    );
    emit(
        r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-3","content":"Updated task #9 activeForm, status"}]},"tool_use_result":{"success":true,"taskId":"9","updatedFields":["activeForm","status"],"statusChange":{"from":"pending","to":"in_progress"}}}"#,
    );
    emit(
        r#"{"type":"result","subtype":"success","result":"planned","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"session_id":"sess-checklist","total_cost_usd":0.01}"#,
    );
}

/// A checklist run that ignored the tool instructions entirely: a plain text
/// reply, no `TaskCreate`/`TaskUpdate` call anywhere. Proves the neutral
/// recorder's `checklist` scenario (`capture/record/scenarios/claude.rs`)
/// still returns a successful capture holding every frame — the case the now
/// deleted `recording.rs` evidence guard used to bail on, with "created 0
/// task(s) and updated 0; needed 2 distinct creates and at least 1 update".
fn checklist_no_tasks() {
    emit(
        r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5-20251001","tools":["Bash","TaskCreate","TaskUpdate"],"cwd":"/tmp","session_id":"sess-checklist-no-tasks"}"#,
    );
    emit(
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"text","text":"capture"}]}}"#,
    );
    emit(
        // Same non-flat shape as `happy()`'s own result frame — reintroducing
        // the flat `{input_tokens, output_tokens}`-only shape in a new
        // scenario is exactly what this slice's finding (both fake CLIs
        // emitted unrealistically flat frames) exists to stop.
        r#"{"type":"result","subtype":"success","result":"capture","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-checklist-no-tasks","total_cost_usd":0.01}"#,
    );
}

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
/// `control_request` frame shape follows `tests/corpus/claude/2.1.228/approval` frame 102,
/// with only the path swapped for one this machine can be
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

/// One non-JSON progress line before the real terminal frame — the neutral
/// recorder's `next_frame` must record it (on stdout) and skip it, not
/// error, and must still return the frame that follows.
fn capture_non_frame_tolerance() {
    emit("not json, a progress line");
    emit(
        // Same non-flat shape as `happy()`'s own result frame — reintroducing
        // the flat `{input_tokens, output_tokens}`-only shape in a new
        // scenario is exactly what this slice's finding (both fake CLIs
        // emitted unrealistically flat frames) exists to stop.
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-tolerance","total_cost_usd":0.01}"#,
    );
}

/// Three `can_use_tool` requests, each read back before the next is
/// emitted — a ping-pong round trip — followed by a terminal `result`.
/// Drives `record/scenarios/claude.rs`'s `approval` scenario body against its
/// real grant-time check, `claude_marker_grant`: the first two requests are
/// exactly the marker Bash command and the marker Write into the scenario's
/// own cwd, so both must be granted; the third is a Write to a file the
/// scenario never asked for, so it must be declined — and the run must still
/// reach its terminal frame rather than being aborted.
///
/// The marker path is built from `std::env::temp_dir()`, not
/// `std::env::current_dir()`: `approval_request` (the production side, in
/// `record/scenarios/claude.rs`) derives the scenario's cwd the same way
/// when `ScenarioInput::default()` leaves `cwd` unset, and that value is
/// passed to this child process unresolved, as the literal string, via
/// `LaunchDescriptor::cwd`/`Command::current_dir`. On macOS the two derivations
/// disagree: the kernel resolves `TMPDIR`'s `/var/folders/...` symlink to
/// `/private/var/folders/...` when the child actually `chdir`s into it, so
/// `current_dir()` returns the resolved path while the production side's
/// `claude_marker_grant` still compares against the unresolved
/// `std::env::temp_dir()` string — a real Claude model never hits this,
/// because it only ever echoes the literal path text from the prompt, but
/// this fixture computing its own answer independently made the two
/// diverge. Matching `temp_dir()` here (same env var, same process
/// inheritance, no `chdir` resolution involved on either side) keeps this
/// fixture and the production check deriving the identical unresolved
/// string on every platform.
fn approval_three_requests(stdin: &mut StdinLock<'_>) {
    let escape = |value: String| value.replace('\\', "\\\\").replace('"', "\\\"");
    let cwd = std::env::temp_dir();
    let marker = escape(cwd.join("capture-marker.txt").display().to_string());
    let unexpected = escape(cwd.join("unexpected.txt").display().to_string());

    emit(
        r#"{"type":"control_request","request_id":"approval-req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"printf capture"},"description":"printf capture","tool_use_id":"toolu_approval_1"}}"#,
    );
    let _ = read_line(stdin);
    emit(&format!(
        r#"{{"type":"control_request","request_id":"approval-req-2","request":{{"subtype":"can_use_tool","tool_name":"Write","input":{{"file_path":"{marker}","content":"capture\n"}},"description":"capture-marker.txt","tool_use_id":"toolu_approval_2"}}}}"#
    ));
    let _ = read_line(stdin);
    emit(&format!(
        r#"{{"type":"control_request","request_id":"approval-req-3","request":{{"subtype":"can_use_tool","tool_name":"Write","input":{{"file_path":"{unexpected}","content":"surprise\n"}},"description":"unexpected.txt","tool_use_id":"toolu_approval_3"}}}}"#
    ));
    let _ = read_line(stdin);
    emit(
        // Same non-flat shape as `happy()`'s own result frame — reintroducing
        // the flat `{input_tokens, output_tokens}`-only shape in a new
        // scenario is exactly what this slice's finding (both fake CLIs
        // emitted unrealistically flat frames) exists to stop.
        r#"{"type":"result","subtype":"success","result":"capture","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-approval-two","total_cost_usd":0.01}"#,
    );
}

/// A single `can_use_tool` request with no `tool_name` field at all,
/// followed by a terminal `result`. Reads the reply back before emitting the
/// terminal frame, exactly like `approval_three_requests` above, so a driver
/// that leaves this request unanswered blocks here forever instead of
/// failing loudly — reproducing the hang `pending_approval`'s fix (missing
/// `tool_name` folded into the decline arm, not treated as "not an approval
/// request") exists to prevent.
fn approval_missing_tool_name(stdin: &mut StdinLock<'_>) {
    emit(
        r#"{"type":"control_request","request_id":"approval-missing-1","request":{"subtype":"can_use_tool","input":{"command":"echo hi"},"description":"no tool name","tool_use_id":"toolu_approval_missing"}}"#,
    );
    let _ = read_line(stdin);
    emit(
        r#"{"type":"result","subtype":"success","result":"capture","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30000,"cache_creation_input_tokens":75},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":10,"outputTokens":20,"contextWindow":200000}},"session_id":"sess-approval-missing-tool-name","total_cost_usd":0.01}"#,
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
