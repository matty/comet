use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::types::{CaptureEvent, Channel, PlatformMetadata, Provider, StdioMode};

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("provider corpus index could not be read ({kind:?})")]
    IndexRead { kind: std::io::ErrorKind },
    #[error("provider corpus index is not valid schema JSON at line {line}, column {column}")]
    InvalidIndex { line: usize, column: usize },
    #[error("provider corpus index schema version {version} is unsupported; expected version 1")]
    UnsupportedIndexSchemaVersion { version: u64 },
    #[error("provider corpus claim id {claim_id:?} is duplicated")]
    DuplicateClaimId { claim_id: String },
    #[error("provider corpus claim {claim_id:?} has a non-canonical or unsafe manifest path")]
    UnsafeManifestPath { claim_id: String },
    #[error("provider corpus claim {claim_id:?} is missing manifest {manifest}")]
    MissingManifest { claim_id: String, manifest: String },
    #[error(
        "provider corpus manifest {manifest} for claim {claim_id:?} is invalid at line {line}, column {column}"
    )]
    InvalidManifest {
        claim_id: String,
        manifest: String,
        line: usize,
        column: usize,
    },
    #[error(
        "provider corpus manifest {manifest} for claim {claim_id:?} uses unsupported schema version {version}; expected version 1"
    )]
    UnsupportedManifestSchemaVersion {
        claim_id: String,
        manifest: String,
        version: u64,
    },
    #[error(
        "provider corpus manifest {manifest} does not name consumer {consumer:?} for claim {claim_id:?}"
    )]
    MissingManifestConsumer {
        claim_id: String,
        manifest: String,
        consumer: String,
    },
    #[error("provider corpus manifest {manifest} names stale consumer {consumer:?}")]
    ExtraManifestConsumer { manifest: String, consumer: String },
    #[error("provider corpus manifest {manifest} repeats consumer {consumer:?}")]
    DuplicateManifestConsumer { manifest: String, consumer: String },
    #[error("provider corpus claim {claim_id:?} has no evidence entries")]
    MissingEvidence { claim_id: String },
    #[error(
        "provider corpus comparative claim {claim_id:?} selects {total_frames} frames but only {distinct_observations} distinct observations; at least two are required"
    )]
    InsufficientComparisonEvidence {
        claim_id: String,
        total_frames: usize,
        distinct_observations: usize,
    },
    #[error("provider corpus claim {claim_id:?} is missing events beside {manifest}")]
    MissingEvents { claim_id: String, manifest: String },
    #[error("provider corpus event line {line} for claim {claim_id:?} is invalid")]
    InvalidEvent { claim_id: String, line: usize },
    #[error(
        "provider corpus events for claim {claim_id:?} expected sequence {expected}, found {actual}"
    )]
    NonContiguousEventSequence {
        claim_id: String,
        manifest: String,
        expected: u64,
        actual: u64,
    },
    #[error("provider corpus claim {claim_id:?} references missing frame {sequence} in {manifest}")]
    MissingFrame {
        claim_id: String,
        manifest: String,
        sequence: u64,
    },
    #[error(
        "provider corpus claim {claim_id:?} frame {sequence} expects {expected:?}, found {actual:?}"
    )]
    FrameChannelMismatch {
        claim_id: String,
        sequence: u64,
        expected: Channel,
        actual: Channel,
    },
    #[error(
        "provider corpus claim {claim_id:?} contains unresolved placeholder syntax at {location}"
    )]
    UnresolvedPlaceholder {
        claim_id: String,
        location: &'static str,
    },
    #[error("provider corpus manifest {manifest} does not define used placeholder {placeholder}")]
    MissingPlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error("provider corpus manifest {manifest} defines unused placeholder {placeholder}")]
    UnusedPlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error("provider corpus manifest {manifest} defines placeholder {placeholder} more than once")]
    DuplicatePlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error(
        "provider corpus manifest {manifest} defines {placeholder} as {actual_kind:?}, expected {expected_kind:?}"
    )]
    PlaceholderKindMismatch {
        claim_id: String,
        manifest: String,
        placeholder: String,
        expected_kind: String,
        actual_kind: String,
    },
    #[error("provider corpus claim {claim_id:?} was not found")]
    ClaimNotFound { claim_id: String },
    #[error("provider corpus claim {claim_id:?} selects {count} frames; exactly one is required")]
    SelectedFrameCount { claim_id: String, count: usize },
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusIndex {
    schema_version: u64,
    claims: Vec<CorpusClaim>,
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusClaim {
    id: String,
    consumer: String,
    #[serde(default)]
    comparison: bool,
    evidence: Vec<ClaimEvidence>,
    #[allow(dead_code)]
    fact: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ClaimEvidence {
    manifest: String,
    frames: Vec<ClaimFrame>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct ClaimFrame {
    sequence: u64,
    channel: Channel,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u64,
    source: String,
    provider: Provider,
    cli_version: String,
    normalized_cli_version: String,
    captured_at_unix_ms: i64,
    scenario: String,
    purpose: String,
    platform: PlatformMetadata,
    command: CorpusCommand,
    channels: Vec<Channel>,
    exit_code: Option<i32>,
    placeholders: Vec<PlaceholderDefinition>,
    redaction_counts: BTreeMap<String, u64>,
    #[serde(default)]
    consumers: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CorpusCommand {
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    configured_env: BTreeMap<String, String>,
    stdin: StdioMode,
    stdout: StdioMode,
    stderr: StdioMode,
    kill_on_drop: bool,
    #[serde(default)]
    creation_flags: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PlaceholderDefinition {
    placeholder: String,
    kind: String,
}

/// One promoted frame, addressed by the scenario directory that holds it.
///
/// Replaces claim-ID indirection: a test names the evidence it rests on
/// directly, so a re-recording that moves or renumbers a frame fails that test
/// by name instead of passing an index check that proves only that a comment
/// still exists.
#[derive(Clone, Debug)]
pub struct Frame {
    pub channel: Channel,
    pub payload: String,
}

/// Read one frame from a corpus rooted anywhere.
///
/// Panics rather than returning an error: every caller is a test that would
/// immediately unwrap, and the panic message carries the scenario and sequence,
/// which is the whole triage path.
pub fn frame(corpus_root: &Path, scenario: &str, sequence: u64) -> Frame {
    let events_path = corpus_root.join(scenario).join("events.jsonl");
    let events = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|error| panic!("corpus {scenario} is unreadable: {error}"));

    for line in events.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("corpus {scenario} has an invalid event: {error}"));
        if event["sequence"].as_u64() != Some(sequence) {
            continue;
        }
        let channel: Channel = serde_json::from_value(event["channel"].clone())
            .unwrap_or_else(|_| panic!("corpus {scenario} frame {sequence} has no known channel"));
        let payload = event["payload"]
            .as_str()
            .unwrap_or_else(|| panic!("corpus {scenario} frame {sequence} has no payload"))
            .to_owned();
        return Frame { channel, payload };
    }

    panic!("corpus {scenario} has no frame {sequence}");
}

/// [`frame`] against this crate's own corpus.
///
/// Kept separate from [`frame`] so the reader can move to its own crate later
/// while this path stays anchored to `comet-harness`.
pub fn corpus_frame(scenario: &str, sequence: u64) -> Frame {
    frame(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
        scenario,
        sequence,
    )
}

/// Validate every claim in a checked-in provider corpus without stopping at the first absent
/// evidence file. Index-shape errors are global; artifact errors are returned once per claim.
pub fn validate_corpus(corpus_root: &Path) -> Vec<CorpusError> {
    let index = match read_index(corpus_root) {
        Ok(index) => index,
        Err(error) => return vec![error],
    };

    if let Some(duplicate) = duplicate_claim_id(&index.claims) {
        return vec![CorpusError::DuplicateClaimId {
            claim_id: duplicate,
        }];
    }

    let expected_consumers = expected_manifest_consumers(&index.claims);
    let mut errors = Vec::new();
    for claim in &index.claims {
        if let Err(error) = validate_comparison_frame_count(claim) {
            errors.push(error);
            continue;
        }
        if claim.evidence.is_empty() {
            errors.push(CorpusError::MissingEvidence {
                claim_id: claim.id.clone(),
            });
            continue;
        }
        let errors_before_claim = errors.len();
        for evidence in &claim.evidence {
            if let Err(error) = validate_evidence(
                corpus_root,
                claim,
                evidence,
                expected_consumers
                    .get(&evidence.manifest)
                    .expect("evidence contributes consumer expectations"),
            ) {
                errors.push(error);
            }
        }
        if errors.len() == errors_before_claim
            && let Err(error) = validate_distinct_comparison_observations(claim)
        {
            errors.push(error);
        }
    }
    errors
}

/// Return the provider's literal payload for a claim that references exactly one event frame.
/// The payload is never decoded into or reserialized through a Comet wire type.
/// Corpus-wide comparison policy does not change this exact-one-frame selection contract.
pub fn selected_payload(corpus_root: &Path, claim_id: &str) -> Result<String, CorpusError> {
    let index = read_index(corpus_root)?;
    if let Some(duplicate) = duplicate_claim_id(&index.claims) {
        return Err(CorpusError::DuplicateClaimId {
            claim_id: duplicate,
        });
    }
    let claim = index
        .claims
        .iter()
        .find(|claim| claim.id == claim_id)
        .ok_or_else(|| CorpusError::ClaimNotFound {
            claim_id: claim_id.to_owned(),
        })?;
    let frame_count = claim
        .evidence
        .iter()
        .map(|evidence| evidence.frames.len())
        .sum();
    if frame_count != 1 {
        return Err(CorpusError::SelectedFrameCount {
            claim_id: claim.id.clone(),
            count: frame_count,
        });
    }
    let expected_consumers = expected_manifest_consumers(&index.claims);
    let evidence = claim
        .evidence
        .iter()
        .find(|evidence| !evidence.frames.is_empty())
        .ok_or_else(|| CorpusError::MissingEvidence {
            claim_id: claim.id.clone(),
        })?;
    let frame = evidence.frames[0];
    let events = validate_evidence(
        corpus_root,
        claim,
        evidence,
        expected_consumers
            .get(&evidence.manifest)
            .expect("selected evidence contributes consumer expectations"),
    )?;
    events
        .into_iter()
        .find(|event| event.sequence == frame.sequence)
        .map(|event| event.payload)
        .ok_or_else(|| CorpusError::MissingFrame {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            sequence: frame.sequence,
        })
}

/// Return the provider's literal payloads for a claim referencing one OR MORE
/// event frames, in the order the claim lists them.
///
/// [`selected_payload`] deliberately refuses a claim that selects more than one
/// frame, and that contract is right for a claim about a single reply shape.
/// It cannot express a claim whose whole point is a RELATIONSHIP between
/// frames — that a tool call's input carries one half of a change and its
/// result the other, or that a resumed run's init says nothing about a task its
/// next frame updates. Those need the frames together, in order, and reading
/// them one call at a time would lose the ordering the claim asserts.
///
/// The validation performed is identical; only the arity differs.
pub fn selected_payloads(corpus_root: &Path, claim_id: &str) -> Result<Vec<String>, CorpusError> {
    let index = read_index(corpus_root)?;
    if let Some(duplicate) = duplicate_claim_id(&index.claims) {
        return Err(CorpusError::DuplicateClaimId {
            claim_id: duplicate,
        });
    }
    let claim = index
        .claims
        .iter()
        .find(|claim| claim.id == claim_id)
        .ok_or_else(|| CorpusError::ClaimNotFound {
            claim_id: claim_id.to_owned(),
        })?;
    let expected_consumers = expected_manifest_consumers(&index.claims);
    let mut payloads = Vec::new();
    for evidence in &claim.evidence {
        let events = validate_evidence(
            corpus_root,
            claim,
            evidence,
            expected_consumers
                .get(&evidence.manifest)
                .expect("selected evidence contributes consumer expectations"),
        )?;
        for frame in &evidence.frames {
            let payload = events
                .iter()
                .find(|event| event.sequence == frame.sequence)
                .map(|event| event.payload.clone())
                .ok_or_else(|| CorpusError::MissingFrame {
                    claim_id: claim.id.clone(),
                    manifest: evidence.manifest.clone(),
                    sequence: frame.sequence,
                })?;
            payloads.push(payload);
        }
    }
    if payloads.is_empty() {
        return Err(CorpusError::MissingEvidence {
            claim_id: claim.id.clone(),
        });
    }
    Ok(payloads)
}

fn read_index(corpus_root: &Path) -> Result<CorpusIndex, CorpusError> {
    let bytes =
        std::fs::read(corpus_root.join("index.json")).map_err(|source| CorpusError::IndexRead {
            kind: source.kind(),
        })?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|source| CorpusError::InvalidIndex {
            line: source.line(),
            column: source.column(),
        })?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(CorpusError::InvalidIndex { line: 0, column: 0 })?;
    if version != 1 {
        return Err(CorpusError::UnsupportedIndexSchemaVersion { version });
    }
    let index: CorpusIndex =
        serde_json::from_value(value.clone()).map_err(|source| CorpusError::InvalidIndex {
            line: source.line(),
            column: source.column(),
        })?;
    debug_assert_eq!(index.schema_version, 1);
    let raw_claims = value
        .get("claims")
        .and_then(Value::as_array)
        .ok_or(CorpusError::InvalidIndex { line: 0, column: 0 })?;
    for (claim, raw_claim) in index.claims.iter().zip(raw_claims) {
        let mut ignored_uses = BTreeMap::new();
        if inspect_placeholder_value(raw_claim, &mut ignored_uses).is_err() {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "index",
            });
        }
    }
    Ok(index)
}

fn duplicate_claim_id(claims: &[CorpusClaim]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    claims
        .iter()
        .find(|claim| !seen.insert(claim.id.as_str()))
        .map(|claim| claim.id.clone())
}

fn comparison_evidence_counts(claim: &CorpusClaim) -> (usize, usize) {
    let mut total_frames = 0;
    let mut distinct_observations = std::collections::BTreeSet::new();
    for evidence in &claim.evidence {
        for frame in &evidence.frames {
            total_frames += 1;
            distinct_observations.insert((
                evidence.manifest.as_str(),
                frame.sequence,
                match frame.channel {
                    Channel::Stdin => 0,
                    Channel::Stdout => 1,
                    Channel::Stderr => 2,
                },
            ));
        }
    }
    (total_frames, distinct_observations.len())
}

fn validate_comparison_frame_count(claim: &CorpusClaim) -> Result<(), CorpusError> {
    let (total_frames, distinct_observations) = comparison_evidence_counts(claim);
    if claim.comparison && total_frames < 2 {
        return Err(CorpusError::InsufficientComparisonEvidence {
            claim_id: claim.id.clone(),
            total_frames,
            distinct_observations,
        });
    }
    Ok(())
}

fn validate_distinct_comparison_observations(claim: &CorpusClaim) -> Result<(), CorpusError> {
    let (total_frames, distinct_observations) = comparison_evidence_counts(claim);
    if claim.comparison && distinct_observations < 2 {
        return Err(CorpusError::InsufficientComparisonEvidence {
            claim_id: claim.id.clone(),
            total_frames,
            distinct_observations,
        });
    }
    Ok(())
}

fn expected_manifest_consumers(
    claims: &[CorpusClaim],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut expected = BTreeMap::<String, BTreeMap<String, String>>::new();
    for claim in claims {
        for evidence in &claim.evidence {
            expected
                .entry(evidence.manifest.clone())
                .or_default()
                .entry(claim.consumer.clone())
                .or_insert_with(|| claim.id.clone());
        }
    }
    expected
}

fn validate_evidence(
    corpus_root: &Path,
    claim: &CorpusClaim,
    evidence: &ClaimEvidence,
    expected_consumers: &BTreeMap<String, String>,
) -> Result<Vec<CaptureEvent>, CorpusError> {
    if !is_canonical_relative_path(&evidence.manifest) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let manifest_path = corpus_root.join(&evidence.manifest);
    let manifest_bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CorpusError::MissingManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
            });
        }
        Err(source) => {
            return Err(CorpusError::InvalidManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                line: 0,
                column: source.raw_os_error().unwrap_or_default() as usize,
            });
        }
    };
    if !existing_path_stays_below(corpus_root, &manifest_path) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let manifest_value: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|source| CorpusError::InvalidManifest {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            line: source.line(),
            column: source.column(),
        })?;
    let version = manifest_value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| CorpusError::InvalidManifest {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            line: 0,
            column: 0,
        })?;
    if version != 1 {
        return Err(CorpusError::UnsupportedManifestSchemaVersion {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            version,
        });
    }
    let manifest: CorpusManifest =
        serde_json::from_value(manifest_value.clone()).map_err(|source| {
            CorpusError::InvalidManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                line: source.line(),
                column: source.column(),
            }
        })?;
    let mut placeholder_uses = BTreeMap::new();
    let mut manifest_payload = manifest_value;
    let mut ignored_definition_uses = BTreeMap::new();
    if manifest_payload
        .get("placeholders")
        .is_some_and(|definitions| {
            inspect_placeholder_value(definitions, &mut ignored_definition_uses).is_err()
        })
    {
        return Err(CorpusError::UnresolvedPlaceholder {
            claim_id: claim.id.clone(),
            location: "manifest",
        });
    }
    if let Some(object) = manifest_payload.as_object_mut() {
        object.remove("placeholders");
    }
    if inspect_placeholder_value(&manifest_payload, &mut placeholder_uses).is_err() {
        return Err(CorpusError::UnresolvedPlaceholder {
            claim_id: claim.id.clone(),
            location: "manifest",
        });
    }
    let mut actual_consumers = BTreeMap::<&str, usize>::new();
    for consumer in &manifest.consumers {
        let count = actual_consumers.entry(consumer).or_default();
        *count += 1;
        if *count > 1 {
            return Err(CorpusError::DuplicateManifestConsumer {
                manifest: evidence.manifest.clone(),
                consumer: consumer.clone(),
            });
        }
    }
    for (consumer, claim_id) in expected_consumers {
        if !actual_consumers.contains_key(consumer.as_str()) {
            return Err(CorpusError::MissingManifestConsumer {
                claim_id: claim_id.clone(),
                manifest: evidence.manifest.clone(),
                consumer: consumer.clone(),
            });
        }
    }
    if let Some(extra) = actual_consumers
        .keys()
        .find(|consumer| !expected_consumers.contains_key(**consumer))
    {
        return Err(CorpusError::ExtraManifestConsumer {
            manifest: evidence.manifest.clone(),
            consumer: (*extra).to_owned(),
        });
    }

    let events_path = manifest_path.with_file_name("events.jsonl");
    let events_bytes = match std::fs::read(&events_path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CorpusError::MissingEvents {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
            });
        }
        Err(_) => {
            return Err(CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: 0,
            });
        }
    };
    if !existing_path_stays_below(corpus_root, &events_path) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let mut events = Vec::new();
    for (offset, line) in events_bytes.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let event_value: Value =
            serde_json::from_slice(line).map_err(|_| CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: offset + 1,
            })?;
        if inspect_placeholder_value(&event_value, &mut placeholder_uses).is_err() {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "events",
            });
        }
        let event: CaptureEvent =
            serde_json::from_value(event_value).map_err(|_| CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: offset + 1,
            })?;
        if let Ok(payload) = serde_json::from_str::<Value>(&event.payload)
            && inspect_placeholder_value(&payload, &mut placeholder_uses).is_err()
        {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "events",
            });
        }
        let expected = events.len() as u64 + 1;
        if event.sequence != expected {
            return Err(CorpusError::NonContiguousEventSequence {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                expected,
                actual: event.sequence,
            });
        }
        events.push(event);
    }

    validate_placeholder_definitions(claim, evidence, &manifest, &placeholder_uses)?;

    for frame in &evidence.frames {
        let Some(event) = events.iter().find(|event| event.sequence == frame.sequence) else {
            return Err(CorpusError::MissingFrame {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                sequence: frame.sequence,
            });
        };
        if event.channel != frame.channel {
            return Err(CorpusError::FrameChannelMismatch {
                claim_id: claim.id.clone(),
                sequence: frame.sequence,
                expected: frame.channel,
                actual: event.channel,
            });
        }
    }
    Ok(events)
}

fn is_canonical_relative_path(path: &str) -> bool {
    if path.is_empty()
        || path.contains('\\')
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path.contains(':')
    {
        return false;
    }
    let path_value = Path::new(path);
    if path_value.file_name().and_then(|name| name.to_str()) != Some("manifest.json") {
        return false;
    }
    let normalized = path_value
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_string_lossy().into_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    !normalized.iter().any(|component| {
        component.is_empty()
            || component.trim_end_matches(['.', ' ']) != component
            || is_windows_reserved_component(component)
    }) && normalized.join("/") == path
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn existing_path_stays_below(corpus_root: &Path, path: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(corpus_root) else {
        return false;
    };
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    path.starts_with(root)
}

#[derive(Clone, Copy)]
enum KnownPlaceholder {
    Static(&'static str),
    Typed(&'static str),
}

fn inspect_placeholder_value(
    value: &Value,
    placeholder_uses: &mut BTreeMap<String, String>,
) -> Result<(), ()> {
    match value {
        Value::Array(values) => {
            for value in values {
                inspect_placeholder_value(value, placeholder_uses)?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                inspect_placeholder_text(key, placeholder_uses)?;
                inspect_placeholder_value(value, placeholder_uses)?;
            }
        }
        Value::String(text) => inspect_placeholder_text(text, placeholder_uses)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn inspect_placeholder_text(
    text: &str,
    placeholder_uses: &mut BTreeMap<String, String>,
) -> Result<(), ()> {
    if text.contains("{{") || text.contains("${") || text.contains("[REDACTED]") {
        return Err(());
    }
    let mut remaining = text;
    while let Some(start) = remaining.find('<') {
        remaining = &remaining[start + 1..];
        let Some(end) = remaining.find('>') else {
            return if marker_like(remaining) {
                Err(())
            } else {
                Ok(())
            };
        };
        let candidate = &remaining[..end];
        if marker_like(candidate) {
            match known_placeholder(candidate) {
                Some(KnownPlaceholder::Static(kind) | KnownPlaceholder::Typed(kind)) => {
                    placeholder_uses.insert(format!("<{candidate}>"), kind.to_owned());
                }
                None => return Err(()),
            }
        }
        remaining = &remaining[end + 1..];
    }
    Ok(())
}

fn marker_like(candidate: &str) -> bool {
    !candidate.is_empty()
        && !candidate.chars().any(char::is_whitespace)
        && candidate
            .bytes()
            .any(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-'))
}

fn known_placeholder(candidate: &str) -> Option<KnownPlaceholder> {
    let static_kind = match candidate {
        "CWD" => Some("cwd_path"),
        "REPO" => Some("repo_path"),
        "HOME" => Some("home_path"),
        "TEMP" => Some("temp_path"),
        "CODEX_HOME" => Some("codex_home_path"),
        "APPROVAL_TARGET" => Some("approval_target_path"),
        "TRUSTED_POWERSHELL" => Some("trusted_powershell_path"),
        _ => None,
    };
    if let Some(kind) = static_kind {
        return Some(KnownPlaceholder::Static(kind));
    }
    const INDEXED: &[(&str, &str)] = &[
        ("CLAUDE_REQUEST_ID", "claude_request_id"),
        ("CODEX_RPC_ID", "codex_rpc_id"),
        ("SESSION_ID", "session_id"),
        ("THREAD_ID", "thread_id"),
        ("TURN_ID", "turn_id"),
        ("TOOL_USE_ID", "tool_use_id"),
        ("USER_TEXT", "user_text"),
        ("ASSISTANT_PROSE", "assistant_prose"),
        ("PROVIDER_PROSE", "provider_prose"),
        ("MACHINE_ID", "machine_id"),
        ("ATTACHMENT_BYTES", "attachment_bytes"),
        ("CLAUDE_MEMORY_PATH", "claude_memory_path"),
        ("CLAUDE_MESSAGE_ID", "claude_message_id"),
        ("CLAUDE_THINKING_SIGNATURE", "claude_thinking_signature"),
        ("CODEX_MCP_SERVER_NAME", "codex_mcp_server_name"),
        ("CODEX_THREAD_PATH", "codex_thread_path"),
    ];
    INDEXED.iter().find_map(|(prefix, kind)| {
        candidate
            .strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('_'))
            .filter(|number| {
                number
                    .as_bytes()
                    .first()
                    .is_some_and(|digit| matches!(digit, b'1'..=b'9'))
                    && number.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map(|_| KnownPlaceholder::Typed(kind))
    })
}

fn validate_placeholder_definitions(
    claim: &CorpusClaim,
    evidence: &ClaimEvidence,
    manifest: &CorpusManifest,
    placeholder_uses: &BTreeMap<String, String>,
) -> Result<(), CorpusError> {
    let mut definitions = BTreeMap::<String, String>::new();
    for definition in &manifest.placeholders {
        let candidate = definition
            .placeholder
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'));
        let Some(known) = candidate.and_then(known_placeholder) else {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "manifest",
            });
        };
        let expected_kind = match known {
            KnownPlaceholder::Static(kind) | KnownPlaceholder::Typed(kind) => kind,
        };
        if definition.kind != expected_kind {
            return Err(CorpusError::PlaceholderKindMismatch {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                placeholder: definition.placeholder.clone(),
                expected_kind: expected_kind.to_owned(),
                actual_kind: definition.kind.clone(),
            });
        }
        if definitions
            .insert(definition.placeholder.clone(), definition.kind.clone())
            .is_some()
        {
            return Err(CorpusError::DuplicatePlaceholderDefinition {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                placeholder: definition.placeholder.clone(),
            });
        }
    }
    if let Some((placeholder, _)) = placeholder_uses
        .iter()
        .find(|(placeholder, _)| !definitions.contains_key(*placeholder))
    {
        return Err(CorpusError::MissingPlaceholderDefinition {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            placeholder: placeholder.clone(),
        });
    }
    if let Some((placeholder, _)) = definitions
        .iter()
        .find(|(placeholder, _)| !placeholder_uses.contains_key(*placeholder))
    {
        return Err(CorpusError::UnusedPlaceholderDefinition {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            placeholder: placeholder.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    /// Addressing a frame by scenario and sequence returns that frame's exact
    /// payload bytes and its channel.
    ///
    /// The payload must be the extracted `payload` field, not the raw event
    /// line: the raw line also contains the substring "control_response" (it
    /// is nested inside the escaped payload text), so a `contains` check on
    /// that substring alone cannot tell the two apart. Parsing the payload as
    /// its own JSON document and checking its *top-level* shape can: the raw
    /// line's top level is `{"sequence", "channel", "payload"}`, while the
    /// extracted payload's top level is the control-response envelope itself,
    /// with no `sequence` or `channel` key of its own.
    #[test]
    fn a_frame_is_addressed_by_scenario_and_sequence() {
        let found = corpus_frame("claude/2.1.228/model-discovery", 2);
        assert_eq!(found.channel, Channel::Stdout);
        let parsed: Value =
            serde_json::from_str(&found.payload).expect("payload is its own valid JSON document");
        assert_eq!(
            parsed.get("type").and_then(Value::as_str),
            Some("control_response"),
            "the model-discovery reply frame: {}",
            found.payload
        );
        assert!(
            parsed.get("sequence").is_none() && parsed.get("channel").is_none(),
            "payload must be the extracted payload, not the raw event line: {}",
            found.payload
        );
    }

    /// A stdin frame is reachable too, so input surface stays addressable.
    #[test]
    fn a_stdin_frame_is_addressable() {
        let found = corpus_frame("claude/2.1.228/attachment", 1);
        assert_eq!(found.channel, Channel::Stdin);
    }

    /// A missing sequence names the scenario and the sequence, so triage starts
    /// at the frame rather than at a grep.
    #[test]
    #[should_panic(expected = "claude/2.1.228/model-discovery has no frame 9999")]
    fn a_missing_sequence_names_the_scenario_and_sequence() {
        corpus_frame("claude/2.1.228/model-discovery", 9999);
    }

    /// A missing scenario directory fails by name too, which is what catches a
    /// re-recording that moved a scenario.
    #[test]
    #[should_panic(expected = "claude/9.9.9/nope")]
    fn a_missing_scenario_names_itself() {
        corpus_frame("claude/9.9.9/nope", 1);
    }
}
