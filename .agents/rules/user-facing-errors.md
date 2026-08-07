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

### Not yet registered

Only the **model list** is wired. These four still load without a toast, and
each is the same three-line transformation above:

| Surface | Function |
| --- | --- |
| Harness catalog | `pickers.rs` `ensure_harnesses` |
| Branch list | `pickers.rs` `ensure_refs` |
| Folder browser | `shell/spaces.rs` folder listing |
| Accounts page | `settings/accounts.rs` `load` |

Wire one when you touch it. Each needs its own re-arm decision — the discrete
"asked for it again" event differs per surface (picker open, page revisit).

## Known debt — check before adding more

- **`RpcClient::call` has no timeout** (`crates/rpc/src/client.rs`). `rx.await`
  resolves when the reply lands or when the connection drops — a live engine
  whose handler never answers leaves the caller pending forever, and the
  `Loadable` slot stays `Loading` with no Retry. Handlers shell out to `git` and
  spawn agent CLIs, so a hang is reachable, not theoretical. **This is the open
  half of the rule: the "no raw errors" half now holds, the "no unbounded wait"
  half does not.** Fixing it needs a decision first — a blanket timeout breaks
  legitimately slow calls (a large `git diff`, a cold repo scan), so the
  candidates are a per-method budget or a "still working — cancel?" state that
  never hard-fails.

The error surfaces themselves are fine: `popover::error_row` and
`widgets::error_strip` both carry a Retry affordance. It is the *message text*
and the *`Loading` state* that break these rules, not the escape hatch.

## Reviewing a change

Ask, for anything that can fail or wait:

- Does every string on this path read as something written for a user?
- If the reply never comes, what does the user see in 30 seconds? In five minutes?
- Is the technical detail still recoverable from `tracing` for diagnosis?
