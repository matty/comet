//! Fake Codex app-server for comet-harness tests.
//!
//! Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, thread
//! start/resume, then a scenario picked from the turn/start prompt text. Driven
//! by crates/harness/tests/codex.rs.
//!
//! Rust rather than `#!/bin/sh` because Windows cannot spawn a shell script:
//! the harness hands the path straight to `CreateProcess`, which rejects a
//! non-PE image with "%1 is not a valid Win32 application" (os error 193).

use std::io::{BufRead, StdinLock, Write};
use std::process::exit;
use std::time::Duration;

/// One JSON-RPC line. Rust's stdout is line-buffered even on a pipe, so each
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

fn main() {
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

    // ---- thread start / resume ---------------------------------------------
    let thread_line = read_line(&mut stdin);
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
        r#""approvalPolicy":"never""#,
        r#""summary":"auto""#,
        r#""serviceTier":"fast""#,
    ] {
        if !turn_line.contains(want) {
            fail_turn(tid, &format!("turn param missing: {want}"));
            return;
        }
    }
    for want in [
        r#""approvalPolicy":"never""#,
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
    // provider; every other scenario pins "user" via `happy`'s assertions.
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

fn approve(stdin: &mut StdinLock<'_>, turn_line: &str, thread_line: &str, tid: &str) {
    // Wire policy is always "never" (unattended parity with the Claude
    // adapter); the requests below are the STRAY-approval path, which must
    // still round-trip as input questions.
    if !thread_line.contains(r#""approvalPolicy":"never""#) {
        fail_turn(tid, "thread approvalPolicy should be never");
        return;
    }
    if !turn_line.contains(r#""approvalPolicy":"never""#) {
        fail_turn(tid, "turn approvalPolicy should be never");
        return;
    }
    emit(&format!(
        r#"{{"id":{tid},"result":{{"turn":{{"id":"t-1"}}}}}}"#
    ));
    emit(r#"{"method":"turn/started","params":{"turn":{"id":"t-1"}}}"#);
    emit(
        r#"{"id":101,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"rm -rf /tmp/x"}}"#,
    );
    let a1 = read_line(stdin);
    if !(a1.contains(r#""id":101"#) && a1.contains(r#""decision":"accept""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"command approval not accepted"}}}}"#,
        );
        return;
    }
    emit(
        r#"{"id":102,"method":"item/fileChange/requestApproval","params":{"itemId":"f1","changes":[{"path":"/tmp/a.rs","kind":"update"}]}}"#,
    );
    let a2 = read_line(stdin);
    if !(a2.contains(r#""id":102"#) && a2.contains(r#""decision":"accept""#)) {
        emit(
            r#"{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"file approval not accepted"}}}}"#,
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
