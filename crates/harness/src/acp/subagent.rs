//! Grok subagent cards on the ACP path.
//!
//! Grok's ordinary ACP tool frames identify `spawn_subagent`, but the task
//! lifecycle arrives separately on `_x.ai/session_notification`. The tracker
//! binds those two streams through the subagent id echoed in the tool's
//! completion output, with a description/FIFO fallback for synchronous
//! spawns, and emits the same `SubagentStarted` / `SubagentUpdated` events the
//! native driver uses.
//!
//! Upstream also tails Grok's private `chat_history.jsonl` into per-subagent
//! documents. Comet intentionally does not have that child-document route yet
//! (D54), so this port stops at native card parity. Parse/correlation failures
//! fail open to tracing and never surface private wire detail to the user.

use std::collections::{HashMap, VecDeque};

use comet_proto::{AgentEvent, SubagentStatus, ToolCall};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::session::{NotificationObservation, NotificationObserver};

const LIFECYCLE_METHOD: &str = "_x.ai/session_notification";
const MAX_TRACKED_SUBAGENTS: usize = 512;
type DescriptionToken = [u8; 32];

#[derive(Clone)]
struct PendingSpawn {
    tool_call_id: String,
    description: String,
    description_token: Option<DescriptionToken>,
    prompt: Option<String>,
    agent_type: String,
}

#[derive(Clone, Default)]
struct Spawned {
    description: String,
    agent_type: String,
}

struct BoundSpawn {
    spawn: PendingSpawn,
    started: bool,
}

pub(crate) struct SubagentTracker {
    session_id: String,
    pending: VecDeque<PendingSpawn>,
    bound: HashMap<String, BoundSpawn>,
    spawned_unbound: HashMap<String, Spawned>,
}

impl SubagentTracker {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            pending: VecDeque::new(),
            bound: HashMap::new(),
            spawned_unbound: HashMap::new(),
        }
    }

    fn observe_tool(&mut self, update: &Value) -> NotificationObservation {
        let id = update["toolCallId"].as_str().unwrap_or_default();
        if id.is_empty() {
            return NotificationObservation::default();
        }

        let is_spawn = update["_meta"]["x.ai/tool"]["name"].as_str() == Some("spawn_subagent");
        let pending_ix = self
            .pending
            .iter()
            .position(|spawn| spawn.tool_call_id == id);
        let bound_id = self.bound.iter().find_map(|(subagent_id, bound)| {
            (bound.spawn.tool_call_id == id).then(|| subagent_id.clone())
        });
        if !is_spawn && pending_ix.is_none() && bound_id.is_none() {
            return NotificationObservation::default();
        }

        let mut events = Vec::new();
        if is_spawn && pending_ix.is_none() && bound_id.is_none() {
            if self.pending.len() >= MAX_TRACKED_SUBAGENTS {
                tracing::debug!(
                    target: "comet_harness::acp::grok",
                    cap = MAX_TRACKED_SUBAGENTS,
                    "subagent correlation cap reached; leaving spawn as an ordinary tool"
                );
                return NotificationObservation::default();
            }
            let raw = &update["rawInput"];
            let description = raw["description"].as_str().unwrap_or_default();
            tracing::debug!(
                target: "comet_harness::acp::grok",
                "subagent description (full text): {description}"
            );
            let prompt = raw["prompt"].as_str().filter(|prompt| !prompt.is_empty());
            if let Some(prompt) = prompt {
                tracing::debug!(
                    target: "comet_harness::acp::grok",
                    "subagent prompt (full text): {prompt}"
                );
            }
            self.pending.push_back(PendingSpawn {
                tool_call_id: id.to_owned(),
                description: crate::cap_prose(description, crate::SUBAGENT_DESCRIPTION_MAX),
                description_token: description_token(description),
                prompt: prompt.map(|p| crate::cap_prose(p, crate::SUBAGENT_PROMPT_MAX)),
                agent_type: raw["subagent_type"].as_str().unwrap_or_default().to_owned(),
            });
            // Existing transcript projection suppresses one bare `Agent` chip
            // for each subagent card in the entry. Emitting this honest
            // fallback first preserves a visible failed spawn if no lifecycle
            // event ever arrives, without double-drawing a successful one.
            events.push(AgentEvent::ToolCall {
                id: id.to_owned(),
                call: ToolCall::Unknown {
                    name: "Agent".into(),
                    input: None,
                },
            });
        }

        let status = update["status"].as_str();
        if matches!(status, Some("completed" | "failed")) {
            if status == Some("completed")
                && bound_id.is_none()
                && let Some(subagent_id) = spawn_output_subagent_id(update)
                && let Some(ix) = self
                    .pending
                    .iter()
                    .position(|spawn| spawn.tool_call_id == id)
                && let Some(spawn) = self.pending.remove(ix)
            {
                self.bind(subagent_id.clone(), spawn);
                if let Some(spawned) = self.spawned_unbound.remove(&subagent_id) {
                    events.extend(self.start(&subagent_id, &spawned));
                }
            }
            if status == Some("failed") {
                self.pending.retain(|spawn| spawn.tool_call_id != id);
            }
            events.push(AgentEvent::ToolResult {
                id: id.to_owned(),
                is_error: status == Some("failed"),
                diff: None,
                diff_ref: None,
                diff_stats: None,
            });
        }

        NotificationObservation {
            events,
            claimed: true,
        }
    }

    fn observe_lifecycle(&mut self, update: &Value) -> NotificationObservation {
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if !matches!(
            kind,
            "subagent_spawned" | "subagent_progress" | "subagent_finished"
        ) {
            return NotificationObservation::default();
        }
        let mut events = Vec::new();
        match kind {
            "subagent_spawned" => events.extend(self.observe_spawned(update)),
            "subagent_progress" => {
                if let Some(event) = self.update_event(update, false) {
                    events.push(event);
                }
            }
            "subagent_finished" => {
                let subagent_id = update["subagent_id"].as_str().unwrap_or_default();
                if !subagent_id.is_empty() {
                    if self
                        .bound
                        .get(subagent_id)
                        .is_some_and(|bound| !bound.started)
                    {
                        events.extend(self.start(subagent_id, &Spawned::default()));
                    }
                    if let Some(event) = self.update_event(update, true) {
                        events.push(event);
                    }
                    self.bound.remove(subagent_id);
                    self.spawned_unbound.remove(subagent_id);
                }
            }
            _ => unreachable!(),
        }
        NotificationObservation {
            events,
            claimed: true,
        }
    }

    fn observe_spawned(&mut self, update: &Value) -> Vec<AgentEvent> {
        if let Some(parent) = update["parent_session_id"].as_str()
            && parent != self.session_id
        {
            tracing::trace!(
                target: "comet_harness::acp::grok",
                "ignoring a nested subagent lifecycle on the parent feed"
            );
            return Vec::new();
        }
        let Some(subagent_id) = update["subagent_id"].as_str().filter(|id| !id.is_empty()) else {
            tracing::debug!(
                target: "comet_harness::acp::grok",
                "subagent_spawned carried no usable id"
            );
            return Vec::new();
        };
        let description = update["description"].as_str().unwrap_or_default();
        if !description.is_empty() {
            tracing::debug!(
                target: "comet_harness::acp::grok",
                "subagent lifecycle description (full text): {description}"
            );
        }
        let spawned = Spawned {
            description: crate::cap_prose(description, crate::SUBAGENT_DESCRIPTION_MAX),
            agent_type: update["subagent_type"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        };
        let description_token = description_token(description);

        if !self.bound.contains_key(subagent_id) {
            // The capped label is the only prose retained or emitted, but it
            // cannot distinguish descriptions whose visible prefix is equal.
            // Match the fixed-size token derived from the full provider value;
            // an absent description still deliberately falls back to FIFO.
            let ix = self
                .pending
                .iter()
                .position(|spawn| {
                    description_token.is_some() && spawn.description_token == description_token
                })
                .or((!self.pending.is_empty()).then_some(0));
            if let Some(spawn) = ix.and_then(|ix| self.pending.remove(ix)) {
                self.bind(subagent_id.to_owned(), spawn);
            } else {
                if self.spawned_unbound.len() < MAX_TRACKED_SUBAGENTS {
                    self.spawned_unbound.insert(subagent_id.to_owned(), spawned);
                }
                return Vec::new();
            }
        }
        self.start(subagent_id, &spawned)
    }

    fn bind(&mut self, subagent_id: String, spawn: PendingSpawn) {
        if self.bound.len() < MAX_TRACKED_SUBAGENTS {
            self.bound.insert(
                subagent_id,
                BoundSpawn {
                    spawn,
                    started: false,
                },
            );
        }
    }

    fn start(&mut self, subagent_id: &str, spawned: &Spawned) -> Vec<AgentEvent> {
        let Some(bound) = self.bound.get_mut(subagent_id) else {
            return Vec::new();
        };
        if bound.started {
            return Vec::new();
        }
        bound.started = true;
        let description = if bound.spawn.description.is_empty() {
            spawned.description.clone()
        } else {
            bound.spawn.description.clone()
        };
        let agent_type = if bound.spawn.agent_type.is_empty() {
            spawned.agent_type.clone()
        } else {
            bound.spawn.agent_type.clone()
        };
        vec![AgentEvent::SubagentStarted {
            task_id: subagent_id.to_owned(),
            tool_use_id: bound.spawn.tool_call_id.clone(),
            agent_type,
            description,
            prompt: bound.spawn.prompt.clone(),
        }]
    }

    fn update_event(&self, update: &Value, finished: bool) -> Option<AgentEvent> {
        let subagent_id = update["subagent_id"].as_str()?.to_owned();
        self.bound.get(&subagent_id)?.started.then_some(())?;
        let status = if finished {
            terminal_status(update["status"].as_str().unwrap_or("completed"))
        } else {
            SubagentStatus::Running
        };
        Some(AgentEvent::SubagentUpdated {
            task_id: subagent_id,
            status,
            activity: (!finished)
                .then(|| update["description"].as_str())
                .flatten()
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            summary: finished
                .then(|| update["output"].as_str())
                .flatten()
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            total_tokens: None,
            duration_ms: None,
            tool_uses: update["tool_calls"]
                .as_u64()
                .and_then(|count| u32::try_from(count).ok()),
        })
    }
}

impl NotificationObserver for SubagentTracker {
    fn observe(&mut self, method: &str, params: &Value) -> NotificationObservation {
        if params["sessionId"].as_str() != Some(self.session_id.as_str()) {
            return NotificationObservation::default();
        }
        match method {
            "session/update" => self.observe_tool(&params["update"]),
            LIFECYCLE_METHOD => self.observe_lifecycle(&params["update"]),
            _ => NotificationObservation::default(),
        }
    }
}

fn description_token(description: &str) -> Option<DescriptionToken> {
    (!description.is_empty()).then(|| Sha256::digest(description.as_bytes()).into())
}

fn spawn_output_subagent_id(update: &Value) -> Option<String> {
    let text = update["rawOutput"]["text"].as_str().or_else(|| {
        update["content"]
            .as_array()?
            .iter()
            .find_map(|block| block["content"]["text"].as_str())
    })?;
    text.lines().find_map(|line| {
        let id = line.trim().strip_prefix("subagent_id:")?.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

fn terminal_status(raw: &str) -> SubagentStatus {
    match raw {
        "failed" | "error" | "errored" => SubagentStatus::Failed,
        "cancelled" | "canceled" | "killed" | "stopped" | "interrupted" => {
            SubagentStatus::Cancelled
        }
        _ => SubagentStatus::Completed,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn envelope(update: Value) -> Value {
        json!({"sessionId": "parent-1", "update": update})
    }

    #[test]
    fn the_spawn_completion_reads_the_echoed_subagent_id_from_both_wire_shapes() {
        assert_eq!(
            spawn_output_subagent_id(&json!({
                "rawOutput": {"type": "Text", "text": "started\nsubagent_id: sub-1\ntype: explore"}
            }))
            .as_deref(),
            Some("sub-1")
        );
        assert_eq!(
            spawn_output_subagent_id(&json!({
                "content": [{"type": "content", "content": {"type": "text", "text": "subagent_id: sub-2"}}]
            }))
            .as_deref(),
            Some("sub-2")
        );
        assert_eq!(
            spawn_output_subagent_id(&json!({"rawOutput": {"text": "started"}})),
            None
        );
    }

    #[test]
    fn the_echoed_id_correlates_spawn_progress_and_finish_to_one_native_card() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        let announced = tracker.observe(
            "session/update",
            &envelope(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "sp1",
                "rawInput": {"description": "Count files", "prompt": "Count them", "subagent_type": "explore"},
                "_meta": {"x.ai/tool": {"name": "spawn_subagent"}}
            })),
        );
        assert!(announced.claimed);
        assert!(matches!(
            announced.events.as_slice(),
            [AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. }] if name == "Agent"
        ));

        let completed = tracker.observe(
            "session/update",
            &envelope(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "sp1",
                "status": "completed",
                "rawOutput": {"text": "subagent_id: sub-1"}
            })),
        );
        assert!(
            matches!(completed.events.as_slice(), [AgentEvent::ToolResult { id, .. }] if id == "sp1")
        );

        let started = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1",
                "parent_session_id": "parent-1",
                "subagent_type": "explore",
                "description": "Count files"
            })),
        );
        assert!(matches!(
            started.events.as_slice(),
            [AgentEvent::SubagentStarted { task_id, tool_use_id, .. }]
                if task_id == "sub-1" && tool_use_id == "sp1"
        ));

        let progress = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_progress",
                "subagent_id": "sub-1",
                "description": "Counting"
            })),
        );
        assert!(matches!(
            progress.events.as_slice(),
            [AgentEvent::SubagentUpdated { status: SubagentStatus::Running, activity: Some(activity), .. }]
                if activity == "Counting"
        ));

        let finished = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_finished",
                "subagent_id": "sub-1",
                "status": "completed",
                "output": "two files",
                "tool_calls": 1
            })),
        );
        assert!(matches!(
            finished.events.as_slice(),
            [AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                summary: Some(summary),
                tool_uses: Some(1),
                ..
            }] if summary == "two files"
        ));
    }

    #[test]
    fn a_synchronous_spawn_uses_description_then_fifo_correlation() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        for (id, description) in [("sp1", "First task"), ("sp2", "Second task")] {
            tracker.observe(
                "session/update",
                &envelope(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "rawInput": {"description": description},
                    "_meta": {"x.ai/tool": {"name": "spawn_subagent"}}
                })),
            );
        }

        let by_description = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-2",
                "description": "Second task"
            })),
        );
        assert!(matches!(
            by_description.events.as_slice(),
            [AgentEvent::SubagentStarted { task_id, tool_use_id, .. }]
                if task_id == "sub-2" && tool_use_id == "sp2"
        ));

        let by_fifo = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1"
            })),
        );
        assert!(matches!(
            by_fifo.events.as_slice(),
            [AgentEvent::SubagentStarted { task_id, tool_use_id, .. }]
                if task_id == "sub-1" && tool_use_id == "sp1"
        ));
    }

    /// Break caught: the pending side capped descriptions before storing them,
    /// while the lifecycle side compared the provider's full string. Any
    /// over-limit description therefore missed its exact match and silently
    /// fell back to FIFO, swapping concurrently spawned cards when lifecycle
    /// notifications arrived out of order.
    #[test]
    fn long_descriptions_still_correlate_out_of_order_before_fifo() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        let first_description = format!("First {}", "a".repeat(crate::SUBAGENT_DESCRIPTION_MAX));
        let second_description = format!("Second {}", "b".repeat(crate::SUBAGENT_DESCRIPTION_MAX));
        for (id, description, prompt) in [
            ("sp1", first_description.as_str(), "prompt one"),
            ("sp2", second_description.as_str(), "prompt two"),
        ] {
            tracker.observe(
                "session/update",
                &envelope(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "rawInput": {"description": description, "prompt": prompt},
                    "_meta": {"x.ai/tool": {"name": "spawn_subagent"}}
                })),
            );
        }

        let second = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-2",
                "description": second_description
            })),
        );
        assert!(matches!(
            second.events.as_slice(),
            [AgentEvent::SubagentStarted {
                task_id,
                tool_use_id,
                prompt: Some(prompt),
                ..
            }] if task_id == "sub-2" && tool_use_id == "sp2" && prompt == "prompt two"
        ));

        let first = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1",
                "description": first_description
            })),
        );
        assert!(matches!(
            first.events.as_slice(),
            [AgentEvent::SubagentStarted {
                task_id,
                tool_use_id,
                prompt: Some(prompt),
                ..
            }] if task_id == "sub-1" && tool_use_id == "sp1" && prompt == "prompt one"
        ));
    }

    /// Break caught: matching only the capped display label makes distinct
    /// descriptions equal when their first 160 bytes are identical. A reverse
    /// lifecycle order then binds the second task to the first pending prompt.
    #[test]
    fn descriptions_sharing_the_visible_prefix_correlate_by_full_text() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        let shared = "x".repeat(crate::SUBAGENT_DESCRIPTION_MAX);
        let first_description = format!("{shared} first-only-tail");
        let second_description = format!("{shared} second-only-tail");
        for (id, description, prompt) in [
            ("sp1", first_description.as_str(), "prompt one"),
            ("sp2", second_description.as_str(), "prompt two"),
        ] {
            tracker.observe(
                "session/update",
                &envelope(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "rawInput": {"description": description, "prompt": prompt},
                    "_meta": {"x.ai/tool": {"name": "spawn_subagent"}}
                })),
            );
        }

        let second = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-2",
                "description": second_description
            })),
        );
        let expected_visible = format!("{}…", "x".repeat(crate::SUBAGENT_DESCRIPTION_MAX));
        assert!(matches!(
            second.events.as_slice(),
            [AgentEvent::SubagentStarted {
                task_id,
                tool_use_id,
                description,
                prompt: Some(prompt),
                ..
            }] if task_id == "sub-2"
                && tool_use_id == "sp2"
                && description == &expected_visible
                && prompt == "prompt two"
        ));

        let first = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1",
                "description": first_description
            })),
        );
        assert!(matches!(
            first.events.as_slice(),
            [AgentEvent::SubagentStarted {
                task_id,
                tool_use_id,
                description,
                prompt: Some(prompt),
                ..
            }] if task_id == "sub-1"
                && tool_use_id == "sp1"
                && description == &expected_visible
                && prompt == "prompt one"
        ));
    }

    #[test]
    fn a_nested_or_foreign_lifecycle_never_binds_the_parent_feed() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        let foreign_session = tracker.observe(
            LIFECYCLE_METHOD,
            &json!({
                "sessionId": "other",
                "update": {"sessionUpdate": "subagent_spawned", "subagent_id": "sub-1"}
            }),
        );
        assert!(!foreign_session.claimed);

        let nested = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-nested",
                "parent_session_id": "sub-1"
            })),
        );
        assert!(nested.claimed);
        assert!(nested.events.is_empty());
    }

    #[test]
    fn absent_optional_lifecycle_fields_stay_absent_without_blocking_the_card() {
        let mut tracker = SubagentTracker::new("parent-1".into());
        tracker.observe(
            "session/update",
            &envelope(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "sp1",
                "rawInput": {},
                "_meta": {"x.ai/tool": {"name": "spawn_subagent"}}
            })),
        );
        let started = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1"
            })),
        );
        assert!(matches!(
            started.events.as_slice(),
            [AgentEvent::SubagentStarted {
                agent_type,
                description,
                prompt: None,
                ..
            }] if agent_type.is_empty() && description.is_empty()
        ));

        let finished = tracker.observe(
            LIFECYCLE_METHOD,
            &envelope(json!({
                "sessionUpdate": "subagent_finished",
                "subagent_id": "sub-1"
            })),
        );
        assert!(matches!(
            finished.events.as_slice(),
            [AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
                ..
            }]
        ));

        let absent_session = tracker.observe(
            LIFECYCLE_METHOD,
            &json!({
                "update": {"sessionUpdate": "subagent_spawned", "subagent_id": "sub-2"}
            }),
        );
        assert!(!absent_session.claimed);
        assert!(absent_session.events.is_empty());
    }
}
