//! Contract tests for the `fake-acp` fixture.
//!
//! The fixture is the spine of every later ACP test: it is the only thing that
//! can pose a dropped reply or a starved turn deterministically, and a fixture
//! nobody has verified is worse than no fixture — a hardening test written
//! against a broken one passes for the wrong reason.
//!
//! These speak raw newline-framed JSON-RPC at it, so they check the fixture
//! itself rather than any decode layered over it.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin};

/// Long enough to be nothing to do with speed, short enough that a genuinely
/// silent fixture fails the suite instead of hanging it.
const QUIET: Duration = Duration::from_millis(750);

struct Fixture {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: i64,
}

impl Fixture {
    fn spawn(no_steering: bool) -> Self {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-acp"));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if no_steering {
            command.env("FAKE_ACP_NO_STEERING", "1");
        }
        let mut child = command.spawn().expect("spawn fake-acp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write");
        self.stdin.flush().await.expect("flush");
        id
    }

    /// The next frame, or `None` if the fixture stays quiet past `QUIET`.
    async fn next_frame(&mut self) -> Option<Value> {
        match tokio::time::timeout(QUIET, self.lines.next_line()).await {
            Ok(Ok(Some(line))) => Some(serde_json::from_str(&line).expect("fixture emits JSON")),
            _ => None,
        }
    }

    /// Frames until the one answering `id`, or `None` if it never comes.
    async fn reply_to(&mut self, id: i64) -> Option<Value> {
        for _ in 0..16 {
            let frame = self.next_frame().await?;
            if frame["id"].as_i64() == Some(id) {
                return Some(frame);
            }
        }
        None
    }

    async fn handshake(&mut self) -> Value {
        let id = self
            .request("initialize", json!({"protocolVersion": 1}))
            .await;
        self.reply_to(id).await.expect("initialize is answered")
    }

    async fn open_session(&mut self) -> String {
        let id = self.request("session/new", json!({"cwd": "."})).await;
        let reply = self.reply_to(id).await.expect("session/new is answered");
        reply["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_owned()
    }

    async fn prompt(&mut self, session: &str, text: &str) -> i64 {
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test]
async fn initialize_answers_protocol_1_and_no_initialized_is_required() {
    let mut fx = Fixture::spawn(false);
    let reply = fx.handshake().await;
    assert_eq!(reply["result"]["protocolVersion"], 1);
    assert_eq!(reply["result"]["_meta"]["steering"]["supported"], true);

    // The point: `session/new` works with no `initialized` notification sent.
    // That notification is Codex's app-server, not ACP, and a client that sends
    // one is speaking a protocol the agent never advertised.
    let session = fx.open_session().await;
    assert!(session.starts_with("fake-session-"), "got {session}");
}

/// The capability-degradation case Hermes represents: no steering extension
/// advertised at all.
#[tokio::test]
async fn the_fixture_can_withhold_the_steering_extension() {
    let mut fx = Fixture::spawn(true);
    let reply = fx.handshake().await;
    assert_eq!(reply["result"]["_meta"]["steering"]["supported"], false);
}

/// `authMethods: []` is what claude-agent-acp really answers, and it is the
/// case a fixture would otherwise never produce — so the fake defaults to it.
#[tokio::test]
async fn the_fixture_answers_an_empty_auth_methods_list() {
    let mut fx = Fixture::spawn(false);
    let reply = fx.handshake().await;
    let methods = reply["result"]["authMethods"]
        .as_array()
        .expect("authMethods is an array");
    assert!(methods.is_empty(), "expected [], got {methods:?}");
}

#[tokio::test]
async fn an_ordinary_turn_streams_then_settles_with_end_turn() {
    let mut fx = Fixture::spawn(false);
    fx.handshake().await;
    let session = fx.open_session().await;
    let id = fx.prompt(&session, "hello").await;

    let first = fx.next_frame().await.expect("an update arrives");
    assert_eq!(first["method"], "session/update");

    let reply = fx.reply_to(id).await.expect("the prompt is answered");
    assert_eq!(reply["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_refusal_settles_the_turn_with_its_own_stop_reason() {
    let mut fx = Fixture::spawn(false);
    fx.handshake().await;
    let session = fx.open_session().await;
    let id = fx.prompt(&session, "please refusal").await;
    let reply = fx.reply_to(id).await.expect("the prompt is answered");
    assert_eq!(reply["result"]["stopReason"], "refusal");
}

/// The dropped-reply shape upstream's settle commits exist for: the agent
/// streams, then never answers the request. Verified as *silence*, not as an
/// error frame — an error would settle the turn and there would be nothing to
/// recover from.
#[tokio::test]
async fn drop_reply_streams_then_never_answers() {
    let mut fx = Fixture::spawn(false);
    fx.handshake().await;
    let session = fx.open_session().await;
    let id = fx.prompt(&session, "please drop-reply now").await;

    let update = fx.next_frame().await.expect("it streams first");
    assert_eq!(update["method"], "session/update");
    assert!(
        fx.reply_to(id).await.is_none(),
        "drop-reply must leave the prompt unanswered"
    );
}

/// The starved turn: nothing at all after `session/new`. Distinct from
/// drop-reply, which streams first — the recovery paths differ, so the fixture
/// has to be able to pose both.
#[tokio::test]
async fn starve_answers_nothing_at_all() {
    let mut fx = Fixture::spawn(false);
    fx.handshake().await;
    let session = fx.open_session().await;
    let id = fx.prompt(&session, "please starve").await;
    assert!(
        fx.reply_to(id).await.is_none(),
        "starve must emit no frame whatsoever"
    );
}

/// An unknown method gets a JSON-RPC error rather than silence. A fixture that
/// ignored them would make a client bug look like a hung agent.
#[tokio::test]
async fn an_unknown_method_is_answered_with_an_error() {
    let mut fx = Fixture::spawn(false);
    fx.handshake().await;
    let id = fx.request("session/invented", json!({})).await;
    let reply = fx.reply_to(id).await.expect("unknown methods are answered");
    assert_eq!(reply["error"]["code"], -32601);
}
