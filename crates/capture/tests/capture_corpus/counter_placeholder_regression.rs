//! D76 regression: the committed `claude/2.1.229/subagent` corpus once had
//! four distinct token-counter fields collapsed onto one shared placeholder.
//!
//! `AGENTS.md`'s "What the providers send" section states the sanitizer's
//! join-preserving contract: "equal values share a number so joins across
//! frames still work." A one-off re-sanitize pass (dropping
//! `.tool_use_result.pin.ref` from the allowlist, 2026-08-15) inverted that
//! for this one file: it reconstructed `capture.json` from the capture's own
//! already-sanitized `manifest.json`/`events.jsonl`, and in doing so patched
//! 172 already-redacted numeric-counter positions (`input_tokens`,
//! `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`,
//! `total_tokens`) to a shared dummy `0` before feeding them back through
//! `sanitize_dir`. `sanitize_dir` then did exactly what it always does with
//! equal input -- gave every `0` the same placeholder -- so 38 genuinely
//! distinct original counters all read back as one value.
//!
//! `sanitize.rs`'s own `distinct_unlisted_counter_values_get_distinct_placeholders_equal_ones_share`
//! proves the *live* sanitizer has nothing to fix here: the defect was
//! upstream of it, in a reconstruction step that no longer exists in this
//! crate. This test instead pins the *data*, so a future re-promotion of
//! this scenario cannot silently reintroduce the same collapse.
//!
//! The fingerprint: within one `usage`-shaped object, `cache_creation_input_tokens`,
//! `cache_read_input_tokens`, `input_tokens` and `output_tokens` are four
//! independently-computed numbers (cache write size, cache hit size, prompt
//! size, completion size). Real Claude usage reports never make all four
//! equal at once -- checked against every other scenario in this corpus,
//! zero of their usage objects do -- so "all four share one placeholder"
//! is the collapse's signature, not a coincidence a real capture could
//! produce.

use comet_capture::{corpus_root, frames};
use serde_json::Value;

const SUBAGENT: &str = "claude/2.1.229/subagent";

/// Every object anywhere in `value` that carries all four sibling counter
/// keys, as `[cache_creation, cache_read, input, output]`. Recurses through
/// arrays and objects alike, since the same shape recurs at `.usage`,
/// `.message.usage`, `.tool_use_result.usage` and `.usage.iterations[]`.
fn find_usage_quads<'a>(value: &'a Value, out: &mut Vec<[&'a Value; 4]>) {
    match value {
        Value::Object(map) => {
            if let (Some(cache_creation), Some(cache_read), Some(input), Some(output)) = (
                map.get("cache_creation_input_tokens"),
                map.get("cache_read_input_tokens"),
                map.get("input_tokens"),
                map.get("output_tokens"),
            ) {
                out.push([cache_creation, cache_read, input, output]);
            }
            for child in map.values() {
                find_usage_quads(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                find_usage_quads(item, out);
            }
        }
        _ => {}
    }
}

/// D76: no usage object in the promoted `subagent` corpus has all four
/// sibling counters sharing one placeholder value.
///
/// Before the fix this failed 41/41: every usage quad in the file had
/// `cache_creation_input_tokens == cache_read_input_tokens == input_tokens
/// == output_tokens`, the exact fingerprint of the shared-dummy collapse
/// described in `docs/debt/README.md`'s D76 row. Sibling scenarios
/// `claude/2.1.229/checklist` and `claude/2.1.229/checklist-resume` hold 0/24
/// and 0/18 respectively, confirming the all-equal shape is the defect and
/// not something a real capture produces.
#[test]
fn subagent_usage_counters_are_not_collapsed_to_one_shared_placeholder() {
    let scenario_dir = corpus_root().join(SUBAGENT);
    let events = frames(&scenario_dir).unwrap_or_else(|error| panic!("{SUBAGENT}: {error}"));

    let mut total = 0usize;
    let mut collapsed: Vec<u64> = Vec::new();
    for event in &events {
        let Some(payload) = event["payload"].as_str() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let mut quads = Vec::new();
        find_usage_quads(&value, &mut quads);
        for quad in quads {
            total += 1;
            if quad[0] == quad[1] && quad[1] == quad[2] && quad[2] == quad[3] {
                collapsed.push(event["sequence"].as_u64().unwrap_or(0));
            }
        }
    }

    assert!(
        total > 0,
        "expected at least one usage object with all four counter fields in {SUBAGENT}"
    );
    assert!(
        collapsed.is_empty(),
        "D76: {}/{total} usage objects in {SUBAGENT} have \
         cache_creation_input_tokens == cache_read_input_tokens == input_tokens == \
         output_tokens (sequences {collapsed:?}) -- the fingerprint of the re-sanitize \
         collapse that patched already-redacted counters to a shared dummy before \
         re-running sanitize_dir. See docs/debt/README.md D76.",
        collapsed.len()
    );
}
