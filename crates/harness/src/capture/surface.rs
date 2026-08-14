//! The corpus walker behind the provider surface map.
//!
//! Answers one question over promoted evidence: which fields do Claude Code and
//! Codex actually put on the wire, in which direction, at which version. The
//! output is read by people deciding what to build, so it records **shape**
//! and publishes a value only when the value's own grammar makes that safe.
//!
//! The safety rule here is the same one [`super::sanitize`] uses to decide what
//! may be carried literally. That is deliberate: two tests for "is this safe to
//! show" would eventually disagree, and the one that drifted would be the one
//! nobody reruns.

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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum JsonType {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

/// Where a field was first seen, so triage starts at the frame and not a grep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameRef {
    /// `provider/version/scenario`, the corpus directory that holds it.
    pub scenario: String,
    pub sequence: u64,
}

/// What may be said about a field's values in a document people read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ValueSample {
    /// Distinct token-shaped values, capped. These are what make a field
    /// designable against: `end_turn | tool_use` beats "a string".
    pub literals: BTreeSet<String>,
    /// Kinds behind redacted values, from the manifest's placeholder table.
    /// More useful than the placeholder and free, because the sanitizer
    /// already did the classification.
    pub redaction_kinds: BTreeSet<String>,
    /// At least one value was prose or otherwise unpublishable.
    pub withheld: bool,
    /// The literal set hit [`MAX_LITERALS`]. Recorded rather than silently
    /// dropped, so a report never implies it listed every value it saw.
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct FieldObservation {
    pub provider: String,
    /// Dotted path from the frame root. `[]` is an array element and `{}` is a
    /// map entry, so `.modelUsage.{}.costUSD` names a field and not a model id.
    pub path: String,
    pub direction: Direction,
    pub versions: BTreeSet<String>,
    pub scenarios: BTreeSet<String>,
    pub json_types: BTreeSet<JsonType>,
    pub count: u64,
    pub first_seen: FrameRef,
    pub values: ValueSample,
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
/// An undeclared map shows up in the report as a field with an obviously
/// data-shaped name, which triage catches and adds here.
const MAP_PATHS: &[&str] = &[".modelUsage"];

/// Beyond this many distinct literals a field is an open set, not an enum, and
/// listing more stops being useful.
const MAX_LITERALS: usize = 8;

/// Characters a value may contain and still be published verbatim. No ASCII
/// whitespace: prose has spaces, and prose is the whole risk.
const SAFE_VALUE_CHARS: &str = "._:/@+-[]()\\";
const MAX_VALUE_BYTES: usize = 64;

pub fn observe_corpus(corpus_root: &Path) -> Result<Vec<FieldObservation>, SurfaceError> {
    let scenarios = promoted_scenarios(corpus_root)?;
    if scenarios.is_empty() {
        return Err(SurfaceError::EmptyCorpus {
            root: corpus_root.to_owned(),
        });
    }

    let mut inventory: BTreeMap<(String, Direction, String), FieldObservation> = BTreeMap::new();
    for scenario in scenarios {
        let placeholders = placeholder_kinds(&scenario.directory);
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
            // A stderr frame is allowed to be plain text; it carries no fields.
            let Some(payload) = event["payload"]
                .as_str()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            else {
                continue;
            };
            let mut visit = Visit {
                inventory: &mut inventory,
                scenario: &scenario,
                direction,
                sequence,
                placeholders: &placeholders,
            };
            visit.walk(&payload, String::new());
        }
    }

    Ok(inventory.into_values().collect())
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
        for version in sorted_directories(&provider).unwrap_or_default() {
            let version_name = file_name(&version);
            for scenario in sorted_directories(&version).unwrap_or_default() {
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

/// The manifest's placeholder table, `<SESSION_ID_1>` to `session_id`.
///
/// Absent or malformed is not an error: a manifest predating a schema is still
/// real evidence, and a missing entry only costs the derived kind below.
fn placeholder_kinds(directory: &Path) -> BTreeMap<String, String> {
    let Ok(bytes) = std::fs::read(directory.join("manifest.json")) else {
        return BTreeMap::new();
    };
    let Ok(manifest) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeMap::new();
    };
    manifest["placeholders"]
        .as_array()
        .map(|definitions| {
            definitions
                .iter()
                .filter_map(|definition| {
                    Some((
                        definition["placeholder"].as_str()?.to_owned(),
                        definition["kind"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

struct Visit<'a> {
    inventory: &'a mut BTreeMap<(String, Direction, String), FieldObservation>,
    scenario: &'a PromotedScenario,
    direction: Direction,
    sequence: u64,
    placeholders: &'a BTreeMap<String, String>,
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
                        self.record(&child_path, child);
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
            _ => {}
        }
    }

    fn record(&mut self, path: &str, value: &Value) {
        let key = (
            self.scenario.provider.clone(),
            self.direction,
            path.to_owned(),
        );
        let observation = self
            .inventory
            .entry(key)
            .or_insert_with(|| FieldObservation {
                provider: self.scenario.provider.clone(),
                path: path.to_owned(),
                direction: self.direction,
                versions: BTreeSet::new(),
                scenarios: BTreeSet::new(),
                json_types: BTreeSet::new(),
                count: 0,
                first_seen: FrameRef {
                    scenario: self.scenario.label.clone(),
                    sequence: self.sequence,
                },
                values: ValueSample::default(),
            });
        observation.count += 1;
        observation.versions.insert(self.scenario.version.clone());
        observation.scenarios.insert(self.scenario.label.clone());
        observation.json_types.insert(json_type(value));
        if let Value::String(text) = value {
            sample(&mut observation.values, text, self.placeholders);
        }
    }
}

fn json_type(value: &Value) -> JsonType {
    match value {
        Value::Null => JsonType::Null,
        Value::Bool(_) => JsonType::Bool,
        Value::Number(_) => JsonType::Number,
        Value::String(_) => JsonType::String,
        Value::Array(_) => JsonType::Array,
        Value::Object(_) => JsonType::Object,
    }
}

fn sample(values: &mut ValueSample, text: &str, placeholders: &BTreeMap<String, String>) {
    if let Some(kind) = redaction_kind(text, placeholders) {
        values.redaction_kinds.insert(kind);
        return;
    }
    if !publishable(text) {
        values.withheld = true;
        return;
    }
    if values.literals.len() >= MAX_LITERALS && !values.literals.contains(text) {
        values.truncated = true;
        return;
    }
    values.literals.insert(text.to_owned());
}

/// `<SESSION_ID_1>` to `session_id`, via the manifest and then by derivation.
///
/// Only a value that is *entirely* a placeholder names a kind. A value that
/// merely contains one (`<CWD>\marker.txt`) is a shape worth publishing whole.
fn redaction_kind(text: &str, placeholders: &BTreeMap<String, String>) -> Option<String> {
    if !is_placeholder(text) {
        return None;
    }
    if let Some(kind) = placeholders.get(text) {
        return Some(kind.clone());
    }
    let inner = text.trim_start_matches('<').trim_end_matches('>');
    let stem = inner
        .rsplit_once('_')
        .filter(|(_, suffix)| suffix.chars().all(|character| character.is_ascii_digit()))
        .map_or(inner, |(stem, _)| stem);
    Some(stem.to_ascii_lowercase())
}

fn is_placeholder(text: &str) -> bool {
    text.len() > 2
        && text.starts_with('<')
        && text.ends_with('>')
        && text[1..text.len() - 1].chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

/// Token-shaped: an enum, an id, a model name, a path fragment. Never prose.
fn publishable(text: &str) -> bool {
    let masked = mask_placeholders(text);
    masked.len() <= MAX_VALUE_BYTES
        && masked.chars().all(|character| {
            character.is_ascii_alphanumeric() || SAFE_VALUE_CHARS.contains(character)
        })
}

fn mask_placeholders(text: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        if is_placeholder(&rest[start..=end]) {
            masked.push_str(&rest[..start]);
            masked.push('P');
            rest = &rest[end + 1..];
        } else {
            masked.push_str(&rest[..=start]);
            rest = &rest[start + 1..];
        }
    }
    masked.push_str(rest);
    masked
}
