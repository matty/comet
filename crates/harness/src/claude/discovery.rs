//! Claude's live model discovery: reading one `initialize` control response.
//!
//! Reviewed evidence lives in `tests/corpus`, addressed by scenario and frame
//! sequence. Three facts from that corpus shape this file, and none of them
//! is in sdk.d.ts 0.3.195:
//!
//! 1. The reply nests twice: `control_response.response.response`.
//! 2. Each model carries an undocumented `resolvedModel`, and it is the only
//!    thing relating Claude's `sonnet` to the curated `claude-sonnet-5`. The
//!    two id families have never matched literally.
//! 3. There is no modality field, so `accepts_images` stays curated here.
//!
//! Canonicalization lives in this file rather than in the shared merge because
//! `resolvedModel` is a Claude field; `crate::discovery` stays
//! provider-agnostic and unchanged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use comet_proto::ReasoningLevel;

use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure, program_path};

/// Matches `PROBE_TIMEOUT`. The wait is paid once per boot behind a cancellable
/// loading surface. Keeping it bounded makes a wedged CLI fall back to the
/// built-in list instead of leaving the picker waiting indefinitely.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The spawn arguments for a session that will never run a turn: the
/// stream-json transport, and `--bare`. Deliberately NOT `build_command`'s
/// list — `--permission-prompt-tool`, `--permission-mode`, `--model` and
/// `--include-partial-messages` all describe a turn, and there is none.
///
/// **`--bare` is load-bearing, not tidiness.** It skips hooks, LSP, plugin
/// sync, auto-memory and CLAUDE.md discovery. Without it the user's
/// `SessionStart` hooks run in a session that will never prompt. Avoiding that
/// unrelated work keeps the bounded discovery path focused on its model-list
/// request and preserves the built-in fallback when the CLI is unhealthy.
///
/// Two consequences worth knowing. `account` degrades to
/// `{"tokenSource":"none"}` because OAuth and keychain are never read — fine
/// here, since nothing reads `account`, and it keeps the user's email off the
/// wire entirely. And `commands` shrinks to the built-ins,
/// because user and project skills are not discovered: **slice 2.4 cannot read
/// the command list from this spawn.**
///
/// An older CLI that does not know `--bare` fails the spawn, which degrades to
/// the built-in list and its caption rather than to a wrong answer.
const DISCOVERY_ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--bare",
];

/// Every field but `subtype` is optional (sdk.d.ts:3227-3264), and an empty
/// request is what the capture drove.
const INITIALIZE_LINE: &str = r#"{"type":"control_request","request_id":"comet-discovery-1","request":{"subtype":"initialize"}}"#;

/// Spawn a short-lived CLI, ask once, and take the first control response.
///
/// Owned arguments because the future is handed to `DiscoveryCache` and
/// outlives the caller's frame.
pub(crate) async fn discover(
    exe: PathBuf,
    curated_ids: Vec<String>,
) -> Result<Discovery, DiscoveryFailure> {
    match tokio::time::timeout(DISCOVERY_TIMEOUT, handshake(&exe, &curated_ids)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!(cli = %exe.display(), "claude discovery timed out");
            Err(DiscoveryFailure::Unreachable)
        }
    }
}

async fn handshake(exe: &Path, curated_ids: &[String]) -> Result<Discovery, DiscoveryFailure> {
    // Any directory would do for models — the capture found the model list
    // identical across cwds while `commands` varied — and a neutral one avoids
    // loading a project's own settings for a session with no turn.
    let launch = model_discovery_launch(exe, &std::env::temp_dir());
    let line = initialize_reply(launch).await?;
    let borrowed: Vec<&str> = curated_ids.iter().map(String::as_str).collect();
    discovery_from_reply(&line, &borrowed)
}

/// Describe a Claude initialize handshake without choosing its purpose.
pub(crate) fn claude_initialize_launch(
    exe: &Path,
    args: &[&str],
    cwd: &Path,
) -> crate::launch::LaunchDescriptor {
    let exe = program_path(exe);
    let mut configured_env = std::collections::BTreeMap::new();
    if let Some(path) = crate::child_path(&exe) {
        configured_env.insert("PATH".into(), path);
    }
    crate::launch::LaunchDescriptor {
        program: exe,
        args: args.iter().map(Into::into).collect(),
        cwd: Some(cwd.into()),
        configured_env,
        stdin: crate::launch::StdioMode::Piped,
        stdout: crate::launch::StdioMode::Piped,
        stderr: crate::launch::StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0x0800_0000,
    }
}

/// Select the exact launch used for Claude model discovery.
pub fn model_discovery_launch(exe: &Path, cwd: &Path) -> crate::launch::LaunchDescriptor {
    claude_initialize_launch(exe, DISCOVERY_ARGS, cwd)
}

/// Build the exact short-lived command used for Claude initialize handshakes.
#[allow(dead_code)] // Preserved for capture drivers that materialize a chosen launch.
pub(crate) fn build_claude_initialize_command(exe: &Path, args: &[&str], cwd: &Path) -> Command {
    claude_initialize_launch(exe, args, cwd).command()
}

/// Spawn a selected short-lived Claude initialize launch, send one initialize,
/// and hand back the raw `control_response` line.
pub(super) async fn initialize_reply(
    launch: crate::launch::LaunchDescriptor,
) -> Result<String, DiscoveryFailure> {
    let exe = launch.program.clone();
    let mut cmd = launch.command();

    let mut child = cmd.spawn().map_err(|err| {
        tracing::debug!(cli = %exe.display(), %err, "claude discovery spawn failed");
        DiscoveryFailure::Unreachable
    })?;
    let mut stdin = child.stdin.take().ok_or(DiscoveryFailure::Unreachable)?;
    let stdout = child.stdout.take().ok_or(DiscoveryFailure::Unreachable)?;
    let stderr = child.stderr.take().ok_or(DiscoveryFailure::Unreachable)?;
    crate::drain_discovery_stderr(stderr, "claude");

    stdin
        .write_all(format!("{INITIALIZE_LINE}\n").as_bytes())
        .await
        .map_err(|_| DiscoveryFailure::Unreachable)?;
    stdin
        .flush()
        .await
        .map_err(|_| DiscoveryFailure::Unreachable)?;

    // The session emits hook and system frames before the answer — the user's
    // SessionStart hooks run even in a session with no turn — so read until the
    // control response rather than taking the first line.
    let mut lines = BufReader::new(stdout).lines();
    let mut answer = Err(DiscoveryFailure::Unreachable);
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("\"control_response\"") {
            answer = Ok(line);
            break;
        }
    }
    // Closing stdin is what ends the session; the capture saw exit 0 within
    // 600ms of the close. `kill_on_drop` covers a CLI that does not.
    drop(stdin);
    answer
}

#[derive(Deserialize)]
struct ControlResponseFrame {
    response: ControlResponseBody,
}

/// `subtype` is read as a plain string rather than an enum: an unknown subtype
/// has to reach the decode logic and be judged there, not fail the whole frame
/// at serde level where the two failure kinds are indistinguishable.
#[derive(Deserialize)]
struct ControlResponseBody {
    subtype: String,
    #[serde(default)]
    response: Option<InitializeReply>,
}

/// Only the field this slice consumes. `agents`, `account`, `commands`,
/// `output_style` and the seven undocumented keys are deliberately not
/// modelled — see debt rows D31 and D32.
#[derive(Deserialize)]
struct InitializeReply {
    /// No `#[serde(default)]`, deliberately: `models` is required in
    /// sdk.d.ts (:3274), so a success reply without it is the provider having
    /// stopped answering the question — drift. Defaulted to an empty list it
    /// would instead serve the curated catalog as `CatalogSource::Live`, with
    /// the fallback caption suppressed and no `Diagnostic` raised. An
    /// explicit `[]` still decodes, because that is the CLI answering.
    models: Vec<InitializeModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeModel {
    value: String,
    #[serde(default)]
    resolved_model: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    supported_effort_levels: Vec<String>,
}

fn to_level(raw: &str) -> Option<ReasoningLevel> {
    Some(match raw {
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        _ => return None,
    })
}

/// An id with the two decorations stripped that make one model look like two:
/// a bracketed variant suffix (`claude-opus-5[1m]`, which comet models as the
/// `contextWindow` option rather than as an id) and a trailing release date
/// (`claude-haiku-4-5-20251001`).
fn undecorated(id: &str) -> &str {
    let base = match id.find('[') {
        Some(at) => &id[..at],
        None => id,
    };
    match base.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => base,
    }
}

/// The curated id this live entry is the same model as, if any.
///
/// Only ever answers with an id the curated list already carries: `value` is
/// the id the provider says to select a model by, and nothing has verified that
/// a *resolved* id is accepted as `--model`. An uncurated model therefore keeps
/// its own `value`.
fn canonical_id<'a>(model: &InitializeModel, curated_ids: &[&'a str]) -> Option<&'a str> {
    let resolved = model.resolved_model.as_deref().unwrap_or(&model.value);
    let base = undecorated(resolved);
    curated_ids
        .iter()
        .copied()
        .find(|id| undecorated(id) == base)
}

/// The single place this reply is read. Its test pins the literal bytes the CLI
/// sent, not a round trip through the structs above.
pub(crate) fn discovery_from_reply(
    line: &str,
    curated_ids: &[&str],
) -> Result<Discovery, DiscoveryFailure> {
    let frame: ControlResponseFrame =
        serde_json::from_str(line).map_err(|_| DiscoveryFailure::Unparseable)?;
    if frame.response.subtype != "success" {
        // The CLI answered and said no. Ordinary; not a protocol change.
        return Err(DiscoveryFailure::Unreachable);
    }
    let reply = frame
        .response
        .response
        .ok_or(DiscoveryFailure::Unparseable)?;

    let mut models: Vec<DiscoveredModel> = Vec::new();
    for model in reply.models {
        // `default` points at whatever the user's default model is; it is a
        // setting wearing a model's shape, and its label says so.
        if model.value == "default" {
            continue;
        }
        let id = canonical_id(&model, curated_ids)
            .map(str::to_string)
            .unwrap_or_else(|| model.value.clone());
        // Two aliases of one model (`opus` and `opus[1m]`) land on one id.
        // First listing wins, which is the provider's own ordering.
        if models.iter().any(|m| m.id == id) {
            continue;
        }
        models.push(DiscoveredModel {
            id,
            label: model.display_name.unwrap_or_else(|| model.value.clone()),
            description: model.description,
            deprecation: None,
            reasoning_levels: model
                .supported_effort_levels
                .iter()
                .filter_map(|l| to_level(l))
                .collect(),
            default_reasoning: None,
            service_tier_available: None,
            // Claude publishes no modality field. `None` is "did not say"; see
            // `.agents/rules/optional-wire-fields.md`.
            accepts_images: None,
        });
    }
    Ok(Discovery { models })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::catalog::static_models;
    use comet_capture::corpus_frame;
    use comet_proto::ReasoningLevel;

    const MODEL_DISCOVERY: &str = "claude/2.1.228/model-discovery";

    fn curated_ids() -> Vec<String> {
        static_models().into_iter().map(|m| m.id).collect()
    }

    fn borrowed(ids: &[String]) -> Vec<&str> {
        ids.iter().map(String::as_str).collect()
    }

    /// The reply nests twice — `control_response.response.response` — and
    /// carries seven fields absent from sdk.d.ts 0.3.195. A decoder written
    /// from the typings lands two levels too shallow; one written strictly
    /// rejects the live CLI.
    ///
    /// The ids are the CURATED ones, because Claude's own spellings never match
    /// them literally and every consumer downstream — the
    /// merge, the picker, `--model` — speaks curated.
    ///
    /// Provider model selectors and resolved identifiers decode onto curated
    /// model identifiers.
    #[test]
    fn the_captured_reply_decodes_onto_curated_ids() {
        let owned = curated_ids();
        let payload = corpus_frame(MODEL_DISCOVERY, 2).payload;
        let discovery = discovery_from_reply(&payload, &borrowed(&owned)).expect("decodes");
        let ids: Vec<&str> = discovery.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-opus-5",
                "claude-fable-5",
                "claude-sonnet-5",
                "claude-haiku-4-5",
                "gpt-5.6-sol"
            ],
            "aliases canonicalized, `default` dropped, live-only selector retained"
        );
        assert_eq!(
            discovery.models[2].label, "Sonnet",
            "the live label is kept"
        );
    }

    /// The slice's whole point, on the real data and the real catalog. The
    /// shared merge is unchanged from 2.1 — if the adapter did not
    /// canonicalize, this is where eleven rows would show up.
    ///
    /// Captured aliases do not duplicate curated rows while a live-only
    /// selector remains available.
    #[test]
    fn the_captured_reply_deduplicates_aliases_and_keeps_a_live_only_row() {
        let owned = curated_ids();
        let payload = corpus_frame(MODEL_DISCOVERY, 2).payload;
        let discovery = discovery_from_reply(&payload, &borrowed(&owned)).unwrap();
        let merged = crate::discovery::merge(static_models(), &discovery);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-fable-5",
                "claude-opus-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-sonnet-5",
                "claude-haiku-4-5",
                "gpt-5.6-sol",
            ],
            "six curated rows, no alias duplicates, and the live-only row remains"
        );
        assert_eq!(merged[4].label, "Sonnet 5", "curated label still wins");
    }

    /// `default` is a pointer at whatever the user's default happens to be,
    /// labelled "Default (recommended)". Kept, it would either duplicate the
    /// model it resolves to or, once that model is not curated, put a row named
    /// after a setting in the picker.
    ///
    /// The captured default selector resolves to an existing model and is not
    /// its own row.
    #[test]
    fn the_default_alias_row_is_dropped() {
        let owned = curated_ids();
        let payload = corpus_frame(MODEL_DISCOVERY, 2).payload;
        let discovery = discovery_from_reply(&payload, &borrowed(&owned)).unwrap();
        assert!(discovery.models.iter().all(|m| m.id != "default"));
        assert!(
            discovery
                .models
                .iter()
                .all(|m| m.label != "Default (recommended)")
        );
    }

    /// Two aliases of one model must not become two rows. `[1m]` is an axis
    /// comet already owns — `build_command` appends it from the `contextWindow`
    /// option — so both of these are Opus 5 with a setting.
    #[test]
    fn two_aliases_of_one_model_produce_one_row() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[{"value":"opus[1m]","resolvedModel":"claude-opus-5[1m]","displayName":"Opus (1M context)"},{"value":"opus","resolvedModel":"claude-opus-5","displayName":"Opus"}]}}}"#;
        let discovery = discovery_from_reply(line, &["claude-opus-5"]).unwrap();
        assert_eq!(discovery.models.len(), 1, "{:?}", discovery.models);
        assert_eq!(discovery.models[0].id, "claude-opus-5");
        assert_eq!(
            discovery.models[0].label, "Opus (1M context)",
            "first listing wins"
        );
    }

    /// A model nobody has curated keeps the id the provider says to SELECT it
    /// by. Its resolved id is a canonical name, and passing one back as
    /// `--model` is not something the capture verified.
    #[test]
    fn an_uncurated_model_keeps_its_selectable_id() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[{"value":"opus-6","resolvedModel":"claude-opus-6-20260901","displayName":"Opus 6"}]}}}"#;
        let discovery = discovery_from_reply(line, &["claude-opus-5"]).unwrap();
        assert_eq!(discovery.models[0].id, "opus-6");
    }

    /// Claude publishes no modality field at all, so every model stays `None` —
    /// "the provider did not say", never "no images".
    ///
    /// Captured Claude model entries omit a modality field.
    #[test]
    fn no_modality_field_means_the_provider_did_not_say() {
        let owned = curated_ids();
        let payload = corpus_frame(MODEL_DISCOVERY, 2).payload;
        let discovery = discovery_from_reply(&payload, &borrowed(&owned)).unwrap();
        assert!(discovery.models.iter().all(|m| m.accepts_images.is_none()));
    }

    /// Captured effort arrays map to the ladder while Haiku reports none.
    #[test]
    fn effort_levels_map_to_the_ladder_and_haiku_reports_none() {
        let owned = curated_ids();
        let payload = corpus_frame(MODEL_DISCOVERY, 2).payload;
        let discovery = discovery_from_reply(&payload, &borrowed(&owned)).unwrap();
        assert_eq!(
            discovery.models[2].reasoning_levels,
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max
            ]
        );
        let haiku = discovery
            .models
            .iter()
            .find(|model| model.id == "claude-haiku-4-5")
            .unwrap();
        assert!(
            haiku.reasoning_levels.is_empty(),
            "no supportedEffortLevels key at all"
        );
    }

    /// A level nobody has heard of is dropped, not guessed at and not fatal:
    /// the rest of the answer is still good.
    #[test]
    fn an_unknown_effort_level_is_dropped_without_failing_the_answer() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[{"value":"m","displayName":"M","supportedEffortLevels":["low","galactic"]}]}}}"#;
        let discovery = discovery_from_reply(line, &[]).expect("still decodes");
        assert_eq!(
            discovery.models[0].reasoning_levels,
            vec![ReasoningLevel::Low]
        );
    }

    /// An explicit error subtype is the CLI telling us it cannot answer —
    /// ordinary (a login problem, most likely), not a protocol change. Only
    /// `Unparseable` raises a `Diagnostic`, so mis-mapping this would report
    /// drift on every logged-out machine.
    #[test]
    fn an_error_reply_is_unreachable_not_drift() {
        let line = r#"{"type":"control_response","response":{"subtype":"error","request_id":"i","error":"not logged in"}}"#;
        assert_eq!(
            discovery_from_reply(line, &[]),
            Err(DiscoveryFailure::Unreachable)
        );
    }

    /// A success reply with no `models` key at all is drift, not an empty
    /// catalog. Decoded leniently it would serve the curated list under
    /// `CatalogSource::Live` — the caption suppressed and no `Diagnostic`
    /// raised, while the provider had in fact stopped answering the question.
    /// `models` is required in sdk.d.ts (:3274); only an explicit `[]` is an
    /// answer.
    #[test]
    fn a_reply_with_no_models_key_is_drift() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"commands":[]}}}"#;
        assert_eq!(
            discovery_from_reply(line, &[]),
            Err(DiscoveryFailure::Unparseable)
        );
    }

    /// An explicitly empty list is the CLI answering, so it stays a success.
    #[test]
    fn an_explicitly_empty_model_list_still_decodes() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[]}}}"#;
        let discovery = discovery_from_reply(line, &[]).expect("an empty answer is an answer");
        assert!(discovery.models.is_empty());
    }

    #[test]
    fn an_unreadable_reply_is_drift() {
        assert_eq!(
            discovery_from_reply("not json at all", &[]),
            Err(DiscoveryFailure::Unparseable)
        );
        let wrong_shape = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":"lots"}}}"#;
        assert_eq!(
            discovery_from_reply(wrong_shape, &[]),
            Err(DiscoveryFailure::Unparseable)
        );
    }
}
