//! Turning transport and engine failures into something a user can act on.
//!
//! Every async slot in the UI used to render `err.to_string()` verbatim, which
//! put `RpcError` on screen — including `BadParams`, which wraps
//! `serde_json::Error`, so a wire-shape mismatch showed the user
//! "bad params: missing field `capabilities` at line 1 column 87".
//!
//! Nothing here inspects the error's *message*. Copy is chosen from the
//! variant plus the caller's [`Loading`] context, because the message text is
//! the part written for developers — `RpcError::Failed` carries
//! `EngineError`'s Display, whose arms are prefixed `store:`, `doc:`, `io:`,
//! and so on. The raw error is logged, never rendered.
//!
//! See `.agents/rules/user-facing-errors.md`.

use comet_engine::EngineError;
use comet_rpc::RpcError;

/// What the user was waiting for. Names the thing in the user's vocabulary,
/// not the RPC method's — the copy reads "Couldn't load the agent list", not
/// "ListHarnesses failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loading {
    Agents,
    Models,
    Repositories,
    Branches,
    Folders,
    Accounts,
    Spaces,
}

impl Loading {
    /// The noun phrase, used as "Couldn't load {noun}".
    ///
    /// Shared with the slow-request toast so a wait, a cancel, and a failure
    /// all name the same thing the same way.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Agents => "the agent list",
            Self::Models => "the model list",
            Self::Repositories => "your repositories",
            Self::Branches => "the branch list",
            Self::Folders => "that folder",
            Self::Accounts => "your accounts",
            Self::Spaces => "your spaces",
        }
    }
}

/// User-facing copy for a failed load, and the raw error into the log.
///
/// One sentence, because it renders next to a Retry control that already
/// supplies the action — the rule's summary/hint split collapses to a summary
/// when the affordance beside it *is* the hint.
pub fn load_failure(what: Loading, err: &RpcError) -> String {
    tracing::warn!(loading = ?what, error = %err, "load failed");
    let noun = what.noun();
    match err {
        // The engine is not there, or stopped being there. Recoverable by
        // waiting or retrying, which is exactly what the Retry control does.
        RpcError::Closed | RpcError::Transport(_) => {
            format!("Couldn't load {noun} — Comet's engine isn't responding.")
        }
        // Both mean this build and the engine disagree about the protocol: a
        // half-updated install, or a paired machine on a different version.
        // Retrying cannot fix it, so the copy must not imply it will.
        RpcError::UnknownMethod(_) | RpcError::BadParams(_) => {
            format!(
                "Couldn't load {noun} — this copy of Comet doesn't match its engine. Restart Comet, and update the paired machine if you're connected to one."
            )
        }
        // The engine reached the operation and it failed. Its own message is
        // `EngineError`'s Display and is not written for a user, so it stays
        // in the log.
        RpcError::Failed(_) => format!("Couldn't load {noun}."),
    }
}

/// Why the engine did not start, for the startup gate.
///
/// Distinct from [`load_failure`] because nothing is loading yet: this is the
/// app with no engine behind it, and the gate is the whole screen. The gate
/// carries a Retry control, so the copy is a summary and the affordance is the
/// hint — except for the one case where retrying is the wrong advice.
///
/// The raw failure is an `anyhow` chain from `EngineHandle::bootstrap`. It was
/// rendered verbatim until this existed: `format!("{err:#}")` put a filesystem
/// path, a pid and `COMET_DATA_DIR` on screen when a second copy of Comet was
/// already running. Downcasting recovers the variant so the one actionable
/// case can say what to do instead.
pub fn engine_start_failure(err: &anyhow::Error) -> String {
    tracing::error!(error = ?err, "engine bootstrap failed");
    match err.downcast_ref::<EngineError>() {
        // Retrying fails identically until the other copy exits, so the copy
        // names the action rather than inviting another attempt.
        Some(EngineError::AlreadyRunning { .. }) => {
            "Comet is already running. Switch to the open window, or quit it and try again."
                .to_string()
        }
        // Everything else is a genuine assembly failure — a corrupt store, a
        // directory Comet cannot write to. The user cannot act on which one,
        // and the diagnosis is already in the log.
        _ => "Comet's engine couldn't start.".to_string(),
    }
}

/// A branch or worktree switch that did not happen.
///
/// Separate from [`load_failure`] because this is a mutation: nothing is
/// waiting on a skeleton, and a plain "try again" is wrong advice when the
/// working tree is what refused. `Failed` is by far the common arm here (git
/// declining the checkout), so it gets the concrete suggestion rather than the
/// generic one.
pub fn switch_failure(err: &RpcError) -> String {
    tracing::warn!(error = %err, "branch switch failed");
    match err {
        RpcError::Closed | RpcError::Transport(_) => {
            "Couldn't switch branch — Comet's engine isn't responding.".to_string()
        }
        RpcError::UnknownMethod(_) | RpcError::BadParams(_) => {
            "Couldn't switch branch — this copy of Comet doesn't match its engine.".to_string()
        }
        RpcError::Failed(_) => {
            "Couldn't switch branch. Check the repository for uncommitted changes, then try again."
                .to_string()
        }
    }
}

/// Moving a session onto an existing worktree.
///
/// Separate from [`switch_failure`] even though both surface in the same ref
/// popover, because they are different operations: this path is two `Mutate`
/// document ops (`setChatCwd`, `setChatBranch`) and performs **no git
/// checkout**. Sending the user to look for uncommitted changes here is advice
/// that cannot resolve a document, store, or I/O failure — it points at a
/// working tree that was never touched.
pub fn session_move_failure(err: &RpcError) -> String {
    tracing::warn!(error = %err, "session retarget failed");
    match err {
        RpcError::Closed | RpcError::Transport(_) => {
            "Couldn't move the session — Comet's engine isn't responding.".to_string()
        }
        RpcError::UnknownMethod(_) | RpcError::BadParams(_) => {
            "Couldn't move the session — this copy of Comet doesn't match its engine.".to_string()
        }
        // The engine reached the mutation and it failed. There is no user-side
        // remedy to name — the working tree is not involved — so the copy says
        // what happened and stops rather than inventing a step.
        RpcError::Failed(_) => "Couldn't move the session to that worktree.".to_string(),
    }
}

/// A decision on an approval that did not reach the engine.
///
/// A mutation, like [`switch_failure`], but with a different remedy: the
/// affordance is still on screen, so "try again" needs no words — what the
/// copy has to do is distinguish a transient failure (press it again) from a
/// version mismatch (pressing it again cannot help).
pub fn approval_failure(err: &RpcError) -> String {
    tracing::warn!(error = %err, "approval decision failed to queue");
    match err {
        RpcError::Closed | RpcError::Transport(_) => {
            "Couldn't send your decision — Comet's engine isn't responding.".to_string()
        }
        RpcError::UnknownMethod(_) | RpcError::BadParams(_) => {
            "Couldn't send your decision — this copy of Comet doesn't match its engine. Restart Comet, and update the paired machine if you're connected to one.".to_string()
        }
        RpcError::Failed(_) => "Couldn't send your decision.".to_string(),
    }
}

/// What the user just tried to change. Same shape as [`Loading`], and for the
/// same reason: copy comes from the caller's context plus the error VARIANT,
/// never from the error's message.
///
/// D4's inventory called this thirteen sites rendering `format!("{err}")`;
/// measured on 2026-08-30 it was eleven — seven bare and four behind a
/// hand-written prefix like `"Stop failed: {err}"`, which the inventory's own
/// grep shape would have missed. The count is corrected where it was recorded
/// rather than repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutating {
    /// The sidebar's generic `Mutate` op — rename, archive, delete, reorder.
    /// Deliberately unnamed: one call site carries every op and does not know
    /// which one it is queueing.
    Document,
    /// Resolving the client for the server that owns the target, before any
    /// mutation is sent.
    OwnerReachable,
    Space,
    Account,
    LoginCode,
    Stop,
    Answer,
    DeviceName,
    Unarchive,
}

impl Mutating {
    /// What the user sees when the engine is unreachable or mismatched — the
    /// two arms where the remedy is about Comet rather than the action.
    fn summary(self) -> &'static str {
        match self {
            Self::Document => "That change didn't go through",
            Self::OwnerReachable => "Couldn't reach that machine",
            Self::Space => "Couldn't add the space",
            Self::Account => "Nothing changed",
            Self::LoginCode => "Couldn't finish signing in",
            Self::Stop => "Couldn't stop the run",
            Self::Answer => "Couldn't send your answer",
            Self::DeviceName => "Couldn't rename the device",
            Self::Unarchive => "Couldn't unarchive that session",
        }
    }

    /// What the user sees when the engine ANSWERED and refused. This is where
    /// the actions genuinely differ, which is why they are one enum and not one
    /// generic sentence: "try again" is wrong advice for half of them.
    fn refused(self) -> &'static str {
        match self {
            Self::Document => "That change didn't go through.",
            Self::OwnerReachable => {
                "That machine isn't connected right now. Reconnect it, then try again."
            }
            Self::Space => {
                "Couldn't add the space. Check the folder still exists and Comet can read it."
            }
            Self::Account => "Nothing changed. The agent's CLI may need signing in again.",
            Self::LoginCode => {
                "That code wasn't accepted. Check you copied all of it, or start the sign-in again."
            }
            Self::Stop => "Couldn't stop the run. It may have already finished.",
            Self::Answer => "Couldn't send your answer. The run may have moved on without it.",
            Self::DeviceName => "Couldn't rename the device.",
            Self::Unarchive => "Couldn't unarchive that session.",
        }
    }
}

/// User-facing copy for a failed mutation, and the raw error into the log.
///
/// The mutation half of [`load_failure`], and the last of D4's sites. A load
/// renders beside a Retry control that supplies the action; a mutation usually
/// does not, which is why [`Mutating::refused`] names a remedy per action
/// instead of one shared sentence.
pub fn mutation_failure(what: Mutating, err: &RpcError) -> String {
    tracing::warn!(error = %err, action = ?what, "mutation failed");
    match err {
        RpcError::Closed | RpcError::Transport(_) => {
            format!("{} — Comet's engine isn't responding.", what.summary())
        }
        // `concat!` rather than a backslash-continued literal: rustfmt rejoins
        // those and the continuation indentation becomes a run of real spaces
        // inside the sentence. Invisible in the source, and caught here by
        // `no_mutation_copy_leaks_the_error_it_came_from`'s own check — which
        // is the fourth time that trap has appeared in this codebase.
        RpcError::UnknownMethod(_) | RpcError::BadParams(_) => format!(
            concat!(
                "{} — this copy of Comet doesn't match its engine. Restart Comet, and update ",
                "the paired machine if you're connected to one.",
            ),
            what.summary()
        ),
        RpcError::Failed(_) => what.refused().to_string(),
    }
}

/// A reply that arrived but would not decode.
///
/// Same cause as [`RpcError::BadParams`] — this build and the engine disagree
/// about the wire shape — just detected on our side of it, so it gets the same
/// copy rather than inventing a second wording for one condition.
pub fn decode_failure(what: Loading, err: &serde_json::Error) -> String {
    load_failure(what, &RpcError::BadParams(err.to_string()))
}

#[cfg(test)]
mod tests {

    /// Break caught (D4): eleven mutation sites rendered `format!("{err}")`, so
    /// `RpcError`'s Display reached the screen — including `BadParams`, which
    /// wraps a serde message. Every `Mutating` arm is checked, not a sample:
    /// the enum exists so each action can name its own remedy, and an arm
    /// nobody wrote copy for would otherwise pass by inheriting a neighbour's.
    #[test]
    fn no_mutation_copy_leaks_the_error_it_came_from() {
        let leaky = "bad params: missing field `capabilities` at line 1 column 87";
        for what in [
            Mutating::Document,
            Mutating::OwnerReachable,
            Mutating::Space,
            Mutating::Account,
            Mutating::LoginCode,
            Mutating::Stop,
            Mutating::Answer,
            Mutating::DeviceName,
            Mutating::Unarchive,
        ] {
            for err in [
                RpcError::Closed,
                RpcError::Transport(leaky.into()),
                RpcError::BadParams(leaky.into()),
                RpcError::UnknownMethod(leaky.into()),
                RpcError::Failed(leaky.into()),
            ] {
                let copy = mutation_failure(what, &err);
                for leak in ["capabilities", "bad params", "column 87", "RpcError"] {
                    assert!(
                        !copy.contains(leak),
                        "{what:?} leaked {leak:?} into: {copy}"
                    );
                }
                assert!(
                    copy.ends_with('.'),
                    "{what:?} must read as a sentence: {copy}"
                );
                assert!(
                    !copy.contains("  "),
                    "{what:?} carries a continuation run: {copy}"
                );
            }
        }
    }

    /// The reason `Mutating` is an enum rather than one generic sentence: when
    /// the engine ANSWERED and refused, "try again" is wrong advice for half of
    /// these, so the arms must not collapse to the same string.
    #[test]
    fn a_refused_mutation_says_something_different_per_action() {
        let refused: std::collections::BTreeSet<String> = [
            Mutating::Space,
            Mutating::Account,
            Mutating::LoginCode,
            Mutating::Stop,
            Mutating::Answer,
        ]
        .into_iter()
        .map(|what| mutation_failure(what, &RpcError::Failed("store: nope".into())))
        .collect();
        assert_eq!(
            refused.len(),
            5,
            "each action's refusal has to name its own remedy: {refused:?}"
        );
    }
    use super::*;

    /// The startup gate is the whole screen when it shows, and it used to show
    /// a filesystem path, a pid and `COMET_DATA_DIR` — everything the rule
    /// forbids, in the one place a first-run user is most likely to see it.
    #[test]
    fn a_second_running_copy_is_named_not_dumped() {
        let err = anyhow::Error::new(EngineError::AlreadyRunning {
            data_dir: "/home/someone/.local/share/comet".into(),
            pid: "48213".into(),
        });
        let copy = engine_start_failure(&err);

        assert!(copy.contains("already running"), "names the cause: {copy}");
        assert!(copy.contains("quit it"), "names the action: {copy}");
        for leak in ["/home/someone", "48213", "COMET_DATA_DIR", "pid"] {
            assert!(!copy.contains(leak), "leaked {leak:?} into: {copy}");
        }
    }

    /// Every other assembly failure carries a developer-facing Display —
    /// `EngineError` prefixes its arms `store:`, `doc:`, `io:` — so the copy is
    /// generic on purpose and the diagnosis stays in the log.
    #[test]
    fn other_startup_failures_never_reach_the_gate_verbatim() {
        let poison = "store: sqlite disk image is malformed";
        for err in [
            anyhow::Error::new(EngineError::Other(poison.into())),
            anyhow::Error::msg(poison),
        ] {
            let copy = engine_start_failure(&err);
            assert_eq!(copy, "Comet's engine couldn't start.");
            assert!(!copy.contains("sqlite"), "leaked into: {copy}");
            assert!(!copy.contains("store:"), "leaked a log prefix: {copy}");
        }
    }

    /// D51's exact failure, pinned rather than assumed closed. Copying a
    /// `COMET_DATA_DIR` to a new path breaks `device-identity.pem`'s
    /// restrictive ACL, and the gate showed
    /// `DACL: identity I/O: C:\...\device-identity.pem does not have the
    /// private...` — truncated mid-sentence, with no action but a Retry that
    /// could not succeed.
    ///
    /// The row was filed against slice 4.1 and was already fixed by #98's
    /// `engine_start_failure`, which nobody went back to record. This test is
    /// what makes that true rather than probable: the identity variant reaches
    /// the catch-all like every other assembly failure, and the anyhow CONTEXT
    /// the real chain carried ("DACL") must not leak either.
    #[test]
    fn a_broken_identity_file_does_not_put_its_path_on_the_gate() {
        let io = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            r"C:\Users\someone\comet\device-identity.pem does not have the private key",
        );
        let err = anyhow::Error::new(EngineError::Identity(comet_identity::IdentityError::Io(io)))
            .context("DACL");
        let copy = engine_start_failure(&err);

        assert_eq!(copy, "Comet's engine couldn't start.");
        for leak in [
            "device-identity",
            "DACL",
            "identity I/O",
            "private key",
            r"C:\Users",
        ] {
            assert!(!copy.contains(leak), "leaked {leak:?} into: {copy}");
        }
    }

    /// The whole point: nothing from the error's message may reach the string
    /// the user reads. This is the regression that put serde's decode text in
    /// a red row.
    #[test]
    fn no_variant_leaks_the_underlying_message() {
        let poison = "missing field `capabilities` at line 1 column 87";
        let errors = [
            RpcError::Closed,
            RpcError::Transport(poison.into()),
            RpcError::UnknownMethod(poison.into()),
            RpcError::BadParams(poison.into()),
            RpcError::Failed(poison.into()),
        ];
        for err in &errors {
            let shown = load_failure(Loading::Agents, err);
            assert!(
                !shown.contains("capabilities") && !shown.contains("column"),
                "leaked the raw error: {shown}"
            );
            assert!(
                !shown.contains("bad params") && !shown.contains("transport:"),
                "leaked the variant's Display prefix: {shown}"
            );
        }
    }

    /// Every context produces copy that names what failed — "Something went
    /// wrong" tells the user nothing about which panel to look at.
    #[test]
    fn every_context_names_what_it_was_loading() {
        for what in [
            Loading::Agents,
            Loading::Models,
            Loading::Repositories,
            Loading::Branches,
            Loading::Folders,
            Loading::Accounts,
            Loading::Spaces,
        ] {
            let shown = load_failure(what, &RpcError::Closed);
            assert!(
                shown.contains(what.noun()),
                "copy must name the thing: {shown}"
            );
            assert!(shown.starts_with("Couldn't load "), "{shown}");
        }
    }

    /// A document mutation must not be described as a git problem. The two
    /// operations behind the ref popover look alike to the user and are not:
    /// only one of them checks anything out.
    #[test]
    fn moving_a_session_is_not_described_as_a_checkout() {
        let moved = session_move_failure(&RpcError::Failed("store: disk full".into()));
        assert!(
            !moved.contains("uncommitted"),
            "no checkout happened, so uncommitted changes cannot be the cause: {moved}"
        );
        assert!(moved.contains("move the session"), "{moved}");
        // The checkout path keeps the advice that IS relevant to it.
        let checkout = switch_failure(&RpcError::Failed("git: would be overwritten".into()));
        assert!(checkout.contains("uncommitted"), "{checkout}");
        // Neither leaks the engine's own prefixed message.
        assert!(!moved.contains("store:") && !checkout.contains("git:"));
    }

    /// A version mismatch is not fixed by retrying, and the copy must not
    /// suggest it is — the Retry control sits right next to this text.
    #[test]
    fn a_protocol_mismatch_does_not_promise_a_retry_will_help() {
        let shown = load_failure(Loading::Models, &RpcError::BadParams("x".into()));
        assert!(shown.contains("Restart Comet"), "{shown}");
        let transient = load_failure(Loading::Models, &RpcError::Closed);
        assert!(
            !transient.contains("Restart Comet"),
            "a transient failure should not send the user to a restart: {transient}"
        );
    }

    /// A queued decision that failed must not put `RpcError`'s Display on
    /// screen — this is rule 1, and the composer's existing `format!("{err}")`
    /// sites are known debt, not a pattern to extend.
    #[test]
    fn a_failed_decision_never_leaks_the_error() {
        let poison = "bad params: missing field `decision` at line 1 column 12";
        for err in [
            RpcError::Closed,
            RpcError::Transport(poison.into()),
            RpcError::UnknownMethod(poison.into()),
            RpcError::BadParams(poison.into()),
            RpcError::Failed(poison.into()),
        ] {
            let shown = approval_failure(&err);
            assert!(
                !shown.contains("missing field") && !shown.contains("column"),
                "{shown}"
            );
            assert!(
                shown.contains("decision"),
                "the copy names what failed: {shown}"
            );
        }
        // A mismatch is not fixed by pressing the button again.
        assert!(approval_failure(&RpcError::BadParams("x".into())).contains("Restart Comet"));
        assert!(!approval_failure(&RpcError::Closed).contains("Restart Comet"));
    }
}
