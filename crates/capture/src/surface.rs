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

use std::borrow::Cow;
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

/// A path whose object keys are **data**, not field names — plus, optionally,
/// a short list of that map's own children which are reviewed field names
/// after all, and stay visible to the sheet despite the fold.
///
/// D123: `MAP_PATHS` used to be `&[&str]`, and one declaration served two
/// jobs at once — "these keys are data, don't publish them as field names"
/// and "stop seeing anything under here at all." Only the first was ever
/// wanted; the second was a side effect of the fold having no finer grain
/// than the whole path. `named_children` is that finer grain: a key listed
/// here is exempted from the fold and walked like an ordinary field (visible
/// to the sheet, its own path recorded), while every key *not* listed still
/// folds to `.{}` exactly as before. See `.params.update.rawInput` below for
/// the motivating case.
///
/// **This does not change what a value earns.** `named_children` is read by
/// this module (the capability sheet) and, for the matching key-survival
/// decision, by `sanitize.rs` — but only to let the *key* spelling survive
/// like any other field name. A named child's *value* still has to earn
/// verbatim survival the ordinary way, one dotted path at a time on
/// `allowlist/{claude,codex,acp}.txt`; naming a child here does not add it to
/// either list, and neither `pattern` nor `path` is on `acp.txt` today.
/// Conflating the two would let a search string or a filesystem path publish
/// verbatim the moment its field name looked reviewed — exactly the leak the
/// map declaration exists to prevent in the first place.
pub struct MapPath {
    pub path: &'static str,
    /// Reviewed field names among this map's own children. Opt-in and static
    /// only — never a heuristic that guesses a key "looks like" a field
    /// name. A model id (`grok-4.6`) rides `.modelUsage` as a map key in real
    /// captures, and any cleverness meant to recognize a field-shaped key
    /// would eventually promote one of those instead.
    pub named_children: &'static [&'static str],
}

/// Declared rather than inferred. A model-keyed map is indistinguishable from a
/// struct by shape alone — every capture on this machine used one model, so its
/// key set looks perfectly stable — and a wrong guess silently renames a field.
/// An undeclared map shows up in the snapshot as a field with an obviously
/// data-shaped name, which triage catches and adds here.
///
/// **`sanitize.rs` reads this same list**, so one declaration decides both
/// questions a map key raises: this module stops recording an unnamed child
/// as a field name, and the sanitizer stops publishing an unnamed child's key
/// verbatim. They must not drift — a path declared here but not there would
/// redact a key the snapshot still expects to see, and the reverse publishes
/// an identifier the snapshot has already agreed is data. A *named* child is
/// the one exception both sides make in the same direction: its key survives
/// in both the sheet and the sanitized corpus, while its value stays governed
/// by the ordinary allowlist, unaffected by this declaration.
pub const MAP_PATHS: &[MapPath] = &[
    MapPath {
        path: ".modelUsage",
        named_children: &[],
    },
    MapPath {
        path: ".params.update.rawInput",
        named_children: &[
            // ACP, promote-the-captures slice (2026-08-28), narrowed by D123
            // (2026-08-29). A tool call's own parameters, keyed by that
            // tool's parameter names (`pattern`, `path`, `target_file`,
            // ...) — a union across every tool an agent offers, the same
            // shape of problem D73 tracks for Claude's tool-argument paths.
            // First seen in `steer-grok` (grok 1.0.5): Grok's own
            // read/search tools populate this from real filesystem paths on
            // the capturing machine.
            //
            // **The declaration is right and it costs something, and both
            // halves are worth stating — the same trade-off `acp.txt`'s
            // header already records for `_meta`/`steering.supported`, just
            // not repeated here the first time.** Declaring this a map is
            // what stops `target_file` (and every other tool's own argument
            // names) from publishing as if they were reviewed field names.
            // But `normalize::typed_call` (`normalize.rs`) genuinely reads
            // two of this map's children on a `search`-kind frame —
            // `rawInput["pattern"]` and `rawInput["path"]` — and folding
            // every child to `.{}` made the sheet unable to distinguish
            // "pattern" from "path" from any other tool's own key: a future
            // ACP agent version that drops `pattern` from its search frames
            // would have broken `typed_call`'s `Search` decode silently, the
            // capability-sheet golden test staying green because the field
            // it would have caught was no longer one the walker could see.
            //
            // Naming `pattern` and `path` here closes that gap without
            // reopening the one the map declaration exists to prevent: their
            // *values* are not added to `acp.txt` by this, and a real
            // filesystem path or search string typed into either still
            // redacts to a placeholder like any other unlisted field — see
            // this type's own doc comment for why the two are decoupled.
            // Every other key `rawInput` carries for any tool (`target_file`,
            // `content`, `glob`, `variant`, ...) still folds to `.{}`, unless
            // a future change decides one of those is worth the same
            // treatment. `normalize.rs`'s own `pattern`/`path` decode test
            // remains useful cover regardless — it is a fixture pinned
            // against a real captured frame, and the corpus has not yet
            // promoted a scenario that exercises this path at all, so this
            // declaration is presently latent: it takes effect the day such
            // a scenario is promoted, not before.
            "pattern", "path",
        ],
    },
    MapPath {
        path: ".params.toolCall.rawInput",
        named_children: &[
            // D77, second live instance (found reviewing PR #142,
            // 2026-08-29). The same tool-argument map as
            // `.params.update.rawInput` above, on ACP's
            // `session/request_permission` instead of a
            // `session/update` notification: `acp::approval::command`
            // (`crates/harness/src/acp/approval.rs`) reads
            // `tool_call["rawInput"]["command"]` and
            // `tool_call["rawInput"]["cwd"]` off exactly this map, off the
            // request's `params.toolCall` value (`acp::session`, the
            // `approval::approval_request(&params["toolCall"])` call site).
            // Nothing has published yet only because no ACP approval
            // scenario is promoted, the same latency
            // `.params.update.rawInput`'s own comment records for
            // `pattern`/`path` — this declaration takes effect the day one
            // is. Every other key this map can carry for any tool
            // (`target_file`, `content`, `glob`, `pattern`, `path`, ...)
            // still folds to `.{}`; naming `command` and `cwd` only decides
            // what their *keys* are called, not what their *values* earn —
            // see `MapPath`'s own doc comment. Their values are not on
            // `allowlist/acp.txt` by this change and must not be.
            "command", "cwd",
        ],
    },
    MapPath {
        path: ".result._meta.usage.modelUsage",
        // ACP, same slice. `session/prompt`'s usage breakdown, keyed by
        // model id — the ACP analog of Claude's `.modelUsage` above, at a
        // different path because ACP nests usage under the prompt reply's
        // own `_meta` rather than the frame root. No named children: every
        // key here is a model id, which is data, never a field name.
        named_children: &[],
    },
    MapPath {
        path: ".params.update.usage.modelUsage",
        // The same map again, on the `turn_completed` notification rather
        // than the reply — Grok sends both, with byte-identical contents.
        // Declared separately because a declaration is matched against the
        // whole path, and this one is reached through `params.update`
        // instead of `result`. Missed when the reply path was declared
        // (2026-08-28): the raw `run-grok`/`steer-grok` captures carry it on
        // every `turn_completed` frame, so leaving it undeclared was a live
        // instance of D77 — the map key `grok-4.6` would have published as
        // if it were a reviewed field name, and the capability sheet would
        // have recorded a model id as a field.
        named_children: &[],
    },
];

/// Whether `key` looks like a field name a developer chose, rather than a
/// runtime identifier a provider generated at that position — the signal
/// D77's own row asks for: "a heuristic *warning* on any object whose keys
/// don't look like identifiers."
///
/// ASCII letters, digits and underscore only, and not starting with a digit
/// — an ordinary programming identifier. Every kind of data key this row has
/// actually recorded fails it for a different, real reason: a model id
/// (`claude-haiku-4-5-20251001`, `grok-4.6`) carries a hyphen and a dot, a
/// UUID or session id carries hyphens, an email address or hostname carries
/// a dot, a bare numeric id starts with a digit. Digits elsewhere are not
/// enough on their own to fail it — `costUSD`-style and versioned field
/// spellings are ordinary — only the *shape* of a hand-picked name is being
/// asked for here, not the absence of digits.
///
/// Deliberately does not special-case a vendor namespace like `_meta`'s own
/// `x.ai/sessionConfig` — that one key fails this check same as any other
/// slash-and-dot key would. [`suspected_map`] is what keeps a single such
/// key, sitting beside ordinary siblings, from reading as a whole object of
/// data: it counts across the object's keys rather than flagging one key in
/// isolation.
pub fn is_identifier_shaped(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// One dotted path where an *undeclared* object's own keys look enough like
/// data to be a [`MAP_PATHS`] candidate — found by shape, at review time,
/// rather than by a human noticing a capability-sheet field with a
/// data-shaped name after promotion (D77's own backstop, and the one the
/// row says cannot fire before a provider has any promoted corpus at all).
///
/// Carries counts only, never a key's actual spelling: the same
/// never-reproduce-what-was-withheld rule [`NovelPath`] in `sanitize.rs`
/// follows for a redacted map key applies here too, before any redaction
/// decision has even been made — the object might turn out to hold exactly
/// the account name or machine id this whole mechanism exists to keep out of
/// the archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuspectedMap {
    pub path: String,
    pub key_count: usize,
    pub non_identifier_count: usize,
}

/// Judges one object's own keys — not `MAP_PATHS`-folded, so a genuinely
/// declared map never reaches this at all; callers are expected to check
/// [`is_map_path`] first, the same way [`Visit::walk`] already does before
/// deciding whether to fold a child.
///
/// **Strict majority, not "any."** A single vendor-namespaced field sitting
/// beside ordinary siblings (`_meta`'s `x.ai/sessionConfig` next to
/// `claudeCode`, `steering`, `goal`, `jetbrains` — four identifier-shaped
/// names against one) must not read as a suspected map; that path is already
/// reviewed field-by-field on `allowlist/acp.txt`, and re-litigating it on
/// every sanitize run is exactly the "fires on everything" failure mode that
/// makes a heuristic useless. A map genuinely keyed by data — model ids,
/// account names, machine ids — has *every* key non-identifier-shaped, so
/// requiring a strict majority still catches it with room to spare,
/// including the one-key case the row's own example describes (a one-model
/// machine's `.modelUsage` has exactly one key, and one non-identifier key
/// out of one is a unanimous, not a narrow, majority).
pub fn suspected_map<'a>(path: &str, keys: impl Iterator<Item = &'a str>) -> Option<SuspectedMap> {
    let mut key_count = 0usize;
    let mut non_identifier_count = 0usize;
    for key in keys {
        key_count += 1;
        if !is_identifier_shaped(key) {
            non_identifier_count += 1;
        }
    }
    if key_count > 0 && non_identifier_count * 2 > key_count {
        Some(SuspectedMap {
            path: path.to_owned(),
            key_count,
            non_identifier_count,
        })
    } else {
        None
    }
}

/// The declared entry for `path`, if any. The one place both this module and
/// its two other readers (`sanitize.rs`'s key-survival check,
/// `allowlist_property.rs`'s audit of the committed corpus) look up a
/// [`MapPath`], so the three cannot drift on how the lookup itself works.
fn map_path(path: &str) -> Option<&'static MapPath> {
    MAP_PATHS.iter().find(|declared| declared.path == path)
}

/// Whether `path` is a declared map at all, irrespective of any named child.
/// `sanitize.rs`'s object arm needs exactly this boolean to decide whether an
/// *un*named child's key is data (default-deny) or an ordinary field name.
pub fn is_map_path(path: &str) -> bool {
    map_path(path).is_some()
}

/// Whether `key` is a reviewed, named child of the map declared at `path` —
/// a field name that stays visible in both the sheet and the sanitized
/// corpus despite the map's default fold, per [`MapPath::named_children`]'s
/// own doc comment. `false` for a path that is not a declared map at all, so
/// a caller never needs to check [`is_map_path`] first.
pub fn is_named_map_child(path: &str, key: &str) -> bool {
    map_path(path).is_some_and(|declared| declared.named_children.contains(&key))
}

/// One object key, escaped so that it cannot impersonate the notation a
/// dotted path is built from.
///
/// Paths are built by joining keys with `.`, so a key that contains one is
/// otherwise indistinguishable from nesting: a root-level
/// `{"result.platformOs": …}` would build `.result.platformOs`, match that
/// listed path, and publish whatever the provider put there. `[`/`]` do the
/// same for the array marker and `{`/`}` for the map marker, and a literal
/// backslash has to escape itself or the escape is ambiguous in turn.
///
/// **This is the whole answer to a question the sanitizer used to refuse.**
/// `validate_key` rejected any key carrying a delimiter outright, and its
/// comment asked for a design decision about path encoding on the day a real
/// provider emitted one. Grok emits nine (`x.ai/sessionConfig`,
/// `x.ai/hooks`, …, every `_meta` key it sends) plus a model id (`grok-4.6`)
/// as a map key, which is why the enumerate-the-known-keys shape D102
/// sketched is not the answer: a model id is data and cannot be reviewed one
/// literal at a time. Escaping removes the ambiguity structurally, at which
/// point neither kind of key needs a carve-out — a field name publishes
/// because field names publish, and a map key still faces `allows_prefix`'s
/// default-deny.
///
/// **Every path builder must use this**, or a sheet and an allowlist would
/// spell the same field two ways. Four of them, not the two this comment
/// listed until the first escaped path was actually promoted (Grok's
/// `x\.ai/sessionConfig` lines, 2026-08-29): [`Visit::walk`] here,
/// `sanitize::Redactor::sanitize_value_tree`, and the two mirrors in
/// `tests/capture_corpus/allowlist_property.rs` (`collect_scalars`,
/// `collect_map_keys`) — which built theirs unescaped and so failed the whole
/// corpus gate on a path its own allowlist licensed. A mirror is a path
/// builder; being in a test does not exempt it. The `[]`/`{}` markers these
/// emit for an array element and a map entry are generated, never escaped —
/// only the characters that came out of a real key are.
pub fn escape_path_segment(key: &str) -> Cow<'_, str> {
    const DELIMITERS: [char; 6] = ['\\', '.', '[', ']', '{', '}'];
    if !key.contains(DELIMITERS) {
        return Cow::Borrowed(key);
    }
    let mut escaped = String::with_capacity(key.len() + 8);
    for character in key.chars() {
        if DELIMITERS.contains(&character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Cow::Owned(escaped)
}

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
/// - `.request.tool_name` — the same tool name a THIRD time, on the
///   `can_use_tool` control request. Declared 2026-08-30, when a lint asked
///   "which tool names has the corpus ever seen" and got a false answer: the
///   two paths above only see a tool the model announced in a message, while
///   an approval-gated call names itself here and nowhere else. The
///   `approval` scenarios' `Write`/`TaskCreate`/`TaskUpdate` were invisible
///   to the sheet until this was added. Same vocabulary, different frame —
///   a name is evidence wherever it appears.
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
/// - `.params.update.sessionUpdate` — **ACP's own frame kind, added
///   2026-08-29 with the first ACP turn evidence.** `.method` alone is not
///   enough for an ACP agent the way it is for Codex: nearly every frame an
///   agent sends is `session/update`, and what kind of update it is lives one
///   level down. Grok 1.0.5's corpus shows thirteen values
///   (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
///   `tool_call_update`, `tool_call_delta_chunk`, `available_commands_update`,
///   `pending_interaction`, `interaction_resolved`, `response_completed`,
///   `turn_completed`, `user_message_chunk`, `session_summary_generated`,
///   `session_info_update`) — five of which Comet decodes nothing from, which
///   is exactly the kind of thing a sheet exists to keep visible. It reads
///   `(none observed)` for the discovery-only codex-acp and claude-agent-acp
///   corpora, and for Claude and Codex, which is the honest answer: those
///   scenarios never opened a turn.
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
    ".request.tool_name",
    ".params.update.sessionUpdate",
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
                let declared = map_path(&path);
                for (key, child) in object {
                    let named_child = declared
                        .is_some_and(|declared| declared.named_children.contains(&key.as_str()));
                    let folds = declared.is_some() && !named_child;
                    let child_path = if folds {
                        format!("{path}.{{}}")
                    } else {
                        format!("{path}.{}", escape_path_segment(key))
                    };
                    // An unnamed map entry is data, not a field, so only its
                    // contents are recorded. A named child (`MapPath`'s own
                    // doc comment) is the opt-in exception: it is walked like
                    // any ordinary field, own path recorded and all.
                    if !folds {
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

#[cfg(test)]
mod tests {
    use super::{escape_path_segment, is_identifier_shaped, suspected_map};
    use std::borrow::Cow;

    /// Ordinary field spellings — camelCase, snake_case, PascalCase, a
    /// digit-bearing but hand-picked name — all read as identifier-shaped.
    #[test]
    fn ordinary_field_names_are_identifier_shaped() {
        for key in ["costUSD", "stop_reason", "ClaudeCode", "gpt4", "_meta"] {
            assert!(
                is_identifier_shaped(key),
                "{key} should be identifier-shaped"
            );
        }
    }

    /// Every data-key shape this row has actually recorded fails for its own
    /// reason: a model id's hyphen and dot, a UUID's hyphens, a vendor
    /// namespace's slash and dot, a bare numeric id's leading digit.
    #[test]
    fn data_shaped_keys_are_not_identifier_shaped() {
        for key in [
            "claude-haiku-4-5-20251001",
            "grok-4.6",
            "550e8400-e29b-41d4-a716-446655440000",
            "x.ai/sessionConfig",
            "12345",
        ] {
            assert!(
                !is_identifier_shaped(key),
                "{key} should not be identifier-shaped"
            );
        }
    }

    /// The majority rule directly: two data-shaped model-id keys, no
    /// ordinary field name in sight, is a unanimous majority.
    #[test]
    fn suspected_map_fires_when_every_key_is_data_shaped() {
        let found = suspected_map(
            ".modelUsage",
            ["claude-haiku-4-5-20251001", "claude-sonnet-5"].into_iter(),
        );
        assert_eq!(
            found,
            Some(super::SuspectedMap {
                path: ".modelUsage".to_owned(),
                key_count: 2,
                non_identifier_count: 2,
            })
        );
    }

    /// The false-positive guard: one vendor-namespaced field beside four
    /// ordinary ones must not read as a suspected map — `_meta`'s own real
    /// shape, already reviewed field-by-field on `allowlist/acp.txt`.
    #[test]
    fn suspected_map_stays_silent_on_one_odd_key_among_ordinary_siblings() {
        let found = suspected_map(
            ".result._meta",
            [
                "x.ai/sessionConfig",
                "claudeCode",
                "steering",
                "goal",
                "jetbrains",
            ]
            .into_iter(),
        );
        assert_eq!(
            found, None,
            "one namespaced field among four plain ones must not warn"
        );
    }

    /// An empty object has no keys to be data-shaped, so it must not warn —
    /// the degenerate case a `key_count > 0` guard exists for.
    #[test]
    fn suspected_map_stays_silent_on_an_empty_object() {
        assert_eq!(suspected_map(".empty", std::iter::empty()), None);
    }

    /// Every character the path notation reserves, escaped — including the
    /// backslash that does the escaping, which is otherwise ambiguous with a
    /// key that genuinely contains one.
    #[test]
    fn every_reserved_character_in_a_key_is_escaped() {
        assert_eq!(
            escape_path_segment("x.ai/sessionConfig"),
            r"x\.ai/sessionConfig"
        );
        assert_eq!(escape_path_segment("grok-4.6"), r"grok-4\.6");
        assert_eq!(escape_path_segment("a[0]"), r"a\[0\]");
        assert_eq!(escape_path_segment("{}"), r"\{\}");
        assert_eq!(escape_path_segment(r"back\slash"), r"back\\slash");
    }

    /// An ordinary key comes back byte-identical and borrowed. Every path in
    /// the promoted corpus, every committed capability sheet and every
    /// allowlist line predating the escape depends on this being a no-op for
    /// the keys real providers actually send.
    #[test]
    fn an_ordinary_key_is_unchanged_and_not_reallocated() {
        assert!(matches!(
            escape_path_segment("modelUsage"),
            Cow::Borrowed("modelUsage")
        ));
    }

    /// The property the whole escape exists for: the flat key
    /// `result.platformOs` and the nested path `.result` -> `.platformOs`
    /// must not build the same string, because `codex.txt` lists the second
    /// one and the first is a provider-controlled impersonation of it.
    #[test]
    fn a_flat_dotted_key_and_the_nested_path_it_imitates_differ() {
        let flat = format!(".{}", escape_path_segment("result.platformOs"));
        let nested = format!(
            ".{}.{}",
            escape_path_segment("result"),
            escape_path_segment("platformOs")
        );
        assert_ne!(flat, nested);
        assert_eq!(nested, ".result.platformOs", "the honest path is untouched");
    }
}
