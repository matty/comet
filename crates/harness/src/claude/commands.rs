//! Claude's slash-command discovery: the same `initialize` reply as
//! `discovery.rs`, read for `commands` instead of `models`, in the chat's own
//! directory.
//!
//! Captured against Claude Code 2.1.227 on 2026-08-11
//! (`captures/2026-08-11-slash-command-expansion.md`). Three facts from that
//! capture shape this file:
//!
//! 1. **The CLI expands `/name` itself** in comet's exact non-interactive
//!    spawn, so the menu this feeds is autocomplete and nothing more — comet
//!    never has to read a command's body or substitute anything.
//! 2. **It cannot reuse the model discovery's spawn**, because that one passes
//!    `--bare`, which skips user and project skill discovery (42 commands
//!    against 67). Debt row D32.
//! 3. **An unrecognized command fails silently-successfully** — `Unknown
//!    command: /x` arrives as ordinary assistant text with `is_error: false`
//!    and no model call — so a stale list cannot be detected after the fact.
//!    Correctness has to live in this list being right.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use comet_proto::AgentCommand;

use crate::discovery::DiscoveryFailure;

/// Twice the model discovery's timeout, deliberately.
///
/// This spawn is the one without `--bare`, so it runs the user's `SessionStart`
/// hooks — and the same non-bare handshake measured **1.4s from a terminal and
/// 10.6s from inside the running app** on this machine (captures 2026-08-11 and
/// 2.2's rendered check). Ten seconds is the number that already failed there
/// once, and the wait is paid at most once per directory per boot, behind a
/// popup that is showing a skeleton and can be dismissed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// `DISCOVERY_ARGS` **without `--bare`**. That single difference is the whole
/// reason this is a second spawn rather than a second read of the first one.
const COMMAND_ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
];

/// Spawn a short-lived CLI **in `cwd`** and read its command list.
///
/// Owned arguments because the future is handed to `CommandCache` and outlives
/// the caller's frame.
pub(crate) async fn discover_commands(
    exe: PathBuf,
    cwd: PathBuf,
) -> Result<Vec<AgentCommand>, DiscoveryFailure> {
    match tokio::time::timeout(COMMAND_TIMEOUT, read_commands(&exe, &cwd)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!(cli = %exe.display(), cwd = %cwd.display(), "claude command discovery timed out");
            Err(DiscoveryFailure::Unreachable)
        }
    }
}

async fn read_commands(exe: &Path, cwd: &Path) -> Result<Vec<AgentCommand>, DiscoveryFailure> {
    let line = super::discovery::initialize_reply(exe, COMMAND_ARGS, cwd).await?;
    commands_from_reply(&line)
}

#[derive(Deserialize)]
struct ControlResponseFrame {
    response: ControlResponseBody,
}

#[derive(Deserialize)]
struct ControlResponseBody {
    subtype: String,
    #[serde(default)]
    response: Option<InitializeReply>,
}

#[derive(Deserialize)]
struct InitializeReply {
    /// No `#[serde(default)]`, for the same reason `models` has none: sdk.d.ts
    /// declares `commands: SlashCommand[]` as required (:3270), so a success
    /// reply without the key is the provider having stopped answering the
    /// question. Defaulted, that would render as an empty menu — "this agent
    /// has no commands" — which is a confident wrong answer rather than a
    /// visible failure. An explicit `[]` still decodes, because that is the CLI
    /// answering.
    commands: Vec<ReplyCommand>,
}

/// Only `name` is required, and that is a deliberate departure from the
/// typings, which mark `description` and `argumentHint` required too.
///
/// Decoding is all-or-nothing across the vector (0.1's review), so one odd
/// entry taken strictly would delete the whole menu. A command missing its
/// prose is still a command the user can run; a menu missing every command is
/// not something they can work around. The `Option`s carry that on their own —
/// serde reads a missing field of `Option` type as `None` — so only `aliases`
/// needs the attribute spelled out.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyCommand {
    name: String,
    description: Option<String>,
    argument_hint: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

/// Blank is not a value here. The CLI sends `argumentHint: ""` for a command
/// that takes no arguments and a `description` is never usefully empty, so an
/// empty string means the same as an absent key: nothing to show.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// The single place this reply's `commands` are read. Its test pins the literal
/// bytes the CLI sent, not a round trip through the structs above.
pub(crate) fn commands_from_reply(line: &str) -> Result<Vec<AgentCommand>, DiscoveryFailure> {
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

    Ok(reply
        .commands
        .into_iter()
        .map(|command| AgentCommand {
            name: command.name,
            description: non_empty(command.description),
            argument_hint: non_empty(command.argument_hint),
            aliases: command.aliases,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal frame Claude Code 2.1.227 sent on 2026-08-11, from
    /// `captures/2026-08-11-slash-command-expansion/run1-init-nonbare.jsonl`,
    /// trimmed to five of its 64 commands: one plain, one with an argument
    /// hint, one with an alias, one project-scoped skill, and one whose
    /// `argumentHint` is the empty string the CLI actually sends.
    ///
    /// Pinned as the CLI's own bytes rather than round-tripped through our own
    /// types on purpose — a round-trip test cannot catch the reply moving under
    /// us, which is how 2.1 shipped a runtime-broken picker (AGENTS.md,
    /// "Changing what an RPC method answers with").
    const CAPTURED_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"init-1","response":{"commands":[{"name":"comet-probe","description":"Probe whether a slash command expands in a stream-json session (project)","argumentHint":"token"},{"name":"debug","description":"Enable debug logging for this session and help diagnose issues","argumentHint":"[issue description]"},{"name":"code-review","description":"Review the current diff for correctness bugs.","argumentHint":"[low|medium|high] [--fix]","aliases":["review"]},{"name":"gpui-ui","description":"Comet's conventions for gpui UI code in crates/ui (project)","argumentHint":""},{"name":"init","description":"Initialize a new CLAUDE.md file with codebase documentation","argumentHint":""}],"agents":[{"name":"Explore"}],"models":[{"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet"}],"account":{"email":"user@example.test"},"pid":1234}}}"#;

    #[test]
    fn the_captured_reply_decodes_every_command() {
        let commands = commands_from_reply(CAPTURED_REPLY).expect("decodes");
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["comet-probe", "debug", "code-review", "gpui-ui", "init"],
            "the provider's own order is kept"
        );
        assert_eq!(
            commands[1].argument_hint.as_deref(),
            Some("[issue description]")
        );
        assert_eq!(commands[2].aliases, vec!["review".to_string()]);
    }

    /// `argumentHint: ""` is what the CLI sends for a command that takes no
    /// arguments — 47 of the 64 in the capture. Kept as `Some("")` it would
    /// render an empty hint slot on most rows in the menu.
    #[test]
    fn an_empty_argument_hint_is_absent_not_blank() {
        let commands = commands_from_reply(CAPTURED_REPLY).unwrap();
        assert_eq!(commands[3].argument_hint, None);
        assert_eq!(commands[4].argument_hint, None);
    }

    /// Aliases are optional on the wire — most commands carry none — and the
    /// absent case must be an empty list rather than a decode failure.
    #[test]
    fn a_command_without_aliases_decodes_to_an_empty_list() {
        let commands = commands_from_reply(CAPTURED_REPLY).unwrap();
        assert!(commands[0].aliases.is_empty());
    }

    /// The blast-radius rule (0.1's review): decoding is all-or-nothing across
    /// the vector, so one entry missing the prose the typings call required
    /// must not delete the other 63.
    #[test]
    fn one_entry_missing_its_prose_does_not_delete_the_menu() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"commands":[{"name":"bare"},{"name":"full","description":"d","argumentHint":"h"}]}}}"#;
        let commands = commands_from_reply(line).expect("still decodes");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].description, None);
        assert_eq!(commands[1].description.as_deref(), Some("d"));
    }

    /// A success reply with no `commands` key at all is drift, not an empty
    /// menu. sdk.d.ts declares it required (:3270); rendered as "no commands"
    /// the user would read a broken read as a provider with nothing to offer.
    #[test]
    fn a_reply_with_no_commands_key_is_drift() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[]}}}"#;
        assert_eq!(
            commands_from_reply(line),
            Err(DiscoveryFailure::Unparseable)
        );
    }

    /// An explicitly empty list is the CLI answering, so it stays a success.
    #[test]
    fn an_explicitly_empty_command_list_still_decodes() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"commands":[]}}}"#;
        assert!(
            commands_from_reply(line)
                .expect("an empty answer is an answer")
                .is_empty()
        );
    }

    /// The CLI answering "no" is ordinary (a login problem, most likely), not a
    /// protocol change.
    #[test]
    fn an_error_reply_is_unreachable_not_drift() {
        let line = r#"{"type":"control_response","response":{"subtype":"error","request_id":"i","error":"not logged in"}}"#;
        assert_eq!(
            commands_from_reply(line),
            Err(DiscoveryFailure::Unreachable)
        );
    }

    #[test]
    fn an_unreadable_reply_is_drift() {
        assert_eq!(
            commands_from_reply("not json at all"),
            Err(DiscoveryFailure::Unparseable)
        );
        let wrong_shape = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"commands":"lots"}}}"#;
        assert_eq!(
            commands_from_reply(wrong_shape),
            Err(DiscoveryFailure::Unparseable)
        );
    }

    /// The one argument that must never be `--bare`: it is what costs the user
    /// and project skills this menu exists to show (42 commands against 67).
    #[test]
    fn the_command_spawn_is_not_bare() {
        assert!(
            !COMMAND_ARGS.contains(&"--bare"),
            "--bare skips user and project skill discovery (D32)"
        );
    }
}
