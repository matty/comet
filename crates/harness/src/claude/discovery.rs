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

use serde::Deserialize;

use comet_proto::ReasoningLevel;

use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure};

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
    #[serde(default)]
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
