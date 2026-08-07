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
    fn noun(self) -> &'static str {
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
    use super::*;

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
}
