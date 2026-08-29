//! Mock harness for engine/UI tests: replays a scripted event sequence.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DoneStatus, FileOperation, HarnessCapabilities,
    HarnessId, Model, ModelCatalog, ReasoningLevel, RunRequest, SteeringMode, SubagentStatus,
    ToolCall, UserInputQuestion,
};

use crate::discovery::{Discovery, DiscoveryCache, DiscoveryFailure};
use crate::{Harness, HarnessError, RunControls};

#[derive(Default)]
pub struct MockHarness {
    pub script: Vec<AgentEvent>,
    /// A scripted discovery answer. The outer `None` means the mock behaves
    /// as it always has (built-in list only); a scripted `Err` exercises the
    /// negative-caching and `Diagnostic` paths without a CLI on the machine.
    scripted_discovery: Option<Result<Discovery, DiscoveryFailure>>,
    discovery_cache: DiscoveryCache,
}

impl MockHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// A scripted event sequence with no discovery answer — the shape every
    /// pre-existing test fixture wants, exposed as a constructor because
    /// `script` is the only field a caller outside this module may set (the
    /// other two are private, so `MockHarness { script, ..Self::new() }`
    /// cannot be written from another crate).
    pub fn with_script(script: Vec<AgentEvent>) -> Self {
        Self {
            script,
            ..Self::new()
        }
    }

    pub fn with_discovery(discovery: Discovery) -> Self {
        Self {
            scripted_discovery: Some(Ok(discovery)),
            ..Self::new()
        }
    }

    pub fn with_failing_discovery(failure: DiscoveryFailure) -> Self {
        Self {
            scripted_discovery: Some(Err(failure)),
            ..Self::new()
        }
    }

    /// The curated model list both the unscripted and scripted arms of
    /// `models()` name — kept in one place so a change to one can't drift
    /// from the other. `mock-1` and `mock-fable-5` are used by scripted UI
    /// runs that expect those exact ids and labels; do not change them.
    fn curated_models() -> Vec<Model> {
        vec![
            Model {
                id: "mock-1".into(),
                label: "Mock 1".into(),
                description: None,
                reasoning_levels: vec![ReasoningLevel::Medium],
                options: vec![],
                accepts_images: true,
            },
            // Claude-mirroring demo model: lets scripted runs carry the same
            // chip labels ("Fable 5 · High") as a real Claude session.
            Model {
                id: "mock-fable-5".into(),
                label: "Fable 5".into(),
                description: None,
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
                options: vec![],
                accepts_images: true,
            },
        ]
    }

    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
            runtime_modes: Vec::new(),
            // The mock scripts Claude-flavoured runs, so the approval surface
            // it drives for a rendered check has to read as Claude's does.
            carries_deny_note: true,
            // The mock scripts a Claude-flavoured run, which titles nothing
            // itself — a rendered check should exercise Comet's own titling.
            self_titles: false,
        }
    }
}

/// The scripted question set for the `COMET_MOCK_QUESTION` variant (exercises
/// the QuestionPanel end-to-end: single-select page, multi-select page).
fn question_script() -> Vec<UserInputQuestion> {
    vec![
        UserInputQuestion {
            id: "q-sync".into(),
            header: "Question".into(),
            question: "Which sync strategy should the rewrite use?".into(),
            options: vec![
                "Poll the doc host every 120ms".into(),
                "Event-driven fold with coalesced commits".into(),
                "Hybrid: event-driven with a polling fallback".into(),
            ],
            multi_select: false,
        },
        UserInputQuestion {
            id: "q-gates".into(),
            header: "Question".into(),
            question: "Which suites should gate the merge?".into(),
            options: vec![
                "Unit tests".into(),
                "End-to-end (two-device)".into(),
                "Golden screenshots".into(),
            ],
            multi_select: true,
        },
    ]
}

/// The approval a `COMET_MOCK_APPROVAL` run asks for. The value names the
/// KIND, so every card shape can be put on screen without a code edit —
/// `1` keeps meaning the file-change run 1.4 shipped. An unrecognized value
/// falls back to that same run rather than silently disabling the knob: a
/// typo should still show a card, not nothing.
fn mock_approval(value: &str) -> Option<ApprovalRequest> {
    Some(match value {
        "" | "0" => return None,
        "command" => ApprovalRequest::Command {
            command: "pwsh -NoProfile -Command \"Get-ChildItem -Recurse crates | Measure-Object\""
                .into(),
            cwd: Some("C:/dev/comet".into()),
        },
        "file-read" => ApprovalRequest::FileRead {
            path: "crates/engine/src/sessions.rs".into(),
        },
        "mcp" => ApprovalRequest::Mcp {
            server: "linear".into(),
            tool: "create_issue".into(),
        },
        "unknown" => ApprovalRequest::Unknown {
            summary: "an action Comet does not model".into(),
        },
        _ => ApprovalRequest::FileChange {
            path: "src/reconcile.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 24,
            removed_lines: 6,
        },
    })
}

#[async_trait]
impl Harness for MockHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        Self::capabilities()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        let curated = Self::curated_models();
        let Some(scripted) = self.scripted_discovery.clone() else {
            return Ok(ModelCatalog::built_in(curated));
        };
        let discovery = self.discovery_cache.get(|| async move { scripted }).await;
        Ok(self.discovery_cache.catalog(curated, discovery))
    }
    /// Without this the mock holds a scripted failure for the whole boot and
    /// `ListModels { force: true }` returns it again — so the mock could not
    /// exercise the Retry path it exists to prove.
    fn clear_discovery(&self) {
        self.discovery_cache.clear();
    }
    /// Only the mock implements this in this slice: the two real adapters
    /// keep the trait's defaulted `None` until 2.2/2.3 give them a cache.
    fn take_unreported_discovery_failure(&self) -> Option<DiscoveryFailure> {
        self.discovery_cache.take_unreported_failure()
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // Optional pacing knob for demos/manual testing: `COMET_MOCK_DELAY_MS`
        // spaces the scripted events out so live-run UI states (working
        // indicator, streaming fade, trailing tool-group auto-open) are
        // observable. Unset (the default, and in tests) streams instantly.
        let delay_ms = std::env::var("COMET_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let delay = std::time::Duration::from_millis(delay_ms);

        // Dev/testing knob: `COMET_MOCK_QUESTION=1` swaps in a run that asks
        // the user questions mid-stream via `controls.request_input` (the
        // engine mints the request id, emits `InputRequested`, and resolves it
        // from the `RespondInput` doc command) — the only data-side way to put
        // the QuestionPanel on screen.
        let question_mode = std::env::var("COMET_MOCK_QUESTION")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        if question_mode {
            let request_input = controls.request_input;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            tokio::spawn(async move {
                let pause = if delay_ms == 0 {
                    std::time::Duration::from_millis(50)
                } else {
                    delay
                };
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text:
                        "Before I wire the reconciliation path I need two decisions from you.\n\n"
                            .into(),
                });
                tokio::time::sleep(pause).await;
                let answers = request_input(question_script()).await.unwrap_or_default();
                let picked: Vec<String> = answers
                    .iter()
                    .flat_map(|a| a.labels.iter().cloned())
                    .collect();
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text: format!(
                        "Locked in: **{}**. Proceeding with the plan.",
                        if picked.is_empty() {
                            "your defaults".to_string()
                        } else {
                            picked.join("**, **")
                        }
                    ),
                });
                let _ = tx.send(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                });
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }

        // Dev/testing knob: `COMET_MOCK_APPROVAL=<kind>` swaps in a run that
        // asks permission mid-stream via `controls.request_approval` (the host
        // mints the request id, emits `ApprovalRequested`, and resolves it from
        // the queued respond-approval command) — the only data-side way to put
        // an approval card on screen. `<kind>` selects the shape (`command`,
        // `file-change`, `file-read`, `mcp`, `unknown`); `1` keeps meaning the
        // file-change run — see `mock_approval` below.
        let approval = std::env::var("COMET_MOCK_APPROVAL")
            .ok()
            .and_then(|v| mock_approval(&v));
        if let Some(approval) = approval {
            let request_approval = controls.request_approval;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            tokio::spawn(async move {
                let pause = if delay_ms == 0 {
                    std::time::Duration::from_millis(50)
                } else {
                    delay
                };
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text: "I need to edit the reconciliation module before I continue.\n\n".into(),
                });
                tokio::time::sleep(pause).await;
                let decision = request_approval(approval).await;
                tokio::time::sleep(pause).await;
                let closing = match decision {
                    Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => {
                        "Applied the edit."
                    }
                    Ok(ApprovalDecision::Deny { .. }) => "Left the file untouched.",
                    // The run outlived its decision channel: expired, or the
                    // resolver was dropped with the run.
                    Ok(ApprovalDecision::Expired) | Err(_) => "Stopped without the edit.",
                };
                let _ = tx.send(AgentEvent::TextDelta {
                    text: closing.into(),
                });
                let _ = tx.send(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                });
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }

        // Dev/testing knob: `COMET_MOCK_HANG=1` emits a tool call and then
        // NOTHING — no result, no Done. A tool call that never returns has no
        // other data-side producer: every fake in this repo answers, which is
        // why the state has only ever been seen against a live provider.
        let hang_mode = std::env::var("COMET_MOCK_HANG")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        if hang_mode {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            tokio::spawn(async move {
                let pause = if delay_ms == 0 {
                    std::time::Duration::from_millis(50)
                } else {
                    delay
                };
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text: "Counting the crates before I continue.\n\n".into(),
                });
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::ToolCall {
                    id: "mock-hang".into(),
                    call: ToolCall::Exec {
                        command:
                            "pwsh -NoProfile -Command \"Get-ChildItem -Recurse crates | Measure-Object\""
                                .into(),
                    },
                });
                // Hold the sender — and therefore the stream — open forever.
                // Dropping it would close the stream and end the turn, which is
                // the opposite of the state being reproduced.
                std::future::pending::<()>().await;
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }

        // Dev/testing knob: `COMET_MOCK_REPEAT=N` loops the script body N times
        // before the final Done — long single-reply streams for frame-cost /
        // smoothness measurement (the terminal `Done` is emitted exactly once,
        // at the very end).
        let repeat = std::env::var("COMET_MOCK_REPEAT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        // Dev/testing knob: `COMET_MOCK_ERROR=1` appends a scripted error
        // before the terminal Done — the only data-side way to put the
        // transcript ErrorChip on screen with the mock harness.
        let mock_error = std::env::var("COMET_MOCK_ERROR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        // Dev/testing knob: `COMET_MOCK_TABLE=1` appends scripted GFM tables
        // before the terminal Done — a plain 3-column grid plus a wide/uneven
        // one (long prose cell beside short cells, mixed alignment) for
        // table-styling checks against the reference app.
        let mock_table = std::env::var("COMET_MOCK_TABLE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let done_ix = self
            .script
            .iter()
            .position(|e| matches!(e, AgentEvent::Done { .. }))
            .unwrap_or(self.script.len());
        let (body, tail) = self.script.split_at(done_ix);
        let error_event = mock_error.then(|| AgentEvent::Error {
            message: "Claude usage limit reached — try again after the limit resets.".into(),
        });
        // Dev/testing knob: `COMET_MOCK_CONTEXT=<percent>` emits a context
        // reading before the terminal Done, which is the only way to put the
        // composer's context gauge on screen without driving a real CLI and
        // filling a real window. `0` suppresses it, which is also what a
        // provider that publishes no window looks like.
        let context_event = std::env::var("COMET_MOCK_CONTEXT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|percent| *percent > 0)
            .map(|percent| {
                const WINDOW: u64 = 200_000;
                AgentEvent::Usage {
                    prompt_tokens: WINDOW * percent.min(100) / 100,
                    output_tokens: 128,
                    context_window: Some(WINDOW),
                }
            });
        // Dev/testing knob: `COMET_MOCK_CODE=1` appends rust + ts code blocks
        // (keywords, strings, numbers, comments) plus inline code — for
        // syntax-palette and inline-code styling checks against the reference.
        let mock_code = std::env::var("COMET_MOCK_CODE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let code_event = mock_code.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Code check\n\n",
                "The `fold_event_into_parts` helper feeds `writer.sync` on a `120ms` cadence:\n\n",
                "```rust\n",
                "// Fold one event into the accumulated parts.\n",
                "pub fn fold(mut acc: Vec<Part>, event: &AgentEvent) -> Vec<Part> {\n",
                "    let label = \"delta\";\n",
                "    if acc.len() > 128 {\n",
                "        acc.truncate(64); // keep the tail hot\n",
                "    }\n",
                "    acc\n",
                "}\n",
                "```\n\n",
                "```ts\n",
                "// Subscribe and fold on the client.\n",
                "const room = await connect(\"wss://mesh.local\", { retries: 3 });\n",
                "export function fold(parts: Part[], event: AgentEvent): Part[] {\n",
                "    return event.kind === \"delta\" ? [...parts, event] : parts;\n",
                "}\n",
                "```\n\n",
            )
            .into(),
        });
        let table_event = mock_table.then(|| AgentEvent::TextDelta {
            text: "\n### Table check\n\n\
                | Column A | Column B | Column C |\n\
                |---|---|---|\n\
                | a1 | b1 | c1 |\n\
                | a2 | b2 | c2 |\n\n\
                And a wide, uneven one:\n\n\
                | Stage | What happens | p95 |\n\
                |:--|:--|--:|\n\
                | Fold | Events fold into parts and diff into the Loro doc on a 120ms coalesced commit cadence, keeping the oplog RLE-merged across devices | 4.2ms |\n\
                | Sync | Session-room fan-out | 18ms |\n\n"
                .into(),
        });
        // Dev/testing knob: `COMET_MOCK_MEND=1` appends a link/list-heavy
        // passage — bold-led list items, inline links, emphasis, strikethrough
        // — the shapes whose half-streamed markers the display mend
        // (crates/ui markdown/mend.rs) must hold steady while streaming.
        let mock_mend = std::env::var("COMET_MOCK_MEND")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let mend_event = mock_mend.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Streaming mend check\n\n",
                "Inline styles hold while text arrives: **bold stays bold**, ",
                "*italic stays italic*, `code stays code`, and ~~this stays struck~~.\n\n",
                "- **Fold** — parts diff into the [Loro doc](https://loro.dev) on a 120ms cadence\n",
                "- **Relay** — commits fan out through the [session room](https://developers.cloudflare.com/durable-objects/) to every device\n",
                "- **Paint** — the [display tree](https://github.com/pulldown-cmark/pulldown-cmark) mends hanging markers in the last block only\n\n",
                "Links above never flash their URLs, and closing markers never reflow the paragraph.\n",
            )
            .into(),
        });
        // Dev/testing knob: `COMET_MOCK_CHECKLIST=1` publishes a plan, which is
        // the only way to put slice 4.4's checklist surface on screen without
        // driving a real CLI through a multi-step task.
        //
        // Shaped from the recorded run at
        // `crates/harness/tests/corpus/claude/2.1.229/checklist{,-resume}`, not
        // composed: three items created pending, the first driven all the way
        // through, the second left mid-flight, and a fourth that arrives as a
        // bare status change carrying only an `activeForm`.
        //
        // Two states here exist because a happy-path fixture never produces
        // them and both are the interesting ones to draw. **An item stuck in
        // `InProgress`** is the state the whole surface exists to show — a
        // fixture that runs every item to `Completed` never renders it. **An
        // item with no `text` at all** is what a resumed run produces for a
        // task the previous process created (capture §7); a card that assumes
        // every row has a subject will render it as a blank line.
        let mock_checklist = std::env::var("COMET_MOCK_CHECKLIST")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let checklist_events = mock_checklist
            .then(|| {
                let created = |id: &str, text: &str| AgentEvent::ChecklistItemChanged {
                    item_id: id.into(),
                    text: Some(text.into()),
                    active_form: None,
                    status: comet_proto::ChecklistStatus::Pending,
                };
                let moved =
                    |id: &str, active: Option<&str>, status| AgentEvent::ChecklistItemChanged {
                        item_id: id.into(),
                        text: None,
                        active_form: active.map(str::to_owned),
                        status,
                    };
                [
                    created("1", "Read the configuration"),
                    created("2", "Count the failing cases"),
                    created("3", "Report both results"),
                    moved(
                        "1",
                        Some("Reading the configuration"),
                        comet_proto::ChecklistStatus::InProgress,
                    ),
                    // No activeForm on a completion, exactly as the wire sends
                    // it — the row must keep the label it already had.
                    moved("1", None, comet_proto::ChecklistStatus::Completed),
                    moved(
                        "2",
                        Some("Counting the failing cases"),
                        comet_proto::ChecklistStatus::InProgress,
                    ),
                    // Never created here: a status change for an id this run
                    // has not seen, which the fold turns into a text-less row.
                    moved(
                        "4",
                        Some("Checking the inherited step"),
                        comet_proto::ChecklistStatus::InProgress,
                    ),
                ]
            })
            .into_iter()
            .flatten();
        // Dev/testing knob: `COMET_MOCK_SUBAGENT=1` fans out scripted
        // subagents, which is the only way to put slice 4.4's card on screen
        // without a real Claude run that happens to delegate.
        //
        // Shaped from the recorded run at
        // `crates/harness/tests/corpus/claude/2.1.229/subagent`, not composed.
        // Each agent here exists to produce a state a happy path never does:
        //
        // - `a` COMPLETES with a multi-paragraph summary and all three
        //   counters. Its two `task_progress` readings are the same LENGTH on
        //   purpose ("Reading normalize.rs" / "Reading discovery.rs"): the row
        //   cache keys on a fingerprint that used to fold text lengths, so an
        //   equal-length activity change is exactly the case a frozen live
        //   line comes from.
        // - `b` FAILS, and reports no counters at all — `None` is "not
        //   reported", never zero, so the card must omit the line rather than
        //   print zeros.
        // - `c` is CANCELLED.
        // - `d` is left RUNNING and never settled, so it exercises a card that
        //   is still in flight when everything around it has finished.
        //
        // **The card's `last seen running` state is NOT reachable from here,
        // and no value of this knob reaches it.** That state needs a genuine
        // steer boundary (D57), and a steer only becomes one when the harness
        // emits `AgentEvent::Steered` — which only the Claude and Codex
        // adapters do. This mock advertises `supports_steering: true` above
        // and then never drains `controls.steering`, so a steer sent to it is
        // silently dropped and the run simply ends; the engine's `Done` sweep
        // then stamps `d` `Cancelled` like any other unfinished agent. Pinning
        // that state is `transcript.rs`'s
        // `a_running_subagent_in_a_finished_entry_reads_last_seen_running`,
        // not this rig. Seeing it on screen needs a real Claude run that
        // delegates, steered mid-flight.
        let mock_subagent = std::env::var("COMET_MOCK_SUBAGENT")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let subagent_events = mock_subagent
            .then(|| {
                let started = |task: &str, kind: &str, what: &str| AgentEvent::SubagentStarted {
                    task_id: task.into(),
                    tool_use_id: format!("toolu_{task}"),
                    agent_type: kind.into(),
                    description: what.into(),
                    prompt: None,
                };
                let progress = |task: &str, activity: &str| AgentEvent::SubagentUpdated {
                    task_id: task.into(),
                    status: SubagentStatus::Running,
                    activity: Some(activity.into()),
                    summary: None,
                    total_tokens: None,
                    duration_ms: None,
                    tool_uses: None,
                };
                vec![
                    // The parent's own tool call for `a`. Present so the
                    // card's suppression of the contentless `Agent` chip is
                    // exercised by the demo rig, not just by tests.
                    AgentEvent::ToolCall {
                        id: "toolu_a".into(),
                        call: comet_proto::ToolCall::Unknown {
                            name: "Agent".into(),
                            input: None,
                        },
                    },
                    started("a", "Explore", "Find every retry call site in the harness"),
                    started("b", "general-purpose", "Summarise the release notes"),
                    started("c", "Explore", "Check the codex adapter for the same bug"),
                    started("d", "Explore", "Read the capability sheets"),
                    progress("a", "Reading normalize.rs"),
                    progress("d", "Reading claude-2.1.229.md"),
                    // Equal length to the reading above — the fingerprint case.
                    progress("a", "Reading discovery.rs"),
                ]
            })
            .into_iter()
            .flatten();
        // The terminal readings ride at the END of the script, so the four
        // agents above spend the whole run visibly in flight. That window is
        // what makes `d`'s `last seen running` state reachable at all: it
        // needs a STEER landing while an agent is still running, and a rig
        // that settles everything two events after it starts leaves nobody
        // time to type.
        let subagent_settle_events = mock_subagent
            .then(|| {
                [
                    AgentEvent::SubagentUpdated {
                        task_id: "a".into(),
                        status: SubagentStatus::Completed,
                        // A terminal reading that reports NO activity, exactly
                        // as the wire sends it. The fold leaves the last live
                        // line standing in the part, so this is the fixture
                        // that proves a finished card does not draw it (D53).
                        activity: None,
                        summary: Some(
                            "Three call sites, all in the Claude adapter: the discovery \
                             probe, the stream reconnect, and the model-catalog refresh.\n\n\
                             Only the first backs off. The other two retry immediately in a \
                             tight loop, which is why a failed catalog hammers the CLI \
                             instead of settling."
                                .into(),
                        ),
                        total_tokens: Some(20_115),
                        duration_ms: Some(4_907),
                        tool_uses: Some(4),
                    },
                    AgentEvent::SubagentUpdated {
                        task_id: "b".into(),
                        status: SubagentStatus::Failed,
                        activity: None,
                        summary: None,
                        total_tokens: None,
                        duration_ms: None,
                        tool_uses: None,
                    },
                    AgentEvent::SubagentUpdated {
                        task_id: "c".into(),
                        status: SubagentStatus::Cancelled,
                        activity: None,
                        summary: None,
                        total_tokens: None,
                        duration_ms: None,
                        tool_uses: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        // With the code knob, also exercise a MULTILINE Exec command — the
        // round-9 chip breaker shape ("set -e\nfixture_in_original=0"): the
        // Run chip must stay one 30px line.
        let code_tool_events = mock_code
            .then(|| {
                [
                    AgentEvent::ToolCall {
                        id: "mock-code-tool".into(),
                        call: comet_proto::ToolCall::Exec {
                            command: "set -e\nfixture_in_original=0\ngrep -rn \"veil\" crates/ui/src | wc -l".into(),
                        },
                    },
                    AgentEvent::ToolResult {
                        id: "mock-code-tool".into(),
                        is_error: false,
                        diff: None,
                        diff_ref: None,
                        diff_stats: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        // The subagent starts lead the script rather than trailing the prose:
        // the cards then stand `running` for the whole run, which is the only
        // window in which a STEER can land on one — and that steer is the only
        // route to the `last seen running` state (D57). Behind the knob, so
        // the default script is untouched.
        let events: Vec<Result<AgentEvent, HarnessError>> = subagent_events
            .chain(
                body.iter()
                    .cycle()
                    .take(body.len() * repeat)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .chain(code_tool_events)
            .chain(checklist_events)
            .chain(code_event)
            .chain(table_event)
            .chain(mend_event)
            .chain(error_event)
            .chain(context_event)
            .chain(subagent_settle_events)
            .chain(tail.iter().cloned())
            .map(Ok)
            .collect();
        // Dev/testing knob: `COMET_MOCK_CHARS=N` re-chunks every TextDelta
        // into N-char deltas, so `COMET_MOCK_DELAY_MS` paces *characters*
        // instead of whole scripted blocks — delta boundaries then land inside
        // inline markers and links, which is the streaming shape real
        // harnesses produce and the display mend exists for.
        let chunk_chars = std::env::var("COMET_MOCK_CHARS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);
        let events: Vec<Result<AgentEvent, HarnessError>> = match chunk_chars {
            None => events,
            Some(n) => events
                .into_iter()
                .flat_map(|event| match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        let chars: Vec<char> = text.chars().collect();
                        chars
                            .chunks(n)
                            .map(|c| {
                                Ok(AgentEvent::TextDelta {
                                    text: c.iter().collect(),
                                })
                            })
                            .collect::<Vec<_>>()
                    }
                    other => vec![other],
                })
                .collect(),
        };
        if delay_ms == 0 {
            return Ok(futures::stream::iter(events).boxed());
        }
        Ok(futures::stream::iter(events)
            .then(move |event| async move {
                tokio::time::sleep(delay).await;
                event
            })
            .boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `COMET_MOCK_*` family is the project's standing answer to "see a UI
    /// state without producing it for real" (D52), and an answer nobody can
    /// find is not one. This pins the register in
    /// `docs/testing/mock-states.md` against the knobs this file actually
    /// reads, in BOTH directions: a knob added here and left undocumented
    /// fails, and a documented knob deleted here fails too.
    ///
    /// A source-text pin, on the precedent of `composer.rs`'s ordering test
    /// and for the same reason: the alternative is setting process-global env
    /// vars from a test, which races every other test in the binary. What it
    /// cannot check is that a knob still *emits* anything — that is what each
    /// surface's own row-level tests are for.
    #[test]
    fn every_mock_knob_is_documented() {
        /// Read at compile time so a missing file is a build error rather than
        /// a test that quietly passes on an empty string.
        const REGISTER: &str = include_str!("../../../docs/testing/mock-states.md");
        let source = include_str!("mock.rs");

        let names = |text: &str| -> std::collections::BTreeSet<String> {
            let mut found = std::collections::BTreeSet::new();
            let mut rest = text;
            while let Some(at) = rest.find("COMET_MOCK_") {
                let tail = &rest[at..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(tail.len());
                let name = &tail[..end];
                // Prose says "the `COMET_MOCK_*` family"; that bare prefix is
                // not a knob.
                if name.len() > "COMET_MOCK_".len() && !name.ends_with('_') {
                    found.insert(name.to_string());
                }
                rest = &tail[end..];
            }
            found
        };

        // The test's own literal would otherwise count as a use.
        let declared: std::collections::BTreeSet<String> = names(
            source
                .split_once("mod tests {")
                .map_or(source, |(before, _)| before),
        );
        let documented = names(REGISTER);

        assert!(!declared.is_empty(), "found no knobs to check");
        let undocumented: Vec<_> = declared.difference(&documented).collect();
        assert!(
            undocumented.is_empty(),
            "these knobs are read by mock.rs but missing from \
             docs/testing/mock-states.md: {undocumented:?}"
        );
        let stale: Vec<_> = documented.difference(&declared).collect();
        assert!(
            stale.is_empty(),
            "docs/testing/mock-states.md lists knobs mock.rs no longer reads: {stale:?}"
        );
    }

    /// Every card shape must be reachable from the knob, or the rendered
    /// check can only ever see the one kind 1.4 happened to script.
    #[test]
    fn every_approval_kind_has_a_mock_shape() {
        assert!(matches!(
            mock_approval("command"),
            Some(ApprovalRequest::Command { .. })
        ));
        assert!(matches!(
            mock_approval("file-change"),
            Some(ApprovalRequest::FileChange { .. })
        ));
        assert!(matches!(
            mock_approval("file-read"),
            Some(ApprovalRequest::FileRead { .. })
        ));
        assert!(matches!(
            mock_approval("mcp"),
            Some(ApprovalRequest::Mcp { .. })
        ));
        assert!(matches!(
            mock_approval("unknown"),
            Some(ApprovalRequest::Unknown { .. })
        ));
    }

    /// `COMET_MOCK_APPROVAL=1` is the value 1.4 shipped and documented; it
    /// must keep meaning the file-change run.
    #[test]
    fn the_legacy_value_still_selects_the_file_change_run() {
        assert!(matches!(
            mock_approval("1"),
            Some(ApprovalRequest::FileChange { .. })
        ));
    }

    /// The absent case, written by hand: unset and "0" are off, and an empty
    /// value is not a request for the default.
    #[test]
    fn the_off_values_produce_no_approval() {
        assert!(mock_approval("").is_none());
        assert!(mock_approval("0").is_none());
    }

    /// The hang knob's value must not accidentally become an "off" value for
    /// the approval knob: an unrecognized value falls into the fallback arm,
    /// not the `None` arm.
    #[test]
    fn an_unrecognized_value_falls_back_to_the_file_change_run() {
        assert!(
            mock_approval("hang").is_some(),
            "unrecognized values fall back"
        );
    }
}
