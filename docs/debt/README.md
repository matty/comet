# Carried-forward debt

Known-open work that is **not** on a phase plan, because it was discovered
mid-slice and deliberately deferred rather than planned. This file is the index:
one row per item, scannable in one pass. An item whose *reasoning* matters gets
its own page beside this one, linked from its row.

Read this before starting a slice — several rows name the slice that owns them.

## How to keep it

- **Add a row when a slice defers something.** The row is the minimum: what it
  is, which review or capture found it, and what state it is in.
- **Write a page when the reasoning outlives the row.** A ruling ("this is a
  decision, not an oversight"), a corrected premise, or a mechanism someone will
  otherwise re-derive. A page is `D<nn>-<slug>.md`, and it is what a PR comment
  or a code comment links to.
- **Delete a row when it merges** — except where the *explanation* still stops
  someone reading the code wrong. Those move to [`closed.md`](closed.md), and
  the row stays with a `☑ merged` state and its PR.
- **Numbers are never reused.** A deleted D-number stays deleted; rows and slice
  logs elsewhere refer to them by number.

Legend: ☐ open · ◐ partly closed · ☑ merged

## Open

| # | Item | Origin | State |
| --- | --- | --- | --- |
| [D3](D03-unavailable-agent-selection.md) | ~~An unavailable agent can still be the selected one~~ — **not a defect**. Residual: no *pre-send* signal in a locked session | 0.2a | ☐ low priority, optional polish |
| D4 | Thirteen mutation sites still `format!("{err}")` an `RpcError` onto the screen | 0.2b | ☐ deferred, **count unchanged by 1.5** — the slice added `crates/ui/src/errors.rs:approval_failure` rather than a fourteenth site. Inventory in `.agents/rules/user-facing-errors.md` under "Known debt" |
| [D7](D07-remote-allowlist-partition.md) | The remote allow/deny partition in `remote_access.rs` is exhaustive only by two hand-maintained literal lists; a new method omitted from both silently defaults to denied with no test failure | 0b.2 Task 8 review | ☐ open, **unowned** |
| [D8](D08-claude-inner-diagnostics.md) | Claude has no **inner** diagnostic sink — unknown `stream_event` delta kinds and unknown content-block kinds are dropped silently inside frames classified as fully Claimed | 0b.2 whole-branch review | ☐ open — plan gap, Codex got sink 4 and Claude did not |
| [D9](D09-unparseable-sentinel.md) | The `unparseable` sentinel merges "garbage on stdout" with "a claimed frame whose schema changed" — the second is the likelier drift and the information to distinguish them is already in hand | 0b.2 whole-branch review | ☐ open |
| [D10](D10-unbounded-logging.md) | The registry is bounded; the logging and journaling behind it are not. A renamed high-volume method moves from Ignored to Unknown and warn-logs its full payload indefinitely | 0b.2 whole-branch review | ☐ open |
| D13 | Codex declares `auto-accept-edits`, but the linked-worktree sandbox workaround silently raises that mode's workspace-write sandbox to `danger-full-access` — trigger is any linked worktree on a slash-named branch, i.e. this repo's own `feature/…` convention — with only a `tracing::warn!` as the signal | 1.3 whole-branch review | ☐ open — 1.3 documents the exception in the capabilities comment; making it visible to the user needs a notice per `user-facing-errors.md`. **1.5 did not claim it and neither did 1.8** — 1.8 made the mode *selectable*, which makes this reachable by a user rather than only by the default, so it is now **unowned and more visible than when it was filed** |
| [D14](D14-approval-card-states.md) | ~~The card and the composer decision row show the **same label and detail line**; `Allowed` / `Denied` / `Allowed for this session` **paint identically**~~ — both fixed in [PR #36](https://github.com/matty/comet/pull/36) (`5a8a16e`). **Residual: the deny note is written to the model and then vanishes from the UI** | 1.5 rendered check | ◐ two of three closed, **unowned** |
| D15 | An approval drained at a steer or by a session-grant sweep can be outrun by its own `ApprovalRequested`: the event still passes the authority guard (its id is in `minted_approvals`) and folds an **open card nothing can answer** — the decision row offers buttons that return `Ok(false)` | 1.6 fix-wave re-review | ☐ open. Bounded — the next steer or `Done` stamps it `Expired`. One-line mitigation: also remove drained ids from `minted_approvals` |
| D16 | `request_input` still parks its resolver when `engine_tx.send` fails, so a question minted after the run task is gone waits forever — the exact twin of the approval bug 1.6 fixed four lines below it | 1.6 fix-wave re-review | ☐ open, and **visibly asymmetric**: 1.6 made the approval bridge fail closed and drains `pending_inputs` in `RunHandle::drop`, but left the input bridge alone. Two lines |
| D17 | A degenerate `Edit` (empty `new_string`, absent `old_string`) maps to `FileChange{Modify, +0, −0}`, which **is** allowlistable, and its signature is identical to a real edit to that path — so allowing that card for the session auto-allows every subsequent real edit to the same file | 1.6 fix-wave re-review | ☐ open. Same hole closed in the same wave (`FileOperation::Unknown` is no longer allowlistable), entered by a different door. Degenerate input, low likelihood |
| D18 | A `Write` that overwrites an existing file carries the right verb ("Edit a file") but still renders `+N −0`, so the card understates the loss — it names what arrives and not what is replaced | 1.6 PR #37 review | ☐ open. Correcting it means reading an arbitrary-size file on the frame loop to count the replaced lines, a different decision from the verb fix (`9029329`) |
| D19 | `ApprovalRequest::Mcp` carries **no arguments**, so an MCP approval cannot be identified precisely enough to allow again — 1.6 made it un-allowlistable, so "Allow for this session" silently degrades to a plain Allow on MCP cards. The card is equally blind: `server · tool` and no arguments | 1.6 PR #37 review | ☐ open, **owner is 1.7 or later**. Needs the proto variant to carry a discriminating digest of the arguments, which is a `PROTOCOL_VERSION` question 1.6 froze. Fixing the signature and the card are the same change |
| D20 | Codex's `acceptForSession` **works** and is deliberately unused: 1.7 sends `accept` on both allow arms so Comet's engine stays the single grant authority. The server's grant is keyed on **path alone** and spans the operation kind, while `approval_signature` keys on path **and** operation — delegating would leave two caches with different scopes | 1.7 capture, runs 5–6 | ☐ open **by decision, not by omission**. Revisit only if the two-cache divergence is judged acceptable; it needs a provider-specific carve-out in `engine/src/approvals.rs`, the one function 1.6 built to be provider-agnostic |
| D21 | `cancel` — Codex's "deny **and** interrupt the turn" decision — has no button. 1.5's decision triple maps Deny to `decline`, which lets the agent continue; nothing expresses the interrupting variant except Stop, a separate gesture the user has to know to reach for | 1.7 capture + generated schema | ☐ open, **unowned**. Untested live. Candidate owner is 1.9 |
| D22 | `item/permissions/requestApproval` — a third Codex approval method (filesystem/network permission overlays) that fits no `ApprovalRequest` variant. 1.7 answers it as `Unknown` written **blind**: no capture run produced one | 1.7 capture / `ServerRequest` schema | ☐ open. Capture the first live one and revisit the mapping; until then the card says "Grant Codex additional permissions" and the grant is un-allowlistable |
| D23 | `availableDecisions` arrives on every command approval, is **absent from the generated schema**, and lists only the decisions the CLI's own picker would show (`accept`, `acceptWithExecpolicyAmendment`, `cancel`) — never `decline`, which nonetheless works. Comet ignores the field | 1.7 capture, runs 1/3/4 | ☐ open, low priority. It becomes a signal worth reading if a future CLI starts *enforcing* it, at which point Deny would silently stop working with no error |
| [D25](D25-composer-picker-rework.md) | **The chat/composer surface and the picker menu need a rework, not another feature bolted on.** Every permission, approval and traits control added in Phase 1 landed inside a composer and a merged model+traits popover designed for neither | 1.8 rendered check, user ruling | ☐ open, **accepted deliberately**. Not a blocker for 1.8 or for Phase 1; the next slice that wants to add another control to either surface should stop and cost the rework |

## Closed

| # | Item | Origin | State |
| --- | --- | --- | --- |
| [D1](closed.md) | Empty `ReasoningDelta` while parked flips the session to Working and permanently disarms the 30-min idle reaper | 0b.1 whole-branch review | ☑ merged [PR #27](https://github.com/matty/comet/pull/27) |
| [D2](closed.md) | "Write the `None` case yourself" — optional wire fields need their absent case written by hand; a plan's fixtures will not generate it | 0b.1 Greptile P1 | ☑ merged [PR #28](https://github.com/matty/comet/pull/28) → `.agents/rules/optional-wire-fields.md` |
| [D5](D05-hung-tool-call.md) | A Codex tool call can hang **forever** with no timeout and no recovery — observed live, 12+ minutes on one exec call, session pinned Working | 0b.2 real-CLI check | ☑ merged [PR #36](https://github.com/matty/comet/pull/36) (`5a8a16e`). **Read the page's "what closed means" note before treating it as gone** |
| [D6](closed.md) | Claude's unclaimed `control_request` subtypes are counted but never answered; `sdk.d.ts` says hosts must reply `{behavior:"cancelled"}` to unrecognized dialog kinds | 0b.2 sink 3 | ☑ closed by 1.6 ([PR #37](https://github.com/matty/comet/pull/37), `61fec51`). Written **blind** — nine capture runs produced zero non-`can_use_tool` control requests. The diagnostic was kept: answering a request does not make it understood |
| [D11](D11-remembered-request-mode.md) | A run's remembered `last_request` carries the **previous** turn's runtime mode; three paths preferred it over the chat row, so a mid-chat mode change would not reach a steered or resumed run | 1.1 whole-branch review | ☑ closed by 1.8 ([PR #39](https://github.com/matty/comet/pull/39)). `DocHost::apply_chat_row_runtime_mode` overlays the row's current mode on all three paths and re-derives the sandbox from it. **The ruling the page records: a mode change applies to the next dispatch, never to a run in flight** — the provider process was spawned with its permission mode |
| [D12](D12-permission-axis-version.md) | The permission axis crosses RPC with no version signal: two same-`PROTOCOL_VERSION` builds can pair, and the older one silently runs a request under the default mode | 1.2 Greptile review, premise verified | ☑ closed by 1.8 ([PR #39](https://github.com/matty/comet/pull/39)). `PROTOCOL_VERSION` is `4`, so a mixed pair is refused outright rather than running a session under a mode nobody chose. Note this is **not** the usual decode-driven bump: the field was always additive, but its absence is indistinguishable from a deliberate value |
| D24 | **The deny note never reaches Codex, and the field said it would.** Comet persists it, but Codex's decision literals have no message field, so the wire carries `"decline"` and nothing else | 1.7 live check, scenario 3 | ☑ closed by 1.8 ([PR #39](https://github.com/matty/comet/pull/39)). `HarnessCapabilities::carries_deny_note` (conservative default `false`) drives the field's label, so a provider that cannot deliver the note no longer has a field promising it. **The note is still undeliverable on Codex — nothing invented a channel.** D14's residual is the mirror image and stays open |
