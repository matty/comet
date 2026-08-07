# Errors and waiting states the user sees

Two rules, and they are not style preferences:

1. **The user never sees a raw technical error.** No `err.to_string()` reaching a
   surface, no serde decode text, no OS error codes, no CLI stderr dumped verbatim.
2. **No waiting state can last forever.** Every skeleton must have a terminal
   state — a reply, a timeout, or a bounded retry that gives up into an error the
   user can act on.

A raw error tells the user nothing they can do and reads as a broken app. A
skeleton that never resolves is worse: an error at least offers a way out, while
an endless spinner leaves no way to tell whether to wait or give up.

## The shape to write instead

Split the failure into a **summary** and a **hint**, and keep the diagnosis in
the log:

```rust
// proto
Unavailable {
    summary: String,          // short label a row can show without hover
    hint: Option<String>,     // one sentence naming what to do
}

// at the source
tracing::debug!(cli = stem, searched = searched_locations(), "cli did not resolve");
("Not installed", format!("Install {stem}, or set {override_var} to its path."))
```

`HarnessAvailability` in `crates/proto/src/agent.rs` is the worked example. It
previously carried one prose string that concatenated an inventory of every
searched location ahead of the only actionable clause — five lines of hover text
whose useful half landed last. The summary/hint split is what fixed it.

Rules of thumb for the copy:

- The summary is a **label**, not a sentence: it renders in a 148px rail column.
  Two words. Never repeat the row's own name in it.
- The hint names the **action**, and comes first in the user's reading order —
  never behind diagnostic detail.
- Use the product's vocabulary. The UI says *Agent*; `harness` is our internal
  word and must not leak into a message (`HarnessError::NotInstalled`'s Display
  reaches the models pane verbatim — that is why it reads "Agent CLI not found").
- Amber (`warning_muted`) for a state to resolve; red (`danger`) for something
  that actually failed. "Not installed" is amber.

## Where the translation lives

`crates/ui/src/errors.rs`. Every failing load goes through it:

```rust
Err(err) => Loadable::Error(errors::load_failure(errors::Loading::Agents, &err)),
```

It picks copy from the `RpcError` *variant* plus the caller's `Loading` context
and never reads the error's message, because the message is the developer-facing
part. `RpcError::Failed` carries `EngineError`'s Display, whose arms are prefixed
`store:`, `doc:`, `io:` — which is why pass-through is not an option even though
some of those messages look presentable. Adding a surface means adding a
`Loading` variant, not writing a new sentence at the call site.

Two distinctions the copy makes, both load-bearing:

- Transient (`Closed`, `Transport`) says the engine isn't responding. A version
  mismatch (`UnknownMethod`, `BadParams`) says to restart — retrying cannot fix
  it, and the Retry control is sitting right there implying it can.
- A single sentence is enough where a Retry affordance is adjacent: the button
  *is* the hint. The summary/hint split matters where there is no such control,
  as on the harness rail rows.

## Waiting: the slow-request toast

`crates/ui/src/toast.rs`. After `SLOW_AFTER` (4s) a request stops being silent:
a top-anchored toast names what is loading and offers Cancel. The wait is never
cut short — a hard timeout would fail work that was about to succeed — so the
bound is on *silence*, not on the request.

Registering a load takes three lines at the call site:

```rust
let (request_id, cancelled) = toast::begin(cx, errors::Loading::Models);
let call = std::pin::pin!(engine.client().call(method, params));
let outcome = futures::future::select(call, cancelled).await;
// …then toast::end(cx, request_id) on every path out, and resolve the slot to
// toast::cancelled_message(what) on the Right arm.
```

Cancel is real: losing the select race drops the RPC future, and `RpcClient`'s
`PendingGuard` turns that drop into a `{id, cancel}` frame, so the engine stops
too. A cancelled slot must then be **re-armed on the next discrete demand**
(`Pickers::rearm_cancelled_models`) — never from render, which runs `ensure_*`
every frame and would restart the request as fast as it was cancelled.

### Two decisions per surface

**Does it register?** Every load that leaves the user looking at a skeleton, yes.

**Does it offer Cancel?** Only if stopping it changes what is on screen. A load
that *revalidates rows already painted* registers with `begin_uncancellable`: it
gets the sentence, not the affordance, because a control with no visible effect
is worse than none — and because its handler would otherwise have to decide
whether to throw away a good list. `ensure_refs`' stale-while-revalidate refresh
and `revalidate_harnesses`' poll loop are the two. This is not an escape hatch:
a skeleton is always cancellable, because there the wait is all the user has.

**Then: how does the cancelled slot re-arm?** Usually it already does. Of the
five registered loads only `ensure_harnesses` needs a marker, because it is the
only one render calls every frame — so it must refuse to reload an `Error`, and
a cancel leaves exactly that. The other four reload unconditionally from their
discrete triggers (`ensure_refs` gets `force: true` on every popover open;
`load_space_folders` is called by every crumb, row and Retry; `accounts::load`
by Retry, Refresh, a device switch and every post-action refresh), so a cancel
there re-arms by construction. Check the call sites before adding a marker.

### A cancel is not a failure, even when it lands in the error slot

Both end up in `Loadable::Error`, so any surface that *rewrites* its error text
will rewrite the cancel too. The folder browser did: it replaced the message
with "{device} didn't respond — is it online?", which turned "you stopped this"
into a machine diagnosis pointing at the wrong thing. `browser_error_line` in
`shell/spaces.rs` is the fix and the worked example — if a surface derives copy
from the fact of an error, it has to know which kind it has.

## Known debt — check before adding more

- **`RpcClient::call` has no timeout** (`crates/rpc/src/client.rs`). `rx.await`
  resolves when the reply lands or when the connection drops — a live engine
  whose handler never answers leaves the caller pending forever. Every *load*
  now answers that with the toast, so the slot is explained and escapable; a
  **mutation** still has nothing. Fixing it properly needs a decision first — a
  blanket timeout breaks legitimately slow calls (a large `git diff`, a cold
  repo scan), so the candidates are a per-method budget or a "still working —
  cancel?" state that never hard-fails.
- **Mutations still render raw errors.** Rule 1 holds for loads and not for the
  rest: `format!("{err}")` on an `RpcError` reaches the user from
  `settings/accounts.rs` (switch/forget, login start, poll),
  `shell/spaces.rs` (space create, delete), `shell.rs` (sidebar notices),
  `composer.rs` (stop, answer), `settings/devices.rs` (rename) and
  `settings/archived.rs` (unarchive) — thirteen sites. `errors.rs` is where the
  copy goes; each needs its own context the way `switch_failure` and
  `session_move_failure` did, because "try again" is wrong advice for some of
  them.

The error surfaces themselves are fine: `popover::error_row` and
`widgets::error_strip` both carry a Retry affordance. It is the *message text*
and the *`Loading` state* that break these rules, not the escape hatch.

## Reviewing a change

Ask, for anything that can fail or wait:

- Does every string on this path read as something written for a user?
- If the reply never comes, what does the user see in 30 seconds? In five minutes?
- Is the technical detail still recoverable from `tracing` for diagnosis?
