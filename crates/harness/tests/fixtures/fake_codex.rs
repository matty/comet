//! Fake Codex app-server for comet-harness tests.
//!
//! Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, then either a
//! `model/list` discovery session or thread start/resume plus a scenario picked
//! from the turn/start prompt text. Driven by crates/harness/tests/codex.rs.
//!
//! Rust rather than `#!/bin/sh` because Windows cannot spawn a shell script:
//! the harness hands the path straight to `CreateProcess`, which rejects a
//! non-PE image with "%1 is not a valid Win32 application" (os error 193).

use std::io::{BufRead, StdinLock, Write};
use std::process::exit;
use std::time::Duration;

use serde_json::{Value, json};

/// One JSON-RPC line. Rust's stdout is line-buffered even on a pipe, so each
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

/// D46: strip `HANDLE_FLAG_INHERIT` from this process's own stdio handles so
/// a child THIS fixture spawns cannot silently inherit a duplicate of the
/// pipe connecting it to the real test harness — see the call site in
/// `wedge_with_child` for why that matters. Unix needs no equivalent:
/// `dup2`-based stdio redirection (what every `Stdio::null()`/`piped()` child
/// gets) overwrites fd 0/1/2 outright rather than leaving a spare inheritable
/// copy sitting in the handle table the way Windows does.
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

/// The request id: the last `"id":<digits>` on the line, mirroring the greedy
/// sed the shell fixture used.
fn rid(line: &str) -> String {
    match line.rfind("\"id\":") {
        Some(at) => line[at + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect(),
        None => String::new(),
    }
}

fn fail_turn(id: &str, message: &str) {
    emit(&format!(
        r#"{{"id":{id},"result":{{"turn":{{"id":"t-bad"}}}}}}"#
    ));
    emit(&format!(
        r#"{{"method":"turn/failed","params":{{"turn":{{"id":"t-bad","error":{{"message":"{message}"}}}}}}}}"#
    ));
}

/// One `model/list` entry, built through `serde_json` so a Windows path or a
/// quoted cursor cannot produce a fixture that emits invalid JSON.
///
/// `modalities` is an `Option` because a model may omit `inputModalities`
/// altogether — the absent case the schema documents as images-on.
fn model_entry(
    id: &str,
    display_name: &str,
    hidden: bool,
    levels: &[&str],
    modalities: Option<&[&str]>,
) -> Value {
    // Objects, not strings, per `tests/corpus/codex/0.147.0/model-discovery` frame 6 — the
    // one the phase spec's field summary gets wrong.
    let efforts: Vec<Value> = levels
        .iter()
        .map(|l| json!({"reasoningEffort": l, "description": format!("effort {l}")}))
        .collect();
    let mut entry = json!({
        "id": id,
        "model": id,
        "displayName": display_name,
        "description": format!("{id} description"),
        "modelSpecialty": null,
        "hidden": hidden,
        "isDefault": false,
        "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": efforts,
        "supportsPersonality": false,
        "additionalSpeedTiers": [],
        "serviceTiers": [],
        "defaultServiceTier": null,
    });
    if let Some(modalities) = modalities {
        entry["inputModalities"] = json!(modalities);
    }
    entry
}

/// The discovery catalog. Three curated ids, two models only the server knows,
/// and one hidden model the adapter must drop:
///
/// - `gpt-5.3-codex-spark` is text-only, so the live answer has to override a
///   curated `accepts_images: true`.
/// - `gpt-5.7-nova` omits `inputModalities` entirely — the absent case, which
///   no live model produces and which the schema documents as images-on.
/// - `gpt-5.7-nova` also reports `ultra`, which the provider genuinely supports
///   and which must survive onto a model nobody has curated.
/// - `codex-home-echo` carries the child's own `CODEX_HOME` as its label. The
///   login check reads `auth.json` from a home the parent resolved, and only
///   the child can say which home the CLI was actually handed; a test that
///   cannot see this cannot tell the two apart.
/// - `gpt-5.5` is flagged `isDefault: true` (D72, `docs/debt/README.md`) even
///   though it is neither the first curated row nor the first row this fixture
///   serves: the merged catalog must still come back led by it, not by
///   whatever `gpt-5.6-sol`'s curated-order coincidence used to paper over.
fn discovery_models() -> Vec<Value> {
    const ULTRA: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
    const XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
    const IMAGE: Option<&[&str]> = Some(&["text", "image"]);
    const TEXT: Option<&[&str]> = Some(&["text"]);
    let home = std::env::var("CODEX_HOME").unwrap_or_else(|_| "unset".into());
    let mut gpt_5_5 = model_entry("gpt-5.5", "gpt-5.5 label", false, XHIGH, IMAGE);
    gpt_5_5["isDefault"] = json!(true);
    vec![
        model_entry("gpt-5.6-sol", "gpt-5.6-sol label", false, ULTRA, IMAGE),
        gpt_5_5,
        model_entry(
            "gpt-5.3-codex-spark",
            "gpt-5.3-codex-spark label",
            false,
            XHIGH,
            TEXT,
        ),
        model_entry("gpt-5.7-nova", "gpt-5.7-nova label", false, ULTRA, None),
        model_entry("codex-home-echo", &home, false, XHIGH, TEXT),
        model_entry(
            "codex-auto-review",
            "codex-auto-review label",
            true,
            XHIGH,
            IMAGE,
        ),
    ]
}

/// Serve `model/list` until the client stops asking.
///
/// **Pages by default**, two at a time, because the real server returns all
/// seven models in one page and would never exercise the client's loop. The
/// last page carries an explicit `"nextCursor":null` rather than omitting the
/// key, as 0.147.0 does.
///
/// **The cursor is deliberately hostile.** The real one is a stringified
/// offset, but the schema calls it opaque, so this one keeps the offset and
/// adds a quote and a backslash: a client that pastes it into a request string
/// rather than serializing it sends malformed JSON on page two, and the paging
/// silently degrades to the curated catalog.
fn cursor_for(offset: usize) -> String {
    format!("{offset}\"\\ opaque")
}

/// The offset back out of a cursor this fixture issued.
fn offset_of(cursor: &str) -> usize {
    cursor
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn model_list(stdin: &mut StdinLock<'_>, first: &str) {
    let models = discovery_models();
    let mut line = first.to_string();
    // The reviewed stream emits this after initialize succeeds and before the
    // model-list reply. A client that reads the next line rather than the line
    // carrying its requested id takes this notification as its answer.
    emit(
        r#"{"method":"remoteControl/status/changed","params":{"status":"disabled","hostname":"fake"}}"#,
    );
    loop {
        // Parsed rather than scanned: the cursor this fixture issues contains a
        // quote, so a substring search would read it back truncated and serve
        // the wrong page — hiding the very bug the hostile cursor exists to
        // catch.
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => exit(1),
        };
        let params = &request["params"];
        let offset = params["cursor"].as_str().map(offset_of).unwrap_or(0);
        let limit = params["limit"].as_u64().unwrap_or(2).max(1) as usize;
        let end = (offset + limit).min(models.len());
        let page: Vec<Value> = models.get(offset..end).unwrap_or_default().to_vec();
        let next = if end < models.len() {
            Value::String(cursor_for(end))
        } else {
            Value::Null
        };
        emit(
            &json!({"id": request["id"], "result": {"data": page, "nextCursor": next}}).to_string(),
        );

        // EOF is the ordinary end of a discovery session: the client closes
        // stdin once it has every page. Unlike `read_line`, that is not a
        // failure here.
        let mut buf = String::new();
        match stdin.read_line(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => line = buf.trim_end_matches(['\r', '\n']).to_string(),
        }
        if !line.contains(r#""method":"model/list""#) {
            return;
        }
    }
}

fn main() {
    // D46: this fixture re-execs itself under this flag to play the
    // "grandchild" role for `wedge_with_child` below — the same binary, so
    // proving a leaked descendant needs no second `[[bin]]` target. It never
    // touches stdio, so it cannot be mistaken for the real protocol child.
    if std::env::args().any(|a| a == "--sleep-forever") {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    if std::env::var_os("CODEX_HOME").is_some() {
        fill_stderr();
    }
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    // ---- handshake ---------------------------------------------------------
    let line = read_line(&mut stdin); // initialize
    for want in [
        r#""method":"initialize""#,
        r#""experimentalApi":true"#,
        r#""name":"comet-native""#,
    ] {
        if !line.contains(want) {
            exit(1);
        }
    }
    emit(&format!(
        r#"{{"id":{},"result":{{"userAgent":"fake-codex"}}}}"#,
        rid(&line)
    ));

    let line = read_line(&mut stdin); // initialized notification (no reply)
    if !line.contains(r#""method":"initialized""#) {
        exit(1);
    }

    // ---- discovery, or a real session --------------------------------------
    // A discovery session asks `model/list` and never starts a thread, so the
    // branch is on the next method rather than on a scenario marker: it carries
    // no prompt to put one in.
    let thread_line = read_line(&mut stdin);
    if thread_line.contains(r#""method":"model/list""#) {
        model_list(&mut stdin, &thread_line);
        return;
    }
    if thread_line.contains(r#""method":"thread/resume""#) {
        if thread_line.contains(r#""threadId":"resume-fail""#) {
            // Missing/foreign rollout: reject, expect the fresh-start fallback.
            emit(&format!(
                r#"{{"id":{},"error":{{"code":-32600,"message":"rollout not found"}}}}"#,
                rid(&thread_line)
            ));
            let start = read_line(&mut stdin);
            if !start.contains(r#""method":"thread/start""#) {
                exit(1);
            }
            emit(&format!(
                r#"{{"id":{},"result":{{"thread":{{"id":"th-fresh"}}}}}}"#,
                rid(&start)
            ));
        } else {
            emit(&format!(
                r#"{{"id":{},"result":{{"thread":{{"id":"th-resumed"}}}}}}"#,
                rid(&thread_line)
            ));
        }
    } else if thread_line.contains(r#""method":"thread/start""#) {
        emit(&format!(
            r#"{{"id":{},"result":{{"thread":{{"id":"th-1"}}}}}}"#,
            rid(&thread_line)
        ));
    } else {
        exit(1);
    }

    // ---- first turn --------------------------------------------------------
    let turn_line = read_line(&mut stdin);
    let tid = rid(&turn_line);

    // The real production prompt from `record/scenarios/codex.rs`'s
    // `approval_request` (built through `codex_approval_prompt`) — driven
    // through its own real production request builder, the same rationale
    // as the `steer`/`interrupt` branches below.
    if turn_line.contains("three separate times, then add exactly one file")
        || turn_line.contains("Run this exact command once and report success:")
    // Same rationale, for `approval_on_request_request`'s real production
    // prompt (built through `approval_on_request_prompt`). The prefix is
    // stable across platforms even though the embedded command text
    // after it is not (`approval_marker_command` differs on Windows vs.
    // Unix).
    {
        capture_approval_two_requests(&mut stdin, &tid);
    } else if turn_line.contains("scenario:capture-fresh")
        // Additive alongside the `scenario:capture-fresh` test marker, same
        // rationale as the `steer`/`interrupt` branches below — the real
        // production prompt from `record/scenarios/codex.rs`'s
        // `fresh_text_request`.
        || turn_line.contains("Reply with the single word capture.")
    {
        simple_completed(&tid);
    } else if turn_line.contains("scenario:happy") {
        happy(&turn_line, &thread_line, &tid);
    // NOTE: steer-race before steer — the first match wins, and "scenario:steer"
    // is a prefix of "scenario:steer-race".
    } else if turn_line.contains("scenario:steer-race") {
        steer_race(&mut stdin, &tid);
    } else if turn_line.contains("scenario:steer")
        // The real capture-recorder prompt (`record/scenarios/codex.rs`'s
        // `steer_request`), additive alongside the `scenario:steer` test
        // marker above so the ported `steer` scenario's own tests can drive
        // this branch through their real production request builder instead
        // of a hand-rolled one — same rationale as `fake_claude.rs` matching
        // a substring of Claude's real checklist prompt.
        || turn_line.contains("Begin a short response, then accept the follow-up instruction.")
    {
        steer(&mut stdin, &tid);
    } else if turn_line.contains("scenario:auto-reviewer") {
        auto_reviewer(&thread_line, &tid);
    } else if turn_line.contains("scenario:echo-policy") {
        echo_policy(&turn_line, &thread_line, &tid);
    } else if turn_line.contains("scenario:approve") {
        approve(&mut stdin, &turn_line, &thread_line, &tid);
    } else if turn_line.contains("scenario:decline") {
        decline(&mut stdin, &tid);
    } else if turn_line.contains("scenario:interrupt")
        // Additive alongside the `scenario:interrupt` test marker, same
        // rationale as the `steer` branch above — the real production prompt
        // from `record/scenarios/codex.rs`'s `interruption_request`.
        || turn_line.contains("Count upward slowly and keep working until interrupted.")
    {
        interrupt(&mut stdin, &tid);
    // D46: before the plain "scenario:wedge" check below — that check is a
    // `.contains`, and "scenario:wedge-with-child" contains "scenario:wedge"
    // as a substring, same ordering hazard `steer-race`/`steer` above solves
    // the same way.
    } else if turn_line.contains("scenario:wedge-with-child") {
        wedge_with_child(&turn_line, &tid);
    } else if turn_line.contains("scenario:wedge") {
        wedge(&tid);
    // D45: reusable lifecycle-fault primitives — see each function's own
    // comment for which fault on docs/debt/D45-provider-lifecycle-fault-matrix.md
    // it expresses.
    } else if turn_line.contains("scenario:crash-mid-turn") {
        crash_mid_turn(&tid);
    } else if turn_line.contains("scenario:partial-frame") {
        partial_frame_then_exit(&tid);
    } else if turn_line.contains("scenario:die-after-approval") {
        die_after_approval(&tid);
    } else if turn_line.contains("scenario:duplicate-completion") {
        duplicate_completion(&tid);
    } else if turn_line.contains("scenario:late-completion-after-interrupt") {
        late_completion_after_interrupt(&mut stdin, &tid);
    // D48: a selected slice of the event-order state space
    // (docs/debt/D48-provider-state-sequences.md) not already covered by a
    // named scenario or by D45's primitives above — see each function's own
    // comment for the exact race it expresses.
    } else if turn_line.contains("scenario:stream-before-ack") {
        stream_before_ack(&tid);
    } else if turn_line.contains("scenario:orphan-after-completion") {
        orphan_notification_after_completion(&tid);
    } else if turn_line.contains("scenario:fail") {
        fail(&tid);
    } else if turn_line.contains("scenario:resumed") {
        resumed(&tid);
    } else if turn_line.contains("scenario:notices") {
        notices(&tid);
    } else if turn_line.contains("scenario:diagnostics") {
        diagnostics(&mut stdin, &tid);
    } else {
        fail_turn(&tid, "unknown scenario");
    }
}

fn simple_completed(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

/// Two `.../requestApproval` requests, each read back before the next is
/// emitted, then a terminal `turn/completed`. Drives
/// `record/scenarios/codex.rs`'s `approval` and `approval_on_request`
/// scenario bodies, which (unlike the deleted `approval/codex.rs`
/// validators this replaces) do not care which approval method is asked
/// about, how many command executions preceded it, or in what order; they
/// only have to answer every `.../requestApproval` request they see. The
/// method name here (`item/fileChange/requestApproval`) is arbitrary for
/// this purpose — `pending_approval` in the production driver recognizes any
/// method ending in `/requestApproval` — so one fixture body serves both
/// dispatch branches above.
fn capture_approval_two_requests(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"id":0,"method":"item/fileChange/requestApproval","params":{"itemId":"f1"}}"#);
    let _ = read_line(stdin);
    emit(r#"{"id":1,"method":"item/fileChange/requestApproval","params":{"itemId":"f2"}}"#);
    let _ = read_line(stdin);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn happy(turn_line: &str, thread_line: &str, tid: &str) {
    // Verify the turn/start + thread/start params the harness must send.
    for want in [
        r#""method":"turn/start""#,
        r#""effort":"ultra""#,
        r#""model":"gpt-5.6-sol""#,
        r#""networkAccess":true"#,
        r#""type":"workspaceWrite""#,
        r#""summary":"auto""#,
        r#""serviceTier":"fast""#,
    ] {
        if !turn_line.contains(want) {
            fail_turn(tid, &format!("turn param missing: {want}"));
            return;
        }
    }
    // `approvalPolicy` is deliberately NOT asserted here: `happy` is reused by
    // tests running in different runtime modes, and the policy is derived from
    // the mode. `scenario:echo-policy` pins all four values instead.
    for want in [
        r#""sandbox":"workspace-write""#,
        r#""cwd":"/tmp""#,
        r#""serviceTier":"fast""#,
        r#""approvalsReviewer":"user""#,
    ] {
        if !thread_line.contains(want) {
            fail_turn(tid, &format!("thread param missing: {want}"));
            return;
        }
    }
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    // Deltas — both field spellings must be accepted.
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"Hello"}}"#);
    emit(
        r#"{"method":"item/reasoning/textDelta","params":{"itemId":"r1","textDelta":"thinking hard"}}"#,
    );
    emit(
        r#"{"method":"item/reasoning/summaryTextDelta","params":{"itemId":"r1","delta":"summary"}}"#,
    );
    // Item lifecycles.
    emit(
        r#"{"method":"item/started","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la"}}}"#,
    );
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la","status":"completed","exitCode":1}}}"#,
    );
    emit(
        r#"{"method":"item/started","params":{"item":{"id":"f1","type":"fileChange","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}"#,
    );
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"f1","type":"fileChange","status":"completed","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}"#,
    );
    emit(
        r#"{"method":"item/started","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","arguments":{"q":"bug"}}}}"#,
    );
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","status":"failed"}}}"#,
    );
    emit(
        r#"{"method":"item/started","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}"#,
    );
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}"#,
    );
    // A `todoList` item used to be emitted here to exercise the old
    // `ToolCall::Todo` decode. It is gone: no supported codex-cli sends one
    // (0.147.0 is the floor — `docs/testing/supported-provider-versions.md`),
    // Codex's plan arrives as `turn/plan/updated`, and a fixture that keeps
    // sending a shape no supported version produces makes the happy path
    // raise a diagnostic that a real healthy run never would.
    // Streamed agentMessage: completed text must NOT re-emit.
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"m1","type":"agentMessage","text":"Hello world"}}}"#,
    );
    // Never-streamed agentMessage: completed text is the fallback delta.
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"m2","type":"agentMessage","text":"unstreamed tail"}}}"#,
    );
    // Unknown notification methods must be tolerated.
    emit(r#"{"method":"some/unknownNotification","params":{"x":1}}"#);
    emit(
        // `total` differs from `last` and the window is present, as on the
        // real wire — a fixture carrying only `last` cannot catch a reader
        // that takes the cumulative figure and draws it against the window.
        r#"{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":900,"outputTokens":90},"last":{"inputTokens":42,"outputTokens":7},"modelContextWindow":258400}}}"#,
    );
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn auto_reviewer(thread_line: &str, tid: &str) {
    // `Auto` is the only runtime mode that hands approval review to the
    // provider. `happy` is the only other scenario whose thread-line
    // assertions inspect `approvalsReviewer` (pinned to "user"); the rest
    // check the thread line only for `approvalPolicy`.
    if !thread_line.contains(r#""approvalsReviewer":"auto_review""#) {
        fail_turn(
            tid,
            "thread param missing: \"approvalsReviewer\":\"auto_review\"",
        );
        return;
    }
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn steer_race(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    let steer_line = read_line(stdin);
    let sid = rid(&steer_line);
    if !steer_line.contains(r#""method":"turn/steer""#) {
        emit(&format!(r#"{{"id":{sid},"result":{{}}}}"#));
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/steer"}}}}"#,
        );
        return;
    }
    // The turn completed under the steer: reject, then announce completion.
    emit(&format!(
        r#"{{"id":{sid},"error":{{"code":-32602,"message":"turn already completed"}}}}"#
    ));
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
    // The harness must fall back to a follow-up turn/start carrying the text.
    let follow_line = read_line(stdin);
    let fid = rid(&follow_line);
    if follow_line.contains(r#""method":"turn/start""#) && follow_line.contains("redirect please") {
        emit(&format!(
            r#"{{"id":{fid},"result":{{"turn":{{"id":"t-2"}}}}}}"#
        ));
        emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-2"}}}"#);
        emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"fallback"}}"#);
        emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-2"}}}"#);
    } else {
        fail_turn(&fid, "expected fallback turn/start with steer text");
    }
}

fn steer(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"first"}}"#);
    let steer_line = read_line(stdin);
    let sid = rid(&steer_line);
    if steer_line.contains(r#""method":"turn/steer""#)
        && steer_line.contains(r#""expectedTurnId":"t-1""#)
        && (steer_line.contains("redirect please")
            || steer_line.contains("Capture steering message."))
    {
        emit(&format!(r#"{{"id":{sid},"result":{{}}}}"#));
        emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"steered"}}"#);
        emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
    } else {
        emit(&format!(
            r#"{{"id":{sid},"error":{{"code":-32600,"message":"bad steer"}}}}"#
        ));
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"steer verification failed"}}}}"#,
        );
    }
}

/// Fail the turn with the `approvalPolicy` seen on both lines, so a test can
/// assert the exact wire value per runtime mode without a scenario each.
fn echo_policy(turn_line: &str, thread_line: &str, tid: &str) {
    let seen = |line: &str| {
        line.split(r#""approvalPolicy":""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("<absent>")
            .to_owned()
    };
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    fail_turn(
        tid,
        &format!("thread={} turn={}", seen(thread_line), seen(turn_line)),
    );
}

fn approve(stdin: &mut StdinLock<'_>, turn_line: &str, thread_line: &str, tid: &str) {
    // The scenario runs in `ApprovalRequired`, the mode that asks before every
    // command.
    if !thread_line.contains(r#""approvalPolicy":"untrusted""#) {
        fail_turn(tid, "thread approvalPolicy should be untrusted");
        return;
    }
    if !turn_line.contains(r#""approvalPolicy":"untrusted""#) {
        fail_turn(tid, "turn approvalPolicy should be untrusted");
        return;
    }
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    // The launcher invocation and the parsed action differ, as they do live:
    // the card must name the action, not the pwsh path around it.
    emit(
        r#"{"id":101,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"pwsh.exe -Command 'rm -rf /tmp/x'","commandActions":[{"type":"unknown","command":"rm -rf /tmp/x"}],"cwd":"/tmp"}}"#,
    );
    let a1 = read_line(stdin);
    if !(a1.contains(r#""id":101"#) && a1.contains(r#""decision":"accept""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"command approval not accepted"}}}}"#,
        );
        return;
    }
    // A file-change approval carries NO path — the detail is only ever on the
    // item that precedes it, so the fake announces it the way the server does.
    emit(
        r#"{"method":"item/started","params":{"item":{"type":"fileChange","id":"f1","status":"inProgress","changes":[{"path":"/tmp/a.rs","kind":{"type":"update"},"diff":"@@ -1 +1,2 @@\n one\n+two\n"}]}}}"#,
    );
    emit(
        r#"{"id":102,"method":"item/fileChange/requestApproval","params":{"itemId":"f1","startedAtMs":1,"reason":null,"grantRoot":null}}"#,
    );
    let a2 = read_line(stdin);
    if !(a2.contains(r#""id":102"#) && a2.contains(r#""decision":"accept""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"file approval not accepted"}}}}"#,
        );
        return;
    }
    // An approval whose item was never announced: the adapter must still ask,
    // as an Unknown, rather than inventing a path or dropping the request.
    emit(
        r#"{"id":103,"method":"item/fileChange/requestApproval","params":{"itemId":"never-seen","startedAtMs":1}}"#,
    );
    let a3 = read_line(stdin);
    if !(a3.contains(r#""id":103"#) && a3.contains(r#""decision":"accept""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"unjoined approval not accepted"}}}}"#,
        );
        return;
    }
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn decline(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(
        r#"{"id":201,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"rm -rf /"}}"#,
    );
    let a1 = read_line(stdin);
    if !(a1.contains(r#""id":201"#) && a1.contains(r#""decision":"decline""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected decline"}}}}"#,
        );
        return;
    }
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn interrupt(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}"#);
    let int_line = read_line(stdin);
    let iid = rid(&int_line);
    if int_line.contains(r#""method":"turn/interrupt""#) && int_line.contains(r#""turnId":"t-1""#) {
        emit(&format!(r#"{{"id":{iid},"result":{{}}}}"#));
        emit(r#"{"method":"turn/aborted","params":{"turn":{"id":"t-1"}}}"#);
    } else {
        emit(&format!(r#"{{"id":{iid},"result":{{}}}}"#));
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/interrupt"}}}}"#,
        );
    }
}

fn wedge(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}"#);
    // Ignore turn/interrupt entirely — forces the kill escalation path.
    std::thread::sleep(Duration::from_secs(30));
}

/// D46's fixture: `wedge`, plus a real OS grandchild recorded before the hang.
///
/// `wedge` alone only proves this fixture's own pid gets reaped — it has no
/// descendants, so it cannot tell a real cleanup from a leak. This spawns one
/// (this same binary, re-invoked with `--sleep-forever`, so no new fixture
/// binary is needed) and records both this process's pid and the
/// grandchild's to the file path carried in the turn's prompt text, so the
/// test can check the grandchild's fate — not just this fixture's — once
/// cancellation has run.
///
/// The prompt is parsed as JSON rather than substring-matched like the other
/// scenario markers: the path after the marker can itself contain a `:` or a
/// backslash on Windows, and this needs the exact string, not a lossy scan.
fn wedge_with_child(turn_line: &str, tid: &str) {
    const MARKER: &str = "scenario:wedge-with-child|";
    let request: Value = match serde_json::from_str(turn_line) {
        Ok(request) => request,
        Err(_) => exit(1),
    };
    let text = match request["params"]["input"][0]["text"].as_str() {
        Some(text) => text,
        None => exit(1),
    };
    let path = match text.strip_prefix(MARKER) {
        Some(path) => path,
        None => exit(1),
    };

    // Windows only: `CreateProcess` inherits every *inheritable* handle open
    // in this process once any stdio redirection makes it pass
    // `bInheritHandles=TRUE` — not just the three handles named in
    // `Stdio::null()` below. Left alone, the grandchild would pick up a
    // duplicate of the pipe THIS fixture was handed to talk to the real
    // harness, and that duplicate — sitting unused in a process that never
    // exits — keeps the pipe from ever reaching EOF for the harness's
    // reader, even after this fixture is killed. That is a real Windows
    // hazard a careless provider subprocess could hit too, not just a test
    // artifact; stripping it here keeps this test isolated to the one claim
    // it means to prove (the grandchild itself outliving cancellation).
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

    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}"#);
    // Ignore turn/interrupt entirely, like `wedge` — forces the kill
    // escalation path.
    std::thread::sleep(Duration::from_secs(30));
}

/// D45 primitive: **stderr_then_exit, mid-turn.** `fake_codex_init_crash.rs`
/// already proves the harness keeps a crash's stderr fully private when it
/// happens during the handshake (`STARTUP_FAILURE_MESSAGE` is a canned
/// sentence that never reads `stderr_tail` at all). Once a turn is active,
/// `run_session`'s teardown takes a DIFFERENT arm — an unexpected exit calls
/// `crate::crash_message`, which deliberately surfaces `describe_exit`'s exit
/// code and a bounded `StderrTail` excerpt (`.agents/rules/user-facing-errors.md`'s
/// "one cleaned line of a failing CLI's own stderr is deliberately kept").
/// Nothing exercised that second call site before this.
///
/// The 100ms sleep after flushing stderr — and before actually exiting — is
/// deliberate, not padding: the harness reads this process's stdout and
/// stderr on two independent tasks, and closing stdout (by exiting) is what
/// the main loop treats as terminal. Without the sleep, the process could
/// exit and close stdout before the OTHER task has drained and stored the
/// stderr line it just wrote, racing `stderr_tail`'s content against the
/// `Eof` that reads it. Giving the stderr reader a bounded head start makes
/// the fault deterministic instead of occasionally losing its own evidence.
fn crash_mid_turn(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}"#);
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "boom: fake codex crashed mid-turn");
    let _ = stderr.flush();
    drop(stderr);
    std::thread::sleep(Duration::from_millis(100));
    exit(66);
}

/// D45 primitive: **partial_line.** "stdout closes halfway through a frame":
/// half a JSON object, no trailing newline, then exit. `BufReader::lines()`
/// (`crates/harness/src/jsonrpc.rs`'s `read_loop`) still yields whatever was
/// buffered as one final "line" once it hits EOF with no delimiter, so this
/// proves that truncated tail becomes `Incoming::Malformed(NotJson)` —
/// exactly one diagnostic — rather than being silently swallowed, and that
/// the exit right behind it still produces a bounded, private `Done` through
/// the same mid-turn crash arm `crash_mid_turn` above exercises.
///
/// The same settle delay as `crash_mid_turn`, for the same reason: give the
/// reader a bounded head start on the half-frame before the pipe closes for
/// real, rather than racing "is there more coming" against "the child died".
fn partial_frame_then_exit(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        r#"{{"method":"item/agentMessage/delta","params":{{"itemId":"m1","delta":"cu"#
    );
    let _ = out.flush();
    std::thread::sleep(Duration::from_millis(100));
    exit(0);
}

/// D45 primitive: **stdin breaks while Comet writes a decision.** Raises a
/// command-approval request, then exits immediately without ever reading a
/// reply. `crates/harness/src/jsonrpc.rs` documents that a write to a dead
/// child's stdin (EPIPE) is "tolerated and logged, matching the TS harness's
/// swallowed-EPIPE behavior" — this is what actually drives that write onto a
/// closed pipe instead of merely asserting the comment is true. Which side of
/// the race the write lands on (before or after this process actually exits)
/// is not pinned down — the OS may deliver either order — so what this proves
/// is the invariant that has to hold either way: the run still ends in one
/// bounded, non-panicking `Done`, never a hang.
fn die_after_approval(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(
        r#"{"id":301,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"echo hi"}}"#,
    );
    exit(0);
}

/// D45 primitive: **duplicate.** The same terminal notification twice for the
/// same turn id, with an unrelated notification in between (also covering "a
/// response ... arrives among unrelated notifications" from the same list).
///
/// This is NOT a test that the harness ignores the duplicate — it does not,
/// and it is not for lack of a guard. `TurnRouter::is_completed`
/// (`crates/harness/src/codex/mod.rs:609`) is real and consulted elsewhere —
/// `note_started` (`:614`) and `adopt_started` (`:640`) both refuse to revive
/// an id it already has, and the steer-queue decision (`:1182`) reads it too.
/// The `"turn/completed"` arm (`:957`) calls `note_completed` to RECORD the
/// id but never calls `is_completed` to check it first, so nothing stops the
/// arm from running its full send-Done sequence a second time for an id
/// already recorded as finished. The test this scenario drives documents
/// that as-observed behavior (a second `Done` reaches the stream) rather than
/// asserting the stronger "exactly one terminal outcome" D45's own page says
/// the suite has no invariant for yet — closing that gap needs wiring
/// `is_completed` into this one arm, a change in
/// `crates/harness/src/codex/mod.rs`, out of scope here.
fn duplicate_completion(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"done"}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
    emit(
        r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":10}}}}"#,
    );
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

/// D45 primitive: **late_reply, specifically "delayed until after
/// cancellation".** Acknowledges `turn/interrupt` like `interrupt()` above,
/// but then — instead of the expected `turn/aborted` — waits past the point
/// the harness has already committed to `interrupted = true` and sends a
/// plain `turn/completed`, as if the provider's own completion notification
/// won its race with the abort. `run_session`'s `"turn/completed"` arm reads
/// `interrupted` when it computes `status`, so the reply arriving late must
/// still resolve to `DoneStatus::Interrupted`, not `Completed`.
///
/// The delay is bounded well inside the test's own `interrupt_grace`, so the
/// kill escalation timer this races against never fires — this is the
/// "arrives before escalation" side of the race, on purpose. Escalation
/// firing anyway (a hard kill during a merely-slow-not-wedged process) is a
/// different fault, already covered by `wedge`.
///
/// The delay itself turned out NOT to be what the assertion depends on:
/// `interrupted` is set synchronously the moment the harness's own interrupt
/// token fires, independent of anything this process does, so an immediate
/// `turn/completed` right after `turn/interrupt`'s ack resolves to
/// `Interrupted` just as reliably. What the test actually falsifies on is
/// whether `interrupted` was ever true in the first place — see the driving
/// test's own comment for how that was proven.
fn late_completion_after_interrupt(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}"#);
    let int_line = read_line(stdin);
    let iid = rid(&int_line);
    if !(int_line.contains(r#""method":"turn/interrupt""#)
        && int_line.contains(r#""turnId":"t-1""#))
    {
        emit(&format!(r#"{{"id":{iid},"result":{{}}}}"#));
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/interrupt"}}}}"#,
        );
        return;
    }
    emit(&format!(r#"{{"id":{iid},"result":{{}}}}"#));
    std::thread::sleep(Duration::from_millis(100));
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

/// D48 selection: the response/notification race in the OTHER direction from
/// every other scenario in this file. Every one of them acks `turn/start`
/// (the JSON-RPC RESPONSE) as literally the first line of the turn, then
/// emits `turn/started` and later notifications. Real JSON-RPC does not
/// require that order — a server may emit its own notifications before, or
/// interleaved with, acking the call that triggered them. `TurnRouter`'s own
/// doc comment (`crates/harness/src/codex/mod.rs`) already says the ack and
/// the lifecycle notifications "may arrive in either order", and a direct
/// unit test (`turn_router_never_revives_completed_turns`, same file) proves
/// `TurnRouter` itself tolerates it — but `start_turn`'s ack is a bare
/// `.await` BEFORE `run_session`'s `tokio::select!` loop even starts, so
/// nothing before this proved the surrounding plumbing (the buffered
/// `incoming` mpsc channel) carries this correctly end to end.
fn stream_before_ack(tid: &str) {
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"before"}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"-ack"}}"#);
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

/// D48 selection: a notification that names no active turn and nothing
/// queued, arriving AFTER `turn/completed`, while the session stays open for
/// more steering (nothing failed and the mailbox is not dropped).
/// `run_session`'s main loop has no gate on `router.active.is_some()` before
/// decoding and forwarding a notification — see this scenario's driving
/// test for the exact arm and why this documents CURRENT behavior, not an
/// invariant the harness actually enforces.
fn orphan_notification_after_completion(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"done"}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
    // Belongs to no turn: router.active is None here and nothing is queued.
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"orphan","delta":"orphaned"}}"#);
}

fn fail(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"boom"}}}}"#);
}

fn resumed(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn notices(tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    // MCP lifecycle: "starting" is transient (no notice); failed is terminal.
    emit(
        r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"linear","status":"starting"}}"#,
    );
    // Carries BOTH the raw `error` (which must never reach the doc) and the
    // structured `failureReason` (which becomes Comet's own actionable copy).
    emit(
        r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"linear","status":"failed","failureReason":"reauthenticationRequired","error":"connect ECONNREFUSED 127.0.0.1:3845"}}"#,
    );
    emit(
        r#"{"method":"mcpServer/oauthLogin/completed","params":{"name":"linear","success":true}}"#,
    );
    // Rolling rate-limit updates: only the FIRST 80% and FIRST 95% crossings
    // may produce notices.
    emit(
        r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":50}}}}"#,
    );
    emit(
        r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":85}}}}"#,
    );
    emit(
        r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":90}}}}"#,
    );
    emit(
        r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":97},"secondary":{"usedPercent":12}}}}"#,
    );
    emit(
        r#"{"method":"thread/environment/disconnected","params":{"environmentId":"env-1","threadId":"th-1"}}"#,
    );
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"done"}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}

fn diagnostics(stdin: &mut StdinLock<'_>, tid: &str) {
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    // Sink 5: a non-JSON line, then a JSON frame that is not JSON-RPC.
    emit("codex: not json at all");
    emit(r#"{"jsonrpc":"2.0","weird":true}"#);
    // Ignored tier — all capture-confirmed on a healthy session; the first
    // two were misclassified as Unknown before the capture caught it.
    emit(r#"{"method":"thread/settings/updated","params":{"settings":{"model":"gpt-5.6-sol"}}}"#);
    emit(
        r#"{"method":"remoteControl/status/changed","params":{"hostname":"box","installationId":"i-1"}}"#,
    );
    emit(r#"{"method":"thread/status/changed","params":{"status":"active"}}"#);
    emit(r#"{"method":"item/reasoning/summaryPartAdded","params":{"itemId":"r1"}}"#);
    // Sink 2: an unknown notification method.
    emit(r#"{"method":"thread/checkpoint/created","params":{"secret":"do-not-carry"}}"#);
    // Sink 4: an unknown item type inside the claimed item lifecycle.
    emit(r#"{"method":"item/started","params":{"item":{"id":"cc1","type":"contextCompaction"}}}"#);
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"cc1","type":"contextCompaction","status":"completed"}}}"#,
    );
    // An unknown server→client REQUEST: the harness must answer -32601 before
    // counting it — verified here the same way approve()/decline() verify an
    // approval reply, by reading it back off stdin.
    emit(r#"{"id":99,"method":"some/unknownRequest","params":{}}"#);
    let reply = read_line(stdin);
    if !(reply.contains(r#""id":99"#) && reply.contains(r#""code":-32601"#)) {
        fail_turn(tid, "expected -32601 reply to unknown request");
        return;
    }
    emit(r#"{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"ok"}}"#);
    emit(r#"{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}"#);
}
