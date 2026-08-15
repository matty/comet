//! Claude's slash-command discovery: the same `initialize` reply as
//! `discovery.rs`, read for `commands` instead of `models`, in the chat's own
//! directory.
//!
//! The capture corpus records the non-bare initialize reply used here. Two
//! facts from that reply shape this file:
//!
//! 1. Commands are scoped to the selected working directory.
//! 2. **It cannot reuse the model discovery's spawn**, because that one passes
//!    `--bare`, which skips user and project skill discovery. Debt row D32.
//!
//! The reviewed reply is `claude/2.1.228/command-discovery` frame 5: non-bare
//! initialization discovers scoped commands from the selected working directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use comet_proto::AgentCommand;

use crate::discovery::DiscoveryFailure;

/// Twice the model discovery's timeout, deliberately.
///
/// This spawn is the one without `--bare`, so it can run the user's
/// `SessionStart` hooks. The bounded wait is paid at most once per directory
/// per boot behind a dismissible loading surface, then fails into the existing
/// unavailable-command fallback instead of waiting forever.
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
    let line = super::discovery::initialize_reply(command_discovery_launch(exe, cwd)).await?;
    commands_from_reply(&line)
}

/// Select the exact launch used for Claude command discovery.
pub(crate) fn command_discovery_launch(exe: &Path, cwd: &Path) -> crate::launch::LaunchDescriptor {
    super::discovery::claude_initialize_launch(exe, COMMAND_ARGS, cwd)
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
    use crate::capture::corpus_frame;

    const COMMAND_DISCOVERY: &str = "claude/2.1.228/command-discovery";

    /// Command objects include `argumentHint` and may include `aliases`.
    ///
    /// `.response.response.commands[].name` and `.aliases[]` are not on
    /// `claude.txt` (the allowlist-sanitizer stage excludes the whole
    /// `commands[]` family as installed-tooling identity), so the archive no
    /// longer holds the literal command names or alias text. The count is
    /// what survives as evidence the decode walks the WHOLE array rather than
    /// stopping early or truncating; the non-empty-aliases check is evidence
    /// the decode reads a populated `aliases` list at all, distinct from
    /// `a_command_without_aliases_decodes_to_an_empty_list` below which only
    /// covers the absent case.
    #[test]
    fn the_captured_reply_decodes_every_command() {
        let payload = corpus_frame(COMMAND_DISCOVERY, 5).payload;
        let commands = commands_from_reply(&payload).expect("decodes");
        assert_eq!(commands.len(), 57, "the literal provider list is complete");
        assert!(
            commands.iter().any(|c| !c.aliases.is_empty()),
            "at least one captured command should carry a non-empty aliases list: {commands:?}"
        );
    }

    /// `argumentHint: ""` is what the CLI sends for a command that takes no
    /// arguments, and a `description` is never usefully empty — an empty
    /// string means the same as an absent key: nothing to show. This is
    /// `non_empty()`'s own contract, not a fact about the provider's wire, so
    /// it is tested here as a hand-written fixture rather than corpus
    /// evidence.
    ///
    /// It CANNOT read the corpus for this: `.response.response.commands[].
    /// argumentHint` is not on `claude.txt` (same exclusion as the test
    /// above), so every archived `argumentHint` is now a non-empty
    /// placeholder — the sanitizer redacts every `String` scalar on an
    /// unlisted path regardless of whether the original value was empty.
    /// "Empty decodes as absent" can never again be demonstrated from
    /// committed evidence once its path is excluded; see `docs/debt/D73` for
    /// the general shape of this trade.
    #[test]
    fn an_empty_argument_hint_is_absent_not_blank() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"commands":[
            {"name":"bare","description":"d","argumentHint":""},
            {"name":"withHint","description":"d","argumentHint":"<file>"}
        ]}}}"#;
        let commands = commands_from_reply(line).unwrap();
        assert_eq!(commands[0].argument_hint, None);
        assert_eq!(commands[1].argument_hint.as_deref(), Some("<file>"));
    }

    /// Aliases are optional on the wire — most commands carry none — and the
    /// absent case must be an empty list rather than a decode failure.
    ///
    /// Captured command entries may omit `aliases`.
    #[test]
    fn a_command_without_aliases_decodes_to_an_empty_list() {
        let payload = corpus_frame(COMMAND_DISCOVERY, 5).payload;
        let commands = commands_from_reply(&payload).unwrap();
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
