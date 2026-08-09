//! Agent-side wire types: harness identity, run requests, streaming events, tool calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    Cursor,
    /// Test harness; never shown in production pickers.
    Mock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// xhigh + harness-specific setting.
    Ultracode,
    /// Prompt-prefix driven (Claude).
    Ultrathink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// How much a run may do without asking.
///
/// For a user session the sandbox is not a separate choice: each mode names
/// the one it implies, and [`RunRequest::for_session`] applies it, so those
/// call sites cannot pair a permissive mode with a restrictive sandbox by
/// accident.
///
/// That is a property of the constructor, not of the type. `RunRequest`
/// carries the two separately on purpose, because chat titling needs a
/// never-ask mode with a read-only sandbox — a pairing no mode expresses.
/// Anything reading a request's sandbox must read the field, not
/// `runtime_mode.sandbox()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// Every tool call is asked about first.
    ApprovalRequired,
    /// Edits inside the workspace proceed; the sandbox is the boundary.
    ///
    /// The default because it is what every chat has already been running:
    /// a workspace-write sandbox with nothing able to block on a question.
    #[default]
    AutoAcceptEdits,
    /// As above, with the provider reviewing its own calls where it can.
    Auto,
    /// No sandbox and no approvals.
    FullAccess,
}

impl RuntimeMode {
    /// The sandbox this mode implies.
    pub fn sandbox(self) -> SandboxLevel {
        match self {
            RuntimeMode::ApprovalRequired => SandboxLevel::ReadOnly,
            RuntimeMode::AutoAcceptEdits | RuntimeMode::Auto => SandboxLevel::WorkspaceWrite,
            RuntimeMode::FullAccess => SandboxLevel::DangerFullAccess,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    /// Steer delivered at the next step boundary within the live turn.
    StepBoundary,
    /// Steer delivered only between turns. The default because it is the
    /// conservative boundary: an unspecified mode never injects into a turn
    /// that is already running.
    #[default]
    TurnBoundary,
}

/// What a harness can honor, declared once per harness.
///
/// The engine's `HarnessDescriptor` used to re-state these values by hand for
/// each lazily-registered slot, so the catalog could change the moment a
/// harness resolved. Owning them here lets the registry name the *same*
/// expression the trait returns.
///
/// **Every** field is serde-defaulted, including the ones present today. Later
/// slices add capabilities (permission modes, supervised approval, image
/// modality), and a descriptor that arrives missing a field must degrade to the
/// conservative value rather than failing to decode — a single unparseable
/// entry would otherwise reject the whole `ListHarnesses` reply.
///
/// The defaults live on the field types, so `Default` is derived rather than
/// written out: "conservative" is stated once per field, not twice.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HarnessCapabilities {
    /// Whether a steer mid-run is accepted at all.
    pub supports_steering: bool,
    /// Where an accepted steer is delivered. Only meaningful when
    /// `supports_steering`; see [`SteeringMode::TurnBoundary`] for why the
    /// default is the boundary that never injects into a live turn.
    pub steering_mode: SteeringMode,
    /// The effort ladder offered in the traits picker.
    pub reasoning_levels: Vec<ReasoningLevel>,
}

/// Whether a harness's CLI is usable on this device, as of the last probe.
///
/// This is deliberately **not** part of [`HarnessCapabilities`]. Capabilities
/// are static per harness — declared once by an associated `capabilities()`
/// that the engine's lazy descriptor and the resolved harness both name, which
/// is what makes drift between them unrepresentable. Availability is the
/// opposite: it is discovered at run time, changes with the device, and is
/// published asynchronously. Folding it in would either break that equality or
/// make it vacuous.
///
/// [`Unknown`] is the default and means *not probed yet*, never *broken*. An
/// unprobed harness must stay selectable — presenting a working provider as
/// unusable because a background probe has not landed is worse than the delay.
///
/// [`Unknown`]: HarnessAvailability::Unknown
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum HarnessAvailability {
    /// No probe has completed. Selectable; renders exactly like an available
    /// harness, because we do not yet know otherwise.
    #[default]
    Unknown,
    /// The CLI resolved and answered `--version`.
    Available {
        /// Whatever the CLI reported, when a version could be read out of it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// The CLI is missing or did not answer.
    ///
    /// Split into two fields rather than one prose blob, because the single
    /// `reason` string this replaces was unusable in the UI: it concatenated a
    /// diagnostic inventory of every searched location ahead of the one clause
    /// worth reading, so the actionable half landed five lines down in the
    /// picker and was the first thing any truncation dropped. `summary` is a
    /// short label a row can show *without* hover; `hint` is the single
    /// actionable sentence. The searched-location inventory is diagnostic and
    /// belongs in the log, not on a surface a user reads.
    Unavailable {
        /// Short label, e.g. `"Not installed"`. The row names the agent, so
        /// this must not repeat it.
        #[serde(default = "unavailable_summary_fallback")]
        summary: String,
        /// One sentence naming what to do about it, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        /// Compatibility only: `summary` and `hint` rejoined, for peers built
        /// before the split.
        ///
        /// `ListHarnesses` is in `remote_method_allowed`, so a LAN-paired
        /// machine on an older build decodes this payload — and its `reason`
        /// is a REQUIRED field. Omitting it fails the whole
        /// `Vec<HarnessDescriptor>` decode, which blanks that machine's entire
        /// agent catalog rather than degrading one row. Same all-or-nothing
        /// blast radius the capabilities slice was reviewed for, in the other
        /// direction.
        ///
        /// Never read by this build — always derive it through
        /// [`HarnessAvailability::unavailable`] rather than setting it, and
        /// delete the field once no peer predates the split.
        #[serde(default)]
        reason: String,
    },
}

/// Decoding fallback for a payload written before `Unavailable` was split.
///
/// Decoding a `HarnessDescriptor` vector is all-or-nothing: one strict field
/// rejects the whole `ListHarnesses` answer and blanks *every* harness, not
/// just the odd one out (the blast radius found in review on the capabilities
/// slice). A peer still sending `{"state":"unavailable","reason":"…"}` must
/// therefore degrade to a usable label rather than fail the batch.
fn unavailable_summary_fallback() -> String {
    "Unavailable".to_string()
}

impl HarnessAvailability {
    /// Build an [`Unavailable`], deriving the compatibility `reason` from the
    /// two halves. The only sanctioned way to construct one — setting the
    /// fields directly lets `reason` drift out of step with what is displayed.
    ///
    /// [`Unavailable`]: HarnessAvailability::Unavailable
    pub fn unavailable(summary: impl Into<String>, hint: Option<String>) -> Self {
        let summary = summary.into();
        let reason = match &hint {
            Some(hint) => format!("{summary}. {hint}"),
            None => summary.clone(),
        };
        Self::Unavailable {
            summary,
            hint,
            reason,
        }
    }

    /// Whether this harness is known to be unusable.
    ///
    /// `Unknown` answers `false` alongside `Available`: an unfinished probe is
    /// not evidence of a problem. This is the predicate that dims and blocks a
    /// harness, so a wrong answer here silently disables a working provider.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// The short label for why this harness cannot be used.
    pub fn unavailable_summary(&self) -> Option<&str> {
        match self {
            Self::Unavailable { summary, .. } => Some(summary),
            Self::Unknown | Self::Available { .. } => None,
        }
    }

    /// The actionable sentence, when the failure has one. A harness can be
    /// unavailable with nothing useful to suggest (a CLI that crashed on
    /// `--version`), which is why this is separate from the summary.
    pub fn unavailable_hint(&self) -> Option<&str> {
        match self {
            Self::Unavailable { hint, .. } => hint.as_deref(),
            Self::Unknown | Self::Available { .. } => None,
        }
    }

    /// Whether this harness is still awaiting its probe.
    ///
    /// A client that cached the catalog before the probes landed uses this to
    /// decide the answer is worth asking for again — probing happens after
    /// boot, so a snapshot taken at startup reports `Unknown` for everything.
    pub fn is_unprobed(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    /// An absent `availability` decodes to `Unknown` — an older engine that
    /// does not send the field must not have its harnesses read as broken.
    #[test]
    fn absent_availability_is_unknown_not_unavailable() {
        let decoded: HarnessAvailability = serde_json::from_str(r#"{"state":"unknown"}"#).unwrap();
        assert_eq!(decoded, HarnessAvailability::default());
        assert!(!decoded.is_unavailable());
    }

    /// Only `Unavailable` blocks. This is the predicate the picker dims on, so
    /// a wrong answer here silently disables a working provider.
    #[test]
    fn only_unavailable_reports_a_reason() {
        assert!(!HarnessAvailability::Unknown.is_unavailable());
        assert!(
            !HarnessAvailability::Available {
                version: Some("1.2.3".into())
            }
            .is_unavailable()
        );
        let missing = HarnessAvailability::unavailable(
            "Not installed",
            Some("Set CODEX_EXECUTABLE to the codex binary.".into()),
        );
        assert!(missing.is_unavailable());
        assert_eq!(missing.unavailable_summary(), Some("Not installed"));
        assert_eq!(
            missing.unavailable_hint(),
            Some("Set CODEX_EXECUTABLE to the codex binary.")
        );
    }

    /// A failure with nothing to suggest still has a label. The summary is what
    /// the row renders, so it can never be the empty half of the pair.
    #[test]
    fn a_hintless_failure_still_carries_a_summary() {
        let crashed = HarnessAvailability::unavailable("Did not respond", None);
        assert_eq!(crashed.unavailable_summary(), Some("Did not respond"));
        assert_eq!(crashed.unavailable_hint(), None);
        assert!(crashed.is_unavailable());
    }

    /// The reverse direction of the compatibility problem, and the one that
    /// actually bites: an OLDER peer decoding a NEWER payload. `ListHarnesses`
    /// is remote-allowed, its `reason` was a required field, and a missing
    /// required field fails the whole `Vec<HarnessDescriptor>` — so the older
    /// machine's entire agent catalog goes blank, not one row.
    #[test]
    fn an_older_peer_still_finds_the_reason_field() {
        let value = serde_json::to_value(HarnessAvailability::unavailable(
            "Not installed",
            Some("Install codex, or set CODEX_EXECUTABLE to its path.".into()),
        ))
        .unwrap();
        let reason = value
            .get("reason")
            .and_then(|r| r.as_str())
            .expect("the compatibility field must be emitted, not skipped");
        // Both halves survive in it, in reading order, so an old client's
        // single-string render is no worse than what it showed before.
        assert!(reason.starts_with("Not installed"), "{reason}");
        assert!(reason.contains("CODEX_EXECUTABLE"), "{reason}");

        // A failure with no hint still emits a non-empty reason: an old client
        // renders that string directly, and an empty one would read as a blank
        // error row.
        let hintless =
            serde_json::to_value(HarnessAvailability::unavailable("Did not respond", None))
                .unwrap();
        assert_eq!(
            hintless.get("reason").and_then(|r| r.as_str()),
            Some("Did not respond")
        );
    }

    /// A peer still sending the pre-split `reason` field must decode to a
    /// usable row rather than failing — the whole `ListHarnesses` vector rides
    /// on this one field, so a hard error would blank every harness at once.
    #[test]
    fn a_pre_split_payload_degrades_instead_of_failing_the_batch() {
        let decoded: HarnessAvailability = serde_json::from_str(
            r#"{"state":"unavailable","reason":"codex (searched PATH; set CODEX_EXECUTABLE)"}"#,
        )
        .expect("an older peer's payload must still decode");
        assert!(decoded.is_unavailable(), "it must still block the harness");
        assert_eq!(decoded.unavailable_summary(), Some("Unavailable"));
        assert_eq!(decoded.unavailable_hint(), None);
    }

    #[test]
    fn availability_round_trips_through_its_tagged_shape() {
        for value in [
            HarnessAvailability::Unknown,
            HarnessAvailability::Available { version: None },
            HarnessAvailability::Available {
                version: Some("2.0.0".into()),
            },
            HarnessAvailability::unavailable(
                "Not installed",
                Some("Set CLAUDE_CODE_EXECUTABLE to the claude binary.".into()),
            ),
            HarnessAvailability::unavailable("Did not respond", None),
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let round: HarnessAvailability = serde_json::from_str(&json).unwrap();
            assert_eq!(round, value, "{json} did not round-trip");
        }
        // An available harness with no readable version omits the key rather
        // than sending `null`.
        let json =
            serde_json::to_string(&HarnessAvailability::Available { version: None }).unwrap();
        assert_eq!(json, r#"{"state":"available"}"#);
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    /// A descriptor missing any capability field decodes to the conservative
    /// value instead of failing. Deserialization is all-or-nothing across the
    /// `ListHarnesses` vector, so one strict field would drop every harness.
    #[test]
    fn absent_capability_fields_fall_back_to_conservative_defaults() {
        let caps: HarnessCapabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(caps, HarnessCapabilities::default());
        assert!(!caps.supports_steering);
        assert_eq!(caps.steering_mode, SteeringMode::TurnBoundary);
        assert!(caps.reasoning_levels.is_empty());

        // A partial payload keeps what it states and defaults the rest.
        let partial: HarnessCapabilities =
            serde_json::from_str(r#"{"supportsSteering":true}"#).unwrap();
        assert!(partial.supports_steering);
        assert_eq!(partial.steering_mode, SteeringMode::TurnBoundary);
    }

    /// The derived `Default` must agree with the field-level serde defaults —
    /// they are the same statement and drifting them apart would mean an
    /// absent field and an explicit `Default::default()` disagreeing.
    #[test]
    fn derived_default_matches_empty_payload() {
        let decoded: HarnessCapabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded, HarnessCapabilities::default());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: String,
    /// Short tagline rendered under the name in the model picker (11px muted),
    /// mirroring the Electron app's `ModelInfo.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// Harness-specific option selections (option id -> choice id), JSON round-tripped.
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub cwd: String,
    /// How much this run may do without asking. The sandbox below is derived
    /// from it for every user session — see [`RunRequest::for_session`].
    ///
    /// Absent on the wire means a request written before the field existed;
    /// it resolves to the default, which is the mode those runs were already
    /// getting.
    #[serde(default)]
    pub runtime_mode: RuntimeMode,
    /// An adapter that needs the sandbox must read this field, not
    /// `runtime_mode.sandbox()`. The two agree for every user session, but
    /// not in general: chat titling pairs a never-ask mode with a read-only
    /// sandbox, a pairing no mode expresses, and that request is built by
    /// hand rather than through [`RunRequest::for_session`].
    pub sandbox: SandboxLevel,
    /// Harness-native session id to resume, if any.
    pub resume: Option<String>,
    /// Absolute paths of image attachments already staged on the run device
    /// (composer uploads: UploadChunk/UploadCommit → durable path). The same
    /// paths also ride the prompt text as `Attached images (local files …)`
    /// refs (comet's `withAttachments` transport — that's what persists in the
    /// doc); this field additionally lets a harness inline the bytes as image
    /// content blocks. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
}

impl RunRequest {
    /// A user-session run, with the sandbox derived from `mode`.
    ///
    /// Use it with struct-update syntax so no call site names a sandbox:
    ///
    /// ```ignore
    /// RunRequest { prompt, cwd, ..RunRequest::for_session(mode) }
    /// ```
    ///
    /// Chat titling is the one caller that does not use this: it needs a
    /// read-only sandbox with nothing able to ask it a question, a pairing no
    /// mode expresses, and it has no surface on which an answer could be given.
    pub fn for_session(mode: RuntimeMode) -> Self {
        Self {
            prompt: String::new(),
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: String::new(),
            runtime_mode: mode,
            sandbox: mode.sandbox(),
            resume: None,
            attachments: Vec::new(),
        }
    }
}

/// A decoded tool invocation, reduced to the fields each kind renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolCall {
    Exec {
        command: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        /// Full content; STRIPPED by the render-parts policy before entering the doc.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    EditFile {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
    },
    ApplyPatch {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    WebFetch {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    Mcp {
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneStatus {
    Completed,
    Interrupted,
    Errored,
}

/// What a provider notice is about. Providers drive this over the wire, so an
/// unknown value must not poison the batch: `#[serde(other)]` degrades it to
/// `Info` — quiet, because we do not know what it is. This IS valid on a
/// plain externally-tagged unit-variant enum (verified empirically; the serde
/// book's wording about internally/adjacently tagged enums reads as a
/// restriction and is not one). Do not rewrite as a hand-written Deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeKind {
    Compaction,
    ModelRerouted,
    Retrying,
    McpStatus,
    AuthStatus,
    RateLimit,
    #[serde(other)]
    Info,
}

/// How loudly a notice paints. Unknown severities degrade LOUD (`Warning`),
/// the opposite direction from `NoticeKind`: a level we cannot interpret is
/// more likely to matter than less, and the cost of over-showing is an amber
/// chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeSeverity {
    Info,
    #[serde(other)]
    Warning,
}

/// How badly a provider frame missed. `Unknown` = the frame decoded fine but
/// is on neither the claimed nor the ignored list. `Malformed` = the line or
/// body never decoded — produced ONLY by the parse-failure sinks; nothing
/// else may use it. `#[serde(other)]` on `Unknown`: an unrecognized severity
/// is, literally, unknown, and must not fail the surrounding batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverity {
    Malformed,
    #[serde(other)]
    Unknown,
}

/// Sanitize a provider-derived frame discriminator. Allowed alphabet
/// `[A-Za-z0-9._/-]`; over-long but clean input truncates to 64 bytes
/// (ASCII-only, so any cut lands on a char boundary); empty input or anything
/// outside the alphabet becomes the literal `"malformed"`.
///
/// The guarantee this gives is narrow: the *output* is always a bounded-length
/// string in an alphabet safe to render and log. It is **not** a path filter,
/// and it does not reject paths in general — only ones containing a byte
/// outside the alphabet. A Windows path fails (the backslash isn't allowed)
/// and becomes `"malformed"`, but a POSIX-style path such as
/// `/home/matty/.ssh/id_rsa` is composed entirely of allowed bytes and passes
/// through completely unchanged. Every current caller feeds this type names
/// and JSON-RPC methods, so that never happens in practice today — but a
/// future caller passing untrusted free text (anything that might contain a
/// path, a secret, or other sensitive prose) must sanitize for its own
/// concerns before calling this; do not rely on this function to do it. The
/// output ends up in a journal, an RPC reply and a settings card — treat the
/// *input* as untrusted regardless. Applied at the harness boundary and
/// again, defensively, by the engine registry.
pub fn sanitize_discriminator(raw: &str) -> String {
    let clean = !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'));
    if !clean {
        return "malformed".to_string();
    }
    raw[..raw.len().min(64)].to_string()
}

/// The normalized streaming event every harness emits.
///
/// Mirrors comet's `AgentEvent` tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    #[serde(rename_all = "camelCase")]
    SessionStarted {
        harness: HarnessId,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
        cwd: String,
        /// Harness-native session id (used for resume).
        session_id: String,
        assistant_message_id: String,
        /// The mode the run was launched under.
        ///
        /// Recorded so a resume can honor it. The journal is the only durable
        /// record of a run whose chat row never landed — a crash can outrun
        /// the debounced workspace write — and without this a resumed run
        /// would silently fall back to the default, which for a chat launched
        /// under a stricter mode means writing where the user asked to be
        /// asked. Absent on the wire means a run recorded before it was
        /// carried; those ran under the default.
        #[serde(default)]
        runtime_mode: RuntimeMode,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Backend-internal steering boundary marker.
    #[serde(rename_all = "camelCase")]
    AssistantMessageCompleted {
        assistant_message_id: String,
    },
    ToolCall {
        id: String,
        call: ToolCall,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        is_error: bool,
    },
    /// Kept as a harness passthrough (rate-limit probes); never persisted to docs.
    #[serde(rename_all = "camelCase")]
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Error {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    InputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    #[serde(rename_all = "camelCase")]
    InputResolved {
        request_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steered {
        assistant_message_id: Option<String>,
        next_assistant_message_id: Option<String>,
    },
    /// A provider notice the user should see in the transcript: compaction,
    /// model reroute, retry backoff, MCP/auth status, rate-limit warnings.
    /// `summary` is one line — Comet copy for structured kinds; provider
    /// prose, capped at the harness boundary, for the passthrough kinds.
    #[serde(rename_all = "camelCase")]
    Notice {
        kind: NoticeKind,
        severity: NoticeSeverity,
        summary: String,
        #[serde(default)]
        detail: Option<String>,
        /// Collapse key — from the wire where the provider gives us one.
        #[serde(default)]
        key: Option<String>,
    },
    /// A provider frame Comet did not recognize (`Unknown`) or could not
    /// parse (`Malformed`). Counted into the engine's per-boot registry and
    /// journaled; NEVER a transcript part, and the payload is never carried —
    /// a frame Comet cannot classify is a frame whose fields it cannot vet.
    /// The full frame goes to `tracing::warn` at the drop site instead.
    #[serde(rename_all = "camelCase")]
    Diagnostic {
        /// Frame `type` / `system/<subtype>` / `control_request/<subtype>` /
        /// JSON-RPC method / `item/<itemType>`, via [`sanitize_discriminator`];
        /// the fixed sentinel `"unparseable"` for parse failures.
        discriminator: String,
        severity: DiagnosticSeverity,
        /// Reserved for structured provider error codes; every current
        /// producer sends `None`. Spec-specified wire shape.
        #[serde(default)]
        code: Option<String>,
        /// Comet copy, never provider text.
        summary: String,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_round_trips() {
        let ev = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    #[test]
    fn run_request_attachments_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.attachments.is_empty());
        // …and an empty list serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("attachments").is_none());
        // Populated lists round-trip.
        let req = RunRequest {
            attachments: vec!["/tmp/a.png".into()],
            ..req
        };
        let round: RunRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(round.attachments, vec!["/tmp/a.png".to_string()]);
    }

    #[test]
    fn run_request_runtime_mode_defaults_when_absent() {
        // A chat created before this field existed: absent is not "unknown", it
        // is the mode that reproduces how the chat has been running.
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert_eq!(req.runtime_mode, RuntimeMode::AutoAcceptEdits);
    }

    #[test]
    fn run_request_runtime_mode_round_trips_on_the_wire() {
        let req = RunRequest {
            prompt: "p".into(),
            cwd: ".".into(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json.get("runtimeMode").unwrap(), "approval-required");
        let round: RunRequest = serde_json::from_value(json).unwrap();
        assert_eq!(round.runtime_mode, RuntimeMode::ApprovalRequired);
    }

    #[test]
    fn for_session_pairs_the_sandbox_with_the_mode() {
        // The whole point of the constructor: a user-session call site cannot
        // name a sandbox that disagrees with its mode.
        for mode in [
            RuntimeMode::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
            RuntimeMode::FullAccess,
        ] {
            let req = RunRequest::for_session(mode);
            assert_eq!(req.runtime_mode, mode);
            assert_eq!(req.sandbox, mode.sandbox());
        }
    }

    #[test]
    fn for_session_default_reproduces_the_previous_hardcode() {
        // The behavioral claim of the runtime-mode work, in one assertion: the
        // derived sandbox equals the literal every user-session site used to
        // write, and the default mode is the never-bypass one.
        let req = RunRequest::for_session(RuntimeMode::default());
        assert_eq!(req.sandbox, SandboxLevel::WorkspaceWrite);
        assert_eq!(req.runtime_mode, RuntimeMode::AutoAcceptEdits);
    }

    /// A peer built before the runtime mode replaced the auto-approve flag
    /// still sends `autoApprove`. It must decode, ignored: the field it stood
    /// in for is now the mode, and every request that carried it `true` was
    /// engine-internal and never crossed the wire.
    #[test]
    fn a_payload_still_sending_auto_approve_decodes() {
        let decoded: RunRequest = serde_json::from_str(
            r#"{"prompt":"hi","model":null,"reasoning":null,"cwd":"/tmp",
                "sandbox":"workspace-write","autoApprove":true,"resume":null}"#,
        )
        .expect("an older peer's payload must still decode");
        assert_eq!(decoded.runtime_mode, RuntimeMode::AutoAcceptEdits);
        assert_eq!(decoded.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn harness_id_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&HarnessId::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
    }

    /// Unknown provider-driven enum values must degrade, not fail: decoding
    /// is all-or-nothing across a containing vector, so one strict value
    /// rejects every element. An unknown KIND degrades quiet (`Info` — we do
    /// not dress up what we cannot name); an unknown SEVERITY degrades loud
    /// (`Warning` — a level we cannot interpret is more likely to matter).
    #[test]
    fn unknown_notice_kind_and_severity_degrade_instead_of_failing() {
        assert_eq!(
            serde_json::from_str::<NoticeKind>("\"compaction\"").unwrap(),
            NoticeKind::Compaction
        );
        assert_eq!(
            serde_json::from_str::<NoticeKind>("\"modelRerouted\"").unwrap(),
            NoticeKind::ModelRerouted
        );
        assert_eq!(
            serde_json::from_str::<NoticeKind>("\"someFutureKind\"").unwrap(),
            NoticeKind::Info
        );
        assert_eq!(
            serde_json::from_str::<NoticeSeverity>("\"info\"").unwrap(),
            NoticeSeverity::Info
        );
        assert_eq!(
            serde_json::from_str::<NoticeSeverity>("\"someFutureSeverity\"").unwrap(),
            NoticeSeverity::Warning
        );
    }

    /// Wire names are camelCase strings and round-trip.
    #[test]
    fn notice_enums_round_trip_camel_case() {
        assert_eq!(
            serde_json::to_string(&NoticeKind::McpStatus).unwrap(),
            "\"mcpStatus\""
        );
        assert_eq!(
            serde_json::to_string(&NoticeSeverity::Warning).unwrap(),
            "\"warning\""
        );
        for kind in [
            NoticeKind::Compaction,
            NoticeKind::ModelRerouted,
            NoticeKind::Retrying,
            NoticeKind::McpStatus,
            NoticeKind::AuthStatus,
            NoticeKind::RateLimit,
            NoticeKind::Info,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<NoticeKind>(&json).unwrap(), kind);
        }
    }

    /// An AgentEvent::Notice with an unknown kind/severity decodes (degraded)
    /// inside a batch instead of failing the whole vector — the 0.1-review
    /// blast radius, tested in the direction that can actually fail.
    #[test]
    fn notice_event_with_unknown_values_does_not_poison_a_batch() {
        let json = r#"[
            {"type":"textDelta","text":"hi"},
            {"type":"notice","kind":"someFutureKind","severity":"someFutureSeverity","summary":"s"}
        ]"#;
        let events: Vec<AgentEvent> = serde_json::from_str(json).unwrap();
        assert_eq!(events.len(), 2);
        match &events[1] {
            AgentEvent::Notice {
                kind,
                severity,
                summary,
                detail,
                key,
            } => {
                assert_eq!(*kind, NoticeKind::Info);
                assert_eq!(*severity, NoticeSeverity::Warning);
                assert_eq!(summary, "s");
                assert_eq!(*detail, None);
                assert_eq!(*key, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn notice_event_round_trips() {
        let ev = AgentEvent::Notice {
            kind: NoticeKind::Retrying,
            severity: NoticeSeverity::Warning,
            summary: "Retrying — attempt 2 of 3".into(),
            detail: Some("Next attempt in 4s.".into()),
            key: Some("retry".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    #[test]
    fn diagnostic_event_round_trips_and_unknown_severity_degrades() {
        let ev = AgentEvent::Diagnostic {
            discriminator: "system/somethingNew".into(),
            severity: DiagnosticSeverity::Unknown,
            code: None,
            summary: "The agent sent a message Comet doesn't recognize.".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
        // A `code` written by a future build decodes; an absent one defaults.
        let old =
            r#"{"type":"diagnostic","discriminator":"x","severity":"malformed","summary":"s"}"#;
        match serde_json::from_str::<AgentEvent>(old).unwrap() {
            AgentEvent::Diagnostic { code, severity, .. } => {
                assert_eq!(code, None);
                assert_eq!(severity, DiagnosticSeverity::Malformed);
            }
            other => panic!("unexpected {other:?}"),
        }
        // A severity this build has never heard of degrades to Unknown
        // instead of failing the surrounding batch.
        assert_eq!(
            serde_json::from_str::<DiagnosticSeverity>("\"someFutureSeverity\"").unwrap(),
            DiagnosticSeverity::Unknown
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticSeverity::Malformed).unwrap(),
            "\"malformed\""
        );
    }

    #[test]
    fn discriminator_sanitizer_rejects_untrusted_shapes() {
        // Clean identifiers pass through untouched.
        assert_eq!(sanitize_discriminator("system/status"), "system/status");
        assert_eq!(
            sanitize_discriminator("item/webSearch.v2_x-y"),
            "item/webSearch.v2_x-y"
        );
        // A path, a quote, a space, a control character, or nothing at all
        // becomes the literal "malformed" — the original never travels.
        assert_eq!(sanitize_discriminator(r"C:\dev\secrets.txt"), "malformed");
        assert_eq!(sanitize_discriminator("say \"hi\""), "malformed");
        assert_eq!(sanitize_discriminator("two words"), "malformed");
        assert_eq!(sanitize_discriminator("a\u{7}b"), "malformed");
        assert_eq!(sanitize_discriminator(""), "malformed");
        // Over-long but clean truncates to 64 bytes (ASCII ⇒ char-safe).
        let long = "a".repeat(80);
        assert_eq!(sanitize_discriminator(&long), "a".repeat(64));
    }

    #[test]
    fn runtime_mode_derives_the_sandbox_for_every_variant() {
        assert_eq!(
            RuntimeMode::ApprovalRequired.sandbox(),
            SandboxLevel::ReadOnly
        );
        assert_eq!(
            RuntimeMode::AutoAcceptEdits.sandbox(),
            SandboxLevel::WorkspaceWrite
        );
        assert_eq!(RuntimeMode::Auto.sandbox(), SandboxLevel::WorkspaceWrite);
        assert_eq!(
            RuntimeMode::FullAccess.sandbox(),
            SandboxLevel::DangerFullAccess
        );
    }

    #[test]
    fn runtime_mode_defaults_to_auto_accept_edits() {
        // The mode that reproduces how every existing chat has been running:
        // a workspace-write sandbox, and no approval a user could not answer.
        assert_eq!(RuntimeMode::default(), RuntimeMode::AutoAcceptEdits);
        assert_eq!(
            RuntimeMode::default().sandbox(),
            SandboxLevel::WorkspaceWrite
        );
    }

    #[test]
    fn runtime_mode_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&RuntimeMode::ApprovalRequired).unwrap(),
            "\"approval-required\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeMode::AutoAcceptEdits).unwrap(),
            "\"auto-accept-edits\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeMode::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeMode::FullAccess).unwrap(),
            "\"full-access\""
        );
        for mode in [
            RuntimeMode::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
            RuntimeMode::FullAccess,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<RuntimeMode>(&json).unwrap(), mode);
        }
    }
}
