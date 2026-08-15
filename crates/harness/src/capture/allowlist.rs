//! The sanitizer's allowlist: which full dotted key paths survive verbatim.
//!
//! Inverted from the blocklist it replaces. A field's value survives sanitizing
//! only if its exact path is on its provider's list; everything else becomes a
//! placeholder. A field nobody has considered arrives redacted, not verbatim —
//! the property the blocklist could not give, because it fails open on a path
//! nobody wrote a rule for.
//!
//! The lists themselves are data, not code: `allowlist/claude.txt` and
//! `allowlist/codex.txt`, one full dotted path per line, reviewed by hand.
//! Adding a line is a decision to publish that field's values forever.
//!
//! **Standing rule (adopted 2026-08-15): a field nothing decodes defaults to
//! redacted.** A path no code reads gains nothing from surviving verbatim, so
//! it does not belong on the list even when its sampled values look dull.
//! Two anchors for what "nothing decodes" means in practice:
//! `crates/harness/src/claude/discovery.rs:188-190` names `agents`,
//! `account`, `commands` and `output_style` as deliberately unmodelled, and
//! `crates/harness/src/codex/normalize.rs:426` reads only `usedPercent` off
//! `rateLimits` — every sibling field under those objects is dead weight the
//! allowlist should not be carrying. Apply this rule on every future
//! regeneration, not just at the paths it has already been applied to.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use super::types::Provider;

const CLAUDE_TXT: &str = include_str!("allowlist/claude.txt");
const CODEX_TXT: &str = include_str!("allowlist/codex.txt");

/// Marker type for the module this file implements: the per-provider path
/// allowlist. The lookups themselves are the free functions below, not
/// methods — there is no per-instance state to hang them on.
pub struct Allowlist;

fn parse(source: &'static str) -> BTreeSet<&'static str> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn claude_paths() -> &'static BTreeSet<&'static str> {
    static PATHS: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    PATHS.get_or_init(|| parse(CLAUDE_TXT))
}

fn codex_paths() -> &'static BTreeSet<&'static str> {
    static PATHS: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    PATHS.get_or_init(|| parse(CODEX_TXT))
}

/// Whether `path`'s value survives sanitizing for `provider`.
///
/// `path` is a full dotted key path, matched exactly against the provider's
/// list — leaf-name matching is the mistake this design deliberately avoids,
/// because it is the mechanism by which the removed disposition report
/// mismatched `.message.diagnostics.cache_miss_reason.type` against a rule
/// meant for something else named `type`.
pub fn allows(provider: Provider, path: &str) -> bool {
    match provider {
        Provider::Claude => claude_paths().contains(path),
        Provider::Codex => codex_paths().contains(path),
    }
}

/// The full leaf-to-name table `named_kind` is driven off. A `const` table
/// rather than a bare `match` so the test below can collapse the table
/// itself into its name set, instead of a second hardcoded leaf list that
/// could drift from the `match` arms — see that test's doc comment for why
/// the drift risk is the point.
const NAMED_LEAVES: &[(&str, &str)] = &[
    ("sessionId", "SESSION"),
    ("threadId", "THREAD"),
    ("turnId", "TURN"),
    ("expectedTurnId", "TURN"),
    ("toolUseId", "TOOL_USE"),
    ("parentToolUseId", "TOOL_USE"),
    ("itemId", "TOOL_USE"),
    ("uuid", "MACHINE"),
    ("requestId", "REQUEST"),
];

/// The readable name for an identifier leaf, or `None` if it is not one of
/// the six kinds actually read. Everything else is numbered (`<V1>`, `<V2>`,
/// …) rather than named — a lookup table keyed on field name, not a
/// taxonomy, and capped at six on purpose: a kind that is not read does not
/// get a name.
pub fn named_kind(leaf: &str) -> Option<&'static str> {
    NAMED_LEAVES
        .iter()
        .find(|(name, _)| *name == leaf)
        .map(|(_, kind)| *kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path on the list survives; one that is not does not. The `.method`
    /// case matters because the capability sheet reads that vocabulary.
    #[test]
    fn a_listed_path_is_allowed_and_an_unlisted_one_is_not() {
        assert!(allows(Provider::Codex, ".method"));
        assert!(!allows(Provider::Codex, ".nobody.wrote.a.rule.for.this"));
    }

    /// The three findings this stage exists to fix must NOT be allowed.
    #[test]
    fn the_local_configuration_leaks_are_not_allowed() {
        assert!(!allows(Provider::Codex, ".result.userAgent"));
        assert!(!allows(Provider::Claude, ".plugins[].source"));
        assert!(!allows(Provider::Claude, ".plugins[].name"));
        assert!(!allows(Provider::Claude, ".agents[]"));
    }

    /// The lists are per provider, so allowing something for one cannot
    /// quietly allow it for the other.
    ///
    /// `.mcp_servers[].status` is picked deliberately: it is on Claude's list
    /// and Codex has no `.mcp_servers[]` array anywhere in its observed wire
    /// shape, so it is genuinely absent from `codex.txt` rather than merely
    /// excluded from both — a provider-blind lookup (search the union of both
    /// sets, ignore which provider was asked for) flips the second assertion
    /// from `false` to `true`. An earlier version of this test used
    /// `.plugins[].source` and `.result.userAgent`, both of which are on
    /// decision 1's exclusion list for *both* providers — absent from both
    /// sets rather than present in only one — so a provider-blind lookup
    /// could not trip it. Falsified against a real union-search mutation
    /// before landing; see the Task 1 report for the quoted failure.
    #[test]
    fn the_lists_are_independent() {
        assert!(allows(Provider::Claude, ".mcp_servers[].status"));
        assert!(!allows(Provider::Codex, ".mcp_servers[].status"));
    }

    /// Six names, no more, no fewer — pinned by counting the table `named_kind`
    /// is actually driven off, not a second hardcoded leaf list.
    ///
    /// The prior body asserted three positives and two negatives over a
    /// hardcoded nine-leaf list and never counted, so a seventh name
    /// reachable through a *new* leaf (for example, `"agentId" =>
    /// Some("AGENT")` added to the `match`) would pass unchanged — the
    /// hardcoded list never learns about a leaf it doesn't already name.
    /// This version collapses `NAMED_LEAVES` itself — the table `named_kind`
    /// reads from — so a seventh distinct name is a cardinality change no
    /// matter which leaf introduces it, and the leaf/name pairing is closed
    /// by the round-trip loop below.
    #[test]
    fn only_the_six_identifier_kinds_are_named() {
        let names: BTreeSet<&str> = NAMED_LEAVES.iter().map(|(_, kind)| *kind).collect();

        assert_eq!(
            names,
            BTreeSet::from([
                "SESSION", "THREAD", "TURN", "TOOL_USE", "MACHINE", "REQUEST"
            ]),
            "NAMED_LEAVES must collapse into exactly these six names"
        );

        for (leaf, kind) in NAMED_LEAVES {
            assert_eq!(
                named_kind(leaf),
                Some(*kind),
                "named_kind must answer exactly what its own table says for {leaf}"
            );
        }

        assert_eq!(named_kind("id"), None, "a bare id is numbered, not named");
        assert_eq!(named_kind("costUSD"), None);
    }

    /// Adjudicated 2026-08-15, in two rounds. These paths were on the
    /// generated allowlist (they always held a literal, non-placeholder
    /// string somewhere in the promoted corpus) but were removed by hand
    /// after reading real sample values. Round 1 covered local-tooling
    /// identity leaks in the same class as `.plugins[]`/`.agents[]`/
    /// `.result.userAgent`, and user/agent-authored free text in the same
    /// class as `.message.content[].input.content`/`.input.description`
    /// (decision 2's own two paths are pinned here too, since round 1 named
    /// only their mirrors and never the originals). Round 2 added the
    /// standing rule this module's doc comment records — a field nothing
    /// decodes gains nothing from surviving — plus two more identity leaks a
    /// second reviewer found by checking sampled values against what code
    /// actually reads. Several of these are mirrored copies of a field
    /// decision 1 or 2, or an earlier round of this same review, already
    /// meant to redact, reachable under a second literal path the exclusion
    /// list didn't name. Bringing one of these back is a fresh decision, not
    /// a regeneration accident: this test is what stops a future run of
    /// `derive-allowlist.py`-style tooling from silently re-adding a line
    /// someone already looked at and said no to.
    #[test]
    fn the_reviewed_exclusions_stay_excluded() {
        // Decision 2's own two paths — round 1 only pinned their mirrors
        // (`.request.input.content`, `.response.response.updatedInput.content`,
        // `.request.description`), never the originals.
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.content"
        ));
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.description"
        ));

        // Decision 1's `.plugins[]` prefix, pinned at leaves round 1 didn't
        // separately name (only `.name` and `.source` were). `.path` holds a
        // filesystem path and was excluded only by the generator's prefix
        // rule until this line gave it its own test.
        assert!(!allows(Provider::Claude, ".plugins[].version"));
        assert!(!allows(Provider::Claude, ".plugins[].path"));

        // Round 1 — local-tooling / configuration identity, same class as
        // the three findings this stage exists to fix.
        assert!(!allows(Provider::Claude, ".mcp_servers[].name"));
        assert!(!allows(
            Provider::Claude,
            ".response.response.agents[].name"
        ));
        assert!(!allows(Provider::Claude, ".skills[]"));
        assert!(!allows(Provider::Claude, ".slash_commands[]"));
        assert!(!allows(
            Provider::Claude,
            ".response.response.commands[].name"
        ));
        assert!(!allows(
            Provider::Claude,
            ".response.response.commands[].aliases[]"
        ));
        assert!(!allows(Provider::Claude, ".message.content[].input.skill"));
        assert!(!allows(Provider::Claude, ".tool_use_result.commandName"));

        // Round 1 — arbitrary command text / raw command output:
        // categorically unsafe regardless of what today's captures happen
        // to run.
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.command"
        ));
        assert!(!allows(Provider::Claude, ".tool_use_result.stdout"));

        // Round 1 — mirrored copies of decision 2's content/description
        // fields under the approval-request/response wrappers.
        assert!(!allows(Provider::Claude, ".request.input.content"));
        assert!(!allows(
            Provider::Claude,
            ".response.response.updatedInput.content"
        ));
        assert!(!allows(Provider::Claude, ".request.description"));
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.message"
        ));
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.subject"
        ));
        assert!(!allows(Provider::Claude, ".tool_use_result.task.subject"));
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.activeForm"
        ));

        // Round 1 — Codex's own conversation-preview snippet, same class as
        // the Claude free-text fields above.
        assert!(!allows(Provider::Codex, ".result.thread.preview"));

        // Round 2 — the fifth mirror of the MCP-server-identity class:
        // `.tools[]` embeds the server name in every `mcp__<server>__<tool>`
        // entry, the same identity `.mcp_servers[].name` already excludes.
        assert!(!allows(Provider::Claude, ".tools[]"));

        // Round 2 — same value space as the excluded `.agents[]` roster,
        // proven by value overlap (`general-purpose` appears in all three).
        assert!(!allows(Provider::Claude, ".subagent_type"));
        assert!(!allows(
            Provider::Claude,
            ".message.content[].input.subagent_type"
        ));
        assert!(!allows(Provider::Claude, ".tool_use_result.agentType"));

        // Round 2 — enumerates installed output styles, same class as the
        // excluded `.skills[]`/`.slash_commands[]`.
        assert!(!allows(
            Provider::Claude,
            ".response.response.available_output_styles[]"
        ));

        // Round 2 — the standing rule applied: `crates/harness/src/claude/
        // discovery.rs:188-190` documents `agents`, `account`, `commands`
        // and `output_style` as deliberately unmodelled, so nothing downstream
        // reads any of these.
        assert!(!allows(
            Provider::Claude,
            ".response.response.account.tokenSource"
        ));
        assert!(!allows(
            Provider::Claude,
            ".response.response.account.apiProvider"
        ));
        assert!(!allows(Provider::Claude, ".output_style"));
        assert!(!allows(Provider::Claude, ".response.response.output_style"));
        // `.terminal_slash_commands[]` is a subset of the already-excluded
        // `.slash_commands[]` catalog (`commands`, unmodelled per the anchor
        // above), distinguished only by an unverified assumption — round 1's
        // keep-ruling on it was reversed on that basis.
        assert!(!allows(Provider::Claude, ".terminal_slash_commands[]"));

        // Round 2 — NOT the standing rule above: `.hook_name` is the
        // user-authored matcher half of a hook definition read out of
        // `settings.json`, not one of discovery.rs's four unmodelled
        // fields. It is excluded as user-authored configuration text, the
        // same class as the command/content/description fields above.
        assert!(!allows(Provider::Claude, ".hook_name"));

        // Round 2 — NOT the standing rule either: both of these ARE read —
        // `.message.content[].input.query` feeds `ToolCall::WebSearch` at
        // `crates/harness/src/claude/normalize.rs:414`. They are excluded
        // as free-form user-authored search text, the same class as the
        // command/content/description fields above, not because nothing
        // downstream reads them.
        assert!(!allows(Provider::Claude, ".message.content[].input.query"));
        assert!(!allows(Provider::Claude, ".tool_use_result.query"));

        // Round 2 — the standing rule applied on the codex side:
        // `crates/harness/src/codex/normalize.rs:426` reads only
        // `usedPercent` off `rateLimits`; balance and plan type are unread
        // account/financial state.
        assert!(!allows(
            Provider::Codex,
            ".params.rateLimits.credits.balance"
        ));
        assert!(!allows(Provider::Codex, ".params.rateLimits.planType"));
    }
}
