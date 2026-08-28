//! Spawn, handshake, one session, and the turn loop.
//!
//! The shape of an ACP turn, and the two ways it differs from Codex's:
//!
//! 1. **There is no `initialized` notification.** ACP is `initialize` →
//!    `session/new` and the agent is ready. The `initialized` follow-up is
//!    Codex's app-server; sending one here would put a frame on the wire no
//!    agent asked for.
//! 2. **A turn ends with the RESPONSE to `session/prompt`**, which carries a
//!    `stopReason` — not with a notification. Reading turn-end off `.method`,
//!    the way the Codex loop legitimately does, hangs here forever.
//!
//! Everything else is a notification stream the loop drains while it waits.
//! **Most of that stream is not addressed to Comet at all**: grok 1.0.5 emits
//! `_x.ai/models/update`, `_x.ai/settings/update`, `_x.ai/announcements/update`
//! and `_x.ai/mcp_initialized` around every session, and two `session/update`
//! frames arrive *before* the `session/new` reply that names the session they
//! belong to. Anything unrecognized is dropped quietly — that is the honest
//! answer for a protocol whose vendors extend it, not a gap.
//!
//! **Steering rides the turn boundary, always, in this module.** No recorded
//! agent advertises the steering extension: grok sends no `_meta.steering` at
//! all. A queued steer is therefore delivered as the next `session/prompt` on
//! the same session, which is slower than an in-turn steer and correct. When an
//! agent that does advertise the extension appears, `AgentDescription::
//! supports_steering` is the gate that would pick the faster path.

use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{AgentEvent, DiagnosticSeverity, DoneStatus, HarnessId, RunRequest};

use super::{AgentDescription, initialize_params, new_session_params, normalize, prompt_params};
use crate::jsonrpc::{Incoming, RpcClient};
use crate::{HarnessError, RunControls, StderrTail, shutdown_child};

/// The timeouts the loop is built from. A struct rather than four arguments so
/// a test can shrink them without every call site naming all four.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// How long `initialize` and `session/new` may take before the open fails
    /// with something a user can act on. Bounded because a child that spawns
    /// and then says nothing is indistinguishable from a hang, and the rule in
    /// `.agents/rules/user-facing-errors.md` is that no waiting state lasts
    /// forever.
    pub handshake: Duration,
    /// After `session/cancel`, how long the agent gets to settle the in-flight
    /// prompt with `stopReason: "cancelled"` before the loop stops waiting and
    /// reports the interrupt itself.
    pub cancel_grace: Duration,
    /// SIGTERM → SIGKILL gap when reaping the child.
    pub kill_grace: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
        }
    }
}

/// A live ACP conversation: a spawned agent that has completed `initialize` and
/// holds one open session.
pub struct AcpSession {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    stderr_tail: StderrTail,
    agent: AgentDescription,
    session_id: String,
    timeouts: Timeouts,
}

impl AcpSession {
    /// Spawn `command`, handshake, and open one session rooted at `cwd`.
    ///
    /// `command` is built by the caller because the launch line is per-agent
    /// (grok's four tokens are not codex-acp's `node <entry>`), and because
    /// production and the capture recorder must derive it from the same place.
    pub async fn open(
        command: Command,
        cwd: &str,
        timeouts: Timeouts,
    ) -> Result<Self, HarnessError> {
        let Connected {
            mut child,
            client,
            incoming,
            stderr_tail,
            initialized,
        } = connect(command, timeouts).await?;
        let agent = AgentDescription::from_initialize(&initialized);

        let opened = match with_timeout(
            timeouts.handshake,
            "session/new",
            client.request("session/new", new_session_params(cwd)),
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => {
                // The handshake succeeded and this did not, so a live child is
                // holding stdio nobody will read again. `kill_on_drop` would
                // reach it eventually, but only once every clone of the client
                // is dropped too — reap it here instead of leaving the timing
                // to drop order.
                shutdown_child(&mut child, timeouts.kill_grace).await;
                return Err(error);
            }
        };
        let session_id = match opened["sessionId"].as_str() {
            Some(id) => id.to_owned(),
            None => {
                shutdown_child(&mut child, timeouts.kill_grace).await;
                return Err(HarnessError::Protocol(
                    "session/new answered without a sessionId".into(),
                ));
            }
        };

        Ok(Self {
            child,
            client,
            incoming,
            stderr_tail,
            agent,
            session_id,
            timeouts,
        })
    }

    /// Spawn and handshake ONLY, answering with the raw `initialize` result.
    ///
    /// For discovery, which needs the handshake and nothing else. The child is
    /// reaped before this returns: an agent's capability block costs one spawn
    /// and no `session/new`, and holding an idle session open on the picker's
    /// render path would be a process per boot for a reply already in hand.
    ///
    /// The raw `Value` rather than [`AgentDescription`] because what a given
    /// agent publishes here is agent-specific — Grok's model list lives at
    /// `_meta.modelState`, a path no other recorded speaker uses, and pushing
    /// every such field onto the shared description would make it a union of
    /// vendors.
    pub async fn open_for_discovery(
        command: Command,
        cwd: &str,
        timeouts: Timeouts,
    ) -> Result<Discovered, HarnessError> {
        let Connected {
            mut child,
            client,
            initialized,
            // **`incoming` MUST stay alive across `session/new`.** Dropping the
            // receiver makes the reader task's `tx.send` fail on the very next
            // notification, and `read_loop` RETURNS on that error — so nothing
            // parses stdout any more and the reply this function is waiting for
            // never resolves. Discovery then burns the full handshake timeout
            // and falls back, on an agent that answered in 550ms.
            //
            // That is not hypothetical: it shipped. Destructuring this field
            // away with `..` made the Grok model picker sit on "loading models"
            // for 30s, while `open()` — which keeps the receiver in its struct —
            // was fine, and while `fake-acp` (silent until prompted) could not
            // reproduce it.
            incoming,
            stderr_tail: _,
        } = connect(command, timeouts).await?;

        // **`session/new` is worth the extra round trip.** It is token-free like
        // the handshake, and it is the only reply that carries the session
        // config — which model rows the agent really offers and which effort is
        // selected. `initialize` carries a model block too, but that surface is
        // the deprecated one: agents that have both enumerate one entry per
        // model x effort there, so reading it as a model list multiplies a
        // 5-model agent into 20 picker rows.
        let session = with_timeout(
            timeouts.handshake,
            "session/new",
            client.request("session/new", new_session_params(cwd)),
        )
        .await;

        // Only now: the reply is in hand, so the reader has nothing left to do.
        drop(incoming);
        shutdown_child(&mut child, timeouts.kill_grace).await;
        Ok(Discovered {
            initialized,
            // A failed `session/new` is NOT a failed discovery. The handshake
            // answered, so the agent is reachable and its `initialize` block is
            // real; losing the richer surface degrades what we can read, not
            // whether we read anything. `Null` reads as absent at every path
            // below it, which is exactly the fallback the caller already writes
            // for an agent that has no config surface at all.
            session: session.unwrap_or(Value::Null),
        })
    }

    pub fn agent(&self) -> &AgentDescription {
        &self.agent
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// What a handshake-only probe learned, both replies kept whole.
///
/// Raw `Value`s rather than a parsed shape because what an agent publishes here
/// is vendor-specific: Grok's config lives at `_meta["x.ai/sessionConfig"]`,
/// which no other recorded speaker sends. Only the agent's own harness knows
/// how to read it, so the seam hands over the bytes.
pub struct Discovered {
    /// The `initialize` result.
    pub initialized: Value,
    /// The `session/new` result, or `Value::Null` if that call did not answer.
    /// Null rather than an error: see [`AcpSession::open_for_discovery`].
    pub session: Value,
}

/// A spawned agent that has completed `initialize`, before anything has been
/// decided about what to do with it. Both [`AcpSession::open`] and
/// [`AcpSession::open_for_discovery`] are built on it, so the two cannot
/// disagree about how a child is started or how the handshake is checked.
struct Connected {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    stderr_tail: StderrTail,
    /// The raw `initialize` result, kept whole: an agent's own `_meta` block is
    /// vendor-shaped and only its harness knows how to read it.
    initialized: Value,
}

async fn connect(mut command: Command, timeouts: Timeouts) -> Result<Connected, HarnessError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotInstalled(program.clone())
        } else {
            HarnessError::Io(e)
        }
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("ACP agent has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("ACP agent has no stdout".into()))?;
    let stderr_tail = StderrTail::default();
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "comet_harness::acp", "stderr: {line}");
                tail.push(&line);
            }
        });
    }

    let (client, incoming) = RpcClient::new(stdin, stdout);

    let initialized = match with_timeout(
        timeouts.handshake,
        "initialize",
        client.request("initialize", initialize_params()),
    )
    .await
    .and_then(|result| check_protocol_version(&result).map(|()| result))
    {
        Ok(initialized) => initialized,
        Err(error) => {
            // A child that spawned but could not handshake is still running.
            // Reap it here rather than leaving it to drop order.
            shutdown_child(&mut child, timeouts.kill_grace).await;
            return Err(error);
        }
    };

    Ok(Connected {
        child,
        client,
        incoming,
        stderr_tail,
        initialized,
    })
}

/// The agent's answer to our `protocolVersion`.
///
/// **A missing key is tolerated; a mismatched one is not.** ACP has the agent
/// reply with the version it will speak, which must be no higher than the one
/// asked for — so a number that is not ours means an agent talking a protocol
/// this build cannot read, and continuing would produce silent nonsense rather
/// than an error anybody could act on. Absence is different: it says the agent
/// did not answer the question, not that it disagreed, and every recorded agent
/// does answer.
fn check_protocol_version(result: &Value) -> Result<(), HarnessError> {
    match result["protocolVersion"].as_u64() {
        None => Ok(()),
        Some(super::PROTOCOL_VERSION) => Ok(()),
        Some(other) => Err(HarnessError::Protocol(format!(
            "this agent speaks ACP v{other}, and Comet speaks v{}",
            super::PROTOCOL_VERSION
        ))),
    }
}

/// Await one handshake request under a deadline, naming the method in both
/// failure directions so the message says which half of the handshake stalled.
async fn with_timeout(
    limit: Duration,
    method: &str,
    request: impl Future<Output = Result<Value, HarnessError>>,
) -> Result<Value, HarnessError> {
    match tokio::time::timeout(limit, request).await {
        Ok(result) => result,
        Err(_) => Err(HarnessError::Protocol(format!(
            "the agent did not answer {method} within {}s",
            limit.as_secs()
        ))),
    }
}

/// Drive `session`'s turns until the consumer hangs up or the agent exits.
///
/// The stream always ends with a `Done` unless the consumer dropped first —
/// a run that simply stopped producing events would leave the transcript
/// spinning with nothing to explain it.
pub fn run(
    session: AcpSession,
    harness: HarnessId,
    request: RunRequest,
    controls: RunControls,
) -> BoxStream<'static, Result<AgentEvent, HarnessError>> {
    let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
    tokio::spawn(run_session(session, harness, request, controls, event_tx));
    futures::stream::unfold(event_rx, |mut rx| async move {
        rx.recv().await.map(|ev| (ev, rx))
    })
    .boxed()
}

/// How a single turn stopped.
enum TurnEnd {
    /// The agent answered `session/prompt`.
    Settled(DoneStatus),
    /// Interrupted, and the agent did not settle within the grace period.
    CancelledUnsettled,
    /// stdout EOF: the agent is gone and no further turn is possible.
    AgentExited,
    /// The consumer dropped the stream; stop without ceremony.
    ConsumerGone,
}

async fn run_session(
    session: AcpSession,
    harness: HarnessId,
    request: RunRequest,
    controls: RunControls,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
) {
    let AcpSession {
        mut child,
        client,
        mut incoming,
        stderr_tail,
        agent: _agent,
        session_id,
        timeouts,
    } = session;
    let RunControls {
        // Unclaimed here. ACP's permission request is `session/request_permission`,
        // and this module answers every server->client request with -32601 rather
        // than half-wiring one: an approval that reached the user through a
        // synthesized question would put a card in the doc under an id no
        // resolver knows. Whichever slice adds approvals claims both fields.
        request_input: _request_input,
        request_approval: _request_approval,
        mut steering,
        interrupt,
    } = controls;

    let assistant_message_id = uuid::Uuid::new_v4().to_string();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id,
            runtime_mode: request.runtime_mode,
        },
    )
    .await
    {
        shutdown_child(&mut child, timeouts.kill_grace).await;
        return;
    }

    let mut prompt = request.prompt.clone();

    // **One `Done` per turn that started, not one per run.** The session is
    // persistent: a steer opens a second turn on the same session, and a UI
    // that saw no `Done` for the first would leave it spinning while the
    // second streamed over it. Codex's loop reports per turn for the same
    // reason. The corollary is that nothing is emitted after the loop — every
    // exit below has already sent its own.
    loop {
        let end = drive_turn(
            &client,
            &mut incoming,
            &event_tx,
            &session_id,
            &prompt,
            &interrupt,
            timeouts.cancel_grace,
        )
        .await;

        let (done, more_turns_possible) = match end {
            // Only a completed turn can be followed by another. A refusal or a
            // cancellation ends the conversation: delivering a queued steer
            // into either would read as Comet ignoring what just happened.
            TurnEnd::Settled(status) => (
                Some(done_event(status, None, &session_id)),
                status == DoneStatus::Completed,
            ),
            TurnEnd::CancelledUnsettled => (
                Some(done_event(DoneStatus::Interrupted, None, &session_id)),
                false,
            ),
            // The agent's stdout closed with a turn in flight. That is a crash,
            // not a quiet success, and the stderr tail is the only thing that
            // can explain it to whoever is looking at the transcript.
            TurnEnd::AgentExited => (
                Some(done_event(
                    DoneStatus::Errored,
                    Some(crate::crash_message(
                        "the ACP agent",
                        child.try_wait().ok().flatten(),
                        &stderr_tail,
                    )),
                    &session_id,
                )),
                false,
            ),
            // Nobody is reading. Sending a Done into a closed channel would be
            // ceremony, not information.
            TurnEnd::ConsumerGone => (None, false),
        };

        if let Some(done) = done
            && !send(&event_tx, done).await
        {
            break;
        }
        if !more_turns_possible || interrupt.is_cancelled() {
            break;
        }

        // Between turns: deliver a queued steer as the next prompt, or end.
        match steering.recv().await {
            Some(steer) => {
                if !send(
                    &event_tx,
                    AgentEvent::Steered {
                        assistant_message_id: None,
                        next_assistant_message_id: None,
                    },
                )
                .await
                {
                    break;
                }
                prompt = steer.prompt;
            }
            // The mailbox closed: the host is finished with this run.
            None => break,
        }
    }

    shutdown_child(&mut child, timeouts.kill_grace).await;
}

fn done_event(status: DoneStatus, error: Option<String>, session_id: &str) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error,
        session_id: Some(session_id.to_owned()),
    }
}

/// One `session/prompt`, from send to `stopReason`.
async fn drive_turn(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    session_id: &str,
    prompt: &str,
    interrupt: &crate::CancellationToken,
    cancel_grace: Duration,
) -> TurnEnd {
    let reply = client.request("session/prompt", prompt_params(session_id, prompt));
    tokio::pin!(reply);

    // Absolute, so re-creating the sleep on each loop pass does not extend it.
    let mut give_up_at: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            // Biased so a reply that arrives in the same pass as the cancel
            // deadline is read as the agent settling, not as it failing to.
            biased;

            answer = &mut reply => {
                return match answer {
                    Ok(result) => {
                        // **Drain first.** The reply and the last deltas of the
                        // message it settles arrive in one batch, and a biased
                        // select takes the reply — so returning here directly
                        // silently truncates the end of every assistant
                        // message. (`fake-acp` sends " done" immediately before
                        // its `end_turn`; that text vanished until this drain
                        // existed.) The reader task pushes notifications into
                        // the channel BEFORE resolving the response, so
                        // whatever `try_recv` finds now provably preceded the
                        // reply on the wire, and nothing later can appear.
                        while let Ok(message) = incoming.try_recv() {
                            match handle_incoming(message, client, event_tx).await {
                                Handled::Continue => {}
                                Handled::ConsumerGone => return TurnEnd::ConsumerGone,
                                // EOF after a settled turn is just the agent
                                // shutting down; the turn itself succeeded.
                                Handled::AgentExited => break,
                            }
                        }
                        let reason = result["stopReason"].as_str().unwrap_or_default();
                        TurnEnd::Settled(normalize::done_status(reason))
                    }
                    // The request failed rather than answering: the reader hit
                    // EOF, or the agent returned a JSON-RPC error. Either way
                    // this turn is over and the agent is not usable.
                    Err(error) => {
                        tracing::debug!(target: "comet_harness::acp", "session/prompt failed: {error}");
                        TurnEnd::AgentExited
                    }
                };
            }

            message = incoming.recv() => match message {
                Some(message) => match handle_incoming(message, client, event_tx).await {
                    Handled::Continue => {}
                    Handled::ConsumerGone => return TurnEnd::ConsumerGone,
                    Handled::AgentExited => return TurnEnd::AgentExited,
                },
                None => return TurnEnd::AgentExited,
            },

            _ = interrupt.cancelled(), if give_up_at.is_none() => {
                // A notification in ACP: the in-flight `session/prompt` is what
                // reports the outcome, so the loop keeps running and waits for
                // it to come back with `cancelled`.
                client.notify("session/cancel", Some(serde_json::json!({"sessionId": session_id})));
                give_up_at = Some(tokio::time::Instant::now() + cancel_grace);
            }

            _ = async {
                match give_up_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                // Cancelled, and the agent never settled the prompt. Reporting
                // the interrupt anyway is the bounded end the waiting rule
                // requires; the child is reaped by the caller.
                tracing::debug!(target: "comet_harness::acp", "agent did not settle a cancelled turn");
                return TurnEnd::CancelledUnsettled;
            }

            _ = event_tx.closed() => return TurnEnd::ConsumerGone,
        }
    }
}

/// What the turn loop should do after one incoming frame.
enum Handled {
    Continue,
    ConsumerGone,
    AgentExited,
}

/// Serve one frame from the agent. Shared by the loop's waiting arm and by the
/// post-settle drain, so a frame is treated identically whichever of the two
/// picks it up — the alternative is two copies that quietly disagree about, say,
/// whether an unsupported request still gets its `-32601`.
async fn handle_incoming(
    message: Incoming,
    client: &RpcClient,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Handled {
    match message {
        Incoming::Notification { method, params } => {
            if method == "session/update" {
                if let Some(event) = normalize::session_update(&params)
                    && !send(event_tx, event).await
                {
                    return Handled::ConsumerGone;
                }
            } else {
                // Vendor and lifecycle chatter -- `_x.ai/*` and friends.
                // Dropped on purpose; see this module's header.
                tracing::trace!(target: "comet_harness::acp", method, "unconsumed notification");
            }
            Handled::Continue
        }

        Incoming::Request { id, method, .. } => {
            // Always answer. An unanswered server->client request leaves the
            // agent waiting on a reply that never comes, which presents as a
            // hung turn with no error anywhere.
            tracing::debug!(target: "comet_harness::acp", method, "declining unsupported request");
            client.respond_error(&id, -32601, "method not supported by this client");
            Handled::Continue
        }

        Incoming::Malformed => {
            let ev = crate::diagnostic(crate::UNPARSEABLE, DiagnosticSeverity::Malformed);
            if send(event_tx, ev).await {
                Handled::Continue
            } else {
                Handled::ConsumerGone
            }
        }

        Incoming::Eof => Handled::AgentExited,
    }
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: treating a missing `protocolVersion` as a mismatch, which
    /// would refuse an agent that simply did not answer the question, or
    /// treating a mismatch as tolerable, which would decode a protocol this
    /// build cannot read and produce silent nonsense.
    #[test]
    fn only_a_stated_and_different_protocol_version_is_refused() {
        assert!(check_protocol_version(&json!({"protocolVersion": 1})).is_ok());
        assert!(check_protocol_version(&json!({})).is_ok());
        assert!(check_protocol_version(&json!({"protocolVersion": null})).is_ok());
        // Not a number at all: unreadable, not a disagreement.
        assert!(check_protocol_version(&json!({"protocolVersion": "1"})).is_ok());

        let refused =
            check_protocol_version(&json!({"protocolVersion": 2})).expect_err("v2 must be refused");
        let text = refused.to_string();
        assert!(text.contains("v2"), "must name the agent's version: {text}");
        assert!(text.contains("v1"), "must name Comet's version: {text}");
    }

    /// The handshake params are production's, not a copy — the capture
    /// recorder re-exports this exact function so the corpus records the
    /// handshake Comet performs. Asserts on the value itself for the reason
    /// its own doc comment gives.
    #[test]
    fn the_handshake_declines_fs_and_terminal() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], false);
    }

    /// ACP carries a prompt as content blocks. Break caught: sending the text
    /// as a bare string, which every recorded agent rejects.
    #[test]
    fn a_prompt_is_an_array_of_content_blocks() {
        let params = prompt_params("s-1", "hello");
        assert_eq!(params["sessionId"], "s-1");
        let blocks = params["prompt"].as_array().expect("prompt is an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
    }

    /// `mcpServers: []` is declared rather than omitted: "none" and "the key
    /// was not sent" are different statements to an agent that reads it.
    #[test]
    fn a_new_session_declares_no_mcp_servers_rather_than_omitting_them() {
        let params = new_session_params("/tmp/x");
        assert_eq!(params["cwd"], "/tmp/x");
        assert_eq!(
            params["mcpServers"].as_array().map(Vec::len),
            Some(0),
            "mcpServers must be present and empty"
        );
    }
}
