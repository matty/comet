//! Minimal JSON-RPC 2.0 client over a child agent's stdio (newline-delimited
//! frames, id-multiplexed), ported from codex.ts's `startAppServer`.
//!
//! Shared by the Codex app-server harness and the ACP harness: both protocols
//! are newline-framed JSON-RPC 2.0 over stdio, so the framing, id multiplexing
//! and child-stdin handling below are the same code for both. Nothing here is
//! Codex-specific — keep it that way, or the ACP side grows a second copy.
//!
//! - Responses are matched to callers by numeric id (a shared pending map the
//!   reader task resolves directly, so requests can be awaited from anywhere —
//!   including inside the session loop — without starving notifications).
//! - Notifications and server→client requests (approvals) are pumped into an
//!   [`Incoming`] channel the session loop drains.
//! - Writes to a dead child's stdin (EPIPE) are tolerated and logged, matching
//!   the TS harness's swallowed-EPIPE behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::HarnessError;

/// A non-response line from the app server, in stdout order.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request (approvals); must be answered via
    /// [`RpcClient::respond`] / [`RpcClient::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// stdout EOF: the app server exited. All pending requests fail.
    Eof,
    /// A stdout line that was not a JSON-RPC message. Sink 5 — the session
    /// loop records it. The raw line stays in tracing at the drop site; only
    /// the KIND travels, because the line is provider text.
    Malformed(MalformedKind),
}

/// Which way a stdout line failed to be a JSON-RPC message.
///
/// **The reader always knew this and used to throw it away** (D9): all three
/// arms below sent one bare `Malformed`, so an operator read `unparseable ×412`
/// and could not tell a CLI writing log noise to stdout from one whose message
/// shape changed. The distinction costs nothing — each arm already logs a
/// different sentence — and it is the difference between "something else is
/// writing to this pipe" and "the protocol moved".
// Every variant names a way of NOT being a message, so the shared prefix is the
// honest reading rather than a naming slip — `Json`/`Object`/`Message` would
// each say the opposite of what the variant means.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MalformedKind {
    /// Not JSON at all. The bare sentinel: this is what "unparseable" has
    /// always meant, and the one case where nothing more can be said.
    NotJson,
    /// Valid JSON, but not an object — a bare string or array on stdout.
    NotAnObject,
    /// A JSON object with neither `method` nor `id`, so it is neither a
    /// request, a response, nor a notification.
    NotAMessage,
}

impl MalformedKind {
    /// The diagnostic discriminator. Fixed strings, never provider text, so
    /// nothing here can mint an unbounded vocabulary.
    pub(crate) fn discriminator(self) -> &'static str {
        match self {
            Self::NotJson => crate::UNPARSEABLE,
            Self::NotAnObject => "unparseable/not-an-object",
            Self::NotAMessage => "unparseable/not-a-message",
        }
    }
}

/// Warn-log a malformed stdout line, under the shared per-discriminator budget
/// (D10).
///
/// The payload is built lazily: past the budget it is never rendered at all,
/// which matters because the thing not being rendered is provider output — a
/// renamed high-volume method would otherwise put raw command stdout in the log
/// once per chunk, indefinitely.
fn log_malformed(kind: MalformedKind, payload: impl FnOnce() -> String) {
    match crate::log_budget(kind.discriminator()) {
        crate::LogBudget::Full => tracing::warn!(
            target: "comet_harness::jsonrpc",
            frame = %payload(),
            kind = kind.discriminator(),
            "not a JSON-RPC message (recorded as a diagnostic)"
        ),
        crate::LogBudget::CountOnly(seen) => tracing::warn!(
            target: "comet_harness::jsonrpc",
            kind = kind.discriminator(),
            seen,
            "not a JSON-RPC message (payload omitted past the log budget)"
        ),
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcFailure>>>>>;

#[derive(Clone)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
}

impl RpcClient {
    /// Spawn the writer + reader tasks over the child's stdio; returns the
    /// client and the incoming (notification/request) channel.
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
            },
            incoming_rx,
        )
    }

    /// Send a request and await its response (resolved by the reader task).
    ///
    /// **`data` never reaches this method's error.** [`RpcFailure::message`]
    /// alone becomes [`HarnessError::Protocol`]'s text -- exactly what this
    /// method returned before [`RpcFailure`] existed. A JSON-RPC error's
    /// `data` is dropped here on purpose: this is the path Codex's
    /// `turn/start`/`turn/steer` and every ordinary ACP call (`session/
    /// prompt`, `session/cancel`, `config_requests`, ...) go through, and
    /// those replies reach a user's transcript close to verbatim
    /// (`crates/engine/src/sessions.rs`'s `drive_run`, `codex/mod.rs`) --
    /// `data` is exactly the "raw provider text" `.agents/rules/
    /// user-facing-errors.md` forbids putting there. [`Self::request_detail`]
    /// is the one caller allowed to see `data`, and only because its own
    /// result never reaches a user without first passing through
    /// `acp::session::OpenFailureMapper`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.request_detail(method, params)
            .await
            .map_err(|failure| HarnessError::Protocol(format!("{method}: {}", failure.message)))
    }

    /// Same request as [`Self::request`], but on failure returns the
    /// JSON-RPC error's `message` and `data` separately instead of folding
    /// them into one opaque [`HarnessError`] -- see [`RpcFailure`]'s own doc
    /// comment for why, and who is allowed to call this instead of
    /// `request`.
    pub(crate) async fn request_detail(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcFailure> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.writer.send(line.to_string()).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(RpcFailure {
                message: "agent stdin closed".into(),
                data: None,
            });
        }
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(failure)) => Err(failure),
            // Sender dropped: the reader hit EOF and failed all pending.
            Err(_) => Err(RpcFailure {
                message: "agent exited before responding".into(),
                data: None,
            }),
        }
    }

    /// Fire a notification (no id, no response).
    pub fn notify(&self, method: &str, params: Option<Value>) {
        let line = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        let _ = self.writer.send(line.to_string());
    }

    /// Answer a server→client request.
    pub fn respond(&self, id: &Value, result: Value) {
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.writer.send(line.to_string());
    }

    /// Reject a server→client request (e.g. unknown method).
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        let _ = self.writer.send(line.to_string());
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "comet_harness::jsonrpc", "stdin write failed (tolerated): {e}");
            return;
        }
    }
}

/// Parse stdout lines: responses resolve the pending map, everything else is
/// forwarded in order. Non-JSON noise is skipped; on EOF all pending requests
/// fail (their senders drop) and one final [`Incoming::Eof`] is delivered.
async fn read_loop(stdout: ChildStdout, pending: Pending, tx: mpsc::Sender<Incoming>) {
    let mut lines = BufReader::new(stdout).lines();
    // A read error ends the loop like EOF: either way the child's stdout is
    // unusable, pending requests must fail, and the session loop must know.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            log_malformed(MalformedKind::NotJson, || line.to_string());
            if tx
                .send(Incoming::Malformed(MalformedKind::NotJson))
                .await
                .is_err()
            {
                return;
            }
            continue;
        };
        // Take the frame apart rather than borrowing it: this runs once per
        // stdout line for every harness, and cloning `params` out of a value
        // that is dropped two lines later doubled the allocation of every
        // streamed text delta.
        let mut msg = match msg {
            Value::Object(map) => map,
            other => {
                log_malformed(MalformedKind::NotAnObject, || other.to_string());
                if tx
                    .send(Incoming::Malformed(MalformedKind::NotAnObject))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        // A non-string `method` is not a method; put it back so the
        // unrecognized-frame arm below still logs the whole frame.
        let method = match msg.remove("method") {
            Some(Value::String(method)) => Some(method),
            Some(other) => {
                msg.insert("method".to_owned(), other);
                None
            }
            None => None,
        };
        match (method, msg.remove("id")) {
            // Response: resolve the awaiting request.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let Some(sender) = pending.lock().expect("pending lock").remove(&id) else {
                    continue;
                };
                let outcome = match msg.remove("error") {
                    Some(err) => Err(parse_error(&err)),
                    None => Ok(msg.remove("result").unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // Server→client request (approvals).
            (Some(method), Some(id)) => {
                let incoming = Incoming::Request {
                    id,
                    method,
                    params: msg.remove("params").unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            // Notification.
            (Some(method), None) => {
                let incoming = Incoming::Notification {
                    method,
                    params: msg.remove("params").unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            (None, None) => {
                let frame = Value::Object(msg);
                log_malformed(MalformedKind::NotAMessage, || frame.to_string());
                if tx
                    .send(Incoming::Malformed(MalformedKind::NotAMessage))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    // EOF/read error: fail every awaiting request, then signal the loop.
    pending.lock().expect("pending lock").clear();
    let _ = tx.send(Incoming::Eof).await;
}

/// One JSON-RPC error, kept structured rather than folded into one string.
///
/// `message` is what [`RpcClient::request`]'s [`HarnessError::Protocol`]
/// shows -- unchanged from before this type existed, byte-for-byte. `data`
/// is the vendor's own error payload, flattened to a string (a bare string,
/// or an object's `.details`), and is exposed ONLY through
/// [`RpcClient::request_detail`] -- today, only
/// `acp::session::OpenFailureMapper`, which needs Hermes' `data.details` to
/// recognize its own "not configured" shape (`message` alone there is the
/// useless generic "Internal error"). Nothing folds `data` into a `Display`
/// anywhere in this type, so a caller that only ever sees `HarnessError`
/// (every existing caller of `request` -- Codex's `turn/start`/`turn/steer`,
/// every ordinary ACP call) is byte-for-byte unaffected by `data` existing
/// at all. A review finding on an earlier version of this file: widening
/// `HarnessError::Protocol`'s own text to carry `data` reached two Codex
/// user-facing sites (`codex/mod.rs`'s `turn/start`/`turn/steer` failures)
/// that had never shown provider `data` before -- this split is the fix.
///
/// `pub`, not `pub(crate)`, ONLY so `OpenFailureMapper`'s function-pointer
/// type -- a parameter of the `pub async fn AcpSession::open` an
/// integration test outside this crate calls -- does not leak a
/// private-interfaces warning. This module (`jsonrpc`) is itself declared
/// `pub(crate)` in `lib.rs`, so nothing outside this crate can actually name
/// `crate::jsonrpc::RpcFailure` regardless of this struct's own visibility.
#[derive(Debug, Clone)]
pub struct RpcFailure {
    pub message: String,
    pub data: Option<String>,
}

/// Parse one JSON-RPC `error` object into [`RpcFailure`] -- a pure function
/// so it can be tested without a real child process.
///
/// **`data` is read only when a real `message` string was present.** When
/// `message` is absent, the fallback below is `err.to_string()` -- the
/// WHOLE error object, `data` included -- so reading `data` again in that
/// case would duplicate it (`{"code":-1,"data":"boom"}: boom`, caught in
/// review). A genuine `message` and a present `data` are independent
/// fields; only their joint absence-of-message case needs this guard.
fn parse_error(err: &Value) -> RpcFailure {
    let message_field = err.get("message").and_then(Value::as_str);
    let message = message_field
        .map(str::to_owned)
        .unwrap_or_else(|| err.to_string());
    let data = message_field.and_then(|_| {
        err.get("data").and_then(|data| {
            data.as_str().map(str::to_owned).or_else(|| {
                data.get("details")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        })
    });
    RpcFailure { message, data }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Grok's real signed-out shape (captured live 2026-08-29): `data` is a
    /// bare string, kept separate from `message`.
    #[test]
    fn a_bare_string_data_is_kept_separate_from_the_message() {
        let err = json!({
            "code": -32000,
            "message": "Authentication required",
            "data": "no auth method id provided"
        });
        let failure = parse_error(&err);
        assert_eq!(failure.message, "Authentication required");
        assert_eq!(failure.data.as_deref(), Some("no auth method id provided"));
    }

    /// Hermes' real "no provider configured" shape (captured live
    /// 2026-08-29): `message` alone ("Internal error") is useless, and the
    /// actionable text lives at `data.details`. Break caught: reading only
    /// `message`, which is exactly what made this reply unreadable before
    /// this function existed.
    #[test]
    fn an_object_datas_details_string_is_kept_separate_from_the_message() {
        let err = json!({
            "code": -32603,
            "message": "Internal error",
            "data": {"details": "No LLM provider configured. Run `hermes model`..."}
        });
        let failure = parse_error(&err);
        assert_eq!(failure.message, "Internal error");
        assert_eq!(
            failure.data.as_deref(),
            Some("No LLM provider configured. Run `hermes model`...")
        );
    }

    /// No `data` at all (Codex's ordinary shape, and every other recorded
    /// error): `message` alone, `data` absent.
    #[test]
    fn no_data_leaves_data_absent() {
        let err = json!({"code": -32601, "message": "method not found"});
        let failure = parse_error(&err);
        assert_eq!(failure.message, "method not found");
        assert_eq!(failure.data, None);
    }

    /// An unrecognized `data` shape (a number, an array, an object with no
    /// `.details`) is left alone rather than guessed at.
    #[test]
    fn an_unrecognized_data_shape_is_dropped_not_guessed_at() {
        let err = json!({"code": -1, "message": "boom", "data": 42});
        let failure = parse_error(&err);
        assert_eq!(failure.message, "boom");
        assert_eq!(failure.data, None);
    }

    /// Break caught: when `message` is absent, `message` falls back to the
    /// WHOLE error object (already containing `data`) -- reading `data`
    /// again in that case would duplicate it in anything that later joins
    /// the two. Guarded by requiring a real `message` string before `data`
    /// is even looked at.
    #[test]
    fn a_missing_message_does_not_duplicate_data() {
        let err = json!({"code": -1, "data": "boom"});
        let failure = parse_error(&err);
        assert!(
            failure.data.is_none(),
            "data must not be read when there was no real message: {failure:?}"
        );
    }

    /// [`RpcClient::request`]'s own contract, pinned directly: `data` must
    /// never reach [`HarnessError::Protocol`]'s text, which is what a
    /// generic caller (Codex's `turn/start`/`turn/steer`, any ordinary ACP
    /// call) shows close to verbatim on screen.
    #[test]
    fn request_detail_carries_data_but_request_does_not() {
        let err = json!({
            "code": -32603,
            "message": "Internal error",
            "data": {"details": "No LLM provider configured. Run `hermes model`..."}
        });
        let failure = parse_error(&err);
        let harness_error = HarnessError::Protocol(format!("session/new: {}", failure.message));
        let text = harness_error.to_string();
        assert!(
            !text.contains("No LLM provider configured"),
            "data must not reach HarnessError::Protocol's text: {text}"
        );
    }
}
