//! Session doc schema over `loro` — Rust port of `packages/session-doc/src/schema.ts`.
//!
//! Container layout (MUST stay shape-compatible with the TS edge/tail materializer):
//! - `meta`:     LoroMap  { chatId: string, schemaVersion: number }         (host-only writer)
//! - `messages`: LoroList of LoroMap {
//!   id, role, parts: LoroList<part map>, createdAt, deviceId, status?, continuationOf? }
//! - `commands`: LoroList of LoroMap {
//!   id, kind, payload(json), issuedBy, issuedAt, basedOn?, expiresAt?, status, resolution? }
//!
//! Part maps: { id, kind: "text"|"tool"|"input"|"error", text?: LoroText, call?: json,
//! isError?, questions?: json, resolved?, message? }. Text bodies are **LoroText** so streaming
//! appends RLE-merge (1.03x oplog overhead vs 125x for whole-value rewrites).

use loro::{ExportMode, LoroDoc, LoroError, LoroList, LoroMap, LoroText, LoroValue, ToJson};
use serde::{Deserialize, Serialize};

use crate::commands::{SessionCommandEntry, SessionCommandStatus};
use crate::constants::{SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT};
use crate::parts::{MessagePart, MessageStatus};

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("schema: {0}")]
    Schema(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// One entry in the doc's `messages` list (`SessionMessageEntry` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    /// Epoch millis.
    pub created_at: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<String>,
}

/// The doc-resident flat part map (`DocMessagePart` in TS). Distinct from the app-layer
/// [`MessagePart`]: input parts key on their request id, error parts store `message`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocPartJson {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    questions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval: Option<serde_json::Value>,
    /// Absent IS the open state — not a torn write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notice_kind: Option<comet_proto::NoticeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    severity: Option<comet_proto::NoticeSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occurrences: Option<u32>,
}

/// App parts → doc part json (mirror of `toDocParts`).
fn to_doc_part(part: &MessagePart) -> Result<DocPartJson, DocError> {
    Ok(match part {
        MessagePart::Text { id, text } => DocPartJson {
            id: id.clone(),
            kind: "text".into(),
            text: Some(text.clone()),
            ..Default::default()
        },
        MessagePart::Tool {
            id,
            call,
            is_error,
            resolved,
        } => DocPartJson {
            id: id.clone(),
            kind: "tool".into(),
            call: Some(serde_json::to_value(call)?),
            // TS shape parity: `isError` is written only once the tool result arrived;
            // its presence IS the resolution marker.
            is_error: if *resolved { Some(*is_error) } else { None },
            ..Default::default()
        },
        MessagePart::Input {
            id: _,
            request_id,
            questions,
            resolved,
        } => DocPartJson {
            id: request_id.clone(),
            kind: "input".into(),
            questions: Some(serde_json::to_value(questions)?),
            resolved: Some(*resolved),
            ..Default::default()
        },
        MessagePart::Approval {
            id: _,
            request_id,
            approval,
            decision,
        } => DocPartJson {
            // The request id IS the persisted part id, matching the input
            // part: the live fold's display prefix does not survive the write,
            // and every doc-side mutation matches on this.
            id: request_id.clone(),
            kind: "approval".into(),
            approval: Some(serde_json::to_value(approval)?),
            decision: decision.as_ref().map(serde_json::to_value).transpose()?,
            ..Default::default()
        },
        MessagePart::Error { id, message } => DocPartJson {
            id: id.clone(),
            kind: "error".into(),
            message: Some(message.clone()),
            ..Default::default()
        },
        MessagePart::Notice {
            id,
            kind,
            severity,
            summary,
            detail,
            key,
            occurrences,
        } => DocPartJson {
            id: id.clone(),
            kind: "notice".into(),
            notice_kind: Some(*kind),
            severity: Some(*severity),
            summary: Some(summary.clone()),
            detail: detail.clone(),
            key: key.clone(),
            occurrences: Some(*occurrences),
            ..Default::default()
        },
    })
}

/// Doc part json → app part (mirror of `fromDocParts`; malformed degrades to empty text).
fn from_doc_part(p: DocPartJson) -> MessagePart {
    match p.kind.as_str() {
        "tool" => match p.call.and_then(|c| serde_json::from_value(c).ok()) {
            Some(call) => MessagePart::Tool {
                id: p.id,
                call,
                is_error: p.is_error.unwrap_or(false),
                resolved: p.is_error.is_some(),
            },
            None => MessagePart::Text {
                id: p.id,
                text: String::new(),
            },
        },
        "input" => MessagePart::Input {
            id: p.id.clone(),
            request_id: p.id,
            questions: p
                .questions
                .and_then(|q| serde_json::from_value(q).ok())
                .unwrap_or_default(),
            resolved: p.resolved.unwrap_or(false),
        },
        "approval" => match p.approval.and_then(|a| serde_json::from_value(a).ok()) {
            Some(approval) => MessagePart::Approval {
                id: p.id.clone(),
                request_id: p.id,
                approval,
                // An absent decision is the OPEN state, not a torn write — it
                // is what an unanswered approval looks like on disk.
                decision: p.decision.and_then(|d| serde_json::from_value(d).ok()),
            },
            None => MessagePart::Text {
                id: p.id,
                text: String::new(),
            },
        },
        "error" => MessagePart::Error {
            id: p.id,
            message: p.message.unwrap_or_default(),
        },
        "notice" => MessagePart::Notice {
            id: p.id,
            // A kind this build has never heard of already degraded to Info
            // inside NoticeKind's serde; an ABSENT kind (torn write) does the
            // same here.
            kind: p.notice_kind.unwrap_or(comet_proto::NoticeKind::Info),
            severity: p.severity.unwrap_or(comet_proto::NoticeSeverity::Warning),
            summary: p.summary.unwrap_or_default(),
            detail: p.detail,
            key: p.key,
            occurrences: p.occurrences.unwrap_or(1),
        },
        _ => MessagePart::Text {
            id: p.id,
            text: p.text.unwrap_or_default(),
        },
    }
}

/// A session doc handle: typed access over a LoroDoc with the schema above.
pub struct SessionDoc {
    doc: LoroDoc,
}

impl SessionDoc {
    /// Wrap an existing doc (e.g. imported from a snapshot).
    pub fn from_doc(doc: LoroDoc) -> Self {
        Self { doc }
    }

    /// Create + initialize a fresh doc for `chat_id` (host-only).
    pub fn init(chat_id: &str) -> Result<Self, DocError> {
        let doc = LoroDoc::new();
        let meta = doc.get_map("meta");
        meta.insert("chatId", chat_id)?;
        meta.insert("schemaVersion", SESSION_SCHEMA_VERSION as i64)?;
        doc.commit();
        Ok(Self { doc })
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    pub fn chat_id(&self) -> Option<String> {
        match self.doc.get_map("meta").get("chatId") {
            Some(loro::ValueOrContainer::Value(LoroValue::String(s))) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Insert a complete message entry (user/system messages, command-side inserts).
    /// Streaming assistant entries go through [`SegmentWriter`].
    pub fn push_message(&self, entry: &SessionMessageEntry) -> Result<(), DocError> {
        let messages = self.doc.get_list("messages");
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(&map, entry)?;
        let parts = map.insert_container("parts", LoroList::new())?;
        for part in &entry.parts {
            push_part(&parts, part)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// Read all entries (continuations NOT joined — see `join_continuation_entries`).
    ///
    /// Malformed entries are SKIPPED, not fatal: a torn intermediate state
    /// (an entry map imported before the update that fills its fields) or a
    /// peer on a newer schema must degrade to a missing row, never blank the
    /// whole transcript — one bad entry took down every publish for the chat
    /// (2026-07-31, "missing field `id`" during a multi-update import).
    pub fn read_entries(&self) -> Result<Vec<SessionMessageEntry>, DocError> {
        // Materialize only the messages container — a whole-doc deep value
        // here also serialized the commands ledger on every 120ms commit tick.
        let messages = self
            .doc
            .get_list("messages")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(messages)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match entry_from_json(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed transcript entry");
                    None
                }
            })
            .collect())
    }

    /// Read the commands ledger.
    ///
    /// Same skip-not-fail policy as `read_entries`: any device can append
    /// here, and one malformed entry must not wedge command draining for the
    /// chat forever (an unparseable command can't be executed anyway).
    pub fn read_commands(&self) -> Result<Vec<SessionCommandEntry>, DocError> {
        // Container-scoped for the same reason as `read_entries`: the drain
        // loop runs this per tick and must not pay for the transcript.
        let commands = self
            .doc
            .get_list("commands")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(commands)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match serde_json::from_value(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed command entry");
                    None
                }
            })
            .collect())
    }

    /// Validate every persisted session row without the runtime readers' tolerant
    /// skip policy. Used before migration makes copied snapshots authoritative.
    pub fn validate_strict(&self) -> Result<(), DocError> {
        let value = self.doc.get_deep_value().to_json_value();
        let messages: Vec<serde_json::Value> = serde_json::from_value(
            value
                .get("messages")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(|err| DocError::Schema(format!("invalid messages container: {err}")))?;
        for (index, row) in messages.into_iter().enumerate() {
            validate_message_parts(&row)
                .map_err(|err| DocError::Schema(format!("invalid message row {index}: {err}")))?;
            entry_from_json(row)
                .map_err(|err| DocError::Schema(format!("invalid message row {index}: {err}")))?;
        }

        let commands: Vec<serde_json::Value> = serde_json::from_value(
            value
                .get("commands")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(|err| DocError::Schema(format!("invalid commands container: {err}")))?;
        for (index, row) in commands.into_iter().enumerate() {
            let stored_kind = row
                .get("kind")
                .cloned()
                .ok_or_else(|| {
                    DocError::Schema(format!("invalid command row {index}: missing field `kind`"))
                })
                .and_then(|value| {
                    serde_json::from_value::<crate::commands::SessionCommandKind>(value).map_err(
                        |err| {
                            DocError::Schema(format!(
                                "invalid command row {index}: invalid kind: {err}"
                            ))
                        },
                    )
                })?;
            let entry = serde_json::from_value::<SessionCommandEntry>(row)
                .map_err(|err| DocError::Schema(format!("invalid command row {index}: {err}")))?;
            if stored_kind != entry.kind() {
                return Err(DocError::Schema(format!(
                    "invalid command row {index}: kind does not match payload"
                )));
            }
        }
        Ok(())
    }

    /// Append a command entry (rule 1: own entries only, append-only).
    pub fn queue_command(&self, entry: &SessionCommandEntry) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        let map = commands.push_container(LoroMap::new())?;
        map.insert("id", entry.id.as_str())?;
        map.insert(
            "kind",
            serde_json::to_value(entry.kind())?
                .as_str()
                .ok_or_else(|| DocError::Schema("kind not a string".into()))?,
        )?;
        map.insert(
            "payload",
            loro_value_from_json(&serde_json::to_value(&entry.payload)?),
        )?;
        map.insert("issuedBy", entry.issued_by.as_str())?;
        map.insert("issuedAt", entry.issued_at)?;
        if let Some(based_on) = &entry.based_on {
            map.insert(
                "basedOn",
                loro_value_from_json(&serde_json::to_value(based_on)?),
            )?;
        }
        if let Some(expires_at) = entry.expires_at {
            map.insert("expiresAt", expires_at)?;
        }
        map.insert(
            "status",
            serde_json::to_value(entry.status)?
                .as_str()
                .ok_or_else(|| DocError::Schema("status not a string".into()))?,
        )?;
        self.doc.commit();
        Ok(())
    }

    /// Rule 2: host (or the issuing composer, for `cancelled`) writes an outcome.
    pub fn set_command_status(
        &self,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        for i in 0..commands.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                commands.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == command_id
                );
                if id_matches {
                    map.insert(
                        "status",
                        serde_json::to_value(status)?
                            .as_str()
                            .ok_or_else(|| DocError::Schema("status not a string".into()))?,
                    )?;
                    if let Some(r) = resolution {
                        map.insert("resolution", r)?;
                    }
                    self.doc.commit();
                    return Ok(());
                }
            }
        }
        Err(DocError::Schema(format!("command {command_id} not found")))
    }

    /// Stamp a terminal status on an existing message entry by id (recovery:
    /// abandoned `streaming` entries from a dead run are stamped `aborted`).
    /// Returns `false` when no entry with that id exists.
    pub fn set_message_status(
        &self,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
                );
                if id_matches {
                    map.insert("status", status_str(status))?;
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Append an error part to an existing entry (crash recovery: the aborted
    /// entry must SAY why it ended — "Run interrupted by engine restart…" —
    /// not just truncate silently). Returns `false` when no entry matches.
    pub fn append_error_part(
        &self,
        message_id: &str,
        part_id: &str,
        message: &str,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let id_matches = matches!(
                entry.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
            );
            if !id_matches {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            // Idempotent per part id (recovery may re-run on a crash loop).
            for j in 0..parts.len() {
                if let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                    && matches!(
                        part.get("id"),
                        Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == part_id
                    )
                {
                    return Ok(true);
                }
            }
            push_part(
                &parts,
                &MessagePart::Error {
                    id: part_id.to_string(),
                    message: message.to_string(),
                },
            )?;
            self.doc.commit();
            return Ok(true);
        }
        Ok(false)
    }

    /// Mark the input part carrying `request_id` resolved, wherever it lives
    /// (input parts store the request id as their part id). The live-run path
    /// resolves through the entry fold; this direct write is for answers to a
    /// question whose run already died — no fold owns the entry anymore.
    /// Returns `false` when no such part exists.
    pub fn resolve_input(&self, request_id: &str) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                let is_input = matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "input"
                );
                let id_matches = matches!(
                    part.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == request_id
                );
                if is_input && id_matches {
                    part.insert("resolved", true)?;
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Export a snapshot (persistence) — `ExportMode::Snapshot`.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }
}

fn write_entry_scalar_fields(map: &LoroMap, entry: &SessionMessageEntry) -> Result<(), DocError> {
    map.insert("id", entry.id.as_str())?;
    map.insert(
        "role",
        match entry.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        },
    )?;
    map.insert("createdAt", entry.created_at)?;
    map.insert("deviceId", entry.device_id.as_str())?;
    if let Some(status) = entry.status {
        map.insert("status", status_str(status))?;
    }
    if let Some(continuation_of) = &entry.continuation_of {
        map.insert("continuationOf", continuation_of.as_str())?;
    }
    Ok(())
}

fn status_str(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Streaming => "streaming",
        MessageStatus::Complete => "complete",
        MessageStatus::Aborted => "aborted",
    }
}

/// Append one part map to a parts list; text bodies become LoroText containers.
fn push_part(parts: &LoroList, part: &MessagePart) -> Result<(), DocError> {
    let map = parts.push_container(LoroMap::new())?;
    let doc_part = to_doc_part(part)?;
    map.insert("id", doc_part.id.as_str())?;
    map.insert("kind", doc_part.kind.as_str())?;
    if let Some(text) = &doc_part.text {
        let t = map.insert_container("text", LoroText::new())?;
        t.insert(0, text)?;
    }
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(approval) = &doc_part.approval {
        map.insert("approval", loro_value_from_json(approval))?;
    }
    if let Some(decision) = &doc_part.decision {
        map.insert("decision", loro_value_from_json(decision))?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(notice_kind) = doc_part.notice_kind {
        map.insert(
            "noticeKind",
            serde_json::to_value(notice_kind)?
                .as_str()
                .ok_or_else(|| DocError::Schema("noticeKind not a string".into()))?,
        )?;
    }
    if let Some(severity) = doc_part.severity {
        map.insert(
            "severity",
            serde_json::to_value(severity)?
                .as_str()
                .ok_or_else(|| DocError::Schema("severity not a string".into()))?,
        )?;
    }
    if let Some(summary) = &doc_part.summary {
        map.insert("summary", summary.as_str())?;
    }
    if let Some(detail) = &doc_part.detail {
        map.insert("detail", detail.as_str())?;
    }
    if let Some(key) = &doc_part.key {
        map.insert("key", key.as_str())?;
    }
    if let Some(occurrences) = doc_part.occurrences {
        map.insert("occurrences", occurrences as i64)?;
    }
    Ok(())
}

fn entry_from_json(v: serde_json::Value) -> Result<SessionMessageEntry, DocError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawEntry {
        id: String,
        role: MessageRole,
        #[serde(default)]
        parts: Vec<DocPartJson>,
        created_at: i64,
        device_id: String,
        #[serde(default)]
        status: Option<MessageStatus>,
        #[serde(default)]
        continuation_of: Option<String>,
    }
    let raw: RawEntry = serde_json::from_value(v)?;
    Ok(SessionMessageEntry {
        id: raw.id,
        role: raw.role,
        parts: raw.parts.into_iter().map(from_doc_part).collect(),
        created_at: raw.created_at,
        device_id: raw.device_id,
        status: raw.status,
        continuation_of: raw.continuation_of,
    })
}

fn validate_message_parts(row: &serde_json::Value) -> Result<(), DocError> {
    let parts = row
        .get("parts")
        .cloned()
        .ok_or_else(|| DocError::Schema("missing field `parts`".into()))?;
    let parts: Vec<DocPartJson> = serde_json::from_value(parts)?;
    for (index, part) in parts.into_iter().enumerate() {
        let invalid = |reason: &str| {
            DocError::Schema(format!("invalid part {index} ({}): {reason}", part.kind))
        };
        match part.kind.as_str() {
            "text" if part.text.is_some() => {}
            "tool" => {
                let call = part.call.ok_or_else(|| invalid("missing call"))?;
                serde_json::from_value::<comet_proto::ToolCall>(call)
                    .map_err(|err| invalid(&format!("invalid call: {err}")))?;
            }
            "input" => {
                let questions = part.questions.ok_or_else(|| invalid("missing questions"))?;
                serde_json::from_value::<Vec<comet_proto::UserInputQuestion>>(questions)
                    .map_err(|err| invalid(&format!("invalid questions: {err}")))?;
                if part.resolved.is_none() {
                    return Err(invalid("missing resolved"));
                }
            }
            "approval" => {
                let approval = part.approval.ok_or_else(|| invalid("missing approval"))?;
                serde_json::from_value::<comet_proto::ApprovalRequest>(approval)
                    .map_err(|err| invalid(&format!("invalid approval: {err}")))?;
                // A corrupt decision would pass silently otherwise:
                // `from_doc_part` degrades an undecodable one to None, so a
                // DENIED approval would read back as still open. No presence
                // check — absent is this kind's open state.
                if let Some(decision) = part.decision {
                    serde_json::from_value::<comet_proto::ApprovalDecision>(decision)
                        .map_err(|err| invalid(&format!("invalid decision: {err}")))?;
                }
            }
            "error" if part.message.is_some() => {}
            "notice" if part.summary.is_some() => {}
            "text" => return Err(invalid("missing text")),
            "error" => return Err(invalid("missing message")),
            "notice" => return Err(invalid("missing summary")),
            _ => return Err(invalid("unknown part kind")),
        }
    }
    Ok(())
}

/// Render-time continuation join at the entry level (`joinContinuations` in TS):
/// concatenate continuation entries' parts onto their root, in list order.
pub fn join_continuation_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    if !entries.iter().any(|e| e.continuation_of.is_some()) {
        return entries;
    }
    let mut out: Vec<SessionMessageEntry> = Vec::with_capacity(entries.len());
    let mut root_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in entries {
        match &entry.continuation_of {
            Some(root_id) => {
                if let Some(&at) = root_index.get(root_id) {
                    out[at].parts.extend(entry.parts);
                } else {
                    // Orphan continuation — surface as its own entry rather than dropping.
                    out.push(entry);
                }
            }
            None => {
                root_index.insert(entry.id.clone(), out.len());
                out.push(entry);
            }
        }
    }
    out
}

/// Incremental streaming writer for one assistant entry.
///
/// Port of comet's `DocSegmentWriter` diff discipline: called with the *folded* parts of the
/// live segment (from `fold_event_into_parts`) at each commit tick, it diffs against what's in
/// the doc and writes only the delta:
/// - trailing text growth → `LoroText` append (RLE-merged),
/// - new parts → pushed,
/// - tool call refresh / resolution / input resolution → in-place map updates.
///
/// Invariant relied upon: the fold only ever APPENDS parts or grows the trailing text; earlier
/// text never mutates. Tool/input parts may update fields in place.
pub struct SegmentWriter<'a> {
    doc: &'a SessionDoc,
    /// Index of this entry in the `messages` list.
    entry_index: usize,
    /// Mirror of what we've written so far (part id → app part).
    written: Vec<MessagePart>,
}

impl<'a> SegmentWriter<'a> {
    /// Begin a streaming assistant entry: pushes the entry with `status: streaming`, no parts.
    pub fn begin(
        doc: &'a SessionDoc,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<Self, DocError> {
        let messages = doc.doc.get_list("messages");
        let entry_index = messages.len();
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(
            &map,
            &SessionMessageEntry {
                id: entry_id.into(),
                role: MessageRole::Assistant,
                parts: vec![],
                created_at,
                device_id: device_id.into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            },
        )?;
        map.insert_container("parts", LoroList::new())?;
        doc.doc.commit();
        Ok(Self {
            doc,
            entry_index,
            written: Vec::new(),
        })
    }

    fn entry_map(&self) -> Result<LoroMap, DocError> {
        let messages = self.doc.doc.get_list("messages");
        match messages.get(self.entry_index) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
            _ => Err(DocError::Schema("streaming entry map missing".into())),
        }
    }

    fn parts_list(&self) -> Result<LoroList, DocError> {
        match self.entry_map()?.get("parts") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => Ok(list),
            _ => Err(DocError::Schema(
                "streaming entry parts list missing".into(),
            )),
        }
    }

    /// Diff `folded` (the full folded segment so far) into the doc.
    pub fn sync(&mut self, folded: &[MessagePart]) -> Result<(), DocError> {
        let parts = self.parts_list()?;
        let mut dirty = false;

        for (i, part) in folded.iter().enumerate() {
            match self.written.get(i) {
                None => {
                    push_part(&parts, part)?;
                    self.written.push(part.clone());
                    dirty = true;
                }
                Some(prev) if prev == part => {}
                Some(prev) => {
                    match (prev, part) {
                        (
                            MessagePart::Text { text: old, .. },
                            MessagePart::Text { text: new, .. },
                        ) if new.starts_with(old.as_str()) => {
                            // Trailing-text growth: append the suffix into the LoroText.
                            let delta = &new[old.len()..];
                            if !delta.is_empty() {
                                let part_map = part_map_at(&parts, i)?;
                                match part_map.get("text") {
                                    Some(loro::ValueOrContainer::Container(
                                        loro::Container::Text(t),
                                    )) => {
                                        let len = t.len_unicode();
                                        t.insert(len, delta)?;
                                    }
                                    _ => {
                                        return Err(DocError::Schema(
                                            "text part missing LoroText".into(),
                                        ));
                                    }
                                }
                                dirty = true;
                            }
                        }
                        _ => {
                            // Field-level update (tool refresh/resolve, input resolve, or a
                            // non-append text rewrite, which the fold shouldn't produce —
                            // rewrite the part map fields defensively).
                            let part_map = part_map_at(&parts, i)?;
                            update_part_fields(&part_map, part)?;
                            dirty = true;
                        }
                    }
                    self.written[i] = part.clone();
                }
            }
        }

        if dirty {
            self.doc.doc.commit();
        }
        Ok(())
    }

    /// Finish the stream: sync final parts and stamp a terminal status.
    pub fn finish(mut self, folded: &[MessagePart], status: MessageStatus) -> Result<(), DocError> {
        self.sync(folded)?;
        let map = self.entry_map()?;
        map.insert("status", status_str(status))?;
        self.doc.doc.commit();
        Ok(())
    }
}

fn part_map_at(parts: &LoroList, index: usize) -> Result<LoroMap, DocError> {
    match parts.get(index) {
        Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
        _ => Err(DocError::Schema(format!("part map missing at {index}"))),
    }
}

/// In-place field refresh for tool/input parts (and defensive text rewrite).
fn update_part_fields(map: &LoroMap, part: &MessagePart) -> Result<(), DocError> {
    let doc_part = to_doc_part(part)?;
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(approval) = &doc_part.approval {
        map.insert("approval", loro_value_from_json(approval))?;
    }
    if let Some(decision) = &doc_part.decision {
        map.insert("decision", loro_value_from_json(decision))?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(notice_kind) = doc_part.notice_kind {
        map.insert(
            "noticeKind",
            serde_json::to_value(notice_kind)?
                .as_str()
                .ok_or_else(|| DocError::Schema("noticeKind not a string".into()))?,
        )?;
    }
    if let Some(severity) = doc_part.severity {
        map.insert(
            "severity",
            serde_json::to_value(severity)?
                .as_str()
                .ok_or_else(|| DocError::Schema("severity not a string".into()))?,
        )?;
    }
    if let Some(summary) = &doc_part.summary {
        map.insert("summary", summary.as_str())?;
    }
    // NULLABLE notice fields: set-or-clear, not insert-only. Audit of the
    // rest: `noticeKind`/`severity`/`summary`/`occurrences` are always `Some`
    // for a notice, `call`/`questions` always `Some` for their kind,
    // `approval` always `Some` for an approval, `resolved` always `Some` for
    // an input, and `isError`/`decision` only ever go absent → present (tool
    // resolution and approval resolution are both monotonic — an approval
    // that has been answered or expired never returns to open). `detail` and
    // `key` are the only two an in-place refresh can legitimately clear.
    set_or_clear(map, "detail", doc_part.detail.as_deref())?;
    set_or_clear(map, "key", doc_part.key.as_deref())?;
    if let Some(occurrences) = doc_part.occurrences {
        map.insert("occurrences", occurrences as i64)?;
    }
    if let Some(text) = &doc_part.text {
        // Defensive path only — the fold never rewrites earlier text.
        if let Some(loro::ValueOrContainer::Container(loro::Container::Text(t))) = map.get("text") {
            t.update(text, Default::default())
                .map_err(|e| DocError::Schema(e.to_string()))?;
        }
    }
    Ok(())
}

/// Write `value` under `key`, or REMOVE the key when `value` is `None`.
///
/// An in-place refresh can *clear* a nullable field, not just change it: the
/// notice fold collapses a repeat into the trailing part and refreshes
/// `detail` from the newest event, including `Some(detail)` → `None` (a failing
/// MCP server that comes back ready). Insert-only refresh left the old value
/// in the doc permanently — the writer's mirror moves to the new part either
/// way, so the divergence never self-heals, and the stale detail would follow
/// the doc to every LAN peer and across restarts.
fn set_or_clear(map: &LoroMap, key: &str, value: Option<&str>) -> Result<(), DocError> {
    match value {
        Some(v) => map.insert(key, v)?,
        // Only delete a key that is actually present: this runs for every part
        // kind, and most kinds legitimately have no `detail`/`key` at all.
        None if map.get(key).is_some() => map.delete(key)?,
        None => {}
    }
    Ok(())
}

fn loro_value_from_json(v: &serde_json::Value) -> LoroValue {
    LoroValue::from(v.clone())
}

/// Tail sidecar shape (`SessionTail` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTail {
    pub chat_id: String,
    pub schema_version: u32,
    pub messages: Vec<SessionMessageEntry>,
    pub total_messages: usize,
    pub updated_at: i64,
}

/// Materialize the last-N joined messages (`materializeTail` in TS).
pub fn materialize_tail(
    doc: &SessionDoc,
    now: i64,
    tail_count: usize,
) -> Result<SessionTail, DocError> {
    let all = join_continuation_entries(doc.read_entries()?);
    let total = all.len();
    let start = total.saturating_sub(if tail_count == 0 {
        TAIL_MESSAGE_COUNT
    } else {
        tail_count
    });
    Ok(SessionTail {
        chat_id: doc.chat_id().unwrap_or_default(),
        schema_version: SESSION_SCHEMA_VERSION,
        messages: all[start..].to_vec(),
        total_messages: total,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::fold_event_into_parts;
    use comet_proto::{AgentEvent, ApprovalDecision, ApprovalRequest, ToolCall};

    fn user_entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn round_trips_message_entries() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");
        assert_eq!(
            entries[0].parts,
            vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into()
            }]
        );
        assert_eq!(doc.chat_id().as_deref(), Some("chat-1"));
    }

    #[test]
    fn resolve_input_stamps_the_part_in_place() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: "r1".into(),
                request_id: "r1".into(),
                questions: vec![],
                resolved: false,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            // The orphan case: the run died and recovery stamped the entry.
            status: Some(MessageStatus::Aborted),
            continuation_of: None,
        })
        .unwrap();
        assert!(!doc.resolve_input("nope").unwrap());
        assert!(doc.resolve_input("r1").unwrap());
        let entries = doc.read_entries().unwrap();
        assert!(matches!(
            &entries[0].parts[0],
            MessagePart::Input { resolved: true, .. }
        ));
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let bytes = doc.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = SessionDoc::from_doc(other);
        assert_eq!(
            restored.read_entries().unwrap(),
            doc.read_entries().unwrap()
        );
    }

    #[test]
    fn two_peers_converge_on_concurrent_inserts() {
        let a = SessionDoc::init("chat-1").unwrap();
        let b = SessionDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        a.push_message(&user_entry("m-a", "from a")).unwrap();
        b.push_message(&user_entry("m-b", "from b")).unwrap();

        // Cross-import updates.
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();

        let ea = a.read_entries().unwrap();
        let eb = b.read_entries().unwrap();
        assert_eq!(ea, eb);
        assert_eq!(ea.len(), 2); // one insert from each peer, converged in the same order
    }

    #[test]
    fn an_open_approval_part_round_trips_through_the_doc() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Approval {
                // The live fold's prefixed id; the write normalizes it away.
                id: "ap-r1".into(),
                request_id: "r1".into(),
                approval: ApprovalRequest::Command {
                    command: "cargo test".into(),
                    cwd: None,
                },
                decision: None,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        })
        .unwrap();
        let parts = doc.read_entries().unwrap()[0].parts.clone();
        assert!(matches!(
            &parts[0],
            MessagePart::Approval { id, request_id, decision: None, .. }
                if id == "r1" && request_id == "r1"
        ));
        doc.validate_strict().unwrap();
    }

    #[test]
    fn a_resolved_approval_part_keeps_its_decision_through_the_doc() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Approval {
                id: "ap-r1".into(),
                request_id: "r1".into(),
                approval: ApprovalRequest::FileRead {
                    path: "a.rs".into(),
                },
                decision: Some(ApprovalDecision::Deny {
                    message: "not that file".into(),
                }),
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        assert!(matches!(
            &doc.read_entries().unwrap()[0].parts[0],
            MessagePart::Approval {
                decision: Some(ApprovalDecision::Deny { message }),
                ..
            } if message == "not that file"
        ));
    }

    #[test]
    fn a_decision_stamped_mid_stream_reaches_the_doc() {
        // The live path: the decision lands as a field update on an ALREADY
        // WRITTEN part, which is `update_part_fields`, not `push_part`.
        // Without that writer the stamp exists only in the in-memory
        // accumulator and no push-driven test would notice.
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ApprovalRequested {
                request_id: "r1".into(),
                approval: ApprovalRequest::FileRead {
                    path: "a.rs".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ApprovalResolved {
                request_id: "r1".into(),
                decision: ApprovalDecision::Allow,
            },
        );
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        assert!(matches!(
            &doc.read_entries().unwrap()[0].parts[0],
            MessagePart::Approval {
                decision: Some(ApprovalDecision::Allow),
                ..
            }
        ));
    }

    #[test]
    fn segment_writer_streams_text_incrementally() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "Hel".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "lo".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "tool-1".into(),
                is_error: false,
            },
        );
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Some(MessageStatus::Complete));
        assert_eq!(entries[0].parts.len(), 2);
        match &entries[0].parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("unexpected {other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Tool {
                resolved, is_error, ..
            } => {
                assert!(*resolved);
                assert!(!*is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// REGRESSION: an in-place notice refresh that CLEARS a nullable field
    /// must clear it in the doc too. `update_part_fields` was insert-only, so
    /// a failing MCP server coming back ready collapsed into a part whose
    /// persisted `detail` still held the old connection error — forever, since
    /// the writer's mirror advances to the new part and never re-diffs it.
    #[test]
    fn notice_refresh_clears_nullable_fields_through_the_doc() {
        use comet_proto::{NoticeKind, NoticeSeverity};
        let notice = |severity, summary: &str, detail: Option<&str>| AgentEvent::Notice {
            kind: NoticeKind::McpStatus,
            severity,
            summary: summary.into(),
            detail: detail.map(str::to_owned),
            key: Some("mcp:linear".into()),
        };

        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &notice(
                NoticeSeverity::Warning,
                "MCP server linear failed to start",
                Some("connect ECONNREFUSED 127.0.0.1:3845"),
            ),
        );
        writer.sync(&folded).unwrap();

        // Same kind, same key, trailing part → collapse, with detail cleared.
        fold_event_into_parts(
            &mut folded,
            &notice(NoticeSeverity::Info, "MCP server linear is ready", None),
        );
        assert_eq!(
            folded.len(),
            1,
            "the repeat collapsed into the trailing part"
        );
        writer.sync(&folded).unwrap();

        match &doc.read_entries().unwrap()[0].parts[0] {
            MessagePart::Notice {
                severity,
                summary,
                detail,
                key,
                occurrences,
                ..
            } => {
                assert_eq!(*severity, NoticeSeverity::Info);
                assert_eq!(summary, "MCP server linear is ready");
                assert_eq!(*detail, None, "stale detail survived the collapse");
                assert_eq!(key.as_deref(), Some("mcp:linear"));
                assert_eq!(*occurrences, 2);
            }
            other => panic!("unexpected {other:?}"),
        }

        // `key` is the other nullable notice field. Collapse can't clear it
        // (a differing key is what *prevents* collapse), so drive the writer
        // directly — the doc must still drop the key rather than keep it.
        let MessagePart::Notice { id, kind, .. } = &folded[0] else {
            panic!("notice part")
        };
        let keyless = vec![MessagePart::Notice {
            id: id.clone(),
            kind: *kind,
            severity: NoticeSeverity::Info,
            summary: "MCP server linear is ready".into(),
            detail: None,
            key: None,
            occurrences: 2,
        }];
        writer.sync(&keyless).unwrap();
        writer.finish(&keyless, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].status, Some(MessageStatus::Complete));
        match &entries[0].parts[0] {
            MessagePart::Notice { detail, key, .. } => {
                assert_eq!(*detail, None);
                assert_eq!(*key, None, "stale key survived the refresh");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn set_message_status_stamps_existing_entry() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("m1", "hello");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        doc.push_message(&entry).unwrap();

        assert!(
            doc.set_message_status("m1", MessageStatus::Aborted)
                .unwrap()
        );
        assert!(
            !doc.set_message_status("nope", MessageStatus::Aborted)
                .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].status, Some(MessageStatus::Aborted));
    }

    #[test]
    fn command_queue_and_outcome_round_trip() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        doc.queue_command(&entry).unwrap();
        doc.set_command_status("c1", SessionCommandStatus::Applied, None)
            .unwrap();
        let commands = doc.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, SessionCommandStatus::Applied);
        assert_eq!(commands[0].payload, entry.payload);
    }

    #[test]
    fn strict_validation_rejects_malformed_message_rows() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let row = doc
            .doc()
            .get_list("messages")
            .push_container(LoroMap::new())
            .unwrap();
        row.insert("role", "user").unwrap();
        doc.doc().commit();

        assert!(doc.read_entries().unwrap().is_empty());
        let err = doc.validate_strict().unwrap_err().to_string();
        assert!(err.contains("invalid message row 0"));
    }

    #[test]
    fn strict_validation_rejects_malformed_command_rows() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.doc()
            .get_list("commands")
            .push_container(LoroMap::new())
            .unwrap();
        doc.doc().commit();

        assert!(doc.read_commands().unwrap().is_empty());
        let err = doc.validate_strict().unwrap_err().to_string();
        assert!(err.contains("invalid command row 0"));
    }

    #[test]
    fn strict_validation_accepts_typed_session_rows() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        doc.queue_command(&SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .unwrap();

        doc.validate_strict().unwrap();
    }

    #[test]
    fn strict_validation_rejects_command_kind_mismatch() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.queue_command(&SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .unwrap();
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) =
            doc.doc().get_list("commands").get(0)
        else {
            panic!("command row missing");
        };
        row.insert("kind", "interrupt").unwrap();
        doc.doc().commit();

        let err = doc.validate_strict().unwrap_err().to_string();
        assert!(err.contains("kind does not match payload"));
    }

    #[test]
    fn tail_materializes_last_n_joined() {
        let doc = SessionDoc::init("chat-1").unwrap();
        for i in 0..5 {
            doc.push_message(&user_entry(&format!("m{i}"), &format!("msg {i}")))
                .unwrap();
        }
        let tail = materialize_tail(&doc, 99, 2).unwrap();
        assert_eq!(tail.total_messages, 5);
        assert_eq!(tail.messages.len(), 2);
        assert_eq!(tail.messages[1].id, "m4");
        assert_eq!(tail.chat_id, "chat-1");
    }

    fn notice_part(id: &str, summary: &str, occurrences: u32) -> MessagePart {
        MessagePart::Notice {
            id: id.into(),
            kind: comet_proto::NoticeKind::Retrying,
            severity: comet_proto::NoticeSeverity::Warning,
            summary: summary.into(),
            detail: Some("Next attempt in 2s.".into()),
            key: Some("retry".into()),
            occurrences,
        }
    }

    #[test]
    fn notice_part_round_trips_through_the_doc() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![notice_part("n0", "Retrying — attempt 1 of 3", 3)],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(
            entries[0].parts,
            vec![notice_part("n0", "Retrying — attempt 1 of 3", 3)]
        );
        // Strict validation (the pre-migration reader) accepts notice rows.
        doc.validate_strict().unwrap();
    }

    /// A notice row written WITHOUT `occurrences` (a build predating collapse,
    /// or a hand-rolled peer) reads back as 1 — never a decode failure.
    #[test]
    fn notice_row_without_occurrences_reads_as_one() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let row = doc
            .doc()
            .get_list("messages")
            .push_container(LoroMap::new())
            .unwrap();
        row.insert("id", "m1").unwrap();
        row.insert("role", "assistant").unwrap();
        row.insert("createdAt", 1i64).unwrap();
        row.insert("deviceId", "dev-a").unwrap();
        let parts = row.insert_container("parts", LoroList::new()).unwrap();
        let part = parts.push_container(LoroMap::new()).unwrap();
        part.insert("id", "n0").unwrap();
        part.insert("kind", "notice").unwrap();
        part.insert("noticeKind", "compaction").unwrap();
        part.insert("severity", "info").unwrap();
        part.insert("summary", "Context compacted automatically")
            .unwrap();
        doc.doc().commit();

        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Notice {
                occurrences,
                kind,
                severity,
                ..
            } => {
                assert_eq!(*occurrences, 1);
                assert_eq!(*kind, comet_proto::NoticeKind::Compaction);
                assert_eq!(*severity, comet_proto::NoticeSeverity::Info);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// An entry written by a build WITHOUT notices must be byte-identical
    /// after this slice: the new DocPartJson fields are Option +
    /// skip_serializing_if, so a plain text part's map carries exactly the
    /// keys it always did.
    #[test]
    fn pre_notice_part_maps_carry_no_new_keys() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let value = doc
            .doc()
            .get_list("messages")
            .get_deep_value()
            .to_json_value();
        let part = &value[0]["parts"][0];
        let keys: Vec<&str> = part
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 3, "unexpected keys: {keys:?}");
        for key in ["id", "kind", "text"] {
            assert!(keys.contains(&key), "missing {key}: {keys:?}");
        }
    }

    /// Strict validation names the missing field for a summary-less notice.
    #[test]
    fn strict_validation_rejects_a_notice_without_summary() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let row = doc
            .doc()
            .get_list("messages")
            .push_container(LoroMap::new())
            .unwrap();
        row.insert("id", "m1").unwrap();
        row.insert("role", "assistant").unwrap();
        row.insert("createdAt", 1i64).unwrap();
        row.insert("deviceId", "dev-a").unwrap();
        let parts = row.insert_container("parts", LoroList::new()).unwrap();
        let part = parts.push_container(LoroMap::new()).unwrap();
        part.insert("id", "n0").unwrap();
        part.insert("kind", "notice").unwrap();
        doc.doc().commit();

        let err = doc.validate_strict().unwrap_err().to_string();
        assert!(err.contains("missing summary"), "{err}");
    }
}
