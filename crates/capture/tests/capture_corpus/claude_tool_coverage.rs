//! Every Claude tool name Comet decodes is one the corpus has seen, and every
//! tool name the corpus shows is one somebody decided about.
//!
//! The third surface to get this treatment, after ACP `sessionUpdate` kinds and
//! Codex methods (D69, D90). It is the one where the answer is least
//! comfortable: **six typed decodes have no capture behind them**, and one of
//! them is `Edit` — the tool two open rows (D17, D18) reason about entirely
//! from typings.
//!
//! **Evidence is counted wherever the name appears**, which took a fix to get
//! right. `.message.content[].name` and `.event.content_block.name` only see a
//! tool the model announced in a message; an approval-gated call names itself
//! at `.request.tool_name` and nowhere else, so the `approval` scenarios' own
//! `Write` was invisible until that path was declared alongside them. There is
//! a fourth spelling, `.message.content[].content[].tool_name`, on a tool
//! result's nested block — checked and deliberately not declared: every name it
//! carries today (`TaskCreate`, `TaskUpdate`) is already evidenced by the
//! announcement paths, so declaring it would add sheet noise and no evidence.

use std::collections::BTreeSet;
use std::path::Path;

use comet_capture::{corpus_root, observe_surface};

/// The three declared paths a Claude tool name can appear at.
const TOOL_NAME_PATHS: &[&str] = &[
    ".message.content[].name",
    ".event.content_block.name",
    ".request.tool_name",
];

/// Tool names Comet decodes that no promoted Claude capture has ever shown, at
/// any path.
///
/// Each is a typed decode reached only by a tool no captured run invoked — the
/// failure `.agents/rules/optional-wire-fields.md` names, one level up: the
/// field-shape assumptions inside these arms have never met a real payload.
const DECODED_WITHOUT_EVIDENCE: &[(&str, &str)] = &[
    (
        "Grep",
        "Reads `pattern`/`path`. No promoted scenario searches; the ACP corpus has \
         Grok's own grep, which is a different tool on a different wire.",
    ),
    (
        "Glob",
        "Reads `pattern`. Same gap as Grep — no promoted scenario globs.",
    ),
    (
        "WebFetch",
        "Reads `url`/`prompt`. Every capture runs offline by design, so the two web \
         tools cannot appear without a scenario that deliberately reaches the \
         network.",
    ),
    (
        "WebSearch",
        "Reads `query`. Offline for the same reason as WebFetch.",
    ),
    (
        "TaskList",
        "The third member of the Task family. `TaskCreate` and `TaskUpdate` are both \
         evidenced; a snapshot read has never been captured, which D67 already \
         notes from the other direction — a `TaskList` is the only route by which \
         an item could leave a list, and nothing has observed one.",
    ),
];

/// Tool names the corpus shows that Comet decodes into no typed `ToolCall`.
///
/// Not a gap by default: `decode_tool_use`'s own comment records that falling
/// through to `ToolCall::Unknown` is how a tool renders as an ordinary chip,
/// which is right for anything Comet has no richer surface for. What this list
/// forces is that the fall-through is a decision per tool, rather than the
/// place unexamined names accumulate.
const EVIDENCED_WITHOUT_A_DECODE: &[(&str, &str)] = &[
    (
        "Agent",
        "Decoded by NAME, just not here: `transcript.rs`'s `is_agent_spawn_chip` \
         matches the `Unknown` chip called \"Agent\" and suppresses it beside the \
         subagent card, because the two cannot be joined once `tool_use_id` is \
         dropped from the part. A typed variant would break that pairing rather \
         than improve it.",
    ),
    (
        "SendMessage",
        "No richer surface exists for it, so an ordinary chip is the honest render. \
         A typed variant would have to invent a shape for a payload nothing \
         displays.",
    ),
    (
        "Skill",
        "Renders as an ordinary chip. The slash-command surface it belongs to is \
         Comet's own (`pickers.rs`), driven by the command list rather than by a \
         tool call in the transcript.",
    ),
    (
        "ToolSearch",
        "An internal capability lookup with nothing a user acts on; an ordinary \
         chip is the whole of what it deserves.",
    ),
];

/// Every tool name `claude::normalize` dispatches on.
///
/// Two matches, both read as source text and both bounded to their own
/// function: `decode_tool_use` for the typed `ToolCall` family, and
/// `TaskCallKind::from_tool_name` for the checklist family, which decodes into
/// `ChecklistReplaced`/`ChecklistItemChanged` rather than a `ToolCall` and would
/// be missed by a scan that only knew about the first.
fn decoded_tools() -> BTreeSet<String> {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/src/claude/normalize.rs"),
    )
    .expect("the Claude normalizer's source");

    let mut names = BTreeSet::new();
    for (function, opener) in [
        ("pub(crate) fn decode_tool_use(", "match name {"),
        ("fn from_tool_name(", "match name {"),
    ] {
        let start = source
            .find(function)
            .unwrap_or_else(|| panic!("{function} is where a Claude tool name is dispatched"));
        let block = &source[start..];
        let match_start = block
            .find(opener)
            .unwrap_or_else(|| panic!("{function}'s own dispatch"));
        let block = &block[match_start..];
        // The catch-all ends the named arms: `_ =>` in both matches.
        let end = block.find("_ =>").unwrap_or(block.len());
        names.extend(arm_names(&block[..end]));
    }
    names
}

/// String-literal patterns of the match arms in `block`.
fn arm_names(block: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in block.lines() {
        let line = line.trim();
        if !line.starts_with('"') || !line.contains("=>") {
            continue;
        }
        let patterns = line.split("=>").next().unwrap_or_default();
        for piece in patterns.split('|') {
            let piece = piece.trim().trim_matches('"');
            if !piece.is_empty() {
                names.push(piece.to_owned());
            }
        }
    }
    names
}

/// Every tool name any promoted Claude capture shows, at any declared path.
fn evidenced_tools() -> BTreeSet<String> {
    let (_, vocabulary) = observe_surface(&corpus_root()).expect("the corpus walks");
    let mut names = BTreeSet::new();
    for ((provider, _, _), paths) in &vocabulary {
        if provider != "claude" {
            continue;
        }
        for path in TOOL_NAME_PATHS {
            if let Some(values) = paths.get(*path) {
                names.extend(values.iter().cloned());
            }
        }
    }
    names
}

#[test]
fn every_decoded_claude_tool_has_evidence_or_a_recorded_reason() {
    let decoded = decoded_tools();
    assert!(
        decoded.len() > 6,
        "the source scan found only {decoded:?} — a dispatch moved and this lint \
         would otherwise pass by checking almost nothing"
    );
    let evidenced = evidenced_tools();
    assert!(
        !evidenced.is_empty(),
        "no Claude capture shows a tool name; a vocabulary path or the corpus moved"
    );

    let declared: BTreeSet<&str> = DECODED_WITHOUT_EVIDENCE
        .iter()
        .map(|(tool, _)| *tool)
        .collect();
    let undeclared: Vec<&String> = decoded
        .iter()
        .filter(|tool| !evidenced.contains(*tool) && !declared.contains(tool.as_str()))
        .collect();
    assert!(
        undeclared.is_empty(),
        "{} decoded Claude tool(s) appear in no capture and have no reason recorded: \
         {undeclared:?}. Capture one, or add it to DECODED_WITHOUT_EVIDENCE saying \
         what the arm assumes about a payload it has never met.",
        undeclared.len()
    );
}

#[test]
fn every_evidenced_claude_tool_is_decoded_or_declined() {
    let decoded = decoded_tools();
    let evidenced = evidenced_tools();
    let declined: BTreeSet<&str> = EVIDENCED_WITHOUT_A_DECODE
        .iter()
        .map(|(tool, _)| *tool)
        .collect();

    let unhandled: Vec<&String> = evidenced
        .iter()
        .filter(|tool| {
            !decoded.contains(*tool)
                && !declined.contains(tool.as_str())
                // MCP tools are decoded structurally, by prefix rather than by
                // name — `decode_tool_use`'s catch-all splits
                // `mcp__<server>__<tool>` — so no list could name them.
                && !tool.starts_with("mcp__")
        })
        .collect();
    assert!(
        unhandled.is_empty(),
        "{} captured Claude tool(s) have no decode and no recorded decision: \
         {unhandled:?}. Decode it, or record why an ordinary chip is the right \
         render.",
        unhandled.len()
    );
}

#[test]
fn a_declared_claude_tool_that_changes_state_is_pruned() {
    let decoded = decoded_tools();
    let evidenced = evidenced_tools();

    let stale: Vec<&str> = DECODED_WITHOUT_EVIDENCE
        .iter()
        .map(|(tool, _)| *tool)
        .filter(|tool| evidenced.contains(*tool))
        .collect();
    assert!(
        stale.is_empty(),
        "{} tool(s) in DECODED_WITHOUT_EVIDENCE now have a capture behind them: \
         {stale:?}. Delete the entry — that is the good outcome, and leaving it \
         understates the coverage.",
        stale.len()
    );

    let gone: Vec<&str> = DECODED_WITHOUT_EVIDENCE
        .iter()
        .map(|(tool, _)| *tool)
        .filter(|tool| !decoded.contains(*tool))
        .collect();
    assert!(
        gone.is_empty(),
        "{} declared tool(s) are no longer decoded: {gone:?}. The arm went and its \
         entry outlived it.",
        gone.len()
    );

    let claimed: Vec<&str> = EVIDENCED_WITHOUT_A_DECODE
        .iter()
        .map(|(tool, _)| *tool)
        .filter(|tool| decoded.contains(*tool))
        .collect();
    assert!(
        claimed.is_empty(),
        "{} declined tool(s) are now decoded: {claimed:?}. Delete the entry.",
        claimed.len()
    );

    for (tool, reason) in DECODED_WITHOUT_EVIDENCE
        .iter()
        .chain(EVIDENCED_WITHOUT_A_DECODE)
    {
        assert!(
            reason.len() > 40,
            "{tool}'s entry has to say WHY, not just that it is so: {reason:?}"
        );
    }
}

/// The bound, as a number that moves only on purpose.
///
/// **Six of eleven decoded tools have never met a real payload.** That is not a
/// count anybody should be comfortable with, and pinning it is what stops it
/// drifting upward one convenient arm at a time.
#[test]
fn the_unevidenced_claude_tool_count_is_pinned() {
    assert_eq!(
        DECODED_WITHOUT_EVIDENCE.len(),
        5,
        "the number of Claude tool decodes with no capture behind them changed"
    );
    assert_eq!(
        EVIDENCED_WITHOUT_A_DECODE.len(),
        4,
        "the number of captured tools deliberately left as ordinary chips changed"
    );
}
