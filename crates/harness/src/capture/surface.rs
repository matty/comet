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
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("corpus root {root} could not be read")]
    UnreadableRoot { root: PathBuf },
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
pub const MAP_PATHS: &[&str] = &[".modelUsage"];

/// Discriminator paths whose observed *values* form a provider's vocabulary —
/// not every field, only the ones whose few distinct values answer "what
/// kinds of thing does this harness say" (design §3.5, SNAPSHOT). `.type`
/// names a frame's own kind, `.subtype` narrows a `system` frame, `.event.type`
/// narrows a streamed `stream_event`, `.method` is Codex's frame kind, and the
/// remaining two name which tool ran.
///
/// Declared rather than inferred, for the reason [`MAP_PATHS`] is: "this
/// field has few distinct values" is a property of a small corpus, not of
/// the protocol, and a set built by scanning today's captures for
/// low-cardinality fields would silently stop growing the day a genuinely
/// new value arrived — inference already trusts whatever it has seen.
///
/// Found by grepping the committed corpus, not guessed:
///
/// - `.type`, `.subtype` — Claude's `system`/`result`/`assistant`/… frame
///   kind and its `init`/`success`/`hook_started`/… narrowing, both at the
///   frame root.
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
///   the stage-5 capability sheet reads" (`ledger-stage-3-allowlist.md:43`),
///   which is why `.method` is on `allowlist/codex.txt` even though this
///   const didn't read it until now.
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
type Vocabulary = BTreeMap<(String, String, Direction), BTreeMap<String, BTreeSet<String>>>;

pub fn observe_corpus(corpus_root: &Path) -> Result<Vec<FieldObservation>, SurfaceError> {
    Ok(walk_corpus(corpus_root)?.0)
}

/// The value vocabulary for every version in the corpus. Walks the same
/// evidence [`observe_corpus`] does — see [`walk_corpus`] — so a value
/// collected here is a value some promoted capture actually contains.
pub fn observe_vocabulary(corpus_root: &Path) -> Result<Vocabulary, SurfaceError> {
    Ok(walk_corpus(corpus_root)?.1)
}

/// The one pass over the archive both [`observe_corpus`] and
/// [`observe_vocabulary`] read from, so the field inventory and the value
/// vocabulary don't each re-walk the corpus independently.
fn walk_corpus(corpus_root: &Path) -> Result<(Vec<FieldObservation>, Vocabulary), SurfaceError> {
    let scenarios = promoted_scenarios(corpus_root)?;
    if scenarios.is_empty() {
        return Err(SurfaceError::EmptyCorpus {
            root: corpus_root.to_owned(),
        });
    }

    let mut inventory: BTreeMap<(String, String, Direction, String), FieldObservation> =
        BTreeMap::new();
    let mut vocabulary: Vocabulary = BTreeMap::new();
    for scenario in scenarios {
        let events =
            std::fs::read_to_string(scenario.directory.join("events.jsonl")).map_err(|_| {
                SurfaceError::UnreadableEvents {
                    scenario: scenario.label.clone(),
                }
            })?;
        for line in events.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let event: Value =
                serde_json::from_str(line).map_err(|_| SurfaceError::UnreadableEvents {
                    scenario: scenario.label.clone(),
                })?;
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

struct PromotedScenario {
    directory: PathBuf,
    /// `provider/version/scenario`.
    label: String,
    provider: String,
    version: String,
}

fn promoted_scenarios(corpus_root: &Path) -> Result<Vec<PromotedScenario>, SurfaceError> {
    let unreadable = || SurfaceError::UnreadableRoot {
        root: corpus_root.to_owned(),
    };
    let mut scenarios = Vec::new();
    for provider in sorted_directories(corpus_root).ok_or_else(unreadable)? {
        let provider_name = file_name(&provider);
        // An unreadable subtree is an error, never an empty one. Treating it as
        // empty would drop every field beneath it from a snapshot whose whole
        // job is saying what the evidence contains.
        for version in sorted_directories(&provider).ok_or_else(unreadable)? {
            let version_name = file_name(&version);
            for scenario in sorted_directories(&version).ok_or_else(unreadable)? {
                if !scenario.join("events.jsonl").is_file() {
                    continue;
                }
                let scenario_name = file_name(&scenario);
                scenarios.push(PromotedScenario {
                    label: format!("{provider_name}/{version_name}/{scenario_name}"),
                    provider: provider_name.clone(),
                    version: version_name.clone(),
                    directory: scenario,
                });
            }
        }
    }
    Ok(scenarios)
}

fn sorted_directories(parent: &Path) -> Option<Vec<PathBuf>> {
    let mut directories: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    directories.sort();
    Some(directories)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
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
        self.inventory
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
            });
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

/// Every observed field as `provider direction path`, the snapshot's line form.
pub fn observed_field_lines(observations: &[FieldObservation]) -> BTreeSet<String> {
    observations
        .iter()
        .map(|observation| {
            let direction = match observation.direction {
                Direction::ToProvider => "to-provider",
                Direction::FromProvider => "from-provider",
            };
            format!("{} {direction} {}", observation.provider, observation.path)
        })
        .collect()
}
