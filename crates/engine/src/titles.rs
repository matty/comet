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
//! 5. write the title with [`WorkspaceHost::rename_chat_auto`] in the workspace
//!    doc;
//! 6. only once that write lands, and when the chat sits in a comet
//!    worktree (`comet/<name>` branch), rename the branch from the title and
//!    update the chat's branch row.
//!
//! **An ACP agent that reports its own title skips this whole run.** Grok sends
//! `sessionUpdate: "session_info_update"` carrying a title it generated itself,
//! during the turn, for free (`normalize::session_update`'s doc comment has the
//! wire evidence) — mapped to [`AgentEvent::SessionTitled`] and applied by
//! [`TitleGenerator::apply_agent_title`]. Spawning the whole flow above for such
//! a harness would be a second model call racing an answer already on the
//! wire, so [`TitleGenerator::maybe_generate_upfront`] (the request-start
//! dispatch site) skips it entirely for a harness that declares
//! [`harness_self_titles`] — see that function's own doc for the policy and
//! its limits.

use std::sync::Arc;

use futures::StreamExt;

use comet_harness::{CancellationToken, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DoneStatus, HarnessId, Model, ReasoningLevel,
    RunRequest, RuntimeMode, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

use crate::EngineError;
use crate::doc_host::{ChatDocHandle, DocHost};
use crate::registry::HarnessRegistry;
use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Throwaway title runs are cheap but still cross a process boundary — retry a
/// couple of times with a short backoff before falling back (comet's ladder).
const RETRY_DELAYS_MS: &[u64] = &[250, 1_000];

struct Inner {
    workspace: WorkspaceHost,
    doc_host: DocHost,
    registry: Arc<HarnessRegistry>,
    repos: Repos,
}

impl Inner {
    /// Rename the worktree branch to match `title` when the chat still sits
    /// on its original `comet/<name>` branch (guards live inside
    /// `rename_worktree_branch`). Shared by `TitleGenerator::generate`
    /// (Comet's own model-run titling) and `TitleGenerator::apply_agent_title`
    /// (an ACP agent's self-reported title) — before this method existed,
    /// only the model-run path did this, so a Grok chat in a comet worktree
    /// kept its generated branch name forever while every other harness got
    /// it renamed. Best-effort and non-fatal either way: a failed branch
    /// rename must never be mistaken for a failed title.
    ///
    /// **Both callers call this only after their own title write has
    /// landed, never before (D116).** Renaming the branch first and only
    /// then attempting the write would leave the branch renamed for a title
    /// that then failed to write or lost its first-writer-wins race (a
    /// last-moment manual rename, or another writer's title landing first) —
    /// `generate` used to do exactly that; it predates `apply_agent_title`,
    /// and nothing forced the two orderings to agree until this fix.
    async fn rename_worktree_branch_for_title(
        &self,
        generation: &Arc<ChatDocHandle>,
        chat: &comet_proto::Chat,
        title: &str,
    ) {
        if !self.doc_host.is_current_handle(generation) {
            return;
        }
        let chat_id = generation.chat_id();
        let (Some(chat_cwd), Some(branch)) = (&chat.cwd, &chat.branch) else {
            return;
        };
        if !branch.starts_with("comet/") {
            return;
        }
        match self
            .repos
            .rename_worktree_branch(std::path::Path::new(chat_cwd), branch, title)
            .await
        {
            Ok(renamed) if &renamed != branch => {
                let Some(updated) = self.doc_host.with_current_handle(generation, || {
                    self.workspace.set_chat_branch(chat_id, &renamed)
                }) else {
                    return;
                };
                if let Err(err) = updated {
                    tracing::warn!(chat = %chat_id, error = %err, "chat branch update failed");
                }
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "automatic worktree branch rename failed");
            }
        }
    }
}

#[derive(Clone)]
pub struct TitleGenerator {
    inner: Arc<Inner>,
}

impl TitleGenerator {
    pub fn new(
        workspace: WorkspaceHost,
        doc_host: DocHost,
        registry: Arc<HarnessRegistry>,
        repos: Repos,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                doc_host,
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
    pub fn maybe_generate(
        &self,
        generation: Arc<ChatDocHandle>,
        harness: HarnessId,
        prompt: &str,
        cwd: &str,
    ) {
        let this = self.clone();
        let prompt = prompt.to_string();
        let cwd = cwd.to_string();
        tokio::spawn(async move {
            if let Err(err) = this.generate(&generation, harness, &prompt, &cwd).await {
                tracing::debug!(chat = %generation.chat_id(), error = %err, "chat auto-titling skipped");
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
    /// answer, but only because [`Self::apply_agent_title`]'s title write is
    /// SYNCHRONOUS, called inline from `drive_run`'s event loop rather than
    /// spawned — see that method's own doc for why. `maybe_generate`, by
    /// contrast, spawns its own task; two spawned tasks would have no
    /// ordering relation to each other, and the fallback could still lose the
    /// race and spend a real model call even though the write itself is
    /// always safe (first-writer-wins refuses whichever side loses).
    pub fn maybe_generate_upfront(
        &self,
        generation: Arc<ChatDocHandle>,
        harness: HarnessId,
        prompt: &str,
        cwd: &str,
    ) {
        if harness_self_titles(&self.inner.registry, harness) {
            return;
        }
        self.maybe_generate(generation, harness, prompt, cwd);
    }

    /// Apply a title the agent generated itself
    /// (`AgentEvent::SessionTitled`, wired from ACP's `session_info_update`
    /// via `normalize::session_update`). Called INLINE from `drive_run`'s
    /// event loop (`sessions.rs`), not spawned — same discipline
    /// `finish_segment`'s doc write already follows in that same loop.
    ///
    /// **Why synchronous is load-bearing, not just tidy.** The turn-end
    /// fallback ([`Self::maybe_generate_upfront`]'s doc explains why it stays
    /// unconditional) reads "does this chat already have a title" before
    /// spending a model call. If this write were spawned, that read would
    /// race a task with no ordering guarantee relative to it — the fallback
    /// could read the row BEFORE this write lands, dispatch a real Grok run
    /// (the exact cost this task exists to cut, reintroduced probabilistically),
    /// and lose nothing but money and time, because `rename_chat_auto`'s
    /// first-writer-wins guard still refuses whichever write arrives second.
    /// Cost, not correctness — but a comment claiming the ordering is
    /// guaranteed has to make it actually guaranteed. Calling this inline in
    /// the same sequential event loop that later dispatches the fallback
    /// (`AgentEvent::SessionTitled` is always processed, and this call
    /// completes, before the turn's terminal `Done` reaches its own handling
    /// a few loop iterations later) is what makes it true.
    ///
    /// The title write itself ([`WorkspaceHost::rename_chat_auto`], which
    /// enforces both the manual-rename lock and first-writer-wins — see that
    /// method's own doc) is cheap: an in-process CRDT commit, not IO. It is
    /// blocking the caller only in the sense any doc write in `drive_run`
    /// already is. **The worktree branch rename that follows a successful
    /// title write is NOT similarly cheap** — it shells out to git — so it
    /// stays a background task, deliberately: it must not stall the live
    /// event stream this method is called from, and its own timing has no
    /// bearing on the race described above (nothing reads the branch name to
    /// decide whether a titling run would be wasted).
    pub fn apply_agent_title(&self, generation: Arc<ChatDocHandle>, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let chat_id = generation.chat_id();
        // Read first (for the branch-rename step below, which needs cwd and
        // the CURRENT branch): rename_chat_auto does not change either field,
        // so a value read just before the write is never stale for that.
        let chat = match self.inner.workspace.doc().chat(chat_id) {
            Ok(Some(chat)) => chat,
            Ok(None) => return,
            Err(err) => {
                tracing::debug!(chat = %chat_id, error = %err, "agent-authored chat title read failed");
                return;
            }
        };
        let Some(title_write) = self.inner.doc_host.with_current_handle(&generation, || {
            self.inner.workspace.rename_chat_auto(chat_id, title)
        }) else {
            return;
        };
        let title_landed = match title_write {
            Ok(true) => {
                tracing::info!(chat = %chat_id, %title, "chat named by agent");
                true
            }
            Ok(false) => false,
            Err(err) => {
                tracing::debug!(chat = %chat_id, error = %err, "agent-authored chat title write failed");
                false
            }
        };
        if !title_landed {
            return;
        }
        let this = self.inner.clone();
        let title = title.to_string();
        tokio::spawn(async move {
            this.rename_worktree_branch_for_title(&generation, &chat, &title)
                .await;
        });
    }

    async fn generate(
        &self,
        generation: &Arc<ChatDocHandle>,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Result<(), EngineError> {
        let chat_id = generation.chat_id();
        let Some(chat_result) = self
            .inner
            .doc_host
            .with_current_handle(generation, || self.inner.workspace.doc().chat(chat_id))
        else {
            return Ok(());
        };
        let chat =
            chat_result?.ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        if already_named(&chat) {
            return Ok(());
        }

        let generated = self
            .run_title_model(generation, harness_id, prompt, cwd)
            .await;
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
        let Some(latest_result) = self
            .inner
            .doc_host
            .with_current_handle(generation, || self.inner.workspace.doc().chat(chat_id))
        else {
            return Ok(());
        };
        let latest = latest_result?.unwrap_or(chat);
        if already_named(&latest) {
            return Ok(());
        }

        // `rename_chat_auto`, not `rename_chat`: this write is system-authored
        // (a model run Comet dispatched), not user-driven, and must not stamp
        // `titleManual` — doing so would permanently block a later
        // self-titling agent (or a future auto-title run) from ever refining
        // a title THIS function itself generated.
        //
        // Write BEFORE renaming the worktree branch (D116), matching
        // `apply_agent_title` — see `rename_worktree_branch_for_title`'s own
        // doc for why the two orderings now agree: a write that loses the
        // first-writer-wins race between the re-read above and this write
        // must never leave the branch renamed for a title the chat never
        // shows.
        let Some(title_write) = self.inner.doc_host.with_current_handle(generation, || {
            self.inner.workspace.rename_chat_auto(chat_id, &title)
        }) else {
            return Ok(());
        };
        if !title_write? {
            return Ok(());
        }
        tracing::info!(chat = %chat_id, title = %title, "chat auto-titled");

        // Rename the worktree branch when the chat still sits on its original
        // comet/<name> branch, now that the write is confirmed landed.
        // Shared with `apply_agent_title`, which needs the identical
        // behavior for a self-titling agent's title — see
        // `rename_worktree_branch_for_title`'s own doc.
        self.inner
            .rename_worktree_branch_for_title(generation, &latest, &title)
            .await;
        Ok(())
    }

    /// One-shot titling run: collect TextDeltas until Done; retries on failure.
    async fn run_title_model(
        &self,
        generation: &Arc<ChatDocHandle>,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Option<String> {
        if !self.inner.doc_host.is_current_handle(generation) {
            return None;
        }
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
        if !self.inner.doc_host.is_current_handle(generation) {
            return None;
        }
        let title_prompt = format!(
            "Reply with ONLY a concise 3-5 word title in Title Case (no quotes, no punctuation) \
             for a coding session that begins with this request:\n\n{prompt}"
        );
        for attempt in 0..=RETRY_DELAYS_MS.len() {
            if !self.inner.doc_host.is_current_handle(generation) {
                return None;
            }
            let request = title_request(harness_id, cheap.clone(), cwd, title_prompt.clone());
            match collect_text(harness.as_ref(), request).await {
                Ok(raw) => {
                    if !self.inner.doc_host.is_current_handle(generation) {
                        return None;
                    }
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

/// Whether `harness` reports its own chat title on the wire — Grok's
/// `session_info_update`, mapped by `normalize::session_update` into
/// [`AgentEvent::SessionTitled`] (see that variant's own doc comment for the
/// captured wire evidence). [`TitleGenerator::maybe_generate_upfront`] skips
/// its whole dispatch when this is true, because the agent's title arrives
/// during the turn, for free, and a model-run titling call would only race it.
///
/// **Read off [`comet_proto::HarnessCapabilities::self_titles`], not a list
/// kept here.** Which frames an agent puts on the wire is a fact about that
/// agent, declared beside its `carries_deny_note` in the harness crate. An
/// enumeration in the engine is silently incomplete the day a second
/// self-titling agent is registered, and nothing fails when it is — the
/// turn-end fallback names the chat anyway, one wasted model call later.
///
/// An unregistered harness answers `false`: the conservative direction here is
/// "run the titling call", since a wrong skip costs a turn's delay and a wrong
/// run costs one cheap model call.
fn harness_self_titles(registry: &HarnessRegistry, harness: HarnessId) -> bool {
    registry
        .capabilities(harness)
        .is_some_and(|caps| caps.self_titles)
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
            deprecation: None,
            reasoning_levels: vec![],
            default_reasoning: None,
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

    /// Break caught: a harness declaring `self_titles` that has no observed
    /// wire evidence for it, or Grok losing the declaration and paying for a
    /// titling model call it does not need. Read through the real registry
    /// rather than a list here, so the answer is the harness's own
    /// `capabilities()` and cannot drift from it.
    #[test]
    fn only_grok_declares_that_it_titles_its_own_chats() {
        let registry = crate::registry::default_registry();
        assert!(harness_self_titles(&registry, HarnessId::Grok));
        for other in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Cursor,
            HarnessId::Hermes,
            HarnessId::Mock,
        ] {
            assert!(
                !harness_self_titles(&registry, other),
                "{other:?} must not self-title"
            );
        }
    }

    /// A harness whose `models()` call counts every invocation — the observable
    /// proxy for "a titling run was dispatched". `id` and `self_titles` are
    /// both fields, so the SAME spy can stand in for a self-titling harness
    /// and a normal one in one registry, each wired to its own counter.
    ///
    /// `self_titles` is declared here rather than inferred from `id` on
    /// purpose: the gate reads the capability, so a spy that claimed the
    /// capability by virtue of its id would pass whichever way the gate was
    /// wired.
    struct CountingHarness {
        id: HarnessId,
        self_titles: bool,
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
            comet_proto::HarnessCapabilities {
                self_titles: self.self_titles,
                ..Default::default()
            }
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
            self_titles: true,
            calls: grok_calls.clone(),
        }));
        registry.register(Arc::new(CountingHarness {
            id: HarnessId::Mock,
            self_titles: false,
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

        let titles = TitleGenerator::new(
            core.workspace.clone(),
            core.doc_host.clone(),
            registry.clone(),
            core.repos.clone(),
        );
        let grok_generation = core.doc_host.open("chat-grok").unwrap();
        let mock_generation = core.doc_host.open("chat-mock").unwrap();

        // Self-titling harness: the dispatch must be a synchronous no-op —
        // nothing spawned, so nothing left to race. A couple of yields flush
        // any task that WOULD have been spawned if the gate were broken.
        titles.maybe_generate_upfront(
            grok_generation,
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
            mock_generation,
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
        let titles = TitleGenerator::new(
            core.workspace.clone(),
            core.doc_host.clone(),
            registry.clone(),
            core.repos.clone(),
        );
        let generation = core.doc_host.open("chat-1").unwrap();

        titles.apply_agent_title(generation.clone(), "  Fix Login Flow  ");
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
        titles.apply_agent_title(generation, "Agent Retitle");
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
