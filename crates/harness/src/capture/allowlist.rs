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
//!
//! Seven listed paths are reviewed exceptions to that rule, not oversights a
//! future regeneration should clean up: `.apiKeySource` and the four
//! `inference_geo` paths (`.event.message.usage.inference_geo`,
//! `.message.usage.inference_geo`, `.tool_use_result.usage.inference_geo`,
//! `.usage.inference_geo`) on `claude.txt`, and `.result.platformFamily` /
//! `.result.platformOs` on `codex.txt`. Nothing decodes any of them either —
//! they were kept by deliberate ruling anyway, on the judgment that each
//! holds a coarse, non-identifying category (a request's auth-source kind, a
//! geographic inference bucket, a platform family/OS string) rather than
//! anything that names a machine, a user, or a path. Regenerating the list
//! should neither strip these seven nor treat their survival as proof the
//! standing rule is optional elsewhere.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use super::sanitize::normalize_field;
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
///
/// **Keys are already `normalize_field`-normalized** (lowercase, no
/// underscores) — not the raw wire spelling. Claude spells these snake_case
/// (`session_id`) and Codex camelCase (`threadId`); a table keyed on one
/// literal spelling silently matched nothing for the other provider, which
/// is exactly the bug `named_kind` had until this fix (`session_id` covered
/// 642 Claude occurrences and the `"sessionId"` entry covered 5; three of
/// the nine leaves — `toolUseId`, `parentToolUseId`, `requestId` — matched
/// *zero*, because Claude never emits those camelCase spellings at all).
/// `named_leaves_are_already_normalized`'s "keys already normalized"
/// assertion is what stops a future editor from adding a raw, unnormalized
/// spelling back in.
const NAMED_LEAVES: &[(&str, &str)] = &[
    ("sessionid", "SESSION"),
    ("threadid", "THREAD"),
    ("turnid", "TURN"),
    ("expectedturnid", "TURN"),
    ("tooluseid", "TOOL_USE"),
    ("parenttooluseid", "TOOL_USE"),
    ("itemid", "TOOL_USE"),
    ("uuid", "MACHINE"),
    ("requestid", "REQUEST"),
];

/// The readable name for an identifier leaf, or `None` if it is not one of
/// the six kinds actually read. Everything else is numbered (`<V1>`, `<V2>`,
/// …) rather than named — a lookup table keyed on field name, not a
/// taxonomy, and capped at six on purpose: a kind that is not read does not
/// get a name.
///
/// `leaf` is normalized before lookup — the same `normalize_field` the
/// sanitizer's own scan uses — so `session_id` (Claude's real wire spelling)
/// and a hypothetical `sessionId` both resolve to the one `sessionid` table
/// entry. Route through the shared function rather than duplicating its
/// rule: two copies of a normalization rule is how they drift.
pub fn named_kind(leaf: &str) -> Option<&'static str> {
    let normalized = normalize_field(leaf);
    NAMED_LEAVES
        .iter()
        .find(|(name, _)| *name == normalized.as_str())
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

    /// `named_kind` normalizes the incoming leaf before comparing it against
    /// `NAMED_LEAVES`, so a table key that is *not itself* already normalized
    /// can never be reached by that comparison — `normalize_field("agent_id")`
    /// is `"agentid"`, which would never equal a stored `"agent_id"` entry.
    /// A future editor adding a leaf with its raw wire spelling (underscores,
    /// mixed case) would silently write a dead row rather than a working one.
    /// This pins every table key to its own normalized form so that mistake
    /// is a test failure, not a silent no-op.
    #[test]
    fn named_leaves_are_already_normalized() {
        for &(leaf, kind) in NAMED_LEAVES {
            assert_eq!(
                normalize_field(leaf),
                leaf,
                "NAMED_LEAVES entry {leaf:?} (-> {kind}) is not already normalized"
            );
        }
    }

    /// The bug the coordinator found: `named_kind`'s table was keyed on one
    /// literal spelling per leaf (all camelCase), so a leaf whose real wire
    /// spelling differs matched nothing at all — three of the original nine
    /// leaves (`toolUseId`, `parentToolUseId`, `requestId`) matched *zero*
    /// occurrences anywhere in the committed corpus, and `sessionId` matched
    /// 5 of Claude's 647 session ids because Claude spells it `session_id`.
    /// Vocabulary tests (`only_the_six_identifier_kinds_are_named`) can't
    /// catch this — they only check that the table's *shape* is six names,
    /// never that each name actually matches something real. This walks the
    /// real committed corpus (reading only; nothing here writes to it) and
    /// fails if any of the six names covers zero occurrences.
    ///
    /// Counted per **leaf** (the normalized wire key, e.g. `parenttooluseid`),
    /// not per kind: `TOOL_USE` has three leaves feeding it
    /// (`toolUseId`/`parentToolUseId`/`itemId`), and counting only the kind
    /// would let a leaf that matches nothing hide behind a sibling that
    /// matches plenty — a tenth leaf spelled `parenttoolusid` (missing the
    /// second `e`) would still leave `TOOL_USE`'s total above zero. Asserting
    /// each leaf's own count is what makes the test's name true.
    #[test]
    fn every_named_leaf_matches_something_in_the_committed_corpus() {
        let mut occurrences: std::collections::BTreeMap<&'static str, u64> =
            std::collections::BTreeMap::new();
        for events_path in corpus_events_files() {
            let text = std::fs::read_to_string(&events_path)
                .unwrap_or_else(|error| panic!("{}: {error}", events_path.display()));
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("{}: {error}", events_path.display()));
                let Some(payload) = event["payload"].as_str() else {
                    continue;
                };
                // Non-JSON stderr prose has no keys to count; anything else
                // unparseable would be a corpus-integrity bug this test isn't
                // responsible for catching.
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload) else {
                    continue;
                };
                count_named_leaves(&payload, &mut occurrences);
            }
        }

        for &(leaf, kind) in NAMED_LEAVES {
            let count = occurrences.get(leaf).copied().unwrap_or(0);
            assert!(
                count > 0,
                "named leaf {leaf:?} (-> {kind}) matches nothing in the committed corpus \
                 (counts: {occurrences:?})"
            );
        }
    }

    fn count_named_leaves(
        value: &serde_json::Value,
        counts: &mut std::collections::BTreeMap<&'static str, u64>,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, child) in object {
                    let normalized = normalize_field(key);
                    if let Some(&(leaf, _)) = NAMED_LEAVES
                        .iter()
                        .find(|(leaf, _)| *leaf == normalized.as_str())
                    {
                        *counts.entry(leaf).or_default() += 1;
                    }
                    count_named_leaves(child, counts);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    count_named_leaves(item, counts);
                }
            }
            _ => {}
        }
    }

    fn corpus_events_files() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut files = Vec::new();
        for provider in subdirectories(&root) {
            for version in subdirectories(&provider) {
                for scenario in subdirectories(&version) {
                    let events = scenario.join("events.jsonl");
                    if events.is_file() {
                        files.push(events);
                    }
                }
            }
        }
        assert!(
            !files.is_empty(),
            "found no events.jsonl under {} -- corpus walk is broken, not just empty",
            root.display()
        );
        files
    }

    fn subdirectories(parent: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(parent)
            .unwrap_or_else(|error| panic!("{}: {error}", parent.display()))
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
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

        // Task 2 review (2026-08-15) — `.request.display_name` exists to hold
        // a *friendly rendering*, not the raw tool name: an MCP invocation's
        // friendly rendering can plausibly read `create_issue
        // (linear)`, naming the server while never containing the
        // literal `mcp__` prefix `is_mcp_tool_identity` matches on. Nothing
        // decodes it (`crates/harness/src/claude/wire.rs:694` records it as
        // present and deliberately undecoded), so the standing "nothing
        // decodes it" rule excludes it regardless of the prefix-check gap.
        assert!(!allows(Provider::Claude, ".request.display_name"));

        // Final review (2026-08-15) — reverses an earlier keep-ruling.
        // `.tool_use_result.pin.id` and `.pin.name` are already excluded;
        // `.pin.ref` was kept as the odd one out. In the corpus it reads
        // `"pin":{"id":"<V245>","name":"<V245>","ref":"18303e"}` -- its two
        // sibling identity fields are redacted and this one alone was
        // published verbatim. Nothing decodes it and no test references it,
        // so by the standing "nothing decodes it" rule, and because it is
        // the only opaque identifier left among the surviving literals
        // (`allowlist_property.rs`'s own documented blind spot -- it only
        // ever inspects a scalar it is about to call an escape, never one an
        // allowed path keeps), it should not have been kept regardless of
        // being a sibling of two excluded fields.
        assert!(!allows(Provider::Claude, ".tool_use_result.pin.ref"));
    }
}
