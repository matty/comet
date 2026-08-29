//! Chat auto-titling — after the first user+assistant exchange completes on an
//! untitled chat, name it with the harness's cheapest model (port of comet's
//! `generateTitle` in `sessions.ts`).
//!
//! Flow (fire-and-forget from the run task; every failure is a silent skip with
//! tracing — a title must never fail or delay a run):
//! 1. skip when the chat already has a title (or has no workspace row);
//! 2. pick the run harness's cheapest model (small-tier name heuristic, else the
//!    last listed model — comet's `cheapestModel`);
//! 3. run a one-shot, non-streaming-collected titling prompt through the
//!    [`Harness`] trait (read-only sandbox, minimal reasoning, auto-approve),
//!    retrying on comet's short backoff ladder; fall back to the prompt's first
//!    words when every attempt produces nothing;
//! 4. re-check the title (a user rename during generation wins);
//! 5. when the chat sits in a comet worktree (`comet/<name>` branch), rename the
//!    branch from the title and update the chat's branch row;
//! 6. [`WorkspaceHost::rename_chat_auto`] in the workspace doc.
//!
//! **An ACP agent that reports its own title skips this whole run.** Grok sends
//! `sessionUpdate: "session_info_update"` carrying a title it generated itself,
//! during the turn, for free (`normalize::session_update`'s doc comment has the
//! wire evidence) — mapped to [`AgentEvent::SessionTitled`] and applied by
//! [`TitleGenerator::apply_agent_title`]. Spawning the whole flow above for such
//! a harness would be a second model call racing an answer already on the
//! wire, so [`TitleGenerator::maybe_generate_upfront`] (the request-start
//! dispatch site) skips it entirely for a harness [`harness_self_titles`]
//! lists — see that function's own doc for the policy and its limits.

use std::sync::Arc;

use futures::StreamExt;

use comet_harness::{CancellationToken, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DoneStatus, HarnessId, Model, ReasoningLevel,
    RunRequest, RuntimeMode, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

use crate::EngineError;
use crate::registry::HarnessRegistry;
use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Throwaway title runs are cheap but still cross a process boundary — retry a
/// couple of times with a short backoff before falling back (comet's ladder).
const RETRY_DELAYS_MS: &[u64] = &[250, 1_000];

struct Inner {
    workspace: WorkspaceHost,
    registry: Arc<HarnessRegistry>,
    repos: Repos,
}

#[derive(Clone)]
pub struct TitleGenerator {
    inner: Arc<Inner>,
}

impl TitleGenerator {
    pub fn new(workspace: WorkspaceHost, registry: Arc<HarnessRegistry>, repos: Repos) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                registry,
                repos,
            }),
        }
    }

    /// Fire-and-forget: title `chat_id` if it's still untitled. Called by the run
    /// task after a completed exchange; runs detached so it never delays anything.
    ///
    /// Unconditional by harness — this is the fallback dispatch, and that is
    /// exactly why it stays unconditional: see [`Self::maybe_generate_upfront`]'s
    /// doc for the request-start site this is NOT, and the policy that splits
    /// them.
    pub fn maybe_generate(&self, chat_id: &str, harness: HarnessId, prompt: &str, cwd: &str) {
        let this = self.clone();
        let chat_id = chat_id.to_string();
        let prompt = prompt.to_string();
        let cwd = cwd.to_string();
        tokio::spawn(async move {
            if let Err(err) = this.generate(&chat_id, harness, &prompt, &cwd).await {
                tracing::debug!(chat = %chat_id, error = %err, "chat auto-titling skipped");
            }
        });
    }

    /// The request-start dispatch site ("name the chat NOW, off the first
    /// prompt" — see the call site's own comment in `sessions.rs`), gated for
    /// a harness that reports its own title.
    ///
    /// For most harnesses this is identical to [`Self::maybe_generate`]. For
    /// one [`harness_self_titles`] lists, calling it here would spend a real
    /// model call racing the agent's own answer, which streams in on the SAME
    /// turn a few events later — the exact waste this task exists to cut.
    /// Skipped synchronously, before `tokio::spawn`: nothing here resolves the
    /// harness, calls its model list, or writes a fallback title. Nothing to
    /// await, nothing to race — the skip is unconditional, not "usually wins
    /// the race".
    ///
    /// **This is not the only dispatch site**, and that is deliberate: the
    /// turn-end fallback ([`Self::maybe_generate`], called from `sessions.rs`
    /// after a completed exchange) stays UNCONDITIONAL, because it is the
    /// safety net for a self-titling harness that, this turn, never actually
    /// sent one (a dropped notification, an older agent build) — without it, a
    /// harness wrongly believed to self-title would leave a chat named
    /// nothing, forever. That fallback still costs nothing when the agent DID
    /// answer: `AgentEvent::SessionTitled` is applied by
    /// [`Self::apply_agent_title`] the moment it streams in, strictly before
    /// the turn's terminal `Done` in the same ordered event loop, so by the
    /// time the fallback's own `generate` checks "does this chat already have
    /// a title", the agent's write already landed and it no-ops for free — no
    /// resolve, no model call.
    pub fn maybe_generate_upfront(
        &self,
        chat_id: &str,
        harness: HarnessId,
        prompt: &str,
        cwd: &str,
    ) {
        if harness_self_titles(harness) {
            return;
        }
        self.maybe_generate(chat_id, harness, prompt, cwd);
    }

    /// Apply a title the agent generated itself
    /// (`AgentEvent::SessionTitled`, wired from ACP's `session_info_update`
    /// via `normalize::session_update`). Fire-and-forget, same discipline as
    /// [`Self::maybe_generate`]: a titling failure must never surface to the
    /// run.
    ///
    /// Writes through [`WorkspaceHost::rename_chat_auto`], which enforces
    /// BOTH guards this needs and needs no local check of its own: never
    /// overwrite a title the user set by hand (`titleManual`), and
    /// first-writer-wins against Comet's own model-run titling — see that
    /// method's own doc for why the second guard also applies to an agent
    /// revising a title IT gave earlier, not only to the cross-system race.
    pub fn apply_agent_title(&self, chat_id: &str, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let this = self.inner.clone();
        let chat_id = chat_id.to_string();
        let title = title.to_string();
        tokio::spawn(async move {
            match this.workspace.rename_chat_auto(&chat_id, &title) {
                Ok(true) => tracing::info!(chat = %chat_id, %title, "chat named by agent"),
                Ok(false) => {}
                Err(err) => tracing::debug!(
                    chat = %chat_id, error = %err,
                    "agent-authored chat title write failed"
                ),
            }
        });
    }

    async fn generate(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Result<(), EngineError> {
        let chat = self
            .inner
            .workspace
            .doc()
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        if already_named(&chat) {
            return Ok(());
        }

        let generated = self.run_title_model(harness_id, prompt, cwd).await;
        // Fallback so a chat is always named even if the model run produced nothing.
        let fallback: String = prompt
            .split_whitespace()
            .take(7)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(48)
            .collect();
        let title = generated.unwrap_or(fallback);
        if title.is_empty() {
            return Ok(());
        }

        // Re-read after the model call: a user may have named the chat (or an
        // agent self-titled it, or checked out another branch) while the
        // throwaway generation was live.
        let latest = self.inner.workspace.doc().chat(chat_id)?.unwrap_or(chat);
        if already_named(&latest) {
            return Ok(());
        }

        // Rename the worktree branch when the chat still sits on its original
        // comet/<name> branch (guards live inside rename_worktree_branch).
        if let (Some(chat_cwd), Some(branch)) = (&latest.cwd, &latest.branch)
            && branch.starts_with("comet/")
        {
            match self
                .inner
                .repos
                .rename_worktree_branch(std::path::Path::new(chat_cwd), branch, &title)
                .await
            {
                Ok(renamed) if &renamed != branch => {
                    if let Err(err) = self.inner.workspace.set_chat_branch(chat_id, &renamed) {
                        tracing::warn!(chat = %chat_id, error = %err, "chat branch update failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "automatic worktree branch rename failed");
                }
            }
        }

        // `rename_chat_auto`, not `rename_chat`: this write is system-authored
        // (a model run Comet dispatched), not user-driven, and must not stamp
        // `titleManual` — doing so would permanently block a later
        // self-titling agent (or a future auto-title run) from ever refining
        // a title THIS function itself generated.
        self.inner.workspace.rename_chat_auto(chat_id, &title)?;
        tracing::info!(chat = %chat_id, title = %title, "chat auto-titled");
        Ok(())
    }

    /// One-shot titling run: collect TextDeltas until Done; retries on failure.
    async fn run_title_model(
        &self,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Option<String> {
        let harness = match self.inner.registry.resolve(harness_id) {
            Ok(harness) => harness,
            Err(err) => {
                tracing::debug!(error = %err, "titling harness unavailable");
                return None;
            }
        };
        let cheap = cheapest_model(
            &harness
                .models()
                .await
                .map(|catalog| catalog.models)
                .unwrap_or_default(),
        );
        let title_prompt = format!(
            "Reply with ONLY a concise 3-5 word title in Title Case (no quotes, no punctuation) \
             for a coding session that begins with this request:\n\n{prompt}"
        );
        for attempt in 0..=RETRY_DELAYS_MS.len() {
            let request = title_request(harness_id, cheap.clone(), cwd, title_prompt.clone());
            match collect_text(harness.as_ref(), request).await {
                Ok(raw) => {
                    let candidate = clean_title(&raw);
                    if !candidate.is_empty() {
                        return Some(candidate);
                    }
                }
                Err(err) => {
                    tracing::warn!(attempt = attempt + 1, error = %err,
                        "automatic chat title generation attempt failed");
                }
            }
            if let Some(delay) = RETRY_DELAYS_MS.get(attempt) {
                tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
            }
        }
        None
    }
}

/// The request that names a chat.
///
/// Set so nothing can ask it a question: this runs with no surface on which
/// an answer could be given, so an approval would hang it forever. The
/// read-only sandbox is enforced by whichever adapters honor `request.sandbox`
/// (Codex today; the Claude harness does not read it at all) — it is not a
/// guarantee this request itself makes. That pairing of a never-ask mode with
/// a read-only sandbox is not one the runtime modes express, which is why
/// this is built by hand instead of through [`RunRequest::for_session`].
fn title_request(
    harness: HarnessId,
    model: Option<String>,
    cwd: &str,
    prompt: String,
) -> RunRequest {
    RunRequest {
        prompt,
        harness: Some(harness),
        model,
        reasoning: Some(ReasoningLevel::Minimal),
        model_options: serde_json::Map::new(),
        cwd: cwd.to_string(),
        runtime_mode: RuntimeMode::FullAccess,
        sandbox: SandboxLevel::ReadOnly,
        attachments: Vec::new(),
        resume: None,
    }
}

/// True when [`Self::generate`]'s own model run should not bother: either a
/// person set the title (`title_manual` — see `Chat`'s own doc) or the chat
/// already has one from any source. A cheap, redundant pre-check —
/// `WorkspaceHost::rename_chat_auto` enforces the same rule at the write
/// itself — that exists so a titling run does not spend a model call on a
/// chat the write would refuse anyway.
fn already_named(chat: &comet_proto::Chat) -> bool {
    chat.title_manual || chat.title.as_deref().is_some_and(|t| !t.trim().is_empty())
}

/// Harnesses that report their own chat title on the wire — Grok's
/// `session_info_update`, mapped by `normalize::session_update` into
/// [`AgentEvent::SessionTitled`] (see that variant's own doc comment for the
/// captured wire evidence). [`TitleGenerator::maybe_generate_upfront`] skips
/// its whole dispatch for a harness this lists, because the agent's title
/// arrives during the turn, for free, and a model-run titling call would only
/// race it.
///
/// **Conservative default: only a harness with observed wire evidence is
/// listed.** Hermes is not — its own module doc (`acp::hermes`) already notes
/// it carries no steering extension and no effort ladder; nothing there
/// claims a self-reported title either, and Hermes cannot presently open a
/// session on this machine to check. A false positive here (a harness that
/// does NOT actually self-title, wrongly skipped) would still get named by
/// the turn-end fallback (`maybe_generate`, unconditional) the moment the
/// title never lands — no chat is ever left permanently unnamed by this
/// gate, only, in the false-positive case, named one turn later than it
/// otherwise would have been.
fn harness_self_titles(harness: HarnessId) -> bool {
    matches!(harness, HarnessId::Grok)
}

/// The cheapest model a harness offers (comet's `cheapestModel` heuristic):
/// prefer a small-tier name (haiku/mini/nano/flash/small/lite), else the last
/// listed model; `None` when the catalog is empty (harness picks its default).
fn cheapest_model(models: &[Model]) -> Option<String> {
    if models.is_empty() {
        return None;
    }
    let small = models.iter().find(|m| {
        let haystack = format!("{} {}", m.id, m.label).to_lowercase();
        ["haiku", "mini", "nano", "flash", "small", "lite"]
            .iter()
            .any(|tier| haystack.contains(tier))
    });
    small.or(models.last()).map(|m| m.id.clone())
}

/// First line, stripped of quote/heading dressing, capped at 60 chars.
fn clean_title(raw: &str) -> String {
    let first = raw.trim().lines().next().unwrap_or("");
    first
        .trim_start_matches(['"', '\'', '#', ' ', '\t'])
        .trim_end_matches(['"', '\'', ' ', '\t'])
        .chars()
        .take(60)
        .collect()
}

/// Drive one titling run through the harness: no steering, questions resolved
/// empty immediately (a titling prompt must never block on input).
async fn collect_text(
    harness: &dyn comet_harness::Harness,
    request: RunRequest,
) -> Result<String, EngineError> {
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<SteerMessage>(1);
    let controls = RunControls {
        request_input: Box::new(|_questions: Vec<UserInputQuestion>| {
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<UserInputAnswer>>();
            let _ = tx.send(Vec::new());
            rx
        }),
        // Titling runs never-ask with a read-only sandbox, so nothing here can
        // answer: the dropped sender resolves the receiver to an error, which
        // a run must treat as not approved.
        request_approval: Box::new(|_approval: ApprovalRequest| {
            let (_tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };
    let mut stream = harness.run(request, controls).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
            AgentEvent::Error { message } => {
                return Err(EngineError::Other(format!("titling run error: {message}")));
            }
            AgentEvent::Done { status, error, .. } => {
                if status == DoneStatus::Completed {
                    break;
                }
                return Err(EngineError::Other(format!(
                    "titling run ended {status:?}: {}",
                    error.unwrap_or_default()
                )));
            }
            _ => {}
        }
    }
    drop(steer_tx); // keep the mailbox open for the run's whole lifetime
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::Model;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
            accepts_images: true,
        }
    }

    #[test]
    fn cheapest_prefers_small_tier_then_last() {
        let models = vec![
            model("opus-4", "Opus"),
            model("haiku-3", "Haiku"),
            model("sonnet-4", "Sonnet"),
        ];
        assert_eq!(cheapest_model(&models).as_deref(), Some("haiku-3"));
        let no_small = vec![model("opus-4", "Opus"), model("sonnet-4", "Sonnet")];
        assert_eq!(cheapest_model(&no_small).as_deref(), Some("sonnet-4"));
        assert_eq!(cheapest_model(&[]), None);
    }

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(clean_title("\"Fix Login Flow\"\nextra"), "Fix Login Flow");
        assert_eq!(clean_title("# Add Dark Mode  "), "Add Dark Mode");
        assert_eq!(clean_title("   "), "");
    }

    #[test]
    fn titling_runs_read_only_and_never_asks() {
        // Titling has no surface on which an approval could be answered, so it
        // must never be able to receive one — and it has no business writing.
        // No RuntimeMode pairs those two, which is why this site builds its own
        // request instead of going through the session constructor.
        let request = title_request(
            HarnessId::Mock,
            Some("cheap-model".into()),
            "/tmp/repo",
            "name this chat".into(),
        );
        assert_eq!(request.sandbox, SandboxLevel::ReadOnly);
        assert_eq!(request.runtime_mode, RuntimeMode::FullAccess);
    }

    #[test]
    fn harness_self_titles_lists_only_grok() {
        assert!(harness_self_titles(HarnessId::Grok));
        for other in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Cursor,
            HarnessId::Hermes,
            HarnessId::Mock,
        ] {
            assert!(!harness_self_titles(other), "{other:?} must not self-title");
        }
    }

    /// A harness whose `models()` call counts every invocation — the observable
    /// proxy for "a titling run was dispatched". `id` is a field, not fixed by
    /// the impl, so the SAME spy can stand in for a self-titling harness
    /// (registered under `HarnessId::Grok`) and a normal one (registered under
    /// `HarnessId::Mock`) in one registry, each wired to its own counter.
    struct CountingHarness {
        id: HarnessId,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for CountingHarness {
        fn id(&self) -> HarnessId {
            self.id
        }
        fn display_name(&self) -> &str {
            "Counting"
        }
        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }
        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(comet_proto::ModelCatalog::built_in(vec![model(
                "counting-1",
                "Counting 1",
            )]))
        }
        async fn run(
            &self,
            _request: RunRequest,
            _controls: RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, comet_harness::HarnessError>>,
            comet_harness::HarnessError,
        > {
            let events = vec![
                Ok(AgentEvent::TextDelta {
                    text: "Counted Title".into(),
                }),
                Ok(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                }),
            ];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    /// Question 3's pin: for a harness that reports its own title, the
    /// request-start dispatch site never spends a model call at all — proven
    /// by a harness whose `models()` call increments a counter this test can
    /// read, not by "the chat ended up named" (a fallback write, or the
    /// agent's own event, would name it too and prove nothing about whether a
    /// run was dispatched).
    #[tokio::test]
    async fn maybe_generate_upfront_dispatches_no_run_for_a_self_titling_harness() {
        let dir = tempfile::tempdir().unwrap();
        let grok_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(CountingHarness {
            id: HarnessId::Grok,
            calls: grok_calls.clone(),
        }));
        registry.register(Arc::new(CountingHarness {
            id: HarnessId::Mock,
            calls: mock_calls.clone(),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                &core.device_id,
                dir.path().to_str().unwrap(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-grok", "space-1", None, None)
            .unwrap();
        core.workspace
            .create_chat("chat-mock", "space-1", None, None)
            .unwrap();

        let titles =
            TitleGenerator::new(core.workspace.clone(), registry.clone(), core.repos.clone());

        // Self-titling harness: the dispatch must be a synchronous no-op —
        // nothing spawned, so nothing left to race. A couple of yields flush
        // any task that WOULD have been spawned if the gate were broken.
        titles.maybe_generate_upfront(
            "chat-grok",
            HarnessId::Grok,
            "name this chat",
            dir.path().to_str().unwrap(),
        );
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            grok_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a self-titling harness must never have its model list resolved by the upfront dispatch"
        );
        assert!(
            core.workspace
                .doc()
                .chat("chat-grok")
                .unwrap()
                .unwrap()
                .title
                .is_none(),
            "no run means no fallback title either, at this dispatch site"
        );

        // Control: the SAME entrypoint, for a harness NOT listed as
        // self-titling, does dispatch — proving the negative result above is
        // the gate, not a broken dispatch mechanism.
        titles.maybe_generate_upfront(
            "chat-mock",
            HarnessId::Mock,
            "name this chat",
            dir.path().to_str().unwrap(),
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while mock_calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "control dispatch never ran"
            );
            tokio::task::yield_now().await;
        }
        assert!(mock_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);

        core.shutdown().await;
    }

    /// End-to-end wiring for `AgentEvent::SessionTitled`'s handler
    /// (`apply_agent_title`): a fresh chat gets named, trimmed; a
    /// hand-renamed chat keeps the user's title. The doc-layer guard itself
    /// is `rename_chat_auto`'s own tests in `comet_doc::workspace` — this
    /// proves the fire-and-forget spawn actually reaches the doc.
    #[tokio::test]
    async fn apply_agent_title_writes_through_and_respects_the_manual_lock() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(comet_harness::mock::MockHarness::new()));
        let core = crate::EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                &core.device_id,
                dir.path().to_str().unwrap(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        let titles =
            TitleGenerator::new(core.workspace.clone(), registry.clone(), core.repos.clone());

        titles.apply_agent_title("chat-1", "  Fix Login Flow  ");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if core
                .workspace
                .doc()
                .chat("chat-1")
                .unwrap()
                .unwrap()
                .title
                .is_some()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "agent title never landed"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(
            core.workspace
                .doc()
                .chat("chat-1")
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("Fix Login Flow"),
            "the title is trimmed on the way in"
        );

        // A person renames it by hand afterward — a later agent title event
        // (Grok re-pushing a revision) must not land over it.
        core.workspace.rename_chat("chat-1", "User Title").unwrap();
        titles.apply_agent_title("chat-1", "Agent Retitle");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            core.workspace
                .doc()
                .chat("chat-1")
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("User Title"),
            "a hand-renamed chat must keep the user's title"
        );

        core.shutdown().await;
    }
}
