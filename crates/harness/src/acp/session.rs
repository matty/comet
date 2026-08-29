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

use std::collections::{HashSet, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus, HarnessId,
    RunRequest,
};

use super::{
    AgentDescription, approval, initialize_params, new_session_params, normalize, prompt_params,
};
use crate::jsonrpc::{Incoming, RpcClient};
use crate::{HarnessError, RunControls, StderrTail, shutdown_child};

/// The run's answer to one `session/request_permission`, boxed the same way
/// [`RunControls::request_approval`] is. A local alias rather than reusing
/// that field's type directly: `codex::handle_server_request` names its own
/// copy for the same reason — the call sites that need to spawn a task with
/// it are simpler naming the closure type once.
type RequestApprovalFn =
    Box<dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision> + Send + Sync>;

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

/// What a `session/new`/`session/load` failure means when it happens before
/// any session ever opened — the difference between "this agent needs
/// sign-in or setup first" and any other reason the open failed.
///
/// **Per-agent, injected by the caller, the same way [`UsageReader`] and
/// [`ConfigRequests`] are** — see [`UsageReader`]'s own doc comment for why
/// the choice stays out of this file rather than a `match` on [`HarnessId`]
/// in disguise. What "not ready yet" looks like on the wire is genuinely
/// vendor-specific: Grok answers `-32000 Authentication required` on
/// `session/new` itself (verified live 2026-08-29 against a `GROK_HOME` with
/// no `auth.json`), while Hermes answers `-32603 Internal error` with the
/// actual reason — `"No LLM provider configured. Run `hermes model`..."` —
/// carried in the error's `data`, not its `message`.
///
/// **Takes the raw [`crate::jsonrpc::RpcFailure`], not a [`HarnessError`].**
/// A review finding on an earlier version of this file: folding a JSON-RPC
/// error's `data` into `HarnessError::Protocol`'s own text (so this mapper
/// could see it via `.contains(...)`) widened what every OTHER caller of
/// `RpcClient::request` shows a user, including two Codex sites
/// (`codex/mod.rs`'s `turn/start`/`turn/steer` failures) that had never
/// carried provider `data` before. `RpcFailure` keeps `message` and `data`
/// separate all the way from the wire to this mapper, and the final
/// `HarnessError` is built only AFTER this function has had its look — from
/// `fallback` (the caller's own safe text) when this returns `None`, or
/// from this function's own `NeedsSetup` payload when it returns `Some`.
/// Neither path ever exposes `data` to `HarnessError::Protocol`'s `Display`.
///
/// Only the vendor module that captured its own shape can tell a genuine
/// "sign in/configure first" from any other reason `open_or_resume` failed
/// — a timeout, a malformed reply, a real bug.
///
/// `None` passes the original error through unchanged: a failure this
/// build's mapper does not recognize falls back to whatever safe text the
/// caller already had rather than being silently reclassified as "you need
/// to sign in" on a guess.
pub(crate) type OpenFailureMapper = fn(&crate::jsonrpc::RpcFailure) -> Option<HarnessError>;

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
    ///
    /// `map_open_failure` runs on a failed `session/new` OR a failed
    /// `session/load` below — see [`OpenFailureMapper`] and
    /// [`open_or_resume`]'s own doc comment for why `session/load`'s
    /// generic-looking fallback text does not shadow it. A failure from the
    /// handshake itself (`initialize`, via `connect`, above) is never a
    /// "sign in first" case: the agent never got far enough to say anything
    /// about auth.
    pub async fn open(
        command: Command,
        cwd: &str,
        timeouts: Timeouts,
        request: &RunRequest,
        config_requests: ConfigRequests,
        map_open_failure: OpenFailureMapper,
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
                Err(OpenFailure { raw, fallback }) => {
                    // The handshake succeeded and this did not, so a live child is
                    // holding stdio nobody will read again. `kill_on_drop` would
                    // reach it eventually, but only once every clone of the client
                    // is dropped too — reap it here instead of leaving the timing
                    // to drop order.
                    shutdown_child(&mut child, timeouts.kill_grace).await;
                    // `raw` is `None` for a failure that was never a JSON-RPC
                    // error at all (`session/new` answering without a
                    // `sessionId`) — nothing for the mapper to recognize, so
                    // `fallback` is used directly.
                    let mapped = raw.as_ref().and_then(map_open_failure);
                    return Err(mapped.unwrap_or(fallback));
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

/// A `session/new`/`session/load` failure that reached [`AcpSession::open`]
/// before any session ever opened.
///
/// **`raw` is what [`OpenFailureMapper`] inspects, and `fallback` is what's
/// returned when the mapper does not recognize it (or there was nothing to
/// recognize — `raw` is `None`).** Keeping the two separate, rather than
/// building one `HarnessError` up front and text-matching it later (an
/// earlier version of this file did exactly that), is what lets
/// `session/load`'s branch below give a resumed-chat failure its own
/// deliberately generic prose — "could not resume the previous session",
/// not raw JSON-RPC text — as the fallback, WITHOUT that prose replacing
/// the real error before the mapper ever sees it. Replacing first, the way
/// the earlier version did, is a real bug caught in review: a signed-out
/// user reopening a resumable Grok chat got the resume fallback with no
/// sign-in guidance, because `map_open_failure`'s `.contains("Authentication
/// required")` had nothing left to match against.
struct OpenFailure {
    raw: Option<crate::jsonrpc::RpcFailure>,
    fallback: HarnessError,
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
/// begins empty loses the user's context with no signal at all — UNLESS
/// [`OpenFailureMapper`] recognizes the failure as "sign in first", in which
/// case the sign-in guidance wins over the generic resume prose (see
/// [`OpenFailure`]'s own doc comment: Grok advertises `loadSession: true`, so
/// a signed-out user reopening an existing chat hits this exact path, not
/// just the fresh-session one).
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
) -> Result<(Value, String), OpenFailure> {
    match request.resume.as_deref() {
        Some(resume_id) if agent.supports_load_session => {
            match with_timeout_detail(
                timeouts.handshake,
                "session/load",
                client.request_detail("session/load", super::load_session_params(resume_id, cwd)),
            )
            .await
            {
                Ok(opened) => Ok((opened, resume_id.to_owned())),
                Err(raw) => {
                    tracing::warn!(
                        target: "comet_harness::acp",
                        session_id = resume_id,
                        message = %raw.message,
                        data = raw.data.as_deref().unwrap_or(""),
                        "session/load failed; reporting rather than starting a fresh session unless the failure mapper recognizes it as sign-in/setup first"
                    );
                    Err(OpenFailure {
                        fallback: HarnessError::Protocol(
                            "could not resume the previous session".into(),
                        ),
                        raw: Some(raw),
                    })
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
) -> Result<(Value, String), OpenFailure> {
    let opened = with_timeout_detail(
        timeouts.handshake,
        "session/new",
        client.request_detail("session/new", new_session_params(cwd)),
    )
    .await
    .map_err(|raw| {
        // The only copy of the raw JSON-RPC text (`message` AND `data`) —
        // logged here before `OpenFailure` either gets mapped to a clean
        // `NeedsSetup` or falls back to `fallback` below, neither of which
        // keeps the wire text on screen. `.agents/rules/user-facing-errors.md`
        // requires the diagnostic detail to stay recoverable in `tracing`;
        // this is that.
        tracing::debug!(
            target: "comet_harness::acp",
            message = %raw.message,
            data = raw.data.as_deref().unwrap_or(""),
            "session/new failed"
        );
        OpenFailure {
            fallback: HarnessError::Protocol(format!("session/new: {}", raw.message)),
            raw: Some(raw),
        }
    })?;
    // The borrow of `opened["sessionId"]` ends here, at `.to_owned()` — so
    // `opened` itself can move into the `Ok` below without a full-JSON clone
    // just to outlive a `&str` that no longer exists by the time it matters.
    let session_id = opened["sessionId"].as_str().map(str::to_owned);
    match session_id {
        Some(id) => Ok((opened, id)),
        // Not a JSON-RPC error at all — the call succeeded and answered
        // something unreadable. `raw: None`: there is nothing here that
        // could be a vendor's "sign in first" shape, so the mapper is never
        // asked about it (see `AcpSession::open`'s own call site).
        None => Err(OpenFailure {
            fallback: HarnessError::Protocol("session/new answered without a sessionId".into()),
            raw: None,
        }),
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

/// Same as [`with_timeout`], but for a request whose failure needs to reach
/// [`OpenFailureMapper`] with `data` intact — see
/// [`crate::jsonrpc::RpcFailure`]'s own doc comment for why `session/new`'s
/// and `session/load`'s calls use `RpcClient::request_detail` (and this
/// wrapper) instead of the ordinary `with_timeout`/`RpcClient::request` pair
/// every other call in this module still uses.
async fn with_timeout_detail(
    limit: Duration,
    method: &str,
    request: impl Future<Output = Result<Value, crate::jsonrpc::RpcFailure>>,
) -> Result<Value, crate::jsonrpc::RpcFailure> {
    match tokio::time::timeout(limit, request).await {
        Ok(result) => result,
        Err(_) => Err(crate::jsonrpc::RpcFailure {
            message: format!(
                "the agent did not answer {method} within {}s",
                limit.as_secs()
            ),
            data: None,
        }),
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
        // Unclaimed here. ACP has no input-request method of its own —
        // `UserInputQuestion`/`UserInputAnswer` have no ACP wire counterpart —
        // so nothing in this loop ever calls it.
        request_input: _request_input,
        request_approval,
        mut steering,
        interrupt,
    } = controls;
    // `Arc`, not a bare `Box`, because [`handle_incoming`]'s approval arm
    // spawns a task per `session/request_permission` — the message loop must
    // keep flowing while the user thinks (a blocked read here would stall the
    // very transcript they are reading to decide), and a spawned task needs
    // its own owned handle to call it from.
    let request_approval: Arc<RequestApprovalFn> = Arc::new(request_approval);

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
    let mut prompt_completions = PromptCompletions::default();

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
                request_approval: &request_approval,
            },
            &prompt,
            turn_images,
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
    prompt_completions: &'a mut PromptCompletions,
    /// The run's answer to `session/request_permission`. `Arc`, not a bare
    /// reference, because [`handle_incoming`]'s approval arm spawns a task
    /// that outlives the borrow of this `Turn` — see [`RequestApprovalFn`].
    request_approval: &'a Arc<RequestApprovalFn>,
}

/// Grok's vendor extension: the AUTHORITATIVE turn-end signal. **Measured
/// live 2026-08-28, raw wire timestamps: this notification at 3618ms,
/// the matching `session/prompt` RPC response at 3621ms — 3ms apart,
/// consistently, this notification first.** That raw capture lives outside
/// this tree (the ACP hardening task's own planning record, not restated
/// verbatim anywhere else in `crates/`); this doc comment is the primary
/// citation inside the tree for the figure, not a pointer to one. No other
/// recorded speaker sends it — Hermes advertises
/// nothing under `_x.ai/*` — so recognizing it by method name alone, rather
/// than gating on a per-agent flag, leaves the arm silently dead for anyone
/// who doesn't; the RPC response stays the fallback for exactly that case.
///
/// A repo-wide guard (`crates/engine/tests/no_runtime_cloud.rs`) forbids a
/// slash, the word "session", and another slash appearing contiguously
/// anywhere under `crates/`, as a check against reintroducing
/// hosted-authority remnants. This vendor method name is not one, and the
/// guard exempts it by name — spelled plainly here so that
/// `grep -r "_x.ai/session/prompt_complete" crates/` finds the constant this
/// doc calls the method's primary citation.
const PROMPT_COMPLETE_METHOD: &str = "_x.ai/session/prompt_complete";

/// Bound on [`PromptCompletions`] (`docs/debt/README.md`'s D124) — the same
/// "cannot become an allocator" concern `normalize`'s `MAX_TRACKED_UPDATE_KINDS`
/// and `MAX_TRACKED_CALLS` exist for, on a third registry whose failure mode
/// is different again.
///
/// **The strategy is eviction by recency, and neither sibling's would do.**
/// `ToolTracker` refuses to insert once full; D113's `seen` treats everything
/// past the cap as already-suppressed. Both keep the OLDEST entries, which is
/// exactly backwards here: this set exists so that a late duplicate of a
/// completion signal cannot settle a LATER turn, and a duplicate is a
/// near-in-time event — it rides one push behind the reply that beat it. The
/// id most likely to be needed is the one just inserted, so refusing to
/// insert it would drop the single entry the guard depends on and let the
/// stale notification through, which is the bug this set was added to
/// prevent. Dropping the oldest instead gives up protection only against a
/// duplicate that arrives more than [`MAX_TRACKED_PROMPT_COMPLETIONS`] turns
/// after its own turn settled.
///
/// **No escalation event, unlike D113's cap.** That one gave up diagnostic
/// SIGNAL, so a reader could no longer tell "nothing unfamiliar arrived" from
/// "Comet stopped looking", and one event per session bought that back. This
/// one gives up nothing observable: the ids are not reported anywhere, the
/// eviction is invisible to the user, and a session would need 64 turns
/// before the first one is dropped. A `trace!` is the honest weight.
const MAX_TRACKED_PROMPT_COMPLETIONS: usize = 64;

/// Every promptId that has already settled a turn this session, bounded.
///
/// A plain `HashSet` until D124: it is caller-owned and session-scoped, so a
/// long-lived session grew it one entry per turn forever. See
/// [`MAX_TRACKED_PROMPT_COMPLETIONS`] for why this evicts rather than refusing.
#[derive(Debug, Default)]
pub(crate) struct PromptCompletions {
    seen: HashSet<String>,
    /// Insertion order, oldest first — `seen`'s keys carry none of their own.
    order: VecDeque<String>,
}

impl PromptCompletions {
    /// Whether `id` has already settled a turn on this session.
    fn contains(&self, id: &str) -> bool {
        self.seen.contains(id)
    }

    /// Record `id`, evicting the oldest entry if that would exceed the cap.
    ///
    /// Re-inserting an id already present is a no-op rather than a refresh of
    /// its position: an id settles exactly one turn, so a second insert is a
    /// duplicate signal for the SAME turn, and moving it to the front would
    /// let a repeating stale notification hold its slot indefinitely.
    fn insert(&mut self, id: &str) {
        if !self.seen.insert(id.to_owned()) {
            return;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > MAX_TRACKED_PROMPT_COMPLETIONS
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
            tracing::trace!(
                target: "comet_harness::acp",
                cap = MAX_TRACKED_PROMPT_COMPLETIONS,
                "settled promptId evicted from the completion guard"
            );
        }
    }
}

/// How long the notification-settle arm below waits for the already-in-flight
/// `session/prompt` reply once [`PROMPT_COMPLETE_METHOD`] has already ended
/// the turn — long enough to catch the ~3ms gap [`PROMPT_COMPLETE_METHOD`]'s
/// own doc measured live, short enough that an agent which never answers the
/// RPC at all (`complete-notification-only` in the fixture, and the shape
/// upstream's original hang report was about) still settles promptly rather
/// than reintroducing that hang.
///
/// **~80x the measured ~3ms gap, not a round number picked for its own
/// sake.** A single measurement on one machine on one day is thin evidence
/// for a bound that fails SILENTLY when it is too tight — this build has no
/// drift sheet, no supported-version floor and no runnable live suite for
/// Grok (`docs/debt/README.md`'s D102), so a miss here would not be caught
/// by anything else in the tree. The margin also has to absorb process
/// scheduling and named-pipe latency on whatever machine runs this, not just
/// scheduler jitter on the one that measured 3ms — this repository's own
/// guidance is that a wall-clock figure from a GPU-less VM is an upper
/// bound, not a measurement. Still 120x below [`Timeouts::prompt_stall`]'s
/// own 30s default, so this can never be mistaken for — or mask — a wedged
/// agent: the only user-visible cost of a genuinely non-answering agent is
/// one extra 250ms per turn, not a hang.
const POST_NOTIFICATION_REPLY_BOUND: Duration = Duration::from_millis(250);

/// One `session/prompt`, from send to `stopReason`.
async fn drive_turn(
    turn: &mut Turn<'_>,
    prompt: &str,
    images: Vec<crate::claude::wire::ImageBlock>,
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
        request_approval,
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
    match drain_buffered(
        incoming,
        client,
        event_tx,
        tools,
        diagnostics,
        streamed,
        request_approval,
    )
    .await
    {
        Handled::Continue => {}
        Handled::ConsumerGone => return TurnEnd::ConsumerGone,
        Handled::AgentExited => return TurnEnd::AgentExited,
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
                        // An EOF mid-drain is just the agent shutting down
                        // after a settled turn; the turn itself succeeded, so
                        // only a gone consumer ends things here.
                        if let Handled::ConsumerGone = drain_buffered(
                            incoming,
                            client,
                            event_tx,
                            tools,
                            diagnostics,
                            streamed,
                            request_approval,
                        )
                        .await
                        {
                            return TurnEnd::ConsumerGone;
                        }
                        // Record the settling promptId (Grok's response
                        // carries it at `_meta.promptId`, verified live) so a
                        // late duplicate of [`PROMPT_COMPLETE_METHOD`]'s
                        // notification for THIS turn — one more push behind
                        // the reply that beat it here — can never re-settle a
                        // LATER one; see `Turn::prompt_completions`.
                        if let Some(id) = result["_meta"]["promptId"].as_str() {
                            prompt_completions.insert(id);
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
                            prompt_completions.insert(id);
                        }
                        // Same drain rationale as the reply arm above: deltas
                        // queued ahead of this notification on the wire must
                        // not be truncated by returning immediately.
                        if let Handled::ConsumerGone = drain_buffered(
                            incoming,
                            client,
                            event_tx,
                            tools,
                            diagnostics,
                            streamed,
                            request_approval,
                        )
                        .await
                        {
                            return TurnEnd::ConsumerGone;
                        }
                        // The notification carries no token counts (verified
                        // live — Grok's own usage rides the RPC response's
                        // `_meta` instead), so this is honestly `None` on the
                        // NORMAL path — this notification wins the race on
                        // every healthy turn ([`PROMPT_COMPLETE_METHOD`]'s own
                        // doc measured it, ~3ms ahead, consistently), not
                        // merely sometimes; a reader that ever gains a
                        // usage-bearing notification shape reads it here for
                        // free.
                        let mut usage = usage_reader(&params, context_window);
                        // Grok's usage lives only in the RPC reply's `_meta`,
                        // and that reply (`reply`, sent above) is already in
                        // flight — dropping it un-polled here is exactly how
                        // Grok's usage went missing on every healthy turn:
                        // the settle above always wins the race, so the
                        // `answer = &mut reply` arm that reads it never gets
                        // a turn. Poll it directly instead, bounded by
                        // [`POST_NOTIFICATION_REPLY_BOUND`] — see that
                        // const's own doc for why the bound is safe.
                        //
                        // **Short-circuited on `usage.is_some()`**: a future
                        // reader that ever gains a usage-bearing notification
                        // shape (the comment above) must return immediately
                        // rather than pay this bound's latency for nothing —
                        // `reply` is left running, not polled a second time.
                        if usage.is_none() {
                            match tokio::time::timeout(POST_NOTIFICATION_REPLY_BOUND, &mut reply)
                                .await
                            {
                                Ok(Ok(result)) => {
                                    if let Some(id) = result["_meta"]["promptId"].as_str() {
                                        prompt_completions.insert(id);
                                    }
                                    usage = usage_reader(&result, context_window);
                                }
                                // Bounded miss, not a hang: either the reply
                                // never came inside
                                // `POST_NOTIFICATION_REPLY_BOUND` (the
                                // notification-only agent this bound exists
                                // to still settle promptly for), or it came
                                // back as an RPC-level error. Either way the
                                // turn already settled correctly off the
                                // notification above — only the harvest
                                // attempt failed — but SILENT here is
                                // exactly how Grok's usage went missing
                                // before this fix, and there is no drift
                                // sheet, version floor or live suite for
                                // Grok to notice a recurrence
                                // (`docs/debt/README.md`'s D102). `debug!`,
                                // not `warn!`: a single miss is expected
                                // under real jitter, not evidence of a bug
                                // by itself — a reader grepping for repeated
                                // occurrences is the point.
                                outcome => {
                                    tracing::debug!(
                                        target: "comet_harness::acp",
                                        bound_ms = POST_NOTIFICATION_REPLY_BOUND.as_millis()
                                            as u64,
                                        timed_out = outcome.is_err(),
                                        "no usage harvested from the post-notification reply"
                                    );
                                }
                            }
                        }
                        // **Second drain, after the harvest above rather than
                        // before it.** The two drains earlier in this fn only
                        // cover what was buffered before their own settle
                        // signal; this one covers a different window — the
                        // up-to-`POST_NOTIFICATION_REPLY_BOUND` wait on
                        // `reply` just above, during which the reader task
                        // can have pushed more frames (a vendor notification
                        // racing the reply on the wire) into `incoming`
                        // before this arm returns. Left undrained, that
                        // content would surface on the NEXT turn's own
                        // leading drain instead of this turn's (D121) —
                        // display-ordering, not loss, but cheap to close.
                        // Same ConsumerGone-only handling as the drain above:
                        // an EOF here is the agent shutting down after a
                        // settled turn, not a reason to fail it.
                        if let Handled::ConsumerGone = drain_buffered(
                            incoming,
                            client,
                            event_tx,
                            tools,
                            diagnostics,
                            streamed,
                            request_approval,
                        )
                        .await
                        {
                            return TurnEnd::ConsumerGone;
                        }
                        if let Some(usage) = usage
                            && !send(event_tx, usage).await
                        {
                            return TurnEnd::ConsumerGone;
                        }
                        let reason = params["stopReason"].as_str().unwrap_or_default();
                        return TurnEnd::Settled(normalize::done_status(reason));
                    }
                    Some(message) => match handle_incoming(
                        message,
                        client,
                        event_tx,
                        tools,
                        diagnostics,
                        streamed,
                        request_approval,
                    )
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

/// Serve one `session/request_permission`.
///
/// **Spawned, not awaited here.** The message loop must keep flowing while
/// the user thinks — a blocked read at this call site would stall the very
/// transcript they are reading to decide, and `Timeouts::prompt_stall` does
/// not protect against this: that bound only watches the gap before the
/// FIRST frame of a turn, and this request always arrives well after one.
/// Thirty seconds of user thought is ordinary, not a wedge.
///
/// Returns the [`AgentEvent::Diagnostic`] to emit for an `options` set that
/// names none of the four kinds this build recognizes — a protocol-drift
/// report, answered `cancelled` on the wire immediately rather than asked
/// through the bridge at all: **guessing which of the four a vendor's own
/// vocabulary means is the difference between asking and not asking**, so a
/// request this build cannot honestly represent to the user is declined
/// without ever reaching them. `None` means either that the request was
/// well-formed and has been handed off to [`RunControls::request_approval`],
/// or that this session already reported the drift once —
/// [`normalize::session_update_once`]'s rate limit, reused here on the same
/// `diagnostics` set, so a vendor that drifts on every turn does not emit one
/// diagnostic per request for the rest of the session.
///
/// **The summary is written for what actually happened, not reused from
/// [`crate::diagnostic`].** That helper's two fixed sentences are written for
/// a dropped or unrecognized FRAME — background protocol noise the user was
/// never going to see acted on. This is different: an action the agent tried
/// to take was declined on the user's behalf, and [`AgentEvent::Diagnostic`]
/// has no separate `hint` field the way `.agents/rules/user-facing-errors.md`
/// asks for elsewhere, so the one `summary` string has to carry both the
/// cause and the effect.
fn handle_permission_request(
    client: &RpcClient,
    id: Value,
    params: &Value,
    request_approval: &Arc<RequestApprovalFn>,
    diagnostics: &mut HashSet<String>,
) -> Option<AgentEvent> {
    let options = params["options"].as_array().cloned().unwrap_or_default();
    if !approval::has_recognized_kind(&options) {
        client.respond(&id, json!({"outcome": {"outcome": "cancelled"}}));
        let discriminator =
            comet_proto::sanitize_discriminator(approval::REQUEST_PERMISSION_METHOD);
        if !diagnostics.insert(discriminator.clone()) {
            tracing::trace!(
                target: "comet_harness::acp",
                "repeated unrecognized session/request_permission options suppressed"
            );
            return None;
        }
        tracing::warn!(
            target: "comet_harness::acp",
            options = %serde_json::Value::Array(options),
            "session/request_permission offered no option kind this build recognizes"
        );
        return Some(AgentEvent::Diagnostic {
            discriminator,
            severity: DiagnosticSeverity::Unknown,
            code: None,
            summary: "The agent asked Comet to approve an action, but the choices it offered \
                      weren't ones Comet understands, so Comet declined the action rather than \
                      guess."
                .to_owned(),
        });
    }
    let request = approval::approval_request(&params["toolCall"]);
    let client = client.clone();
    let request_approval = Arc::clone(request_approval);
    tokio::spawn(async move {
        // A dropped resolver means the run ended with this approval still
        // pending — the user never answered and never will. `Expired` maps
        // to `cancelled` on the wire (`approval::outcome_for`), never to a
        // decision nobody made.
        let decision = (request_approval)(request)
            .await
            .unwrap_or(ApprovalDecision::Expired);
        client.respond(&id, approval::outcome_for(&decision, &options));
    });
    None
}

/// Serve everything already buffered, without waiting for more.
///
/// Three of [`drive_turn`]'s arms need exactly this — the pre-send backlog
/// drain and the two settle arms — and each had the loop written out in full,
/// so a change to [`handle_incoming`]'s argument list was a three-site edit
/// and two of the three were character-identical.
///
/// Returns [`Handled::Continue`] once the channel is dry. What an agent exit
/// *during* the drain means is the one thing the three sites genuinely
/// disagree on — before the send it ends the turn, after it the turn already
/// succeeded — so that stays with the caller.
async fn drain_buffered(
    incoming: &mut mpsc::Receiver<Incoming>,
    client: &RpcClient,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    tools: &mut normalize::ToolTracker,
    diagnostics: &mut HashSet<String>,
    streamed: &mut bool,
    request_approval: &Arc<RequestApprovalFn>,
) -> Handled {
    while let Ok(message) = incoming.try_recv() {
        match handle_incoming(
            message,
            client,
            event_tx,
            tools,
            diagnostics,
            streamed,
            request_approval,
        )
        .await
        {
            Handled::Continue => {}
            other => return other,
        }
    }
    Handled::Continue
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
    request_approval: &Arc<RequestApprovalFn>,
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

        Incoming::Request { id, method, params } => {
            if method == approval::REQUEST_PERMISSION_METHOD {
                if let Some(drift) =
                    handle_permission_request(client, id, &params, request_approval, diagnostics)
                    && !send(event_tx, drift).await
                {
                    return Handled::ConsumerGone;
                }
                return Handled::Continue;
            }
            // Always answer. An unanswered server->client request leaves the
            // agent waiting on a reply that never comes, which presents as a
            // hung turn with no error anywhere. Every OTHER server->client
            // method still gets -32601 here: the engine owns file access and
            // terminals (`initialize_params` already declines `fs` and
            // `terminal`), and widening the blanket for any method but the
            // one approval arm above would contradict that authority
            // argument.
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
        let params = prompt_params("s-1", "hello", Vec::new());
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

    /// Break caught (D124): a plain `HashSet` here grew one entry per turn for
    /// a session's whole life. Asserts the bound AND the direction of the
    /// eviction — the newest id must survive, because a late duplicate of a
    /// completion signal arrives just behind its own turn, so keeping the
    /// oldest ids (what `ToolTracker`'s refuse-to-insert would do) drops the
    /// one entry the stale-completion guard actually depends on.
    #[test]
    fn the_completion_guard_evicts_its_oldest_promptid_rather_than_refusing_new_ones() {
        let mut completions = PromptCompletions::default();
        let ids: Vec<String> = (0..MAX_TRACKED_PROMPT_COMPLETIONS + 10)
            .map(|i| format!("prompt-{i}"))
            .collect();
        for id in &ids {
            completions.insert(id);
        }

        assert_eq!(
            completions.order.len(),
            MAX_TRACKED_PROMPT_COMPLETIONS,
            "the guard must stay bounded across a long-lived session"
        );
        assert_eq!(completions.seen.len(), completions.order.len());

        let newest = ids.last().expect("ids is not empty");
        assert!(
            completions.contains(newest),
            "the most recent settled turn is exactly the one a late duplicate would target"
        );
        assert!(
            !completions.contains(&ids[0]),
            "the oldest entry is the one given up"
        );
    }

    /// A duplicate signal for the SAME turn must not refresh that id's place
    /// in the queue — a repeating stale notification would otherwise hold its
    /// slot forever and evict live turns behind it.
    #[test]
    fn re_recording_a_settled_promptid_does_not_move_it_back_to_the_front() {
        let mut completions = PromptCompletions::default();
        completions.insert("first");
        completions.insert("second");
        completions.insert("first");

        assert_eq!(completions.order.len(), 2, "no duplicate entry is queued");
        assert_eq!(
            completions.order.front().map(String::as_str),
            Some("first"),
            "the re-inserted id keeps its original position"
        );
    }
}
