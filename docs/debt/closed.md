# Closed debt

Kept for the reasoning, not the status. Each of these merged;
`README.md` carries the row and the PR. Delete an entry only if its
explanation stops being useful to someone reading the code it touched.

## D1 — empty `ReasoningDelta` while parked

**The bug.** `sessions.rs`'s run loop treats the first event after parking as the
next turn beginning:

```rust
if !parked_notice && idle_since.take().is_some() {
    inner.set_status(&chat_id, SessionStatus::Working, true);
}
// ... 7 lines later
if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
    continue;
}
```

An empty reasoning delta is a **pure heartbeat** — redacted thinking and
tool-input-generation windows stream them with no text, hundreds per long turn —
and persistent sessions emit them between turns too. So the heartbeat clears
`idle_since` and flips the status to Working, then `continue`s without folding
anything. No `Done` is coming. The session sits Working forever, and because the
reaper's `select!` arm is gated on `idle_since.is_some()`, the child is never
released.

**Why it is the same bug 0b.1 fixed.** 0b.1 found this exact wedge for
`AgentEvent::Notice` and fixed it with the `parked_notice` guard. It recorded the
`ReasoningDelta` instance as pre-existing on `main` and out of that slice's
scope. Same one-line class of guard covers it.

**The fix.** Move the empty-heartbeat `continue` *above* the turn-start block, so
a heartbeat is dropped before it can transition any state. `touch_session` still
runs above both, which is the only thing a heartbeat is legitimately for. A real
turn's first non-empty event still flips the status.

## D2 — optional wire fields

**The lesson.** 0b.1's only escaped P1 was keyless notice collapse: two emitters
passed an `Option` straight through from the wire, the fold two files away
compared them, and an absent key was read as "these are the same thing" instead
of "no evidence either way". Two unrelated CLI messages merged into one chip in a
persisted, LAN-replayed transcript.

**Why it escaped.** Three per-task reviews and a whole-branch review all missed
it, because **every test and fixture in the slice supplied a key**. The plan's
fixtures were written from the happy path, so the `None` case was never
constructed anywhere in the slice.

**Where it lands.** `.agents/rules/` in the repo, so it binds every agent and not
just the next session — 0b.2 and Phase 1 both add optional wire fields.

## D6 — unclaimed control requests are counted but unanswered

0b.2's sink 3 records a diagnostic for every inbound Claude `control_request`
whose subtype is not `can_use_tool`, and deliberately does not reply. `sdk.d.ts`
documents that hosts must answer unrecognized `request_user_dialog` kinds with
`{behavior:"cancelled"}`. Replying is a behaviour change to a frame the real-CLI
capture never saw fire (zero `control_request`s even with
`--permission-prompt-tool stdio`), so its frequency is unknown rather than zero,
and it was out of 0b.2's scope. The count sits on the return path and never
delays an answer, so adding the reply later is additive.

## D60 — the scenario name lived in three unsynchronized places

**The bug.** `comet-provider-capture`'s `--help` text, its `supported_pair()`
argument check, and its scenario dispatch `match` each spelled out the same set
of `(provider, name)` pairs by hand, with nothing tying the three together.
Adding `checklist` to two of the three (the help text and the dispatch) but not
the third produced a binary whose `--help` advertised the scenario and whose
argument validation refused it — *"Scenario \"checklist\" is not supported for
claude. Run with --help to see the valid pairs."* — pointing the operator back
at the text that had just told them otherwise.

**The fix.** The provider-capture-simplification design (stage 4,
"neutral-recorder") replaced all three copies with one:
`crates/harness/src/capture/record/scenarios.rs`'s `SCENARIOS: &[ScenarioSpec]`
declares each scenario's name, purpose, provider, runtime mode and argument
requirements exactly once. `comet-provider-capture.rs` generates its `--help`
text and validates arguments by looking a `(provider, name)` pair up in the
table (`scenario()`), and `record()` dispatches off the same table by the same
lookup. There is no second copy left to drift, and `scenarios.rs`'s own test
`every_scenario_name_the_binary_advertises_is_in_the_table` pins the row count
so an added or removed scenario cannot pass silently.

## D61 — the checklist evidence guard encoded a prompt's content, from a different file

**The bug.** `recording.rs`'s Claude run loop required `created.len() >= 2 &&
!updated.is_empty()` to accept a `checklist` capture — two distinct confirmed
creates plus at least one confirmed update — and a non-empty
`updated.difference(&created)` to accept a `checklist-resume` capture, i.e. at
least one update to a task the process had never created. Those thresholds were
correct only because `capture/checklist.rs`'s prompt happened to ask the model
for exactly that much — two facts in two files, agreeing today, with nothing
enforcing that they keep agreeing.

**And the row that filed this said "4 and 2".** Those were the *prompt's*
tool-call counts, not the predicate's, and they were quoted onward into a plan
and a doc comment before anyone read the deleted code. Quoting a threshold from
a prompt rather than from the code is the same mistake one level up, which is
why the predicate is written out above rather than summarized. Worse, per the design's own safety principle: a model that ignored
the instructions and did nothing produced *a recording of a model ignoring
instructions*, which is real evidence of real CLI behavior, and the guard threw
that recording away rather than keep it — reacting to a frame's content and
aborting, not driving.

**The fix.** The neutral-recorder stage deletes the guard along with the rest
of `recording.rs`, per design §3.2 ("delete every frame check that aborts").
The ported `checklist`/`checklist-resume` scenario bodies
(`capture/record/scenarios/claude.rs`) record whatever the model actually does,
with no mutation count enforced. `fake_claude.rs`'s `checklist_no_tasks`
fixture is the case the guard used to bail on — a model that replies with plain
text and calls neither `TaskCreate` nor `TaskUpdate` — and the neutral recorder
now returns a successful capture holding every frame, evidence intact.

## D79 — the Codex pre-spawn fence was derived from `runtime_mode`, not declared

**The bug.** `record.rs`'s `codex_fence` ran unconditionally for every Codex
row and picked its trusted-PowerShell/cwd-identity branch by testing
`spec.runtime_mode == Some(RuntimeMode::ApprovalRequired)`. That is a coincidence
standing in for a decision: the `approval` row happens to be the only row that
both wants that fence and sets that mode, so the check worked, but nothing
tied the two together on purpose. A future Codex row that legitimately wanted
`ApprovalRequired` for some unrelated reason would have silently inherited the
fence anyway. The row that filed this named the right mitigating fact —
`resolve_trusted_powershell` fails closed on every platform but Windows, so the
wrong inheritance would read as a scenario refusing to run, never as an
unprotected spawn — and deferred the fix to "the next Codex row added to
`SCENARIOS`," on the reasoning that the derivation was otherwise harmless to
leave alone.

**Closed as an exception to that deferral, not by hitting the trigger.** The
`scenario-request-builders` plan's Task 4 closed it directly, in the same
change that moved `launch` and `request` onto the row for the identical
reason — deriving a spawn-time decision from a field that means something else
is the same defect class the whole plan exists to remove, in the same file,
so the plan's own "Decisions already made" section overrode the row's
"convert it when the trigger fires" note on purpose. No new row was added to
`SCENARIOS` when this landed.

**The fix.** `ScenarioSpec` gained a `fence: fn(&ScenarioSpec, &CaptureConfig,
&LaunchDescriptor) -> anyhow::Result<FenceOutcome>` field
(`capture/record/scenarios.rs`). `no_fence` — a plain `Ok(FenceOutcome::none())`
— is the default for every Claude row and every Codex row except two;
`approval` and `approval-on-request` name `codex_fence` directly. `record.rs`
no longer branches on `runtime_mode` at all: `codex_fence` still picks between
its two fences by `spec.requirements.needs_approval_target`, but reaching the
function in the first place is now the row's own explicit choice, not
something inferred from a field that means "this scenario starts a turn under
approval-required," full stop.

`record_claude`/`record_codex` collapsed into one generic `record_provider`
(same change) — with `launch`, `request` and `fence` all on the row, the two
functions differed only in which executable-resolver and `run_launch` fn to
pass, both plain per-provider function pointers. They were passed in as
parameters rather than added to `CaptureProvider` as a fifth trait member:
that trait's own doc comment (`provider.rs`) says a fifth member is earned by
a third provider having a *recording* to design against, not added ahead of
one, and neither value varies per scenario the way `launch`/`fence` do (the
reason those two live on the row rather than the trait).

**Residual, found by this task's falsification — closed.** Pointing a
*non-approval* Codex row (`steer`) at `codex_fence` and running the full
`comet-harness` suite (500 tests) found nothing that fails at the time this
task landed. The reason: every test that reached `spec.fence` at all did so
either by calling `record()` end-to-end for a scenario that already names the
right fence (`approval`, `approval-on-request`, and the no-fence rows), or by
hand-building a `Session` directly with `FenceOutcome::none()` and never
touching `spec.fence` in the first place (`start_codex_run_session`,
`record/scenarios/codex.rs`). No test iterated `SCENARIOS` checking each
row's `fence` against an expected table the way the run-builder purity/wiring
loop (then `every_run_rows_request_builder_is_pure_and_derives_its_own_launch`)
did for `launch`. So the hazard this closed — *derivation* from an unrelated
field — was gone, but *declaring the wrong function* on a row was still
caught by nobody.

`scenario-request-builders`'s fix pass (2026-08-16) closed it: a fourth loop,
`every_row_s_fence_matches_the_kind_its_name_declares` (`record.rs`), checked
every `SCENARIOS` row's `fence` against an exhaustive `(Provider, name,
expected kind)` table — the `EXPECTED_FENCES`-style table this entry named as
the future fix. It did not compare `spec.fence` by function-pointer identity
(`std::ptr::fn_addr_eq` is not reliable across codegen units); it fingerprinted
by observable behavior instead — `codex_fence`'s very first statement in both
of its branches reads `launch.cwd`, so calling a row's fence with a `cwd:
None` launch reliably tells `codex_fence` (errors, naming the missing cwd)
apart from `no_fence` (always `Ok`), with no real filesystem state needed.
Falsified by pointing `steer` at `codex_fence` — the same probe that found
this residual — and confirming the loop failed naming the row, then
restoring.

A same-day review-fixes pass merged that fourth loop with the run-builder
purity/wiring loop it sat beside — one `EXPECTED_ROWS` table covering both
concerns, one coverage guard, in `every_row_s_builder_and_fence_match_its_declared_wiring`
(`record.rs`) — rather than leaving the two as separate full-roster tables.
Same fingerprinting mechanism, same falsification, one fewer enumeration.

