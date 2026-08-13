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
    /// The runtime modes this harness honors, in the order they should be
    /// offered. An empty list means the harness has not declared any — read it
    /// as unknown, never as "supports nothing", or a harness that predates the
    /// field is presented as offering no way to run at all.
    pub runtime_modes: Vec<RuntimeMode>,
    /// Whether the note attached to a denial reaches the model. Claude's deny
    /// response carries a message; Codex's decisions are bare literals with
    /// nowhere to put one, so the same control means different things per
    /// provider (`DEBT.md` D24). The composer labels its note field from this:
    /// a promise the provider cannot keep is worse than copy admitting the
    /// limit, which is why the conservative default is "cannot carry it".
    pub carries_deny_note: bool,
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

/// Where the CLI Comet spawns came from.
///
/// Always *derived* from the resolved path against the directory lists the
/// harness already searches — never asked of the CLI. `claude doctor` exists
/// but is a health checkup rather than a machine-readable install report, and
/// it costs a subprocess to learn less than the path already says.
///
/// [`Unknown`] is the default and the catch-all, which is load-bearing: this
/// rides inside the `Vec<HarnessDescriptor>` that `ListHarnesses` answers with,
/// and that decode is all-or-nothing. A strict enum would mean adding a variant
/// here blanks the entire agent catalog on every older peer, so the
/// `Deserialize` below folds anything unrecognized into `Unknown` instead. That
/// is also why adding a variant needs no `PROTOCOL_VERSION` bump.
///
/// [`Unknown`]: InstallMethod::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallMethod {
    /// `CLAUDE_CODE_EXECUTABLE` / `CODEX_EXECUTABLE` named this binary.
    ///
    /// Deliberately not a method. When the user points an env var at a file,
    /// how it was installed is genuinely unknowable, and naming the override is
    /// honest where guessing "native" would not be.
    Override,
    /// The provider's own installer, in its per-user directory.
    Native,
    /// A global npm install.
    Npm,
    Fnm,
    Volta,
    Pnpm,
    Bun,
    Nvm,
    Winget,
    Scoop,
    Homebrew,
    /// Resolved from somewhere we do not recognize. A real answer rather than a
    /// failure — a CLI on PATH in a bespoke location works fine, and saying so
    /// beats implying something is wrong with it.
    #[default]
    Unknown,
}

impl InstallMethod {
    /// The wire spelling, matching what `Serialize` emits.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Native => "native",
            Self::Npm => "npm",
            Self::Fnm => "fnm",
            Self::Volta => "volta",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
            Self::Nvm => "nvm",
            Self::Winget => "winget",
            Self::Scoop => "scoop",
            Self::Homebrew => "homebrew",
            Self::Unknown => "unknown",
        }
    }

    /// Anything unrecognized becomes [`Unknown`] rather than an error. See the
    /// type's own doc for why that is not laziness.
    ///
    /// [`Unknown`]: InstallMethod::Unknown
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "override" => Self::Override,
            "native" => Self::Native,
            "npm" => Self::Npm,
            "fnm" => Self::Fnm,
            "volta" => Self::Volta,
            "pnpm" => Self::Pnpm,
            "bun" => Self::Bun,
            "nvm" => Self::Nvm,
            "winget" => Self::Winget,
            "scoop" => Self::Scoop,
            "homebrew" => Self::Homebrew,
            _ => Self::Unknown,
        }
    }

    /// How this reads on a settings card.
    ///
    /// The package managers keep their own lowercase spelling — `npm` and
    /// `pnpm` are wordmarks, and title-casing them reads as a typo.
    pub fn label(self) -> &'static str {
        match self {
            Self::Override => "Set by override",
            Self::Native => "Native installer",
            Self::Npm => "npm (global)",
            Self::Fnm => "fnm",
            Self::Volta => "Volta",
            Self::Pnpm => "pnpm",
            Self::Bun => "Bun",
            Self::Nvm => "nvm",
            Self::Winget => "WinGet",
            Self::Scoop => "Scoop",
            Self::Homebrew => "Homebrew",
            Self::Unknown => "Unrecognized location",
        }
    }
}

impl<'de> Deserialize<'de> for InstallMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

/// The install Comet actually spawns: which binary, and how it got there.
///
/// A sibling of [`HarnessAvailability`] rather than a field inside it, because
/// the path is worth most exactly when the CLI is *broken* — a "Not working"
/// row that names the binary is the difference between a shrug and a diagnosis,
/// and hanging it off the `Available` variant would lose it there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessInstall {
    /// The resolved executable, as a display string.
    ///
    /// A path and nothing more. Never read, hashed, or copied: a native
    /// `claude.exe` is ~300MB, so anything that touches the file itself turns a
    /// settings card into a disk-bound operation.
    pub path: String,
    #[serde(default)]
    pub method: InstallMethod,
}

/// How current the installed CLI is, to the extent the provider will say.
///
/// The variants exist to keep two genuinely different answers apart, because
/// only one provider can give the stronger one. Codex publishes the latest
/// version it knows of, so Comet can compare and say [`Current`] or
/// [`Available`]. Claude publishes only what its own last update *attempt* did,
/// so the most Comet can honestly report is [`SelfUpdating`] — never "up to
/// date", which would be a claim about a version nobody here has seen.
///
/// Collapsing those into one "ok" variant is the mistake this type exists to
/// prevent: it would let the card tell a user Claude is current at the moment
/// its auto-updater has quietly been failing for a week.
///
/// [`Unknown`] is the default and the catch-all, for the same all-or-nothing
/// decode reason spelled out on [`InstallMethod`] — this rides in the same
/// `Vec<HarnessDescriptor>`. Adding a variant therefore needs no
/// `PROTOCOL_VERSION` bump.
///
/// [`Current`]: UpdateState::Current
/// [`Available`]: UpdateState::Available
/// [`SelfUpdating`]: UpdateState::SelfUpdating
/// [`Unknown`]: UpdateState::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateState {
    /// A real comparison against a published latest version said the installed
    /// one is not behind. Only claimable when [`HarnessUpdate::latest`] is set.
    Current,
    /// A newer version exists; [`HarnessUpdate::latest`] names it.
    Available,
    /// The CLI keeps itself updated and its last attempt succeeded, but it does
    /// not publish what the latest version is. Deliberately weaker than
    /// [`Current`]: it says the mechanism is working, not that the binary is
    /// newest.
    ///
    /// [`Current`]: UpdateState::Current
    SelfUpdating,
    /// The CLI's own last update attempt failed. The one state here a user can
    /// act on, and the reason this slice reads the attempt record at all — it
    /// is exactly when self-updating has stopped working silently.
    UpdateFailed,
    /// Nothing readable: no state file, unparseable contents, or a provider
    /// that publishes neither. Renders no line rather than an empty one.
    #[default]
    Unknown,
}

impl UpdateState {
    /// The wire spelling, matching what `Serialize` emits.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Available => "available",
            Self::SelfUpdating => "selfUpdating",
            Self::UpdateFailed => "updateFailed",
            Self::Unknown => "unknown",
        }
    }

    /// Anything unrecognized becomes [`Unknown`] rather than an error. See the
    /// type's own doc for why that is not laziness.
    ///
    /// [`Unknown`]: UpdateState::Unknown
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "current" => Self::Current,
            "available" => Self::Available,
            "selfUpdating" => Self::SelfUpdating,
            "updateFailed" => Self::UpdateFailed,
            _ => Self::Unknown,
        }
    }
}

impl<'de> Deserialize<'de> for UpdateState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

/// What the provider itself says about how current its CLI is.
///
/// Read from the state file each CLI already maintains for its own updater —
/// never by spawning one. Both providers' `update` subcommands check *and
/// install* in a single shot with no dry-run flag, so asking them the question
/// is indistinguishable from telling them to act on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUpdate {
    #[serde(default)]
    pub state: UpdateState,
    /// The newest version the provider knows of. Set only when the provider
    /// publishes one, which today means Codex alone — it is never inferred from
    /// an install method, a registry, or a changelog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// When the provider last did the thing this state describes, as it wrote
    /// it. Its meaning follows `state` rather than being fixed: a check time
    /// for [`Current`]/[`Available`], an update-attempt time for
    /// [`SelfUpdating`]/[`UpdateFailed`]. The card's copy differs per state for
    /// exactly that reason, so the two never render as the same sentence.
    ///
    /// [`Current`]: UpdateState::Current
    /// [`Available`]: UpdateState::Available
    /// [`SelfUpdating`]: UpdateState::SelfUpdating
    /// [`UpdateFailed`]: UpdateState::UpdateFailed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

/// What one resolve-and-probe pass learned about a provider's CLI.
///
/// Deliberately **not** a wire type: `HarnessDescriptor` publishes the three
/// halves as siblings, matching how they are consumed. This exists so the pass
/// that produces them cannot produce one without the others — a path shown
/// beside a version it was not probed with would be worse than showing
/// neither.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HarnessProbe {
    pub availability: HarnessAvailability,
    /// Absent only when resolution never yielded a path at all.
    pub install: Option<HarnessInstall>,
    /// Absent when the provider publishes no update state, or its state file is
    /// missing or unreadable. Independent of `install`: a CLI can resolve and
    /// still say nothing about updates.
    pub update: Option<HarnessUpdate>,
}

impl HarnessProbe {
    /// A probe that never got as far as a path: the CLI did not resolve, or the
    /// harness could not be constructed to ask.
    pub fn unresolved(availability: HarnessAvailability) -> Self {
        Self {
            availability,
            install: None,
            update: None,
        }
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    /// The blast-radius rule, in the direction that actually bites: a NEWER
    /// peer sending a method this build has never heard of. `ListHarnesses`
    /// decodes as one vector, so an error here would blank every harness rather
    /// than degrade one line of one card.
    #[test]
    fn an_unknown_method_degrades_instead_of_failing_the_batch() {
        let decoded: HarnessInstall = serde_json::from_str(
            r#"{"path":"/opt/nix/store/abc/bin/codex","method":"nixProfile"}"#,
        )
        .expect("an unrecognized method must not fail the decode");
        assert_eq!(decoded.method, InstallMethod::Unknown);
        // The path still arrives, which is the half that was worth sending.
        assert_eq!(decoded.path, "/opt/nix/store/abc/bin/codex");
    }

    /// An older engine sends the path with no method at all.
    #[test]
    fn an_absent_method_reads_as_unknown() {
        let decoded: HarnessInstall =
            serde_json::from_str(r#"{"path":"C:\\Users\\a\\.local\\bin\\claude.exe"}"#).unwrap();
        assert_eq!(decoded.method, InstallMethod::Unknown);
    }

    /// `as_wire_str` and the derived `Serialize` must not drift — `from_wire`
    /// is written against the strings the derive emits, so a `rename_all`
    /// change that silently altered one would break decoding in a way no
    /// round-trip through the Rust type alone would catch.
    #[test]
    fn every_variant_round_trips_through_its_wire_spelling() {
        for method in [
            InstallMethod::Override,
            InstallMethod::Native,
            InstallMethod::Npm,
            InstallMethod::Fnm,
            InstallMethod::Volta,
            InstallMethod::Pnpm,
            InstallMethod::Bun,
            InstallMethod::Nvm,
            InstallMethod::Winget,
            InstallMethod::Scoop,
            InstallMethod::Homebrew,
            InstallMethod::Unknown,
        ] {
            let json = serde_json::to_string(&method).unwrap();
            assert_eq!(
                json,
                format!(r#""{}""#, method.as_wire_str()),
                "the derive and as_wire_str disagree"
            );
            assert_eq!(
                InstallMethod::from_wire(method.as_wire_str()),
                method,
                "{} did not decode back",
                method.as_wire_str()
            );
        }
    }

    /// Every label is non-empty and none of them repeat, so a card can never
    /// render a blank method or two indistinguishable ones.
    #[test]
    fn labels_are_distinct_and_present() {
        let labels: Vec<&str> = [
            InstallMethod::Override,
            InstallMethod::Native,
            InstallMethod::Npm,
            InstallMethod::Fnm,
            InstallMethod::Volta,
            InstallMethod::Pnpm,
            InstallMethod::Bun,
            InstallMethod::Nvm,
            InstallMethod::Winget,
            InstallMethod::Scoop,
            InstallMethod::Homebrew,
            InstallMethod::Unknown,
        ]
        .iter()
        .map(|m| m.label())
        .collect();
        assert!(labels.iter().all(|l| !l.trim().is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "two methods share a label");
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    /// Same blast-radius rule as `InstallMethod`, and the same direction: a
    /// newer peer sending a state this build predates must not blank the whole
    /// harness vector. The siblings still arrive, so the card can fall back to
    /// the version and path it already had.
    #[test]
    fn an_unknown_state_degrades_instead_of_failing_the_batch() {
        let decoded: HarnessUpdate = serde_json::from_str(
            r#"{"state":"rollbackPending","latest":"0.148.0","checkedAt":"2026-08-12T00:48:08Z"}"#,
        )
        .expect("an unrecognized state must not fail the decode");
        assert_eq!(decoded.state, UpdateState::Unknown);
        assert_eq!(decoded.latest.as_deref(), Some("0.148.0"));
    }

    /// An older engine sends no update block at all — the field is absent, not
    /// null, which is what `skip_serializing_if` produces on the wire.
    #[test]
    fn an_absent_update_block_reads_as_none() {
        #[derive(Deserialize)]
        struct Carrier {
            #[serde(default)]
            update: Option<HarnessUpdate>,
        }
        let decoded: Carrier = serde_json::from_str(r#"{}"#).unwrap();
        assert!(decoded.update.is_none());
    }

    /// A state with neither sibling is legal: `SelfUpdating` with no timestamp
    /// is what a Claude install that has never updated looks like.
    #[test]
    fn a_bare_state_decodes_with_empty_siblings() {
        let decoded: HarnessUpdate = serde_json::from_str(r#"{"state":"selfUpdating"}"#).unwrap();
        assert_eq!(decoded.state, UpdateState::SelfUpdating);
        assert_eq!(decoded.latest, None);
        assert_eq!(decoded.checked_at, None);
    }

    /// `as_wire_str` and the derived `Serialize` must not drift — `from_wire` is
    /// written against the strings the derive emits, so a `rename_all` change
    /// that altered one would break decoding in a way no round-trip through the
    /// Rust type alone would catch. `selfUpdating` is the variant that would
    /// actually break: it is the only multi-word one.
    #[test]
    fn every_state_round_trips_through_its_wire_spelling() {
        for state in [
            UpdateState::Current,
            UpdateState::Available,
            UpdateState::SelfUpdating,
            UpdateState::UpdateFailed,
            UpdateState::Unknown,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(
                json,
                format!(r#""{}""#, state.as_wire_str()),
                "the derive and as_wire_str disagree"
            );
            assert_eq!(
                UpdateState::from_wire(state.as_wire_str()),
                state,
                "{} did not decode back",
                state.as_wire_str()
            );
        }
    }

    /// The absent case a peer actually sends: `latest` and `checkedAt` are
    /// skipped rather than nulled, so a `SelfUpdating` block is two keys
    /// shorter than an `Available` one and an older decoder sees no `null`.
    #[test]
    fn empty_siblings_are_omitted_from_the_wire() {
        let json = serde_json::to_string(&HarnessUpdate {
            state: UpdateState::SelfUpdating,
            latest: None,
            checked_at: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"state":"selfUpdating"}"#);
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

    /// An undeclared mode list is the absent case a consumer has to write
    /// itself: it means the harness has not said, not that it supports
    /// nothing. Decoding must produce it rather than failing the batch.
    #[test]
    fn absent_runtime_modes_decode_to_an_empty_list() {
        let caps: HarnessCapabilities = serde_json::from_str("{}").unwrap();
        assert!(caps.runtime_modes.is_empty());

        let partial: HarnessCapabilities =
            serde_json::from_str(r#"{"runtimeModes":["approval-required","full-access"]}"#)
                .unwrap();
        assert_eq!(
            partial.runtime_modes,
            vec![RuntimeMode::ApprovalRequired, RuntimeMode::FullAccess]
        );
    }

    /// The absent case is what a peer predating the field sends. A note that
    /// silently goes nowhere is worse than copy that says so, so "has not
    /// declared" has to land on "cannot carry it".
    #[test]
    fn absent_deny_note_capability_reads_as_cannot_carry() {
        let caps: HarnessCapabilities = serde_json::from_str("{}").unwrap();
        assert!(!caps.carries_deny_note);
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
    /// Whether this model accepts image input.
    ///
    /// Defaulted TRUE, and the direction matters: a payload written before
    /// this field existed, or a provider that does not report modality at
    /// all (Claude's `ModelInfo` has no modality field), must not read as
    /// "cannot take images" and silently disable the attachment button.
    /// Codex's schema documents the same default for its own
    /// `inputModalities`.
    #[serde(default = "accepts_images_default")]
    pub accepts_images: bool,
}

fn accepts_images_default() -> bool {
    true
}

/// One slash command a provider offers in a given directory.
///
/// Cwd-scoped, unlike [`Model`]: the same CLI answers with a different list per
/// directory, because user and project skills are discovered from it (67 in
/// comet's own checkout against 63 in a home directory — capture
/// `2026-08-11-claude-initialize-handshake.md`). That is why the cache behind
/// this keys on the directory and the model cache does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCommand {
    /// Without the leading `/`, as the provider reports it.
    pub name: String,
    /// The provider's own prose. Claude appends a `(user)` / `(project)` scope
    /// suffix and these run to several hundred characters, so a caller that
    /// renders it inline has to give it room or leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The argument shape, e.g. `[low|medium|high] [--fix]`. Absent on most
    /// commands; empty and missing mean the same thing here, because the
    /// provider sends `""` for a command that takes no arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Other names that invoke the same command (`code-review` → `review`).
    /// Matched when completing, never listed as rows of their own.
    #[serde(default)]
    pub aliases: Vec<String>,
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

/// What a provider is asking permission to do, reduced to the fields a
/// decision card renders — the same reduction [`ToolCall`] applies.
///
/// A file change carries counts, never the patch: the render-parts policy
/// strips heavy tool inputs before anything enters the doc, and an approval is
/// subject to the same limit. A richer preview has to be read from the
/// host-resident diff sidecar rather than carried here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ApprovalRequest {
    Command {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FileChange {
        path: String,
        operation: FileOperation,
        added_lines: u32,
        removed_lines: u32,
    },
    FileRead {
        path: String,
    },
    Mcp {
        server: String,
        tool: String,
    },
    /// A provider asked for something Comet does not model. `summary` is Comet
    /// copy naming the action, never provider prose.
    Unknown {
        summary: String,
    },
}

/// Unit variants only, so `#[serde(other)]` applies: an operation a later
/// provider introduces decodes as `Unknown` instead of failing the whole part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileOperation {
    Create,
    Modify,
    Delete,
    #[serde(other)]
    Unknown,
}

/// The answer to an approval. `Expired` is host-stamped when a pending
/// approval's run ends and is never a decision a client may send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum ApprovalDecision {
    Allow,
    AllowForSession,
    Deny { message: String },
    Expired,
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

/// Lifecycle state of a subagent Claude delegated to. See
/// [`AgentEvent::SubagentStarted`] for why this is Claude-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
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
    /// A live context-occupancy reading. Never persisted to docs — it is the
    /// *current* occupancy, not history, and `doc::parts` drops it deliberately.
    ///
    /// **`prompt_tokens` is the last request's prompt size, cache INCLUSIVE**,
    /// normalized by the adapters so the field means one thing. The providers
    /// disagree at source and the disagreement is invisible: Claude's
    /// `input_tokens` *excludes* cache (it read 10 for a ~35,000-token prompt),
    /// while Codex's `inputTokens` includes it. Captured 2026-08-12 —
    /// `captures/2026-08-12-token-context-usage.md`.
    ///
    /// Cumulative totals are a different quantity and must not be drawn against
    /// the window: Codex's `total` passed 41% of the window after three trivial
    /// turns.
    #[serde(rename_all = "camelCase")]
    Usage {
        /// The `inputTokens` alias is for **run-journal lines written before
        /// the rename**, not for peers — `AgentEvent` crosses no RPC boundary
        /// (verified: no references in `crates/rpc` or `crates/client`).
        /// Without it `read_lines` skips every pre-upgrade usage line and
        /// warns once per line. The old name is accepted, never written: it
        /// says "input", and the value is now the cache-inclusive prompt.
        #[serde(alias = "inputTokens")]
        prompt_tokens: u64,
        output_tokens: u64,
        /// `None` is "the provider did not say", never a default. Codex
        /// declares it optional upstream; Claude's is undocumented.
        /// Serde already treats a missing `Option` as `None`; the test pins
        /// that as a contract so a future change to a bare `u64` fails loudly
        /// rather than rejecting every frame from a provider that omits it.
        context_window: Option<u64>,
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
    /// The host's approval bridge minted this id and parked a resolver before
    /// emitting. An adapter must not emit its own copy — the run pipeline
    /// drops one, because a card under an id no resolver knows is
    /// unanswerable.
    #[serde(rename_all = "camelCase")]
    ApprovalRequested {
        request_id: String,
        approval: ApprovalRequest,
    },
    #[serde(rename_all = "camelCase")]
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
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
    /// A subagent this run delegated to. Claude-only: Codex has no subagent
    /// concept on this surface (`guardian_subagent` is an approvals reviewer, a
    /// different feature). Captured 2026-08-13 —
    /// `captures/2026-08-13-plan-todo-subagent.md`.
    #[serde(rename_all = "camelCase")]
    SubagentStarted {
        /// The DURABLE identity. A `SendMessage`-resumed agent fires a second
        /// `task_started` with this same id under a new `tool_use_id`, so keying
        /// on the tool id splits one agent into two cards.
        task_id: String,
        /// The parent `Agent` tool call. Also the value of `parent_tool_use_id`
        /// on every frame the child emits — kept for the journal and for a later
        /// slice that renders the child's own transcript.
        tool_use_id: String,
        /// e.g. "general-purpose", "Explore". Straight off the wire; never looked
        /// up in the discovery handshake's `agents` catalogue (see D31).
        agent_type: String,
        description: String,
        /// Capped at the harness boundary. `None` when the provider sent none.
        prompt: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SubagentUpdated {
        task_id: String,
        status: SubagentStatus,
        /// The live activity line — Claude's `description` on `task_progress`
        /// moves ("Read README and report first heading" → "Reading README.md").
        #[serde(default)]
        activity: Option<String>,
        /// The child's answer, on completion only.
        #[serde(default)]
        summary: Option<String>,
        /// `None` is "not reported yet", never zero: nothing on the wire promises
        /// a `task_progress` before `task_notification`.
        #[serde(default)]
        total_tokens: Option<u64>,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        tool_uses: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
    },
}

/// Where a model list came from. The picker renders a quiet caption for
/// `BuiltIn`, because a user looking at a stale list should be able to tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogSource {
    /// Merged with a live discovery answer.
    Live,
    /// The curated catalog alone — discovery failed or has not run.
    BuiltIn,
}

/// A model list plus its provenance. Replaces the bare `Vec<Model>` that
/// `ListModels` used to answer with; see `PROTOCOL_VERSION`'s doc comment for
/// why that reshape bumps the version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub models: Vec<Model>,
    pub source: CatalogSource,
}

impl ModelCatalog {
    pub fn built_in(models: Vec<Model>) -> Self {
        Self {
            models,
            source: CatalogSource::BuiltIn,
        }
    }

    pub fn live(models: Vec<Model>) -> Self {
        Self {
            models,
            source: CatalogSource::Live,
        }
    }
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

    /// `prompt_tokens` is the last request's prompt size, cache-inclusive, and
    /// it means the same thing on both providers only because the adapters
    /// converge it — Claude reports the parts separately, Codex reports one
    /// inclusive number. The round-trip pins the wire names the UI decodes.
    #[test]
    fn usage_round_trips_with_a_window() {
        let ev = AgentEvent::Usage {
            prompt_tokens: 35_017,
            output_tokens: 26,
            context_window: Some(200_000),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"promptTokens\":35017"), "{json}");
        assert!(json.contains("\"contextWindow\":200000"), "{json}");
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// An absent window is "the provider did not say", never a number. Codex
    /// declares `modelContextWindow` optional upstream and Claude's
    /// `contextWindow` appears in no published field list, so the absent arm
    /// is reachable in production and must decode rather than fail the batch.
    #[test]
    fn usage_without_a_window_decodes_to_none() {
        let ev: AgentEvent =
            serde_json::from_str(r#"{"type":"usage","promptTokens":42,"outputTokens":7}"#).unwrap();
        assert_eq!(
            ev,
            AgentEvent::Usage {
                prompt_tokens: 42,
                output_tokens: 7,
                context_window: None,
            }
        );
    }

    /// A journal line written before the rename still decodes, and the new
    /// name is what gets written back. The alias exists for the journal, which
    /// skips undecodable lines with a warning — not for peers, since
    /// `AgentEvent` crosses no RPC boundary.
    #[test]
    fn a_pre_rename_journal_line_still_decodes() {
        let old = r#"{"type":"usage","inputTokens":10,"outputTokens":20}"#;
        let ev: AgentEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            ev,
            AgentEvent::Usage {
                prompt_tokens: 10,
                output_tokens: 20,
                context_window: None,
            }
        );
        let rewritten = serde_json::to_string(&ev).unwrap();
        assert!(rewritten.contains("promptTokens"), "{rewritten}");
        assert!(
            !rewritten.contains("inputTokens"),
            "the old name is read, never written: {rewritten}"
        );
    }

    #[test]
    fn approval_request_round_trips_each_kind() {
        let cases = vec![
            ApprovalRequest::Command {
                command: "cargo test".into(),
                cwd: Some("/repo".into()),
            },
            ApprovalRequest::FileChange {
                path: "src/main.rs".into(),
                operation: FileOperation::Modify,
                added_lines: 12,
                removed_lines: 3,
            },
            ApprovalRequest::FileRead {
                path: "/etc/hosts".into(),
            },
            ApprovalRequest::Mcp {
                server: "linear".into(),
                tool: "create_issue".into(),
            },
            ApprovalRequest::Unknown {
                summary: "an action Comet does not model".into(),
            },
        ];
        for case in cases {
            let json = serde_json::to_value(&case).unwrap();
            assert_eq!(
                serde_json::from_value::<ApprovalRequest>(json).unwrap(),
                case
            );
        }
    }

    #[test]
    fn command_approval_without_a_cwd_round_trips() {
        // The absent case, written by hand: a provider reporting no working
        // directory must not be indistinguishable from one reporting "".
        let case = ApprovalRequest::Command {
            command: "ls".into(),
            cwd: None,
        };
        let json = serde_json::to_value(&case).unwrap();
        assert!(json.get("cwd").is_none(), "absent cwd must not serialize");
        assert_eq!(
            serde_json::from_value::<ApprovalRequest>(json).unwrap(),
            case
        );
    }

    #[test]
    fn an_unrecognized_file_operation_decodes_as_unknown() {
        let json = serde_json::json!({
            "kind": "fileChange",
            "path": "a.rs",
            "operation": "rename",
            "addedLines": 0,
            "removedLines": 0
        });
        assert!(matches!(
            serde_json::from_value::<ApprovalRequest>(json).unwrap(),
            ApprovalRequest::FileChange {
                operation: FileOperation::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn approval_decision_round_trips() {
        for case in [
            ApprovalDecision::Allow,
            ApprovalDecision::AllowForSession,
            ApprovalDecision::Deny {
                message: "not this path".into(),
            },
            ApprovalDecision::Expired,
        ] {
            let json = serde_json::to_value(&case).unwrap();
            assert_eq!(
                serde_json::from_value::<ApprovalDecision>(json).unwrap(),
                case
            );
        }
    }

    #[test]
    fn approval_events_round_trip() {
        let ev = AgentEvent::ApprovalRequested {
            request_id: "r1".into(),
            approval: ApprovalRequest::FileRead {
                path: "a.rs".into(),
            },
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(serde_json::from_value::<AgentEvent>(json).unwrap(), ev);

        let ev = AgentEvent::ApprovalResolved {
            request_id: "r1".into(),
            decision: ApprovalDecision::Allow,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(serde_json::from_value::<AgentEvent>(json).unwrap(), ev);
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
    /// still sends `autoApprove`. It must decode, ignored — the field it stood
    /// in for is now the mode.
    ///
    /// Such a payload loses its never-ask intent: `true` degrades to the
    /// default mode rather than to `FullAccess`. That is the safe direction and
    /// the reason this is a plain ignore rather than a translation. Requests
    /// carrying `true` did reach the wire — the queued run commands in the doc
    /// command log are the durable, synced path — so this is a real degradation
    /// for a pre-upgrade peer, not a hypothetical one.
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

    /// Absent means "this model takes images", because that is what every model
    /// in both catalogs does today and what the Codex schema documents as its
    /// default. Read as `false`, the gate would disable attachment on every
    /// model whose payload predates the field — the exact `None`-as-a-value trap
    /// `.agents/rules/optional-wire-fields.md` exists for.
    #[test]
    fn absent_accepts_images_reads_as_images_work() {
        let model: Model =
            serde_json::from_str(r#"{"id":"m","label":"M","reasoningLevels":[],"options":[]}"#)
                .unwrap();
        assert!(model.accepts_images);
    }

    #[test]
    fn explicit_false_accepts_images_survives_a_round_trip() {
        let model: Model = serde_json::from_str(
            r#"{"id":"m","label":"M","reasoningLevels":[],"options":[],"acceptsImages":false}"#,
        )
        .unwrap();
        assert!(!model.accepts_images);
        let back: Model = serde_json::from_str(&serde_json::to_string(&model).unwrap()).unwrap();
        assert!(!back.accepts_images);
    }

    /// The reply shape the picker decodes. `source` is what the caption reads.
    #[test]
    fn catalog_round_trips_and_defaults_to_built_in() {
        let catalog = ModelCatalog::built_in(vec![]);
        assert_eq!(catalog.source, CatalogSource::BuiltIn);
        let json = serde_json::to_string(&catalog).unwrap();
        assert!(json.contains(r#""source":"builtIn""#), "got {json}");
        let back: ModelCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, CatalogSource::BuiltIn);
    }

    #[test]
    fn subagent_started_round_trips() {
        let ev = AgentEvent::SubagentStarted {
            task_id: "task-1".into(),
            tool_use_id: "toolu_01".into(),
            agent_type: "general-purpose".into(),
            description: "Investigate the flaky test".into(),
            prompt: Some("Find why test_foo is flaky".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// `prompt` is `None` when the provider sent none — never a synthesized
    /// empty string.
    #[test]
    fn subagent_started_without_a_prompt_decodes_to_none() {
        let json = r#"{
            "type":"subagentStarted",
            "taskId":"task-1",
            "toolUseId":"toolu_01",
            "agentType":"Explore",
            "description":"Find the config loader"
        }"#;
        let ev: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            ev,
            AgentEvent::SubagentStarted {
                task_id: "task-1".into(),
                tool_use_id: "toolu_01".into(),
                agent_type: "Explore".into(),
                description: "Find the config loader".into(),
                prompt: None,
            }
        );
    }

    #[test]
    fn subagent_updated_round_trips() {
        let ev = AgentEvent::SubagentUpdated {
            task_id: "task-1".into(),
            status: SubagentStatus::Completed,
            activity: Some("Reading README.md".into()),
            summary: Some("The first heading is \"Comet\".".into()),
            total_tokens: Some(1_204),
            duration_ms: Some(8_431),
            tool_uses: Some(3),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// Every optional field absent decodes to all-`None` rather than erroring —
    /// nothing on the wire promises a `task_progress` before a
    /// `task_notification`, so a bare status update is legal.
    #[test]
    fn subagent_updated_with_every_optional_absent_decodes_to_all_none() {
        let json = r#"{"type":"subagentUpdated","taskId":"task-1","status":"running"}"#;
        let ev: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            ev,
            AgentEvent::SubagentUpdated {
                task_id: "task-1".into(),
                status: SubagentStatus::Running,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }
        );
    }

    /// An extra key a future build adds must not fail this decode — the same
    /// forward-compat contract every other `AgentEvent` variant honors.
    #[test]
    fn subagent_updated_with_an_unknown_extra_key_still_decodes() {
        let json = r#"{
            "type":"subagentUpdated",
            "taskId":"task-1",
            "status":"failed",
            "fromAFutureBuild":"ignore me"
        }"#;
        let ev: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            ev,
            AgentEvent::SubagentUpdated {
                task_id: "task-1".into(),
                status: SubagentStatus::Failed,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }
        );
    }

    #[test]
    fn subagent_status_round_trips_camel_case() {
        for (status, wire) in [
            (SubagentStatus::Running, "\"running\""),
            (SubagentStatus::Completed, "\"completed\""),
            (SubagentStatus::Failed, "\"failed\""),
            (SubagentStatus::Cancelled, "\"cancelled\""),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, wire);
            assert_eq!(
                serde_json::from_str::<SubagentStatus>(&json).unwrap(),
                status
            );
        }
    }
}
