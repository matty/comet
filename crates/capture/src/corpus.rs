//! Reading one promoted frame out of the corpus.
//!
//! An earlier version routed this through a claim index: a test named a claim
//! id, and a validator proved the claim's manifest, frames, placeholder
//! accounting and a reciprocal `consumers` list all agreed. Of 49 claims only
//! 21 were ever read by code; the rest asserted that a comment naming the claim
//! id still existed. What that machinery actually protected — a test resting on
//! a frame that moved — is protected better by naming the frame directly, which
//! fails by name when it moves.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;

use super::types::Channel;

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

/// `tests/corpus`, rooted at this crate's own manifest directory regardless
/// of which crate calls it — `env!("CARGO_MANIFEST_DIR")` expands where this
/// function is *defined*, not where it is called from. The one place the
/// literal `Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")` is
/// spelled; every other file that used to repeat it calls this instead.
pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// The fixed filename an exploratory capture entry carries beside any evidence it keeps, so the
/// entry can never be mistaken for promoted corpus evidence by inspection alone — not the
/// directory it happens to sit in, a single named file with fixed content
/// (`docs/testing/provider-captures.md`'s "Exploratory captures" section states what it says).
/// A guard test can grep for this name specifically instead of inferring "exploratory" from a
/// path convention that a copy-paste could silently violate.
///
/// D63: a capture recorded to answer a question, not to back a scenario, still needs somewhere
/// to live that nothing downstream mistakes for evidence — a capability sheet, a decode
/// coverage lint, or the version floor all treat everything under [`corpus_root`] as reviewed.
pub const EXPLORATORY_MARKER_FILENAME: &str = "NOT-CORPUS-EVIDENCE.md";

/// `tests/exploratory`, the sibling of [`corpus_root`] that holds a written-up exploratory
/// finding and, optionally, the sanitized raw evidence behind it (D63).
///
/// Never a subtree of [`corpus_root`] and never walked by [`promoted_scenarios`] or anything
/// built on it — [`promoted_scenarios`] only ever descends from the path it is given, and every
/// caller in this crate gives it [`corpus_root`], never this function. That is what makes the
/// separation structural rather than a naming convention: nothing here can enter a capability
/// sheet, an allowlist property test, the coverage policy, or the version floor, because none of
/// those walk this directory at all, not because they filter it out once they arrive.
pub fn exploratory_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/exploratory")
}

/// One promoted scenario directory: `provider/version/scenario`, holding its
/// own `events.jsonl`.
#[derive(Clone, Debug)]
pub struct PromotedScenario {
    pub directory: PathBuf,
    /// `provider/version/scenario`.
    pub label: String,
    pub provider: String,
    pub version: String,
}

/// Every promoted scenario under `root`: every `provider/version/scenario`
/// three levels deep that holds an `events.jsonl`, sorted by provider, then
/// version, then scenario name.
///
/// The one corpus walk every caller used to reimplement separately —
/// `surface.rs`'s field-inventory walk, the allowlist property tests, and the
/// capability-sheet golden test all repeated this same
/// read-directories/sort/filter-by-`events.jsonl` loop, and the copies had
/// already drifted on what an unreadable subtree does. This function owns the
/// walk only: an unreadable provider or version subtree — `read_dir` failing
/// outright, or one of its entries failing to read — is always an `Err` here,
/// and it is each caller's own job to map that into its own policy
/// (propagate, panic, or otherwise) — this must not be read as unifying that
/// policy, only the traversal.
pub fn promoted_scenarios(root: &Path) -> anyhow::Result<Vec<PromotedScenario>> {
    let mut scenarios = Vec::new();
    for provider in sorted_directories(root)
        .with_context(|| format!("{} could not be walked", root.display()))?
    {
        let provider_name = file_name(&provider);
        // An unreadable subtree is an error, never an empty one. Treating it
        // as empty would drop every field beneath it from a caller whose
        // whole job is saying what the evidence contains.
        for version in sorted_directories(&provider)
            .with_context(|| format!("{} could not be walked", provider.display()))?
        {
            let version_name = file_name(&version);
            for scenario in sorted_directories(&version)
                .with_context(|| format!("{} could not be walked", version.display()))?
            {
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

/// Every directory directly under `parent`, sorted.
///
/// An unreadable entry propagates as an `Err` rather than being filtered
/// out — the same reasoning the two `subdirectories` helpers this function
/// replaced each carried on their own doc comments: skipping an entry
/// silently would let a walk visit a subset of the archive and still read as
/// complete, and "a walk that silently skipped a subtree would report the
/// property as holding over evidence it never read" is exactly the failure
/// mode a corpus-wide property test exists to refuse. `read_dir` itself
/// failing was already an `Err` before this fix; what changed is that a
/// single bad `DirEntry` inside an otherwise-readable directory no longer
/// disappears through a `filter_map(Result::ok)` — it now stops the walk
/// too, same as it always stopped the panicking callers this replaced.
fn sorted_directories(parent: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let path = entry?.path();
        if path.is_dir() {
            directories.push(path);
        }
    }
    directories.sort();
    Ok(directories)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every JSON event line in `scenario_dir`'s `events.jsonl`, blank lines
/// skipped, in file order.
///
/// The read-file -> skip-blank -> parse-event loop every caller that scans
/// more than one frame used to reimplement by hand, disagreeing on how to
/// treat a frame whose payload will not parse (error, panic, or silent
/// skip). This owns only reading the file and parsing each line as JSON;
/// `sequence`, `channel` and `payload` are left as raw `Value` indexing
/// (`event["payload"].as_str()`, …) so each caller keeps deciding — at the
/// call site, not buried in here — what an absent or malformed field means
/// for it.
pub fn frames(scenario_dir: &Path) -> anyhow::Result<Vec<Value>> {
    let events_path = scenario_dir.join("events.jsonl");
    let text = std::fs::read_to_string(&events_path)
        .with_context(|| format!("{} could not be read", events_path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("{} has an invalid event line", events_path.display()))
        })
        .collect()
}

/// Read one frame from a corpus rooted anywhere.
///
/// Panics rather than returning an error: every caller is a test that would
/// immediately unwrap, and the panic message carries the scenario and sequence,
/// which is the whole triage path.
///
/// **One copy of this body exists outside this crate and has to move with it:**
/// `crates/harness/tests/fixtures/fake_claude.rs`'s `corpus_frame_payload`.
/// That file is a `[[bin]]` target, and Cargo never links a bin target's
/// dev-dependencies, so it cannot call this function at all (D87 stage 7) — it
/// re-spells the same addressing convention by hand: corpus root, read
/// `events.jsonl`, skip blank lines, match `sequence`, take `payload`.
///
/// Change any of those and the copy keeps compiling. It fails, but only at run
/// time and only obliquely: the fixture panics inside the spawned child, that
/// panic goes to the child's stderr where no test reads it, and the harness
/// suite reports a discovery mismatch instead. Checked by breaking the copy's
/// corpus root: `comet-harness::claude changing_the_executable_re_runs_discovery`
/// failed with `assertion left == right failed: the good fixture answers` and
/// no mention of the corpus anywhere. So a change here means editing that file
/// too — nothing will tell you legibly if you don't.
pub fn frame(corpus_root: &Path, scenario: &str, sequence: u64) -> Frame {
    let scenario_dir = corpus_root.join(scenario);
    let events = frames(&scenario_dir)
        .unwrap_or_else(|error| panic!("corpus {scenario} is unreadable: {error}"));

    for event in events {
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
/// Kept separate from [`frame`] so a caller can address a corpus rooted
/// somewhere else without this wrapper's baked-in root. Written when the reader
/// still lived inside `comet-harness` and the split was only anticipated; the
/// split happened (D87 stage 7) and the root is now `crates/capture/tests/corpus`.
pub fn corpus_frame(scenario: &str, sequence: u64) -> Frame {
    frame(&corpus_root(), scenario, sequence)
}

/// One frame located by what it is rather than where it landed, together
/// with the sequence it happened to land at.
///
/// An unsolicited notification — Codex's `remoteControl/status/changed` is
/// the one that forced this — can land at a different sequence on every
/// capture, so pinning a frame, or anything recorded after one, to a
/// [`frame`] sequence pins a race. `sequence` still rides along so a caller
/// that located two frames by predicate can assert their relative order
/// (a reply after its request) without pinning either one absolutely.
#[derive(Clone, Debug)]
pub struct FoundFrame {
    pub sequence: u64,
    pub channel: Channel,
    pub value: Value,
}

/// Every frame in `scenario`'s corpus directory whose parsed JSON payload
/// satisfies `predicate`.
///
/// A frame whose payload does not parse as JSON is skipped, the same
/// contract [`frame`] leaves to its callers: payload shape is each caller's
/// concern, not this scan's.
fn frames_where(
    corpus_root: &Path,
    scenario: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Vec<FoundFrame> {
    let scenario_dir = corpus_root.join(scenario);
    let events = frames(&scenario_dir)
        .unwrap_or_else(|error| panic!("corpus {scenario} is unreadable: {error}"));

    events
        .into_iter()
        .filter_map(|event| {
            let sequence = event["sequence"].as_u64()?;
            let channel: Channel = serde_json::from_value(event["channel"].clone()).ok()?;
            let value: Value = serde_json::from_str(event["payload"].as_str()?).ok()?;
            predicate(&value).then_some(FoundFrame {
                sequence,
                channel,
                value,
            })
        })
        .collect()
}

/// [`frames_where`] against this crate's own corpus, requiring exactly one
/// match.
///
/// Panics naming the scenario and `description` on zero matches or on more
/// than one — a predicate that silently matched nothing would let the
/// assertion downstream of it pass vacuously, which is the failure mode a
/// predicate-based lookup exists to avoid, not reintroduce.
pub fn corpus_frame_where(
    scenario: &str,
    description: &str,
    predicate: impl Fn(&Value) -> bool,
) -> FoundFrame {
    let mut found = frames_where(&corpus_root(), scenario, predicate);
    match found.len() {
        1 => found.remove(0),
        0 => panic!("corpus {scenario} has no frame matching: {description}"),
        n => panic!(
            "corpus {scenario} has {n} frames matching: {description} (expected exactly one)"
        ),
    }
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
