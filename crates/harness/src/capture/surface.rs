//! The corpus walker behind the new-field gate.
//!
//! Answers one question over promoted evidence: which fields do Claude Code and
//! Codex put on the wire, and in which direction. A committed snapshot of that
//! set is what makes a new CLI version's added field arrive as a test failure
//! instead of as a surprise later.
//!
//! It records the field's *name and direction only*. An earlier version also
//! sampled values, types, versions and counts to render a per-provider report;
//! the report's central column turned out to be a leaf-name heuristic, so it and
//! everything feeding it were removed. If a question needs a field's values, the
//! corpus is right there and greppable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::corpus::PromotedScenario;

/// Which way a field was observed travelling.
///
/// Input surface is not decoration: `ToProvider` fields are how Comet *drives*
/// the client, and an unused input capability (Codex's structured `skill` item)
/// is exactly as much unused surface as an unread reply field.
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    ToProvider,
    FromProvider,
}

/// Where a field was first seen, so triage starts at the frame and not a grep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameRef {
    /// `provider/version/scenario`, the corpus directory that holds it.
    pub scenario: String,
    pub sequence: u64,
}

#[derive(Clone, Debug)]
pub struct FieldObservation {
    pub provider: String,
    /// The CLI version the observation came from — a directory name under
    /// `provider/` in the corpus, e.g. `2.1.229`. Carried so the inventory can
    /// be filtered to exactly one version's surface; without it, the same
    /// field seen in two versions collapses into a single entry and a sheet
    /// can no longer show what changed between them (§2.1).
    pub version: String,
    /// Dotted path from the frame root. `[]` is an array element and `{}` is a
    /// map entry, so `.modelUsage.{}.costUSD` names a field and not a model id.
    pub path: String,
    pub direction: Direction,
    pub first_seen: FrameRef,
    /// Every scenario (bare directory name, e.g. `fresh-text` — not the
    /// `provider/version/scenario` form [`FrameRef::scenario`] uses) whose
    /// evidence produced this field, for this `(provider, version,
    /// direction)`. D85: `first_seen` alone answers "where do I start
    /// looking," but not "is this field's presence here explained by one
    /// narrow scenario or by most of the corpus" — a fact the capability
    /// sheet's Fields section needs to tell a real capability change apart
    /// from a field whose only scenario this version happens not to run.
    pub scenarios: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    /// `reason` carries `promoted_scenarios`'s own error chain, which names
    /// the actual provider/version/scenario directory that failed to read —
    /// `root` alone would otherwise be the only path in this message even
    /// when the failure is three levels deeper, misreporting a bad
    /// `claude/2.1.228` entry as the corpus root itself being unreadable.
    #[error("corpus root {root} could not be walked: {reason}")]
    UnreadableRoot { root: PathBuf, reason: String },
    #[error("corpus root {root} holds no promoted scenario")]
    EmptyCorpus { root: PathBuf },
    #[error("{scenario} has events that are not valid capture JSON")]
    UnreadableEvents { scenario: String },
}

/// Paths whose object keys are **data**, not field names.
///
/// Declared rather than inferred. A model-keyed map is indistinguishable from a
/// struct by shape alone — every capture on this machine used one model, so its
/// key set looks perfectly stable — and a wrong guess silently renames a field.
/// An undeclared map shows up in the snapshot as a field with an obviously
/// data-shaped name, which triage catches and adds here.
///
/// **`sanitize.rs` reads this same list**, so one declaration decides both
/// questions a map key raises: this module stops recording it as a field name,
/// and the sanitizer stops publishing it verbatim. They must not drift — a
/// path declared here but not there would redact a key the snapshot still
/// expects to see, and the reverse publishes an identifier the snapshot has
/// already agreed is data.
pub const MAP_PATHS: &[&str] = &[
    ".modelUsage",
    // ACP, promote-the-captures slice (2026-08-28). A tool call's own
    // parameters, keyed by that tool's parameter names (`pattern`, `path`,
    // `target_file`, ...) — a union across every tool an agent offers, the
    // same shape of problem D73 tracks for Claude's tool-argument paths.
    // First seen in `steer-grok` (grok 1.0.5): Grok's own read/search tools
    // populate this from real filesystem paths on the capturing machine.
    //
    // **The declaration is right and it costs something, and both halves are
    // worth stating — the same trade-off `acp.txt`'s header already records
    // for `_meta`/`steering.supported`, just not repeated here the first
    // time.** Declaring this a map is what stops `target_file` (and every
    // other tool's own argument names) from publishing as if they were
    // reviewed field names. But `normalize::typed_call` (`normalize.rs`)
    // genuinely reads two of this map's children on a `search`-kind frame —
    // `rawInput["pattern"]` and `rawInput["path"]` — and once a path is a
    // declared map, `Visit::walk` folds every child under it to `.{}`
    // (`.params.update.rawInput.{}`), so this sheet can no longer distinguish
    // "pattern" from "path" from any other tool's own key. A future ACP
    // agent version that drops `pattern` from its search frames breaks
    // `typed_call`'s `Search` decode silently — the capability-sheet golden
    // test stays green, because the field it would have caught is no longer
    // a field this walker can see at all. That is exactly the failure the
    // sheets exist to prevent, reopened by the one declaration that also
    // prevents a worse one (publishing a filesystem path as a field value).
    // `normalize.rs`'s own `pattern`/`path` decode test is what covers this
    // gap instead — see that test's doc comment for why a unit test has to
    // stand in for the sheet here.
    ".params.update.rawInput",
    // ACP, same slice. `session/prompt`'s usage breakdown, keyed by model
    // id — the ACP analog of Claude's `.modelUsage` above, at a different
    // path because ACP nests usage under the prompt reply's own `_meta`
    // rather than the frame root.
    ".result._meta.usage.modelUsage",
];

/// Discriminator paths whose observed *values* form a provider's vocabulary —
/// not every field, only the ones whose few distinct values answer "what
/// kinds of thing does this harness say" (design §3.5, SNAPSHOT). `.type`
/// names a frame's own kind; `.subtype` narrows it further for `system` and
/// `result` frames; `.request.subtype` and `.response.subtype` do the same
/// one level down, inside Claude's control-protocol envelope; `.event.type`
/// narrows a streamed `stream_event`; `.method` is Codex's frame kind; and
/// the remaining two name which tool ran.
///
/// Declared rather than inferred, for the reason [`MAP_PATHS`] is: "this
/// field has few distinct values" is a property of a small corpus, not of
/// the protocol, and a set built by scanning today's captures for
/// low-cardinality fields would silently stop growing the day a genuinely
/// new value arrived — inference already trusts whatever it has seen.
///
/// Found by grepping the committed corpus, not guessed:
///
/// - `.type` — Claude's frame kind: `assistant`, `control_request`,
///   `control_response`, `rate_limit_event`, `result`, `stream_event`,
///   `system`, `user`.
/// - `.subtype` — narrows a `system` frame (`init`, `status`, `hook_started`,
///   `hook_response`, `thinking_tokens`, `task_started`, `task_progress`,
///   `task_updated`, `task_notification`, `background_tasks_changed` — ten
///   values, 126 frames) or a `result` frame (`success` — one value, 7
///   frames). `success` is a `result` subtype, not a `system` one; the two
///   `.type`s never share a `.subtype` value in the committed corpus.
/// - `.request.subtype`, `.response.subtype` — one level below `.subtype`,
///   inside the `control_request`/`control_response` envelope's own
///   `request`/`response` object, so a bare `.subtype` match never sees
///   them. The control protocol is bidirectional, so both paths carry a
///   genuinely different vocabulary per [`Direction`]: `.request.subtype`
///   is `initialize` when Comet opens the request (`ToProvider`) and
///   `can_use_tool` when Claude Code does (`FromProvider`);
///   `.response.subtype` is `success` on both sides, but one `success` is
///   Comet's reply to `can_use_tool` and the other is Claude Code's reply to
///   `initialize` — the same string, unrelated occurrences, exactly what
///   direction-keying exists to keep apart. `success` here is also not the
///   `result`-frame `.subtype` above — same string, unrelated discriminator,
///   which is exactly why leaf-name matching is refused elsewhere in this
///   codebase. `.type` alone only says a frame is a `control_request`;
///   `.request.subtype` says *which* request, and `can_use_tool` is the one
///   Comet's entire approval surface hangs on — added 2026-08-16 after being
///   missed the same way `.method` was: the declared set looked complete
///   because Claude's sheet was non-empty without it, not because nothing
///   was missing.
/// - `.event.type` — the frame kind carried inside a streamed
///   `stream_event`'s `event` object (`content_block_start`,
///   `message_delta`, …); distinct from `.type` because a `stream_event`
///   frame's own `.type` is always the literal string `stream_event`.
/// - `.message.content[].name` — a tool's name in a buffered `assistant`
///   message's content array. Observed values include `Bash`, `Write`,
///   `Skill`, `TaskCreate`, `ToolSearch`, `Agent`.
/// - `.event.content_block.name` — the same tool name, streamed: it appears
///   on the `content_block_start` event that precedes the buffered form
///   above, with the identical vocabulary.
/// - `.method` — **not a Claude-shaped discriminator; added 2026-08-16.**
///   Codex is JSON-RPC and carries no root `.type`/`.subtype`/`.event.type`
///   at all, so without this path Codex's vocabulary was entirely empty
///   despite the corpus exercising 22 distinct methods
///   (`thread/start`, `turn/completed`, `item/agentMessage/delta`, …). This
///   is not the archive's documented absence-blind-spot (design §5) — these
///   captures exercised plenty; the declared set was simply Claude-shaped.
///   Stage 3's allowlist ledger already names `.method` as "the vocabulary
///   the stage-5 capability sheet reads", which is why `.method` is on
///   `allowlist/codex.txt` even though this const didn't read it until now.
///
/// **No Codex tool-name path is declared.** Codex's turn items carry a kind
/// at `.params.item.type` (`agentMessage`, `reasoning`, `userMessage`, …),
/// but no scenario in the committed corpus exercises a Codex tool call — no
/// `command_execution`, no MCP tool item, anywhere in `codex/0.147.0/` — so
/// there is no real dotted form to read off the evidence. Declaring one
/// without a capture backing it is exactly the guess this list exists to
/// refuse.
pub const VOCABULARY_PATHS: &[&str] = &[
    ".type",
    ".subtype",
    ".request.subtype",
    ".response.subtype",
    ".event.type",
    ".method",
    ".message.content[].name",
    ".event.content_block.name",
];

/// Distinct scalar values seen at each [`VOCABULARY_PATHS`] entry, keyed by
/// `(provider, version, direction)` — matching the field inventory's key
/// exactly, not folding direction away. For Codex `.method` the two
/// directions are different vocabularies: what Comet can *drive* the CLI
/// with (`thread/start`, `turn/steer`, …) versus what the CLI *emits*
/// (`turn/completed`, `item/agentMessage/delta`, …). Merging them would
/// misreport both — the same reasoning [`Direction`]'s own doc comment makes
/// for fields applies with more force to a discriminator.
pub type Vocabulary = BTreeMap<(String, String, Direction), BTreeMap<String, BTreeSet<String>>>;

/// Both the field inventory and the value vocabulary from one pass over the
/// archive. A sheet needs both, and walking the corpus for each separately
/// would visit the same ~800 committed frames twice for no reason — this is
/// the one entry point; a caller that wants only one half destructures the
/// tuple.
pub fn observe_surface(
    corpus_root: &Path,
) -> Result<(Vec<FieldObservation>, Vocabulary), SurfaceError> {
    let scenarios = super::corpus::promoted_scenarios(corpus_root).map_err(|error| {
        SurfaceError::UnreadableRoot {
            root: corpus_root.to_owned(),
            reason: format!("{error:#}"),
        }
    })?;
    if scenarios.is_empty() {
        return Err(SurfaceError::EmptyCorpus {
            root: corpus_root.to_owned(),
        });
    }

    let mut inventory: BTreeMap<(String, String, Direction, String), FieldObservation> =
        BTreeMap::new();
    let mut vocabulary: Vocabulary = BTreeMap::new();
    for scenario in scenarios {
        let events = super::corpus::frames(&scenario.directory).map_err(|_| {
            SurfaceError::UnreadableEvents {
                scenario: scenario.label.clone(),
            }
        })?;
        for event in events {
            let sequence = event["sequence"].as_u64().unwrap_or_default();
            let direction = match event["channel"].as_str() {
                Some("stdin") => Direction::ToProvider,
                _ => Direction::FromProvider,
            };
            // Only stderr is allowed to be plain text. A structured frame that
            // will not parse must stop the walk: skipping it would produce a
            // snapshot that looks complete and is quietly missing a frame's
            // fields, which is the failure this module refuses to have.
            let payload = match event["payload"]
                .as_str()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            {
                Some(payload) => payload,
                None if event["channel"].as_str() == Some("stderr") => continue,
                None => {
                    return Err(SurfaceError::UnreadableEvents {
                        scenario: scenario.label.clone(),
                    });
                }
            };
            let mut visit = Visit {
                inventory: &mut inventory,
                vocabulary: &mut vocabulary,
                scenario: &scenario,
                direction,
                sequence,
            };
            visit.walk(&payload, String::new());
        }
    }

    Ok((inventory.into_values().collect(), vocabulary))
}

struct Visit<'a> {
    inventory: &'a mut BTreeMap<(String, String, Direction, String), FieldObservation>,
    vocabulary: &'a mut Vocabulary,
    scenario: &'a PromotedScenario,
    direction: Direction,
    sequence: u64,
}

impl Visit<'_> {
    fn walk(&mut self, value: &Value, path: String) {
        match value {
            Value::Object(object) => {
                let is_map = MAP_PATHS.contains(&path.as_str());
                for (key, child) in object {
                    let child_path = if is_map {
                        format!("{path}.{{}}")
                    } else {
                        format!("{path}.{key}")
                    };
                    // A map entry is data, not a field, so only its contents
                    // are recorded.
                    if !is_map {
                        self.record(&child_path);
                    }
                    self.walk(child, child_path);
                }
            }
            Value::Array(values) => {
                for child in values {
                    let child_path = format!("{path}[]");
                    self.walk(child, child_path);
                }
            }
            // A leaf. Only a declared discriminator path's value is worth
            // remembering, and only as a scalar — `Object` and `Array`
            // already matched above and can never reach here, so a shape
            // change at a declared path (the value stops being a leaf)
            // silently stops contributing to the vocabulary instead of
            // being stringified into it.
            scalar if VOCABULARY_PATHS.contains(&path.as_str()) => {
                if let Some(value) = scalar_string(scalar) {
                    self.record_vocabulary(&path, value);
                }
            }
            _ => {}
        }
    }

    fn record(&mut self, path: &str) {
        let key = (
            self.scenario.provider.clone(),
            self.scenario.version.clone(),
            self.direction,
            path.to_owned(),
        );
        let observation = self
            .inventory
            .entry(key)
            .or_insert_with(|| FieldObservation {
                provider: self.scenario.provider.clone(),
                version: self.scenario.version.clone(),
                path: path.to_owned(),
                direction: self.direction,
                first_seen: FrameRef {
                    scenario: self.scenario.label.clone(),
                    sequence: self.sequence,
                },
                scenarios: BTreeSet::new(),
            });
        // Every scenario that reaches this path adds itself here, not just
        // the first — `or_insert_with` above only guards the entry's
        // creation, so a field seen in five of a version's eight scenarios
        // still ends up with all five, not the one that happened to sort
        // first.
        observation
            .scenarios
            .insert(scenario_name(&self.scenario.directory));
    }

    fn record_vocabulary(&mut self, path: &str, value: String) {
        self.vocabulary
            .entry((
                self.scenario.provider.clone(),
                self.scenario.version.clone(),
                self.direction,
            ))
            .or_default()
            .entry(path.to_owned())
            .or_default()
            .insert(value);
    }
}

/// The bare scenario directory name (`fresh-text`, `resume`, …) — not the
/// `provider/version/scenario` form [`FrameRef::scenario`] carries. What
/// [`FieldObservation::scenarios`] and the capability sheet's Scenario
/// groups both key on, so a field's tag can be cross-checked directly
/// against the `###`-level scenario names the Scenarios section itself
/// renders.
fn scenario_name(directory: &Path) -> String {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// A JSON scalar's value as vocabulary text, or `None` for `null` — which is
/// "no value", not a value worth recording. `Object` and `Array` are
/// unreachable through [`Visit::walk`]'s leaf arm but are handled here too,
/// so this function stays correct on its own terms rather than by relying on
/// its one caller.
fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Object(_) | Value::Array(_) => None,
    }
}
