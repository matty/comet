//! Reading one promoted frame out of the corpus.
//!
//! An earlier version routed this through a claim index: a test named a claim
//! id, and a validator proved the claim's manifest, frames, placeholder
//! accounting and a reciprocal `consumers` list all agreed. Of 49 claims only
//! 21 were ever read by code; the rest asserted that a comment naming the claim
//! id still existed. What that machinery actually protected — a test resting on
//! a frame that moved — is protected better by naming the frame directly, which
//! fails by name when it moves.

use std::path::Path;

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
