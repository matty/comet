//! Codex's live model discovery: a short-lived `codex app-server` paging
//! `model/list`.
//!
//! Captured against codex-cli 0.147.0 on 2026-08-11
//! (`captures/2026-08-11-codex-model-list.md`). Four facts from that capture
//! shape this file:
//!
//! 1. `model/list` answers cold, before any thread exists — but only after the
//!    `initialize` handshake, which is why this spawn repeats `run`'s.
//! 2. The ids the server reports are byte-identical to the curated ones, so
//!    there is no canonicalization layer here. 2.2's `resolvedModel` machinery
//!    is a Claude fact and must not be imported.
//! 3. `supportedReasoningEfforts` is an array of OBJECTS, not strings.
//! 4. Every live model carries `inputModalities`, so the schema's documented
//!    absent-case default has no live producer and is fixture-tested by hand.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use comet_proto::ReasoningLevel;

use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure, program_path};

/// Matches `claude/discovery.rs`'s `DISCOVERY_TIMEOUT` and `PROBE_TIMEOUT`.
/// Model discovery may include process startup and a remote catalog fetch, so
/// the wait allows both while remaining bounded. A wedged CLI still degrades
/// to the built-in list rather than hanging the picker.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A ceiling on the paging loop. The live server answers in one page and only
/// pages at all when the client asks it to, so anything approaching this is a
/// server behaving in a way nothing in the schema describes.
const MAX_PAGES: usize = 20;

/// Comet's own handshake, verbatim from `codex/mod.rs`'s `run`. `experimentalApi`
/// is not required for `model/list` on 0.147.0, but a discovery session that
/// declares itself differently from a real one is a difference nobody would
/// think to look for later.
fn initialize_line() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "comet-native",
                "title": "Comet",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": { "experimentalApi": true },
        },
    })
    .to_string()
}

const INITIALIZED_LINE: &str = r#"{"jsonrpc":"2.0","method":"initialized"}"#;

/// `includeHidden` is deliberately never sent: the default excludes hidden
/// models, and the two the server hides are a Work Mode routing alias and
/// `codex-auto-review` — the model slice 1.7 delegates an auto-review to, not
/// something to offer in a picker.
///
/// Serialized rather than interpolated, because **the cursor is the server's
/// string, not ours**. 0.147.0 sends a stringified offset, but the schema calls
/// it opaque, and one containing a quote or a backslash pasted into a request
/// literal would send malformed JSON on the second page — degrading to the
/// curated list with nothing saying why.
fn model_list_line(id: u32, cursor: Option<&str>) -> String {
    let mut params = serde_json::Map::new();
    if let Some(cursor) = cursor {
        params.insert("cursor".into(), serde_json::Value::String(cursor.into()));
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "model/list",
        "params": params,
    })
    .to_string()
}

/// Where this device's Codex credentials live: `$CODEX_HOME`, else `~/.codex`.
///
/// The engine resolves the same pair for the accounts surface
/// (`crates/engine/src/agent_accounts.rs:103,112`) and cannot be reused here —
/// `engine` depends on `harness`, not the other way round. Change one and read
/// the other.
pub(crate) fn codex_home() -> Option<PathBuf> {
    crate::env_dir("CODEX_HOME").or_else(|| crate::home_dir().map(|home| home.join(".codex")))
}

/// Whether there is any point asking this CLI at all.
///
/// A logged-out `codex` does not fail `model/list` — it answers, in 14ms,
/// with a five-model list baked into the binary that contains a model the
/// account cannot use and misses three it has (capture
/// `2026-08-11-codex-model-list.md`, run 6). The envelope is identical to a
/// real answer, so "the call succeeded" cannot mean the list is the account's.
///
/// API-key auth lives INSIDE `auth.json` (`agent_accounts.rs:1409` reads
/// `OPENAI_API_KEY` as a field of it), so this covers both login kinds. Someone
/// authenticated purely by an `OPENAI_API_KEY` environment variable with no
/// `auth.json` is the accepted pessimistic case: they get the curated list and
/// the caption that says so, never a list belonging to nobody.
fn logged_in(home: Option<&Path>) -> bool {
    home.is_some_and(|home| home.join("auth.json").exists())
}

/// `home`, resolved against the parent's working directory.
///
/// The same trap [`program_path`] exists for, from the other side: the child is
/// spawned with `current_dir(temp_dir())`, so a relative `CODEX_HOME` would
/// resolve there while the parent's `auth.json` check resolved it here. The
/// check would pass against one directory and the CLI answer from another —
/// its logged-out fallback list, labelled live. `env_dir` returns the variable
/// verbatim (`lib.rs:129-131`), so a relative value is real configuration.
///
/// Joined rather than canonicalized, deliberately: on Windows canonicalization
/// yields a `\\?\` verbatim path, and this value is handed to another program
/// as an environment variable. Both sides resolving the same directory is what
/// is needed here, not a normalized spelling.
fn absolute_home(home: PathBuf) -> Option<PathBuf> {
    if home.is_absolute() {
        return Some(home);
    }
    match std::env::current_dir() {
        Ok(cwd) => Some(cwd.join(home)),
        Err(err) => {
            tracing::debug!(%err, "codex discovery cannot resolve a relative CODEX_HOME");
            None
        }
    }
}

/// Spawn a short-lived app-server, page `model/list`, and hand back what it
/// said.
///
/// Owned arguments because the future is handed to `DiscoveryCache` and
/// outlives the caller's frame.
pub(crate) async fn discover(
    exe: PathBuf,
    home: Option<PathBuf>,
) -> Result<Discovery, DiscoveryFailure> {
    let Some(home) = home
        .and_then(absolute_home)
        .filter(|home| logged_in(Some(home)))
    else {
        tracing::debug!(
            "codex discovery skipped: no auth.json, so model/list would answer with the CLI's own fallback list"
        );
        return Err(DiscoveryFailure::Unreachable);
    };
    match tokio::time::timeout(DISCOVERY_TIMEOUT, handshake(&exe, &home)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!(cli = %exe.display(), "codex discovery timed out");
            Err(DiscoveryFailure::Unreachable)
        }
    }
}

/// Describe the exact short-lived launch used for Codex model discovery.
pub(crate) fn discovery_launch(
    exe: &Path,
    codex_home: &Path,
    cwd: &Path,
) -> crate::capture::LaunchDescriptor {
    let exe = program_path(exe);
    let mut configured_env = std::collections::BTreeMap::new();
    if let Some(path) = crate::child_path(&exe) {
        configured_env.insert("PATH".into(), path);
    }
    // The child is told the same home the login check just read `auth.json`
    // from. Left to the ambient environment, the two can be different homes.
    configured_env.insert("CODEX_HOME".into(), codex_home.into());
    crate::capture::LaunchDescriptor {
        program: exe,
        args: vec!["app-server".into()],
        cwd: Some(cwd.into()),
        configured_env,
        stdin: crate::capture::StdioMode::Piped,
        stdout: crate::capture::StdioMode::Piped,
        stderr: crate::capture::StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0x0800_0000,
    }
}

/// Build the exact short-lived command used for Codex model discovery.
pub(crate) fn build_codex_discovery_command(exe: &Path, codex_home: &Path, cwd: &Path) -> Command {
    discovery_launch(exe, codex_home, cwd).command()
}

async fn handshake(exe: &Path, home: &Path) -> Result<Discovery, DiscoveryFailure> {
    // No project settings are wanted for a session with no turn, and the
    // capture found the model list identical across working directories.
    let mut cmd = build_codex_discovery_command(exe, home, &std::env::temp_dir());

    let mut child = cmd.spawn().map_err(|err| {
        tracing::debug!(cli = %exe.display(), %err, "codex discovery spawn failed");
        DiscoveryFailure::Unreachable
    })?;
    let mut stdin = child.stdin.take().ok_or(DiscoveryFailure::Unreachable)?;
    let stdout = child.stdout.take().ok_or(DiscoveryFailure::Unreachable)?;
    let stderr = child.stderr.take().ok_or(DiscoveryFailure::Unreachable)?;
    crate::drain_discovery_stderr(stderr, "codex");
    let mut lines = BufReader::new(stdout).lines();

    send(&mut stdin, &initialize_line()).await?;
    reply_to(&mut lines, 1).await?;
    send(&mut stdin, INITIALIZED_LINE).await?;

    let mut models: Vec<DiscoveredModel> = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..MAX_PAGES {
        let id = 2 + page as u32;
        send(&mut stdin, &model_list_line(id, cursor.as_deref())).await?;
        let line = reply_to(&mut lines, id).await?;
        let (mut page_models, next) = page_from_reply(&line)?;
        models.append(&mut page_models);
        match next {
            // An explicit null and an absent key both mean "no more items".
            None => {
                // Closing stdin ends the session; `kill_on_drop` covers a CLI
                // that ignores it.
                drop(stdin);
                return Ok(Discovery { models });
            }
            // The cursor is opaque and server-chosen, so nothing but this stops
            // a server that keeps handing back the same one from spinning the
            // loop forever against a picker that is awaiting the answer.
            Some(next) if Some(&next) == cursor.as_ref() => {
                tracing::debug!(cursor = %next, "codex model/list cursor did not advance");
                return Err(DiscoveryFailure::Unparseable);
            }
            Some(next) => cursor = Some(next),
        }
    }
    tracing::debug!(pages = MAX_PAGES, "codex model/list did not terminate");
    Err(DiscoveryFailure::Unparseable)
}

async fn send(stdin: &mut tokio::process::ChildStdin, line: &str) -> Result<(), DiscoveryFailure> {
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|_| DiscoveryFailure::Unreachable)?;
    stdin
        .flush()
        .await
        .map_err(|_| DiscoveryFailure::Unreachable)
}

/// Read until the reply to `id` arrives.
///
/// Matching on the id rather than taking the next line is load-bearing: every
/// reviewed discovery emitted `remoteControl/status/changed` after initialize
/// succeeded but before the later `model/list` reply.
async fn reply_to<R>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
    id: u32,
) -> Result<String, DiscoveryFailure>
where
    R: tokio::io::AsyncRead + Unpin,
{
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(u64::from(id)) {
            return Ok(line);
        }
    }
    // The child died or closed stdout without answering.
    Err(DiscoveryFailure::Unreachable)
}

#[derive(Deserialize)]
struct ModelListReply {
    #[serde(default)]
    result: Option<ModelListResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListResult {
    /// No `#[serde(default)]`, deliberately: `data` is required
    /// (`schema.gen.ts:38604`), so a result without it is the provider having
    /// stopped answering the question — drift. Defaulted to an empty list it
    /// would instead serve the curated catalog as `CatalogSource::Live` with
    /// the fallback caption suppressed, which is the exact defect slice 2.2's
    /// review found on the Claude side (`94fa0d3`).
    data: Vec<ListedModel>,
    /// Absent and explicitly null both mean "no more items to return"
    /// (`schema.gen.ts:38614`).
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Only the fields this slice consumes. The reply carries eleven more —
/// `upgrade`/`upgradeInfo` (real deprecation copy), `defaultReasoningEffort`,
/// `serviceTiers`, `supportsPersonality`, `availabilityNux`, `modelSpecialty`
/// and the rest — all deliberately unmodelled; see the debt rows.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// `hidden` is required in the schema, but defaulting it to `false` here
    /// keeps an older server that omits it visible rather than invisible.
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    /// Absent means `["text","image"]`, not "unknown" — the schema documents
    /// the default (`schema.gen.ts:22253`). See
    /// `.agents/rules/optional-wire-fields.md`.
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

/// An effort is an object, not a string: `{reasoningEffort, description}`.
/// A decoder written from the phase spec's field summary fails on every model.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningEffortOption {
    reasoning_effort: String,
}

/// `minimal` is deliberately not mapped. Codex has never reported it, and
/// `catalog::to_effort` clamps `Minimal` to `"low"` on the wire — so offering
/// it on a model nobody has curated would promise an effort the run then
/// silently changes.
fn to_level(raw: &str) -> Option<ReasoningLevel> {
    Some(match raw {
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        // Codex-native on gpt-5.6+, and already sent on the wire today by
        // `to_effort`. Not a Comet-layered level, unlike ultracode/ultrathink.
        "ultra" => ReasoningLevel::Ultra,
        _ => return None,
    })
}

/// The single place a `model/list` page is read. Its test pins the literal
/// bytes the server sent, not a round trip through the structs above.
pub(crate) fn page_from_reply(
    line: &str,
) -> Result<(Vec<DiscoveredModel>, Option<String>), DiscoveryFailure> {
    let reply: ModelListReply =
        serde_json::from_str(line).map_err(|_| DiscoveryFailure::Unparseable)?;
    // An error reply decodes as a frame with no `result`: the server answered
    // and refused. `-32600 Not initialized` is the shape that reaches here, and
    // it means our handshake stopped being accepted — a protocol change, not a
    // machine without a CLI.
    let result = reply.result.ok_or(DiscoveryFailure::Unparseable)?;

    let models = result
        .data
        .into_iter()
        // A hidden model is not pickable. The default already excludes them;
        // this is what keeps that true if the default ever changes.
        .filter(|model| !model.hidden)
        .map(|model| DiscoveredModel {
            label: model.display_name.unwrap_or_else(|| model.id.clone()),
            id: model.id,
            description: model.description,
            reasoning_levels: model
                .supported_reasoning_efforts
                .iter()
                .filter_map(|effort| to_level(&effort.reasoning_effort))
                .collect(),
            accepts_images: Some(match model.input_modalities {
                Some(modalities) => modalities.iter().any(|m| m == "image"),
                // The documented default, written by hand because no live model
                // omits the field and this branch would otherwise ship never
                // having been constructed.
                None => true,
            }),
        })
        .collect();
    Ok((models, result.next_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal reply codex-cli 0.147.0 sent on 2026-08-11, from
    /// `captures/2026-08-11-codex-model-list/run1-cold.jsonl`, with the five
    /// middle models elided — the first and last are the two that matter, and
    /// nothing else in the envelope differs.
    ///
    /// Pinned as the server's own bytes rather than round-tripped through our
    /// own types on purpose: a round-trip test cannot catch the reply moving
    /// under us (AGENTS.md, "Changing what an RPC method answers with").
    const CAPTURED_PAGE: &str = r#"{"id":2,"result":{"data":[{"id":"gpt-5.6-sol","model":"gpt-5.6-sol","upgrade":null,"upgradeInfo":null,"availabilityNux":null,"displayName":"GPT-5.6-Sol","description":"Latest frontier agentic coding model.","modelSpecialty":null,"hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast responses with lighter reasoning"},{"reasoningEffort":"medium","description":"Balances speed and reasoning depth for everyday tasks"},{"reasoningEffort":"high","description":"Greater reasoning depth for complex problems"},{"reasoningEffort":"xhigh","description":"Extra high reasoning depth for complex problems"},{"reasoningEffort":"max","description":"Maximum reasoning depth for the hardest problems"},{"reasoningEffort":"ultra","description":"Maximum reasoning with automatic task delegation"}],"defaultReasoningEffort":"low","inputModalities":["text","image"],"supportsPersonality":false,"additionalSpeedTiers":["fast"],"serviceTiers":[{"id":"priority","name":"Fast","description":"1.5x speed, increased usage"}],"defaultServiceTier":null,"isDefault":true},{"id":"gpt-5.3-codex-spark","model":"gpt-5.3-codex-spark","upgrade":null,"upgradeInfo":null,"availabilityNux":null,"displayName":"GPT-5.3-Codex-Spark","description":"Ultra-fast coding model.","modelSpecialty":null,"hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast responses with lighter reasoning"},{"reasoningEffort":"medium","description":"Balances speed and reasoning depth for everyday tasks"},{"reasoningEffort":"high","description":"Greater reasoning depth for complex problems"},{"reasoningEffort":"xhigh","description":"Extra high reasoning depth for complex problems"}],"defaultReasoningEffort":"high","inputModalities":["text"],"supportsPersonality":true,"additionalSpeedTiers":[],"serviceTiers":[],"defaultServiceTier":null,"isDefault":false}],"nextCursor":null}}"#;

    #[test]
    fn the_captured_reply_decodes_onto_curated_ids() {
        let (models, next) = page_from_reply(CAPTURED_PAGE).expect("captured reply decodes");
        assert_eq!(next, None, "an explicit null cursor ends the paging");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.6-sol", "gpt-5.3-codex-spark"]);
        assert_eq!(models[0].label, "GPT-5.6-Sol");
    }

    /// The effort array is objects, and `ultra` is a real provider-reported
    /// level rather than one Comet layers on.
    #[test]
    fn efforts_decode_from_objects_including_ultra() {
        let (models, _) = page_from_reply(CAPTURED_PAGE).expect("decodes");
        assert_eq!(
            models[0].reasoning_levels,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
                ReasoningLevel::Ultra,
            ]
        );
    }

    /// The field 2.4's modality gate reads, and the one place the live answer
    /// contradicts the curated catalog today.
    #[test]
    fn input_modalities_split_text_only_from_image_capable() {
        let (models, _) = page_from_reply(CAPTURED_PAGE).expect("decodes");
        assert_eq!(models[0].accepts_images, Some(true));
        assert_eq!(
            models[1].accepts_images,
            Some(false),
            "gpt-5.3-codex-spark reports text only"
        );
    }

    /// No live model omits `inputModalities`, so this branch exists only if it
    /// is written by hand. Absent is the documented `["text","image"]`, NOT
    /// "unknown, disable the button".
    #[test]
    fn an_absent_modality_list_means_images_are_supported() {
        let line = r#"{"id":2,"result":{"data":[{"id":"gpt-new","displayName":"New","hidden":false,"supportedReasoningEfforts":[]}],"nextCursor":null}}"#;
        let (models, _) = page_from_reply(line).expect("decodes");
        assert_eq!(models[0].accepts_images, Some(true));
    }

    #[test]
    fn hidden_models_are_dropped() {
        let line = r#"{"id":2,"result":{"data":[{"id":"codex-auto-review","displayName":"Auto Review","hidden":true,"supportedReasoningEfforts":[]},{"id":"gpt-5.5","displayName":"GPT-5.5","hidden":false,"supportedReasoningEfforts":[]}],"nextCursor":null}}"#;
        let (models, _) = page_from_reply(line).expect("decodes");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.5"]);
    }

    /// An absent `data` key is the provider having stopped answering the
    /// question. Read as an empty list it would serve the curated catalog under
    /// a caption claiming it came from Codex.
    #[test]
    fn a_missing_data_key_is_drift_not_an_empty_catalog() {
        let line = r#"{"id":2,"result":{"nextCursor":null}}"#;
        assert_eq!(page_from_reply(line), Err(DiscoveryFailure::Unparseable));
    }

    /// An explicit empty list is the server answering, and must not be drift.
    #[test]
    fn an_explicitly_empty_list_is_an_answer() {
        let line = r#"{"id":2,"result":{"data":[],"nextCursor":null}}"#;
        let (models, next) = page_from_reply(line).expect("decodes");
        assert!(models.is_empty());
        assert_eq!(next, None);
    }

    /// A JSON-RPC error reply carries no `result`. `-32600 Not initialized` is
    /// the shape that reaches here, and it means the handshake this file sends
    /// stopped being accepted.
    #[test]
    fn an_error_reply_is_drift() {
        let line = r#"{"id":2,"error":{"code":-32600,"message":"Not initialized"}}"#;
        assert_eq!(page_from_reply(line), Err(DiscoveryFailure::Unparseable));
    }

    #[test]
    fn a_cursor_is_carried_when_the_server_sends_one() {
        let line = r#"{"id":2,"result":{"data":[],"nextCursor":"2"}}"#;
        let (_, next) = page_from_reply(line).expect("decodes");
        assert_eq!(next.as_deref(), Some("2"));
    }

    /// `includeHidden` is never sent, and a cursor is sent only once there is
    /// one to send.
    #[test]
    fn the_request_line_asks_for_exactly_what_the_capture_asked_for() {
        assert!(!model_list_line(2, None).contains("includeHidden"));
        assert!(!model_list_line(2, None).contains("cursor"));
        assert!(model_list_line(3, Some("2")).contains(r#""cursor":"2""#));
    }
}
