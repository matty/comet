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

## Known debt — check before adding more

Do not extend these patterns; fix the one you touch.

- `Err(e) => Loadable::Error(e.to_string())` appears in ~12 places across
  `pickers.rs`, `settings/accounts.rs`, and `shell/spaces.rs`. Each one puts an
  `RpcError` on screen verbatim, including `BadParams` — which wraps
  `serde_json::Error`, so a wire mismatch shows the user
  "bad params: missing field `capabilities` at line 1 column 87".
- **`RpcClient::call` has no timeout** (`crates/rpc/src/client.rs`). `rx.await`
  resolves when the reply lands or when the connection drops — a live engine
  whose handler never answers leaves the caller pending forever, and the
  `Loadable` slot stays `Loading` with no Retry. Handlers shell out to `git` and
  spawn agent CLIs, so a hang is reachable, not theoretical.

The error surfaces themselves are fine: `popover::error_row` and
`widgets::error_strip` both carry a Retry affordance. It is the *message text*
and the *`Loading` state* that break these rules, not the escape hatch.

## Reviewing a change

Ask, for anything that can fail or wait:

- Does every string on this path read as something written for a user?
- If the reply never comes, what does the user see in 30 seconds? In five minutes?
- Is the technical detail still recoverable from `tracing` for diagnosis?
