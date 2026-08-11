//! Claude's live model discovery: reading one `initialize` control response.
//!
//! Captured against Claude Code 2.1.227 on 2026-08-11
//! (`captures/2026-08-11-claude-initialize-handshake.md`). Three facts from
//! that capture shape this file, and none of them is in sdk.d.ts 0.3.195:
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
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use comet_proto::ReasoningLevel;

use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure};

/// Matches `PROBE_TIMEOUT`. With `--bare` the observed answer is under a
/// second, and the wait is paid once per boot by a caller that already has a
/// spinner and a Cancel (2.1's `Loadable` slot). Ten seconds is therefore a
/// long way past a healthy answer — but see `DISCOVERY_ARGS`: a spawn that
/// runs the user's hooks blew through it, and the margin is what makes a slow
/// machine degrade to the built-in list rather than a slow one hang.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The spawn arguments for a session that will never run a turn: the
/// stream-json transport, and `--bare`. Deliberately NOT `build_command`'s
/// list — `--permission-prompt-tool`, `--permission-mode`, `--model` and
/// `--include-partial-messages` all describe a turn, and there is none.
///
/// **`--bare` is load-bearing, not tidiness.** It skips hooks, LSP, plugin
/// sync, auto-memory and CLAUDE.md discovery. Without it the user's
/// `SessionStart` hooks run in a session that will never prompt: measured in
/// the real app on Windows, that was 7.1s of startup plus 3.5s of hook before
/// the reply, i.e. **10.6s — past this timeout**, and the picker fell back to
/// the built-in list. With `--bare` the same machine answers in 0.7s with a
/// byte-identical model list.
///
/// Two consequences worth knowing. `account` degrades to
/// `{"tokenSource":"none"}` because OAuth and keychain are never read — fine
/// here, since nothing reads `account`, and it keeps the user's email off the
/// wire entirely. And `commands` shrinks to the built-ins (42 vs 66 observed),
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
    let mut cmd = Command::new(exe);
    crate::compose_child_path(&mut cmd, exe);
    cmd.args(DISCOVERY_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // The timeout arm drops this future; without kill_on_drop the child
        // outlives the discovery and we leak a CLI per attempt.
        .kill_on_drop(true)
        // Any directory would do for models — the capture found the model list
        // identical across cwds while `commands` varied — and a neutral one
        // avoids loading a project's own settings for a session with no turn.
        .current_dir(std::env::temp_dir());
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW, as in `probe_cli_version`: the `.cmd` shims are
        // console apps and would flash a window on every boot otherwise.
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd.spawn().map_err(|err| {
        tracing::debug!(cli = %exe.display(), %err, "claude discovery spawn failed");
        DiscoveryFailure::Unreachable
    })?;
    let mut stdin = child.stdin.take().ok_or(DiscoveryFailure::Unreachable)?;
    let stdout = child.stdout.take().ok_or(DiscoveryFailure::Unreachable)?;

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
            let borrowed: Vec<&str> = curated_ids.iter().map(String::as_str).collect();
            answer = discovery_from_reply(&line, &borrowed);
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
            reasoning_levels: model
                .supported_effort_levels
                .iter()
                .filter_map(|l| to_level(l))
                .collect(),
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
    use comet_proto::ReasoningLevel;

    /// The literal frame Claude Code 2.1.227 sent on 2026-08-11, from
    /// `captures/2026-08-11-claude-initialize-handshake/run2-close.jsonl`, with
    /// only the `account` block replaced (it carries a real email).
    ///
    /// Pinned as the CLI's own bytes rather than round-tripped through our own
    /// types on purpose: a round-trip test cannot catch the reply moving under
    /// us, which is exactly how 2.1 shipped a runtime-broken picker (AGENTS.md,
    /// "Changing what an RPC method answers with").
    const CAPTURED_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"init-1","response":{"commands":[{"name":"verify","description":"Run the gate. (project)","argumentHint":""}],"agents":[{"name":"Explore"}],"output_style":"default","available_output_styles":["default","Explanatory"],"models":[{"value":"default","resolvedModel":"claude-opus-5[1m]","displayName":"Default (recommended)","description":"Opus 5 with 1M context","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsAdaptiveThinking":true,"supportsFastMode":true,"supportsAutoMode":true},{"value":"opus[1m]","resolvedModel":"claude-opus-5[1m]","displayName":"Opus (1M context)","description":"Opus 5 with 1M context","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsAdaptiveThinking":true,"supportsFastMode":true,"supportsAutoMode":true},{"value":"claude-fable-5[1m]","resolvedModel":"claude-fable-5","displayName":"Fable","description":"Fable 5","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsAdaptiveThinking":true,"supportsAutoMode":true},{"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet","description":"Sonnet 5","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsAdaptiveThinking":true,"supportsAutoMode":true},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku","description":"Haiku 4.5"}],"account":{"email":"user@example.test","organization":"Example","subscriptionType":"Claude Max","apiProvider":"firstParty"},"pid":1234,"current_permission_mode":"acceptEdits","remote_control_auto_enable":false,"remote_control_auto_on_by_default":false,"ide_rc_auto_enable_gate":false,"fast_mode_state":"off","fast_mode_disabled_reason":null}}}"#;

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
    /// them literally (capture, 2026-08-11) and every consumer downstream — the
    /// merge, the picker, `--model` — speaks curated.
    #[test]
    fn the_captured_reply_decodes_onto_curated_ids() {
        let owned = curated_ids();
        let discovery = discovery_from_reply(CAPTURED_REPLY, &borrowed(&owned)).expect("decodes");
        let ids: Vec<&str> = discovery.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-opus-5",
                "claude-fable-5",
                "claude-sonnet-5",
                "claude-haiku-4-5"
            ],
            "aliases canonicalized, `default` dropped"
        );
        assert_eq!(
            discovery.models[2].label, "Sonnet",
            "the live label is kept"
        );
    }

    /// The slice's whole point, on the real data and the real catalog. The
    /// shared merge is unchanged from 2.1 — if the adapter did not
    /// canonicalize, this is where eleven rows would show up.
    #[test]
    fn the_captured_reply_adds_no_rows_to_the_real_catalog() {
        let owned = curated_ids();
        let discovery = discovery_from_reply(CAPTURED_REPLY, &borrowed(&owned)).unwrap();
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
            ],
            "six curated rows, no duplicates and nothing deleted"
        );
        assert_eq!(merged[4].label, "Sonnet 5", "curated label still wins");
    }

    /// `default` is a pointer at whatever the user's default happens to be,
    /// labelled "Default (recommended)". Kept, it would either duplicate the
    /// model it resolves to or, once that model is not curated, put a row named
    /// after a setting in the picker.
    #[test]
    fn the_default_alias_row_is_dropped() {
        let owned = curated_ids();
        let discovery = discovery_from_reply(CAPTURED_REPLY, &borrowed(&owned)).unwrap();
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
    #[test]
    fn no_modality_field_means_the_provider_did_not_say() {
        let owned = curated_ids();
        let discovery = discovery_from_reply(CAPTURED_REPLY, &borrowed(&owned)).unwrap();
        assert!(discovery.models.iter().all(|m| m.accepts_images.is_none()));
    }

    #[test]
    fn effort_levels_map_to_the_ladder_and_haiku_reports_none() {
        let owned = curated_ids();
        let discovery = discovery_from_reply(CAPTURED_REPLY, &borrowed(&owned)).unwrap();
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
        let haiku = discovery.models.last().unwrap();
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
