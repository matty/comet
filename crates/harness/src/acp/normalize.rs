//! Wire → `AgentEvent`, and prompt `stopReason` → `DoneStatus`.
//!
//! Pure functions over `serde_json::Value`, deliberately: every decode here is
//! testable against a literal frame the way `pickers.rs`'s `decode_models_reply`
//! is, without standing up a child process. Round-tripping through a Rust type
//! would let a reshaped wire stay green.

use std::collections::{HashMap, HashSet};

use comet_proto::{AgentCommand, AgentEvent, DoneStatus, ToolCall};
use serde_json::Value;

/// ACP's terminal `stopReason`, mapped onto Comet's three outcomes.
///
/// **The unknown arm is `Completed`, not `Errored`.** A reason this build has
/// never heard of still describes a turn that *ended*; calling it an error puts
/// a failure in front of the user for the sole reason that Comet is older than
/// the agent. The upstream vocabulary is `end_turn`, `cancelled`, `refusal`,
/// `max_tokens`, `max_turn_requests`.
pub(crate) fn done_status(stop_reason: &str) -> DoneStatus {
    match stop_reason {
        "cancelled" => DoneStatus::Interrupted,
        "refusal" => DoneStatus::Errored,
        _ => DoneStatus::Completed,
    }
}

/// A `session/update` notification's payload → zero or one `AgentEvent`.
///
/// `None` means "nothing Comet renders", which is a real answer and not a
/// failure: ACP carries update kinds this build does not consume, and an
/// unrecognized one must be dropped quietly rather than surfaced or errored.
pub(crate) fn session_update(params: &Value) -> Option<AgentEvent> {
    let update = &params["update"];
    match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => {
            text_of(&update["content"]).map(|text| AgentEvent::TextDelta { text })
        }
        "agent_thought_chunk" => {
            text_of(&update["content"]).map(|text| AgentEvent::ReasoningDelta { text })
        }
        _ => None,
    }
}

/// A content block's text. ACP blocks are `{type, text}`, but a block whose
/// `type` is not `text` (an image, say) legitimately carries no text — so this
/// returns `Option` rather than an empty string, and an empty string is
/// likewise treated as nothing to render.
fn text_of(content: &Value) -> Option<String> {
    let text = content["text"].as_str()?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// The slash commands an `available_commands_update` advertises.
///
/// **These are PUSHED, not asked for.** Grok sends the whole list unsolicited,
/// twice, before its `session/new` reply even lands — measured on 1.0.5: 45
/// commands at 613ms and again at 620ms, with no prompt ever sent. So the list
/// is free, and it is a full snapshot each time rather than a delta: a later
/// frame REPLACES an earlier one.
///
/// `input.hint` is the argument shape (18 of the 45 carry one). A `null` input
/// and an absent one mean the same thing, matching how `AgentCommand`'s own
/// `argument_hint` treats an empty string.
pub(crate) fn commands(raw: &Value) -> Vec<AgentCommand> {
    raw.as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|c| {
                    // A command with no name cannot be typed, so it cannot be
                    // offered. Skipped rather than rendered as a blank row.
                    let name = c["name"].as_str()?.to_owned();
                    Some(AgentCommand {
                        name,
                        description: c["description"].as_str().map(str::to_owned),
                        argument_hint: c["input"]["hint"]
                            .as_str()
                            .filter(|h| !h.is_empty())
                            .map(str::to_owned),
                        // ACP has no alias concept; Claude's suffix convention
                        // is its own. Empty is the honest answer, not a gap.
                        aliases: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The turn's token reading, from the ACP spec's own (unstable) `usage` block
/// on the `session/prompt` response — `result.usage.inputTokens` /
/// `.outputTokens`, a TOP-LEVEL field, never `_meta`.
///
/// **This is genuinely spec-general, not a second vendor path wearing a
/// generic name.** Grok does not use it at all: its reading is vendor-
/// namespaced under `_meta.inputTokens` (see `grok::usage`), so the split
/// between the two functions is real, not cosmetic — moving Grok's reader out
/// of this file is what slice PR1 did when Hermes' shape turned out to
/// disagree with it. Hermes' own `session/prompt` puts its numbers here
/// instead, at the path the ACP Python/TS SDKs both define for this
/// capability.
///
/// **Not pinned against a live capture.** Hermes' `session/new` requires a
/// configured LLM provider before it will even open a session — unlike
/// Grok's, whose handshake needs no auth — and no authenticated Hermes turn
/// could be run to capture one (see the PR1 task report). The field names and
/// their placement are confirmed from Hermes' own installed `acp`/
/// `acp_adapter` source (`server.py`'s `prompt()` builds `Usage(input_tokens=
/// result["prompt_tokens"], output_tokens=result["completion_tokens"], ...)`
/// from the underlying provider's own per-call completion usage) — ground
/// truth for what bytes it emits, but not a wire capture. That source shows
/// the numbers come from ONE turn's own completion call, which is the
/// cache-inclusive, per-turn reading `AgentEvent::Usage::prompt_tokens`
/// requires, not the "across all turns" wording the SDK's own docstring
/// carries (docstring text copied from the shared schema, not necessarily
/// accurate to this one implementation). Flagged as unverified until a real
/// capture confirms it.
///
/// `None` when the agent reported no numbers at all: an empty meter that says
/// "not measured" is honest, where zeros would read as a measurement of zero.
pub(crate) fn usage(result: &Value, context_window: Option<u64>) -> Option<AgentEvent> {
    let usage = &result["usage"];
    let prompt_tokens = usage["inputTokens"].as_u64();
    let output_tokens = usage["outputTokens"].as_u64();
    if prompt_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(AgentEvent::Usage {
        prompt_tokens: prompt_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        // `None` is "the agent did not say", never "no limit".
        context_window,
    })
}

/// The context window of the model a session opened on.
///
/// Read from `session/new`, not from the turn: the window is a property of the
/// MODEL, and the prompt response never carries it. Matched to
/// `models.currentModelId` rather than taking the first entry — a session on
/// the second of several models would otherwise be metered against the wrong
/// ceiling, which is worse than no meter.
pub(crate) fn context_window(session: &Value) -> Option<u64> {
    let models = &session["models"];
    let current = models["currentModelId"].as_str()?;
    models["availableModels"]
        .as_array()?
        .iter()
        .find(|m| m["modelId"].as_str() == Some(current))?["_meta"]["totalContextTokens"]
        .as_u64()
}

/// One call in flight, from its announcement until it can be named.
#[derive(Debug, Clone)]
struct Announced {
    /// The raw tool name. ACP puts it in `title` on the announcing frame; the
    /// later frames replace `title` with a human sentence ("Read `alpha.txt`"),
    /// so the name has to be kept from the first one or it is gone.
    name: String,
    input: Option<Value>,
}

/// Tool-call assembly across frames.
///
/// **ACP splits one call over several `session/update`s, and the field that
/// says what the call IS does not arrive on the first one.** Measured against
/// grok 1.0.5 (2026-08-28), each call came as three frames:
///
/// 1. `tool_call` — `toolCallId`, `title` holding the raw tool name, `rawInput`.
///    **No `kind`, no `status`.**
/// 2. `tool_call_update` — ACP's `kind` (`read`/`search`/`other`), a human
///    `title`, and `locations`.
/// 3. `tool_call_update` — `status: "completed"` with `content`/`rawOutput`.
///
/// So the announcement alone cannot produce a typed card, and Comet emits
/// [`AgentEvent::ToolCall`] once. This holds the announcement until a frame
/// carries `kind`, then emits. A call that reaches a terminal `status` without
/// ever being named still emits — as an honest `Unknown` chip — because a tool
/// that ran and finished must not be invisible.
#[derive(Debug, Default)]
pub(crate) struct ToolTracker {
    announced: HashMap<String, Announced>,
    /// Ids already emitted as a `ToolCall`, so a later frame becomes a result
    /// rather than a second card.
    emitted: HashSet<String>,
}

/// Bound on both maps. Comfortably above any turn a human watches, and small
/// enough that a runaway agent cannot use them as an allocator — the same
/// reasoning as `codex::track_file_change`'s cap, and `DEBT.md` D10 is the
/// standing version of the mistake it avoids.
const MAX_TRACKED_CALLS: usize = 512;

impl ToolTracker {
    fn announce(&mut self, id: &str, name: String, input: Option<Value>) {
        if self.announced.len() >= MAX_TRACKED_CALLS {
            return;
        }
        self.announced
            .insert(id.to_owned(), Announced { name, input });
    }

    fn mark_emitted(&mut self, id: &str) {
        if self.emitted.len() < MAX_TRACKED_CALLS {
            self.emitted.insert(id.to_owned());
        }
    }
}

/// ACP's terminal statuses. `pending` and `in_progress` are not terminal and
/// leave the call open.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

/// Build a typed [`ToolCall`] from ACP's OWN `kind`, never from a vendor tool
/// name.
///
/// **The name table is the thing being avoided.** Grok calls its tools
/// `list_dir`, `read_file`, `grep`; the next ACP agent will call them something
/// else, and a table keyed on those names silently degrades to nothing the day
/// a vendor renames one. `kind` is the standard field every ACP agent
/// populates, and paired with the input's own field names it is enough for the
/// cards that matter.
///
/// **Everything else is `Unknown`, deliberately.** An `Unknown` chip still
/// shows the real tool name and its input, so it is honest and useful; a
/// guessed typed card claims to know what a tool did. When those two compete,
/// the wrong claim is worse than the plain one.
fn typed_call(kind: &str, announced: &Announced, update: &Value) -> ToolCall {
    let input = announced.input.clone();
    match kind {
        // `locations[0].path` is where ACP puts what a read touched. Falling
        // back to Unknown rather than an empty path: a ReadFile card with no
        // file names nothing.
        "read" => match first_location(update) {
            Some(path) => ToolCall::ReadFile { path },
            None => ToolCall::Unknown {
                name: announced.name.clone(),
                input,
            },
        },
        // The search input carries its own `pattern`/`path` names.
        "search" => match update["rawInput"]["pattern"].as_str() {
            Some(pattern) => ToolCall::Search {
                pattern: pattern.to_owned(),
                path: update["rawInput"]["path"].as_str().map(str::to_owned),
            },
            None => ToolCall::Unknown {
                name: announced.name.clone(),
                input,
            },
        },
        _ => ToolCall::Unknown {
            name: announced.name.clone(),
            input,
        },
    }
}

fn first_location(update: &Value) -> Option<String> {
    update["locations"]
        .as_array()?
        .iter()
        .find_map(|l| l["path"].as_str())
        .map(str::to_owned)
}

/// A `tool_call` / `tool_call_update` frame → zero or more events.
pub(crate) fn tool_update(tracker: &mut ToolTracker, update: &Value) -> Vec<AgentEvent> {
    let Some(id) = update["toolCallId"].as_str() else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if update["sessionUpdate"].as_str() == Some("tool_call") {
        // The announcing frame's `title` IS the raw tool name; later frames
        // overwrite it with prose.
        let name = update["title"].as_str().unwrap_or("tool").to_owned();
        let input = match &update["rawInput"] {
            Value::Null => None,
            other => Some(other.clone()),
        };
        tracker.announce(id, name, input);
    }

    let status = update["status"].as_str();
    let terminal = status.is_some_and(is_terminal);

    // Emit the card once: as soon as a frame names the call, or at the latest
    // when it finishes.
    if !tracker.emitted.contains(id)
        && let Some(announced) = tracker.announced.get(id).cloned()
    {
        let kind = update["kind"].as_str();
        if kind.is_some() || terminal {
            let call = typed_call(kind.unwrap_or("other"), &announced, update);
            out.push(AgentEvent::ToolCall {
                id: id.to_owned(),
                call,
            });
            tracker.mark_emitted(id);
        }
    }

    if terminal {
        tracker.announced.remove(id);
        out.push(AgentEvent::ToolResult {
            id: id.to_owned(),
            is_error: status == Some("failed"),
            diff: None,
            diff_ref: None,
            diff_stats: None,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Entries grok 1.0.5 really pushed, captured 2026-08-28. 18 of its 45
    /// carry an `input.hint`; the rest send `input: null`.
    #[test]
    fn the_captured_commands_decode_with_and_without_a_hint() {
        let raw = json!([
            {"name": "compact",
             "description": "Compress conversation history to save context window",
             "input": {"hint": "optional context about what to preserve"}},
            {"name": "context",
             "description": "Show context window usage and session stats",
             "input": null},
        ]);

        let decoded = commands(&raw);
        assert_eq!(decoded.len(), 2);

        assert_eq!(decoded[0].name, "compact", "the name has no leading slash");
        assert_eq!(
            decoded[0].argument_hint.as_deref(),
            Some("optional context about what to preserve")
        );
        assert!(decoded[0].description.is_some());

        assert_eq!(
            decoded[1].argument_hint, None,
            "`input: null` is no hint, not an empty one"
        );
        assert!(
            decoded[1].aliases.is_empty(),
            "ACP has no alias concept; empty is the honest answer"
        );
    }

    /// A nameless command cannot be typed, so it cannot be offered — skipped
    /// rather than drawn as a blank row.
    #[test]
    fn a_command_without_a_name_is_skipped() {
        let raw = json!([
            {"description": "no name at all"},
            {"name": "real"},
        ]);
        let decoded = commands(&raw);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].name, "real");
        assert_eq!(decoded[0].description, None);
    }

    /// An empty hint and an absent one are the same thing, matching how
    /// `AgentCommand::argument_hint` documents its own contract.
    #[test]
    fn an_empty_hint_reads_as_no_hint() {
        let raw = json!([{"name": "c", "input": {"hint": ""}}]);
        assert_eq!(commands(&raw)[0].argument_hint, None);
    }

    /// An agent that pushes nothing has no commands — not an error, and not a
    /// crash on a shape that is not an array.
    #[test]
    fn an_absent_command_list_is_empty() {
        for raw in [
            json!(null),
            json!([]),
            json!({"availableCommands": []}),
            json!("nope"),
        ] {
            assert!(commands(&raw).is_empty(), "must be empty: {raw}");
        }
    }

    /// A hand-built literal matching the top-level shape Hermes' own installed
    /// `acp`/`acp_adapter` source builds (`Usage(inputTokens=...,
    /// outputTokens=..., totalTokens=...)` on the `PromptResponse`, never under
    /// `_meta`). **Not a live capture** — see [`usage`]'s doc comment for why
    /// none could be taken — so this pins the field NAMES and LOCATION the
    /// source proves, not a wire exchange this build has watched happen.
    #[test]
    fn the_spec_shaped_reply_reads_as_prompt_and_output() {
        let result = json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 14500,
                "outputTokens": 171,
                "totalTokens": 14671,
                "cachedReadTokens": 10624,
                "thoughtTokens": 169,
            },
        });

        match usage(&result, Some(500_000)).expect("the agent reported numbers") {
            AgentEvent::Usage {
                prompt_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(prompt_tokens, 14500);
                assert_eq!(output_tokens, 171);
                assert_eq!(context_window, Some(500_000));
                // **Not 14671.** `totalTokens` is a different quantity and
                // must not be drawn against the window — see `AgentEvent::
                // Usage`'s own doc comment.
                assert_ne!(prompt_tokens, 14671);
            }
            other => panic!("{other:?}"),
        }
    }

    /// **No numbers means no reading, not a reading of zero.** An empty meter
    /// that says "not measured" is honest; zeros claim a measurement.
    #[test]
    fn an_absent_usage_block_yields_no_event() {
        for result in [
            json!({"stopReason": "end_turn"}),
            json!({"stopReason": "end_turn", "usage": {}}),
            json!({"usage": {"totalTokens": 999}}),
            // `_meta` is not this path — that is Grok's vendor reader.
            json!({"_meta": {"inputTokens": 100, "outputTokens": 1}}),
            json!({}),
        ] {
            assert!(
                usage(&result, Some(500_000)).is_none(),
                "must not report: {result}"
            );
        }
    }

    /// A partial report is still a report — one half missing does not discard
    /// the other.
    #[test]
    fn a_half_reported_usage_still_counts() {
        let only_input = json!({"usage": {"inputTokens": 100}});
        match usage(&only_input, None).expect("input alone is a reading") {
            AgentEvent::Usage {
                prompt_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(prompt_tokens, 100);
                assert_eq!(output_tokens, 0);
                assert_eq!(context_window, None, "absent window is unknown, not zero");
            }
            other => panic!("{other:?}"),
        }
    }

    /// **The window is matched to the CURRENT model.** Break caught: taking the
    /// first `availableModels` entry, which meters a session against another
    /// model's ceiling — worse than showing no meter at all.
    #[test]
    fn the_context_window_follows_the_current_model() {
        let session = json!({"models": {
            "currentModelId": "grok-mini",
            "availableModels": [
                {"modelId": "grok-4.6", "_meta": {"totalContextTokens": 500000}},
                {"modelId": "grok-mini", "_meta": {"totalContextTokens": 128000}},
            ],
        }});
        assert_eq!(context_window(&session), Some(128_000));
    }

    /// An agent that names no window reports none, rather than a made-up one.
    #[test]
    fn an_unstated_context_window_is_none() {
        for session in [
            json!({}),
            json!({"models": {}}),
            json!({"models": {"currentModelId": "x", "availableModels": []}}),
            // The current model is not in the list at all.
            json!({"models": {"currentModelId": "gone",
                              "availableModels": [{"modelId": "other",
                                                   "_meta": {"totalContextTokens": 1}}]}}),
            // Present, but the window is not stated.
            json!({"models": {"currentModelId": "x",
                              "availableModels": [{"modelId": "x", "_meta": {}}]}}),
        ] {
            assert!(
                context_window(&session).is_none(),
                "must be none: {session}"
            );
        }
    }

    /// The three frames grok 1.0.5 really sent for one `read_file` call,
    /// captured 2026-08-28. Pinned against the literal wire: a reshaped frame
    /// must fail here, which a test built from our own types would not catch.
    fn captured_read_frames() -> [Value; 3] {
        [
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-abc-1",
                "title": "read_file",
                "rawInput": {"target_file": "alpha.txt"},
                "_meta": {"x.ai/tool": {
                    "version": 1, "name": "read_file", "kind": "read",
                    "namespace": "grok_build", "label": "Read", "read_only": true,
                }},
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-abc-1",
                "kind": "read",
                "title": "Read `alpha.txt`",
                "locations": [{"path": "alpha.txt"}],
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-abc-1",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "hello alpha"}}],
            }),
        ]
    }

    fn drain(frames: &[Value]) -> Vec<AgentEvent> {
        let mut tracker = ToolTracker::default();
        frames
            .iter()
            .flat_map(|f| tool_update(&mut tracker, f))
            .collect()
    }

    /// **Exactly one card and one result, from three frames.** Break caught:
    /// emitting a `ToolCall` per frame, which would draw the same call three
    /// times in the transcript.
    #[test]
    fn three_frames_make_one_call_and_one_result() {
        let events = drain(&captured_read_frames());
        assert_eq!(events.len(), 2, "one card, one result: {events:#?}");

        match &events[0] {
            AgentEvent::ToolCall { id, call } => {
                assert_eq!(id, "call-abc-1");
                assert_eq!(
                    call,
                    &ToolCall::ReadFile {
                        path: "alpha.txt".into()
                    },
                    "kind=read plus locations[0].path is a real read card"
                );
            }
            other => panic!("expected a ToolCall first, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::ToolResult { id, is_error, .. } => {
                assert_eq!(id, "call-abc-1");
                assert!(!is_error, "a completed call is not an error");
            }
            other => panic!("expected a ToolResult second, got {other:?}"),
        }
    }

    /// **The announcing frame alone must not emit.** It carries no `kind`, so
    /// naming the call from it would mean guessing — and Comet emits a
    /// `ToolCall` once, with no way to correct it afterwards.
    #[test]
    fn the_announcement_alone_emits_nothing() {
        let mut tracker = ToolTracker::default();
        let frames = captured_read_frames();
        assert!(
            tool_update(&mut tracker, &frames[0]).is_empty(),
            "the first frame has no kind; waiting for one is the whole design"
        );
        // And the very next frame, which does carry `kind`, releases it.
        assert_eq!(tool_update(&mut tracker, &frames[1]).len(), 1);
    }

    /// A search maps off ACP's `kind` plus the input's OWN field names — no
    /// vendor tool-name table. Grok calls this one `grep`; the decode never
    /// looks at that.
    #[test]
    fn a_search_is_typed_from_the_standard_kind_not_the_tool_name() {
        let events = drain(&[
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-abc-2",
                "title": "grep",
                "rawInput": {"variant": "Grep", "pattern": "needle", "path": null},
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-abc-2",
                "kind": "search",
                "title": "needle",
                "locations": [],
                "rawInput": {"variant": "Grep", "pattern": "needle", "path": null},
            }),
        ]);

        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCall { call, .. } => assert_eq!(
                call,
                &ToolCall::Search {
                    pattern: "needle".into(),
                    path: None
                },
                "a null path is absent, not an empty string"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// **An unrecognized kind is an honest chip, not a guess.** `list_dir`
    /// arrives as `kind: "other"`; the card carries the real tool name and its
    /// input rather than being forced into a typed variant that would claim
    /// something untrue.
    #[test]
    fn an_unmodelled_kind_keeps_the_real_name_and_input() {
        let events = drain(&[
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-abc-3",
                "title": "list_dir",
                "rawInput": {"target_directory": "."},
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-abc-3",
                "kind": "other",
                "title": "List `.`",
            }),
        ]);

        match &events[0] {
            AgentEvent::ToolCall { call, .. } => match call {
                ToolCall::Unknown { name, input } => {
                    assert_eq!(name, "list_dir", "the RAW name, not the prose title");
                    assert_eq!(
                        input.as_ref().and_then(|i| i["target_directory"].as_str()),
                        Some("."),
                        "the chip still shows what the tool was asked to do"
                    );
                }
                other => panic!("an unmodelled kind must stay Unknown, got {other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// **A `read` with no location degrades rather than lying.** A `ReadFile`
    /// card naming no file is worse than a chip that names the tool.
    #[test]
    fn a_read_without_a_location_falls_back_to_a_chip() {
        let events = drain(&[
            json!({"sessionUpdate": "tool_call", "toolCallId": "r", "title": "read_file"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "r",
                   "kind": "read", "locations": []}),
        ]);
        match &events[0] {
            AgentEvent::ToolCall { call, .. } => {
                assert!(matches!(call, ToolCall::Unknown { .. }), "{call:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// **A call that finishes without ever being named still appears.** The
    /// tool ran; leaving it out of the transcript loses a step the user needs
    /// to see, which is the whole complaint tool cards exist to fix.
    #[test]
    fn a_call_that_completes_unnamed_still_emits_both_events() {
        let events = drain(&[
            json!({"sessionUpdate": "tool_call", "toolCallId": "z", "title": "mystery",
                   "rawInput": {"a": 1}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "z", "status": "completed"}),
        ]);
        assert_eq!(events.len(), 2, "{events:#?}");
        assert!(matches!(
            &events[0],
            AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. } if name == "mystery"
        ));
        assert!(matches!(&events[1], AgentEvent::ToolResult { .. }));
    }

    /// `failed` is terminal AND an error; `pending`/`in_progress` are neither.
    /// Break caught: treating any `status` as terminal, which would close a
    /// call the moment it started running.
    #[test]
    fn only_terminal_statuses_close_a_call() {
        assert!(is_terminal("completed"));
        assert!(is_terminal("failed"));
        assert!(!is_terminal("pending"));
        assert!(!is_terminal("in_progress"));
        assert!(!is_terminal("a_status_from_2027"));

        let events = drain(&[
            json!({"sessionUpdate": "tool_call", "toolCallId": "f", "title": "boom"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "f",
                   "kind": "other", "status": "in_progress"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "f",
                   "kind": "other", "status": "failed"}),
        ]);
        let results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { is_error, .. } => Some(*is_error),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec![true], "one result, and it is an error");
    }

    /// A frame with no id is dropped rather than panicking or inventing one.
    #[test]
    fn a_frame_without_an_id_is_dropped() {
        let mut tracker = ToolTracker::default();
        assert!(tool_update(&mut tracker, &json!({"sessionUpdate": "tool_call"})).is_empty());
        assert!(tool_update(&mut tracker, &json!({})).is_empty());
    }

    /// The tracker is bounded, so a runaway agent cannot use it as an
    /// allocator. Same reasoning as `codex::track_file_change`'s cap.
    #[test]
    fn the_tracker_is_bounded() {
        let mut tracker = ToolTracker::default();
        for i in 0..(MAX_TRACKED_CALLS + 50) {
            tool_update(
                &mut tracker,
                &json!({"sessionUpdate": "tool_call", "toolCallId": format!("c{i}"), "title": "t"}),
            );
        }
        assert!(tracker.announced.len() <= MAX_TRACKED_CALLS);
    }

    /// Break caught: mapping an unknown `stopReason` to `Errored`, which shows
    /// the user a failure whose only cause is Comet being older than the agent.
    #[test]
    fn an_unknown_stop_reason_completes_rather_than_errors() {
        assert_eq!(done_status("end_turn"), DoneStatus::Completed);
        assert_eq!(done_status("max_tokens"), DoneStatus::Completed);
        assert_eq!(done_status("a_reason_from_2027"), DoneStatus::Completed);
        assert_eq!(done_status("cancelled"), DoneStatus::Interrupted);
        assert_eq!(done_status("refusal"), DoneStatus::Errored);
    }

    #[test]
    fn message_and_thought_chunks_map_to_their_own_deltas() {
        let msg = json!({"update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"},
        }});
        assert!(matches!(
            session_update(&msg),
            Some(AgentEvent::TextDelta { text }) if text == "hello"
        ));

        let thought = json!({"update": {
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "thinking"},
        }});
        assert!(matches!(
            session_update(&thought),
            Some(AgentEvent::ReasoningDelta { text }) if text == "thinking"
        ));
    }

    /// Break caught: an unrecognized update kind returning a rendered event, or
    /// panicking. Both are wrong — ACP carries kinds this build does not read,
    /// and the honest answer is to drop them.
    #[test]
    fn unreadable_updates_are_dropped_not_surfaced() {
        for payload in [
            json!({"update": {"sessionUpdate": "tool_call", "content": {"text": "x"}}}),
            json!({"update": {"sessionUpdate": "plan"}}),
            // A kind that does not exist at all.
            json!({"update": {"sessionUpdate": "invented_in_2027"}}),
            // No `sessionUpdate` key whatsoever.
            json!({"update": {}}),
            // No `update` key whatsoever.
            json!({}),
        ] {
            assert!(session_update(&payload).is_none(), "must drop: {payload}");
        }
    }

    /// A text block whose text is absent or empty is nothing to render. The
    /// absent case is the one a fixture would never produce, so it is written
    /// here deliberately rather than left to the wire to demonstrate.
    #[test]
    fn a_chunk_with_no_text_yields_no_event() {
        for content in [
            json!({"type": "image", "data": "..."}),
            json!({"type": "text", "text": ""}),
            json!({}),
        ] {
            let frame = json!({"update": {
                "sessionUpdate": "agent_message_chunk",
                "content": content,
            }});
            assert!(session_update(&frame).is_none(), "must drop: {frame}");
        }
    }
}
