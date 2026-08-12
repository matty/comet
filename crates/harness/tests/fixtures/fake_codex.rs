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
    // Objects, not strings — the shape the real server answers with (capture
    // `2026-08-11-codex-model-list.md`), and the one the phase spec's field
    // summary gets wrong.
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
fn discovery_models() -> Vec<Value> {
    const ULTRA: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
    const XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
    const IMAGE: Option<&[&str]> = Some(&["text", "image"]);
    const TEXT: Option<&[&str]> = Some(&["text"]);
    let home = std::env::var("CODEX_HOME").unwrap_or_else(|_| "unset".into());
    vec![
        model_entry("gpt-5.6-sol", "gpt-5.6-sol label", false, ULTRA, IMAGE),
        model_entry("gpt-5.5", "gpt-5.5 label", false, XHIGH, IMAGE),
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

    // NOTE: steer-race before steer — the first match wins, and "scenario:steer"
    // is a prefix of "scenario:steer-race".
    if turn_line.contains("scenario:happy") {
        happy(&turn_line, &thread_line, &tid);
    } else if turn_line.contains("scenario:steer-race") {
        steer_race(&mut stdin, &tid);
    } else if turn_line.contains("scenario:steer") {
        steer(&mut stdin, &tid);
    } else if turn_line.contains("scenario:auto-reviewer") {
        auto_reviewer(&thread_line, &tid);
    } else if turn_line.contains("scenario:echo-policy") {
        echo_policy(&turn_line, &thread_line, &tid);
    } else if turn_line.contains("scenario:approve") {
        approve(&mut stdin, &turn_line, &thread_line, &tid);
    } else if turn_line.contains("scenario:decline") {
        decline(&mut stdin, &tid);
    } else if turn_line.contains("scenario:interrupt") {
        interrupt(&mut stdin, &tid);
    } else if turn_line.contains("scenario:wedge") {
        wedge(&tid);
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
    // Completion-only lifecycle: must still open AND close the tool call.
    emit(
        r#"{"method":"item/completed","params":{"item":{"id":"td1","type":"todoList","items":[{"text":"a","completed":true},{"text":"b","completed":false}]}}}"#,
    );
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
        r#"{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"inputTokens":42,"outputTokens":7}}}}"#,
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
        && steer_line.contains("redirect please")
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
