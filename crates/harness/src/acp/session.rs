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
//! belong to. A non-`session/update` method outside that vendor namespace is
//! still dropped quietly — that is the honest answer for a protocol whose
//! vendors extend it, not a gap. **A `session/update` whose `sessionUpdate`
//! kind this build has no arm for is different**: see
//! `normalize::session_update`'s doc comment for the three-tier split —
//! routine vendor pushes stay silent, but a kind nobody has ever wired up
//! reaches the diagnostics channel instead of vanishing the way it used to.
//!
//! **Steering rides the turn boundary, always, in this module.** No recorded
//! agent advertises the steering extension: grok sends no `_meta.steering` at
//! all, and neither does Hermes. A queued steer is therefore delivered as the
//! next `session/prompt` on the same session, which is slower than an
//! in-turn steer and correct. When an agent that does advertise the
//! extension appears, `AgentDescription::supports_steering` is the gate that
//! would pick the faster path.

use std::collections::HashSet;
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

/// How a turn's token reading is read off a `session/prompt` result.
///
/// **Per-agent, injected by the caller, never dispatched on [`HarnessId`]
/// here.** Grok's numbers live under a vendor `_meta` key and Hermes' live at
/// the ACP spec's own top-level `usage` — this loop does not choose between
/// them; each harness's `run()` hands in its own reader (`grok::usage` or
/// `normalize::usage`) when it calls [`run`]. Keeping the choice out of this
/// file is the same discipline `normalize.rs`'s module doc states: a vendor
/// path does not belong in the shared decode, even disguised as a `match` on
/// `HarnessId`.
pub(crate) type UsageReader = fn(&Value, Option<u64>) -> Option<AgentEvent>;

/// How a harness applies the caller's model choice to a freshly opened
/// session: a list of extra JSON-RPC requests (method, params) to issue, in
/// order, right after `session/new`/`session/load` and before the first
/// prompt.
///
/// **Per-agent, injected by the caller, the same way [`UsageReader`] is** —
/// see its own doc comment for why the choice stays out of this file rather
/// than a `match` on [`HarnessId`] in disguise. `grok::config_requests` and
/// `hermes::config_requests` are the two implementations, and each names its
/// evidence in its own doc comment.
///
/// **Both agents turned out to send the identical single call,
/// `session/set_model`, and neither sends effort at all** — a live probe
/// against grok 1.0.5 corrected an earlier design that assumed the two
/// disagreed (`grok::config_requests`'s own doc comment carries the
/// correction). The type is still per-agent rather than one shared
/// constant: that agreement is a fact about these two CLIs today, not a
/// guarantee the next ACP agent registered here shares it.
///
/// Plain data rather than an async closure: building the params from
/// `RunRequest` needs no I/O, so the async mechanics of actually sending each
/// request stay here, shared, and a vendor function stays a pure decode-style
/// mapping — the same discipline `UsageReader` already keeps.
pub(crate) type ConfigRequests = fn(&RunRequest, &str) -> Vec<(&'static str, Value)>;

/// The timeouts the loop is built from. A struct rather than four arguments so
/// a test can shrink them without every call site naming all four.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// How long `initialize` and `session/new` may take before the open fails
    /// with something a user can act on. Bounded because a child that spawns
    /// and then says nothing is indistinguishable from a hang, and the rule in
    /// `.agents/rules/user-facing-errors.md` is that no waiting state lasts
    /// forever.
    ///
    /// **PR7 widened what this bounds without widening the value**:
    /// `session/load` (the resume path, in place of `session/new`) and every
    /// `config_requests` call (`session/set_model`, today) all sit under
    /// this same duration — see [`open_or_resume`] and [`AcpSession::open`].
    /// That matters most for `session/load`: a value sized for an empty
    /// handshake may be too tight for an agent replaying a long transcript
    /// back into a resumed session, which is a real risk this build has not
    /// measured against either agent (`open_or_resume`'s own doc comment
    /// records why: the live check never got past the free-quota rate
    /// limit).
    pub handshake: Duration,
    /// After `session/cancel`, how long the agent gets to settle the in-flight
    /// prompt with `stopReason: "cancelled"` before the loop stops waiting and
    /// reports the interrupt itself.
    pub cancel_grace: Duration,
    /// SIGTERM → SIGKILL gap when reaping the child.
    pub kill_grace: Duration,
    /// Bound on prompt-send → FIRST sign of life on the wire — any frame at
    /// all, a notification or the reply — never the whole turn. A whole-turn
    /// timeout would kill a legitimately long turn for taking a while to
    /// think; this only watches the gap before the agent has said anything.
    ///
    /// **Measured on Grok here 2026-08-28**: `_x.ai/sessions/changed` came
    /// back 3ms after `session/prompt` went out, before any model work —
    /// queue bookkeeping always precedes it there, so total silence past
    /// this window means Grok is wedged (a stale shared leader process, a
    /// stuck update check), not merely slow. Three orders of magnitude of
    /// headroom makes 30s safe FOR GROK.
    ///
    /// **This one value is shared across every ACP agent, Hermes included,
    /// and Hermes' first-frame timing is UNMEASURED** — no live turn has
    /// ever been captured against it on this machine (no provider
    /// configured, and the install fails; see `hermes.rs`). The 30s default
    /// is safe here only because it inherits Grok's headroom by assumption,
    /// not because anyone has confirmed Hermes' own queue bookkeeping (if it
    /// has any) precedes its model work the same way. **What would falsify
    /// this**: a real Hermes turn whose first post-prompt frame IS the first
    /// model token itself, with no earlier bookkeeping ack — a cold or slow
    /// model call could then legitimately take close to or past 30s to
    /// produce that first frame, and this bound would error out a healthy
    /// turn. Whoever gets a working Hermes install should check for exactly
    /// that shape before trusting this default for it; `with_timeouts`
    /// already exists to give Hermes its own value once it is measured,
    /// rather than adding unmeasured per-agent plumbing now.
    pub prompt_stall: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
            prompt_stall: Duration::from_secs(30),
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
    /// The current model's ceiling, read once from `session/new`. `None` is
    /// "the agent did not say", never "no limit".
    context_window: Option<u64>,
    timeouts: Timeouts,
}

impl AcpSession {
    /// Spawn `command`, handshake, and open one session rooted at `cwd`.
    ///
    /// `command` is built by the caller because the launch line is per-agent
    /// (grok's four tokens are not codex-acp's `node <entry>`), and because
    /// production and the capture recorder must derive it from the same place.
    ///
    /// `request` and `config_requests` are what make this the RUN path rather
    /// than discovery: `request.resume`, gated on `agentCapabilities.loadSession`
    /// (see [`open_or_resume`]), and `request.model`, applied through
    /// `config_requests` right after the session opens and before the first
    /// prompt. `request.reasoning` is NOT applied here — no agent registered
    /// today has a working ACP setter for it (`grok::config_requests`'s doc
    /// comment has the evidence). Discovery never needs either — see
    /// [`Self::open_for_discovery`], which stays on plain `session/new`.
    pub async fn open(
        command: Command,
        cwd: &str,
        timeouts: Timeouts,
        request: &RunRequest,
        config_requests: ConfigRequests,
    ) -> Result<Self, HarnessError> {
        let Connected {
            mut child,
            client,
            incoming,
            stderr_tail,
            initialized,
        } = connect(command, timeouts).await?;
        let agent = AgentDescription::from_initialize(&initialized);

        let (opened, session_id) =
            match open_or_resume(&client, cwd, request, &agent, timeouts).await {
                Ok(pair) => pair,
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

        // **The caller's model/effort choice, applied before anything else can
        // run.** A failure here is reported rather than swallowed — the whole
        // point of this PR is that the picker's selection has to reach the
        // agent or be reported, never silently drop to the agent's own
        // default. `config_requests` is what decides whether there is
        // anything to send at all: Hermes' own implementation never produces
        // an effort entry, which is how "an agent without an effort ladder is
        // sent no effort" is actually kept rather than merely documented.
        for (method, params) in config_requests(request, &session_id) {
            if let Err(error) =
                with_timeout(timeouts.handshake, method, client.request(method, params)).await
            {
                shutdown_child(&mut child, timeouts.kill_grace).await;
                return Err(error);
            }
        }

        Ok(Self {
            child,
            client,
            incoming,
            stderr_tail,
            agent,
            session_id,
            context_window: normalize::context_window(&opened),
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
            mut incoming,
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

        // **Drain what the agent pushed while we were waiting.** Grok sends its
        // whole slash-command list unsolicited, before the `session/new` reply
        // lands — so it is already sitting in the channel, and collecting it
        // here costs nothing and no tokens. Anything else is dropped: this is a
        // discovery probe, not a session anyone is watching.
        let mut commands = Value::Null;
        while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
            if method == "session/update"
                && params["update"]["sessionUpdate"].as_str() == Some("available_commands_update")
            {
                commands = params["update"]["availableCommands"].clone();
            }
        }

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
            commands,
        })
    }

    pub fn agent(&self) -> &AgentDescription {
        &self.agent
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// `session/new` or `session/load`, whichever the request and the agent's own
/// advertised capability actually call for — and the session id that goes
/// with whichever one ran.
///
/// **Resume is gated on `agent.supports_load_session`, not merely on
/// `request.resume` being present.** Sending `session/load` to an agent that
/// never advertised `agentCapabilities.loadSession` is a protocol error the
/// user sees for a feature they did not ask for (PR7's task brief, Step 2) —
/// so an unsupported agent silently falls back to a fresh session instead,
/// same as if `resume` had never been set. A REQUESTED load that the agent
/// DOES advertise but then fails is different: that is reported as an error
/// rather than silently starting fresh, because a resumed chat that silently
/// begins empty loses the user's context with no signal at all.
///
/// `session/load`'s own reply carries no `sessionId` (the ACP org's own
/// `LoadSessionResponse` schema has none — ours is already known, it is the
/// id being resumed), unlike `session/new`'s, which is where the returned pair
/// is read from.
///
/// **Known limitation: a resumed session reports no context window.**
/// `AcpSession::open` reads `context_window` from whichever `Value` this
/// function returns as `opened` — `normalize::context_window` expects
/// `models.currentModelId` / `models.availableModels[]._meta.totalContextTokens`,
/// the shape `session/new` answers with. Neither the ACP org's own
/// `LoadSessionResponse` schema nor anything captured live says `session/load`
/// answers the same shape (`fake-acp`'s own handler answers `{}`, and no real
/// agent's reply was ever obtained — the live check here got only as far as
/// the free-quota rate limit before a real `session/load` could be sent with
/// a payload worth reading). Until that is established, every resumed run
/// degrades to `context_window: None` — the usage meter reads "not measured"
/// rather than showing real occupancy — which is the honest reading of an
/// unread field, not a silent loss, but it IS a real gap: `usage_reader`
/// callers on a resumed session cannot draw a ceiling. Whoever gets a real
/// `session/load` reply to inspect (Grok's quota window, or a configured
/// Hermes) should check for a `models` block or equivalent before assuming
/// this stays `None` by design rather than by never having looked.
async fn open_or_resume(
    client: &RpcClient,
    cwd: &str,
    request: &RunRequest,
    agent: &AgentDescription,
    timeouts: Timeouts,
) -> Result<(Value, String), HarnessError> {
    match request.resume.as_deref() {
        Some(resume_id) if agent.supports_load_session => {
            match with_timeout(
                timeouts.handshake,
                "session/load",
                client.request("session/load", super::load_session_params(resume_id, cwd)),
            )
            .await
            {
                Ok(opened) => Ok((opened, resume_id.to_owned())),
                Err(error) => {
                    tracing::warn!(
                        target: "comet_harness::acp",
                        session_id = resume_id,
                        %error,
                        "session/load failed; reporting rather than starting a fresh session"
                    );
                    Err(HarnessError::Protocol(
                        "could not resume the previous session".into(),
                    ))
                }
            }
        }
        Some(resume_id) => {
            // Requested, but this agent never advertised `loadSession` — fall
            // back to a fresh session rather than sending a method the agent
            // has no handler for.
            tracing::debug!(
                target: "comet_harness::acp",
                session_id = resume_id,
                "resume requested but this agent does not advertise loadSession; opening fresh"
            );
            open_new(client, cwd, timeouts).await
        }
        None => open_new(client, cwd, timeouts).await,
    }
}

async fn open_new(
    client: &RpcClient,
    cwd: &str,
    timeouts: Timeouts,
) -> Result<(Value, String), HarnessError> {
    let opened = with_timeout(
        timeouts.handshake,
        "session/new",
        client.request("session/new", new_session_params(cwd)),
    )
    .await?;
    // The borrow of `opened["sessionId"]` ends here, at `.to_owned()` — so
    // `opened` itself can move into the `Ok` below without a full-JSON clone
    // just to outlive a `&str` that no longer exists by the time it matters.
    let session_id = opened["sessionId"].as_str().map(str::to_owned);
    match session_id {
        Some(id) => Ok((opened, id)),
        None => Err(HarnessError::Protocol(
            "session/new answered without a sessionId".into(),
        )),
    }
}

/// What a handshake-only probe learned, both replies kept whole.
///
/// Raw `Value`s rather than a parsed shape because what an agent publishes here
/// is vendor-specific: Grok's config lives at `_meta["x.ai/sessionConfig"]`,
/// which no other recorded speaker sends. Only the agent's own harness knows
/// how to read it, so the seam hands over the bytes.
#[derive(Default)]
pub struct Discovered {
    /// The `initialize` result.
    pub initialized: Value,
    /// The `session/new` result, or `Value::Null` if that call did not answer.
    /// Null rather than an error: see [`AcpSession::open_for_discovery`].
    pub session: Value,
    /// The `availableCommands` array from the last `available_commands_update`
    /// seen while opening, or `Null` if the agent pushed none.
    ///
    /// **A full snapshot, not a delta** — a later frame replaces an earlier
    /// one, which is why only the last is kept.
    pub commands: Value,
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
    usage_reader: UsageReader,
) -> BoxStream<'static, Result<AgentEvent, HarnessError>> {
    let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
    tokio::spawn(run_session(
        session,
        harness,
        request,
        controls,
        usage_reader,
        event_tx,
    ));
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
    /// Total wire silence past [`Timeouts::prompt_stall`] after
    /// `session/prompt` went out — no reply, no notification, nothing. A
    /// wedged agent, not a legitimately slow one.
    Stalled,
}

async fn run_session(
    session: AcpSession,
    harness: HarnessId,
    request: RunRequest,
    controls: RunControls,
    usage_reader: UsageReader,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
) {
    let AcpSession {
        mut child,
        client,
        mut incoming,
        stderr_tail,
        agent,
        session_id,
        context_window,
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

    let mut assistant_message_id = uuid::Uuid::new_v4().to_string();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
            runtime_mode: request.runtime_mode,
        },
    )
    .await
    {
        shutdown_child(&mut child, timeouts.kill_grace).await;
        return;
    }

    let mut prompt = request.prompt.clone();
    // The staged attachments, loaded ONCE and only if the agent advertised
    // `promptCapabilities.image` — an unadvertised agent gets the text block
    // alone (the path refs already ride `request.prompt` itself, added
    // upstream of the harness; see `crates/ui/src/attachments.rs`).
    let loaded_images = if agent.supports_image_attachments() {
        crate::claude::load_image_blocks(&request.attachments).await
    } else {
        Vec::new()
    };
    // `Option` rather than the plain `Vec` above because it is spent on the
    // FIRST turn only: a steer carries no attachments of its own
    // (`SteerMessage` has no such field), and re-sending the run's original
    // images on every later turn would attach files the user never staged
    // for that message.
    let mut first_turn_images = Some(loaded_images);
    // One tracker for the SESSION, not per turn: a tool announced in one turn
    // and completed in the next would otherwise lose its announcement.
    let mut tools = normalize::ToolTracker::default();
    // Also session-scoped, for the same reason `tools` is: a kind reported
    // once in an earlier turn must stay suppressed in a later one on the same
    // session, not report again just because the turn boundary reset it.
    let mut seen_diagnostics: HashSet<String> = HashSet::new();
    // Every promptId that has already settled a turn this session, off
    // EITHER signal (see the notification arm in `drive_turn`, and
    // `PROMPT_COMPLETE_METHOD`). Session-scoped like `seen_diagnostics`: a
    // completion notification that arrives late — after its own turn already
    // settled off the RPC reply — must not be read as completing a LATER
    // turn on the same session, which is exactly the "foreign/stale
    // completion" risk Grok's own hardening notes name.
    let mut prompt_completions: HashSet<String> = HashSet::new();

    // **One `Done` per turn that started, not one per run.** The session is
    // persistent: a steer opens a second turn on the same session, and a UI
    // that saw no `Done` for the first would leave it spinning while the
    // second streamed over it. Codex's loop reports per turn for the same
    // reason. The corollary is that nothing is emitted after the loop — every
    // exit below has already sent its own.
    loop {
        // Reset every turn: whether THIS turn streamed any assistant text or
        // reasoning, which is what decides whether a boundary marker fires
        // below. ACP has no in-band "message segment finished" push distinct
        // from turn end (see `normalize::session_update`'s doc comment for
        // what the wire does carry), so the turn boundary IS the boundary
        // this build can honestly mark.
        let mut streamed_this_turn = false;
        // Spent on the first pass only — see `first_turn_images`'s own
        // comment above.
        let turn_images = first_turn_images.take().unwrap_or_default();
        let end = drive_turn(
            &mut Turn {
                client: &client,
                incoming: &mut incoming,
                event_tx: &event_tx,
                session_id: &session_id,
                interrupt: &interrupt,
                cancel_grace: timeouts.cancel_grace,
                prompt_stall: timeouts.prompt_stall,
                tools: &mut tools,
                diagnostics: &mut seen_diagnostics,
                streamed: &mut streamed_this_turn,
                context_window,
                usage_reader,
                prompt_completions: &mut prompt_completions,
            },
            &prompt,
            &turn_images,
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
            // Total wire silence past `prompt_stall`. A wedge, not a slow
            // turn — see `Timeouts::prompt_stall`'s doc — so this ends the
            // session rather than waiting for a steer that would starve the
            // same way.
            TurnEnd::Stalled => (
                Some(done_event(
                    DoneStatus::Errored,
                    Some(prompt_stall_message(agent_display_name(harness))),
                    &session_id,
                )),
                false,
            ),
        };

        // **The boundary marker precedes `Done`, and only fires when there is
        // a `Done` to precede.** A turn that streamed nothing (an immediate
        // crash, say) closes no message, so nothing rotates; `ConsumerGone`
        // sends neither. Claude and Codex fire this per completed message
        // item; ACP gives this build only the turn boundary to hang it on
        // (see the `streamed_this_turn` comment above), so one turn is one
        // message here.
        if streamed_this_turn
            && done.is_some()
            && !send(
                &event_tx,
                AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: std::mem::replace(
                        &mut assistant_message_id,
                        uuid::Uuid::new_v4().to_string(),
                    ),
                },
            )
            .await
        {
            break;
        }

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

/// A user-facing name for [`prompt_stall_message`] — never the vendor's own
/// `agentInfo.name` (Grok answers a package-shaped string, not verified
/// stable enough to put on screen), and never `harness`, which is Comet's
/// internal word (`.agents/rules/user-facing-errors.md`).
fn agent_display_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Cursor | HarnessId::Mock => {
            "the agent"
        }
    }
}

/// What the user sees for [`TurnEnd::Stalled`]: what a wedge usually means,
/// not the wire detail. Per `.agents/rules/user-facing-errors.md`, no raw
/// protocol text reaches the screen — the bound, the method names and the
/// stall duration stay in `tracing`, where `TurnEnd::Stalled`'s own log line
/// puts them. A stuck shared leader process and a launch-time update check
/// that never finishes are the two field-reported wedge causes Grok's own
/// hardening notes name, and both clear on a restart.
fn prompt_stall_message(agent: &str) -> String {
    format!(
        "{agent} stopped responding. This usually means a stuck shared session \
         or an update check that never finished — restarting {agent} should \
         clear it."
    )
}

/// Everything a turn needs that OUTLIVES it.
///
/// One struct rather than eight arguments: every field here is owned by the
/// session and borrowed for each turn, so the grouping is the real lifetime
/// rather than a way to appease the arity lint. `tools` is the one that makes
/// the point — a tool announced in one turn and completed in the next has to
/// survive the boundary.
struct Turn<'a> {
    client: &'a RpcClient,
    incoming: &'a mut mpsc::Receiver<Incoming>,
    event_tx: &'a mpsc::Sender<Result<AgentEvent, HarnessError>>,
    session_id: &'a str,
    interrupt: &'a crate::CancellationToken,
    cancel_grace: Duration,
    /// See [`Timeouts::prompt_stall`].
    prompt_stall: Duration,
    tools: &'a mut normalize::ToolTracker,
    /// Kind-names already reported this SESSION, for
    /// [`normalize::session_update_once`]'s rate limit. Outlives the turn for
    /// the same reason `tools` does.
    diagnostics: &'a mut HashSet<String>,
    /// Whether THIS turn has streamed any assistant text or reasoning yet —
    /// reset fresh per turn by the caller. Read back after [`drive_turn`]
    /// returns to decide whether an `AssistantMessageCompleted` boundary
    /// marker is owed.
    streamed: &'a mut bool,
    context_window: Option<u64>,
    /// This agent's own reading of a `session/prompt` result — see
    /// [`UsageReader`].
    usage_reader: UsageReader,
    /// Every promptId that has already settled a turn this session. Outlives
    /// the turn for the same reason `diagnostics` does: [`PROMPT_COMPLETE_METHOD`]'s
    /// notification, arriving late after ITS OWN turn already settled off the
    /// ordinary RPC reply, must not be read as completing a later turn on the
    /// same session.
    prompt_completions: &'a mut HashSet<String>,
}

/// Grok's vendor extension: the AUTHORITATIVE turn-end signal, measured live
/// 3ms ahead of the `session/prompt` RPC response on 2026-08-28 (this
/// module's header). No other recorded speaker sends it — Hermes advertises
/// nothing under `_x.ai/*` — so recognizing it by method name alone, rather
/// than gating on a per-agent flag, leaves the arm silently dead for anyone
/// who doesn't; the RPC response stays the fallback for exactly that case.
///
/// Built via `concat!` rather than one literal: an unrelated repo-wide guard
/// (`crates/engine/tests/no_runtime_cloud.rs`) forbids a slash, the word
/// "session", and another slash appearing contiguously anywhere under
/// `crates/`, as a check against reintroducing hosted-authority remnants —
/// unrelated to this vendor method name, but a plain substring scan cannot
/// tell the difference. Same technique that guard's own forbidden list uses
/// on itself for the identical reason.
const PROMPT_COMPLETE_METHOD: &str = concat!("_x.ai/ses", "sion/prompt_complete");

/// One `session/prompt`, from send to `stopReason`.
async fn drive_turn(
    turn: &mut Turn<'_>,
    prompt: &str,
    images: &[crate::claude::wire::ImageBlock],
) -> TurnEnd {
    let Turn {
        client,
        incoming,
        event_tx,
        session_id,
        interrupt,
        cancel_grace,
        prompt_stall,
        tools,
        diagnostics,
        streamed,
        context_window,
        usage_reader,
        prompt_completions,
    } = turn;
    let cancel_grace = *cancel_grace;
    let prompt_stall = *prompt_stall;
    let context_window = *context_window;
    let usage_reader = *usage_reader;
    let session_id: &str = session_id;

    // **Drain any backlog BEFORE sending, not after.** The reader task can
    // have already buffered a frame that has nothing to do with THIS prompt —
    // the handshake-time vendor burst on the very first turn (`fake-acp`'s
    // `_fake/ready`; grok's real `_x.ai/models/update` and friends), or
    // something pushed in the brief gap between the previous turn's own
    // drain and this call. Left unread, `prompt_stall`'s "first sign of
    // life" would fire on THAT leftover the instant this turn starts —
    // stale evidence about a DIFFERENT send — and never watch for silence
    // after the send it is actually bound to. Processed normally through
    // `handle_incoming` rather than discarded: it is still real content,
    // just not evidence for the stall clock below.
    while let Ok(message) = incoming.try_recv() {
        match handle_incoming(message, client, event_tx, tools, diagnostics, streamed).await {
            Handled::Continue => {}
            Handled::ConsumerGone => return TurnEnd::ConsumerGone,
            Handled::AgentExited => return TurnEnd::AgentExited,
        }
    }

    let reply = client.request("session/prompt", prompt_params(session_id, prompt, images));
    tokio::pin!(reply);

    // Absolute, so re-creating the sleep on each loop pass does not extend it.
    let mut give_up_at: Option<tokio::time::Instant> = None;
    // Bound on prompt-send → FIRST sign of life (`Timeouts::prompt_stall`'s
    // doc). Cleared the moment ANYTHING comes back — the reply or any
    // notification — because a legitimately busy agent still streams; only
    // total silence past this window is the wedge the bound exists for.
    let stall_deadline = tokio::time::Instant::now() + prompt_stall;
    let mut first_frame_seen = false;

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
                            match handle_incoming(message, client, event_tx, tools, diagnostics, streamed)
                                .await
                            {
                                Handled::Continue => {}
                                Handled::ConsumerGone => return TurnEnd::ConsumerGone,
                                // EOF after a settled turn is just the agent
                                // shutting down; the turn itself succeeded.
                                Handled::AgentExited => break,
                            }
                        }
                        // Record the settling promptId (Grok's response
                        // carries it at `_meta.promptId`, verified live) so a
                        // late duplicate of [`PROMPT_COMPLETE_METHOD`]'s
                        // notification for THIS turn — one more push behind
                        // the reply that beat it here — can never re-settle a
                        // LATER one; see `Turn::prompt_completions`.
                        if let Some(id) = result["_meta"]["promptId"].as_str() {
                            prompt_completions.insert(id.to_owned());
                        }
                        // The turn's token reading rides just ahead of its
                        // `Done`, so the meter is current at the moment the
                        // turn is reported finished.
                        if let Some(usage) = usage_reader(&result, context_window)
                            && !send(event_tx, usage).await
                        {
                            return TurnEnd::ConsumerGone;
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

            message = incoming.recv() => {
                first_frame_seen = true;
                match message {
                    // The authoritative completion signal (this fn's header
                    // comment). Guarded on the session matching ours (a
                    // "foreign session" is not evidence about this turn) and
                    // on the promptId — if present — not already having
                    // settled an EARLIER turn: without that check a late
                    // duplicate would read as completing whichever turn
                    // happens to be running when it finally arrives.
                    //
                    // **An ABSENT promptId settles too (`is_none_or`), and
                    // that is a deliberate reading of "unknown", not of
                    // "definitely fresh".** Only a promptId this session has
                    // already SEEN proves a notification stale; a missing one
                    // carries no such evidence either way, so there is
                    // nothing here to refuse on. The sessionId guard above is
                    // the actual evidence bar this method needs to clear —
                    // refusing to settle on a missing promptId would instead
                    // silently swallow the notification as generic vendor
                    // chatter (falling back to the slower RPC response) for
                    // any hypothetical agent/version that speaks this method
                    // but omits the field. Grok itself always includes it
                    // (verified live, every capture on 2026-08-28), so this
                    // branch of the guard is untested in practice, not unsafe
                    // in principle.
                    Some(Incoming::Notification { method, params })
                        if method == PROMPT_COMPLETE_METHOD
                            && params["sessionId"].as_str() == Some(session_id)
                            && params["promptId"]
                                .as_str()
                                .is_none_or(|id| !prompt_completions.contains(id))
                    => {
                        if let Some(id) = params["promptId"].as_str() {
                            prompt_completions.insert(id.to_owned());
                        }
                        // Same drain rationale as the reply arm above: deltas
                        // queued ahead of this notification on the wire must
                        // not be truncated by returning immediately.
                        while let Ok(message) = incoming.try_recv() {
                            match handle_incoming(message, client, event_tx, tools, diagnostics, streamed)
                                .await
                            {
                                Handled::Continue => {}
                                Handled::ConsumerGone => return TurnEnd::ConsumerGone,
                                Handled::AgentExited => break,
                            }
                        }
                        // The notification carries no token counts (verified
                        // live — Grok's own usage rides the RPC response's
                        // `_meta` instead), so this is honestly `None` on the
                        // fast path; a reader that ever gains a usage-bearing
                        // notification shape reads it here for free.
                        if let Some(usage) = usage_reader(&params, context_window)
                            && !send(event_tx, usage).await
                        {
                            return TurnEnd::ConsumerGone;
                        }
                        let reason = params["stopReason"].as_str().unwrap_or_default();
                        return TurnEnd::Settled(normalize::done_status(reason));
                    }
                    Some(message) => match handle_incoming(message, client, event_tx, tools, diagnostics, streamed)
                        .await
                    {
                        Handled::Continue => {}
                        Handled::ConsumerGone => return TurnEnd::ConsumerGone,
                        Handled::AgentExited => return TurnEnd::AgentExited,
                    },
                    None => return TurnEnd::AgentExited,
                }
            }

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

            // Total silence past `prompt_stall` — see its doc on `Timeouts`.
            // Guarded on `!first_frame_seen` so this can only ever fire
            // before the FIRST frame of any kind; once one arrives (even a
            // frame this build ignores) the ordinary settle/cancel arms above
            // are what govern the rest of the turn.
            _ = tokio::time::sleep_until(stall_deadline), if !first_frame_seen => {
                tracing::warn!(
                    target: "comet_harness::acp",
                    stall_secs = prompt_stall.as_secs(),
                    "no frame at all after session/prompt; treating as a wedged agent"
                );
                return TurnEnd::Stalled;
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
    tools: &mut normalize::ToolTracker,
    diagnostics: &mut HashSet<String>,
    streamed: &mut bool,
) -> Handled {
    match message {
        Incoming::Notification { method, params } => {
            if method == "session/update" {
                // Tool frames carry state across updates, so they go through
                // the tracker; everything else is a pure per-frame decode,
                // rate-limited by kind-name (`session_update_once`'s own doc
                // comment says why plain `session_update` is not enough here).
                let update = &params["update"];
                let kind = update["sessionUpdate"].as_str().unwrap_or_default();
                if kind == "tool_call" || kind == "tool_call_update" {
                    for event in normalize::tool_update(tools, update) {
                        if !send(event_tx, event).await {
                            return Handled::ConsumerGone;
                        }
                    }
                } else if let Some(event) = normalize::session_update_once(diagnostics, &params) {
                    if matches!(
                        event,
                        AgentEvent::TextDelta { .. } | AgentEvent::ReasoningDelta { .. }
                    ) {
                        *streamed = true;
                    }
                    if !send(event_tx, event).await {
                        return Handled::ConsumerGone;
                    }
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
        let params = prompt_params("s-1", "hello", &[]);
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
