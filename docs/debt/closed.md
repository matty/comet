# Closed debt

Kept for the reasoning, not the status. Each of these merged;
`README.md` carries the row and the PR. Delete an entry only if its
explanation stops being useful to someone reading the code it touched.

## D36 — provider default reasoning

Codex's `model/list` reports `defaultReasoningEffort` per model. Comet now
preserves the recognised value as `Model.defaultReasoning` and uses it only
when the user has no draft, stored-chat, or sticky reasoning selection. An
absent or unrecognised value remains unknown and keeps the existing ladder
heuristic.

This optional field does not bump `PROTOCOL_VERSION`: a new peer reading an
old model list has no metadata and takes that same heuristic, while an old
peer ignores the new key and also takes the heuristic. The compatibility
ruling stays beside `PROTOCOL_VERSION` in `crates/proto/src/remote.rs`, the
source that owns it.

## D37 — live service-tier removal

Codex's `model/list` is authoritative for service-tier availability only when
it successfully returns the field for a matched model. An explicitly empty
`serviceTiers` list removes that model's curated `serviceTier` option; a
missing field, an unmatched curated model, or failed discovery keeps the
curated fallback. Other curated capabilities continue to win the generic
merge because the providers still under-report them.

The picker keeps a remembered service-tier choice in draft or chat state while
the option is unavailable, but filters it out of the effective run request.
That makes model switches and temporary catalog changes reversible without
sending a tier the live model rejected.

No `PROTOCOL_VERSION` bump is needed: the availability bit never enters the
wire model. It only changes the contents of the existing `Model.options` list,
whose entries are catalog data older peers already consume dynamically.

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

## D13 — linked-worktree sandbox escalation was invisible

**The bug.** Codex's compatibility workaround widens a `workspace-write`
request to `danger-full-access` for a linked worktree whose branch name contains
a slash. The escalation kept the session runnable, but only `tracing` recorded
it, so a user who selected workspace-only access could not see the wider access.

**The fix.** Normalization retains whether it widened the sandbox and the run
emits exactly one warning immediately after `SessionStarted`. The notice is
Comet-authored, avoids the local path and provider internals, explicitly says
the run can write anywhere on the machine outside the workspace, and tells the
user that naming the branch without a slash avoids the workaround. The original
runtime mode and approval policy remain untouched. A real linked-worktree
integration test proves the event order, exact-once behavior, widened sandbox,
and preserved policy. Its companion test keeps main checkouts, non-worktree
directories, plain branch names, and other requested sandboxes outside the
condition. Unreadable worktree metadata remains fail-closed in the predicate;
it is not claimed as portable permission-test coverage.

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
`crates/capture/src/record/scenarios.rs`'s `SCENARIOS: &[ScenarioSpec]`
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

## D80 — the three-row Claude/Codex model-discovery family was one observation, not three

**The row insisted on a real answer, not a reading of the table.** `claude
model-discovery` and `model-discovery-neutral-cwd` set the same
`runtime_mode` (`None`), the same `Requirements::discovery()`, the same
`launch` and the same `body` — the only difference anywhere in the table was
the `name` string and the `purpose` prose. D80 named this and explicitly
refused to let anyone collapse the rows from the code alone: cwd
*demonstrably* changes discovery output elsewhere (D32's 66-vs-63 commands),
so "the two rows look identical in the table" was not evidence the CLI
treats them identically. The row's mitigation was Stage 5's capability
sheet, which printed all three rows' byte-identical argv/cwd/env side by
side — visible as the same evidence recorded three times, but still not a
verdict, because a redaction placeholder cannot rule out a real difference
the archive no longer shows.

**Stage 6 ran the re-capture the row demanded.** 2026-08-16, Claude Code
2.1.233 (three versions newer than the corpus) and codex-cli 0.147.0, each
provider's three model-discovery rows run back to back, one process at a
time, from a disposable directory for the two `needs_cwd: false` rows and
from inside this repository for `-project-cwd`. Byte-comparing the Claude
`initialize` reply across all three runs found exactly one difference, at a
single offset: `"pid":5912` vs `"pid":7408` vs `"pid":17260` — a per-process
value the sanitizer already redacts. The Codex `model/list` reply was
byte-identical across all three, confirmed by direct comparison, not merely
by length.

**The mechanism, no longer an inference.** Both `-neutral-cwd`'s and
`model-discovery`'s `needs_cwd: false` makes the runner discard `--cwd` and
spawn from a neutral temp directory — confirmed empirically: a disposable
directory was passed to both and the recorded cwd was the same temp path for
both. `model-discovery-project-cwd` genuinely ran from inside the
repository (its recorded cwd proves it) and still returned identical bytes
apart from `pid`. The reason is `--bare`: `model-discovery`'s argv includes
it and `command-discovery`'s does not, and `--bare` is what skips the
cwd-scoped project/user skill discovery D32 found varying (66 vs 63
commands). D32's counter-evidence was always about `command-discovery`, a
different scenario with a different launch — it never applied to this
family, and the re-capture is what finally showed that instead of arguing it
from the code.

**The close.** `model-discovery-neutral-cwd` and `model-discovery-project-cwd`
are deleted from `SCENARIOS` for both providers — four rows across the two
provider tables — along with their committed corpus directories
(`crates/capture/tests/corpus/{claude/2.1.228,codex/0.147.0}/model-discovery-{neutral-cwd,project-cwd}/`).
Every reference to the deleted names (the CLI's own token-free-discovery
tests, `record.rs`'s `EXPECTED_ROWS` wiring table, the two doc comments that
explained the three-way split) is updated or removed, and the capability
sheets are regenerated (`$env:COMET_UPDATE_SHEETS = "1"; cargo test -p
comet-harness --test capture_corpus`) — `docs/providers/claude-2.1.228.md`
and `docs/providers/codex-0.147.0.md` each lose two Scenarios entries and
gain nothing, since the deleted rows added no field or vocabulary value the
surviving `model-discovery` row didn't already show.

**`model-discovery-logged-out` is untouched.** It varies by auth state, not
cwd, and the re-capture never touched it — it remains a genuinely distinct
observation.

## D86 — the sheet could not show tool-roster growth

**The gap.** The `system`/`init` frame's `tools` array is redacted
element-by-element (`.tools[]` names no line on `claude.txt`), so the Fields
section can only ever print the bare path `.tools`, never a count or a name
from it. But the array's *length* survives redaction untouched — 29 tools in
every `2.1.228` scenario, 35 in `2.1.229`'s `subagent`, 59 in `checklist` and
`checklist-resume` (the two scenarios captured with MCP servers connected) —
and nothing in the sheet rendered it. A real roster change (a new built-in
tool shipped, or the recording account gaining or losing an MCP connector)
was invisible to `git diff` between two sheets: the Fields section shows
`.tools` either way, with no way to tell 29 from 59 apart.

**The fix.** `SheetScenario` (`crates/capture/src/sheet.rs`) grew a
`tool_count: Option<usize>` field, rendered as one line in the Scenarios
section — `tools: 29`, or `tools: (not observed)` for a scenario whose
archive holds no `system`/`init` frame (every discovery-only scenario, and
every Codex scenario — Codex's corpus has no equivalent frame at all today).
`crates/capture/tests/capture_corpus/capability_sheets.rs`'s `scenarios_for`
sources it by reading the scenario's own `events.jsonl` directly and taking
the first `system`/`init` frame's `tools` array length — never a name from
inside it, and never through `observe_surface`, which records field names
only and has no notion of a value at all. Per scenario, not per version: the
count differs *within* `2.1.229` (35 vs 59) depending on what was connected
at capture time, so a single merged number would have hidden exactly the
change this exists to show.

Regenerating the three committed sheets after this change makes the growth
legible in the diff: `claude-2.1.228.md` reads `tools: 29` in every scenario
that has one; `claude-2.1.229.md` reads `tools: 35` for `subagent` and
`tools: 59` for `checklist`/`checklist-resume`.

## D85 — the sheet named a field but not which scenario produced it

**The gap, measured.** `claude/2.1.228` and `claude/2.1.229` share zero
scenario names — 2.1.228 holds `approval`, `attachment`, `command-discovery`,
`fresh-text`, three `model-discovery*` and `resume`; 2.1.229 holds
`checklist`, `checklist-resume` and `subagent`. So `git diff --no-index
docs/providers/claude-2.1.228.md docs/providers/claude-2.1.229.md` — the
whole version-change mechanism — showed ~200 insertions and ~200 deletions
that described *different scenarios*, not different CLI behaviour, and
roughly a third of each sheet was caveat prose whose entire subject was that
a reader could not tell those two cases apart. `FieldObservation::first_seen`
(`crates/capture/src/surface.rs`) computed a scenario and sequence
for exactly this triage and was read nowhere outside `surface_map.rs`'s own
pinning tests.

**Why `first_seen` alone wasn't the fix.** A single frame reference answers
"where do I start looking," but not the actual question a diff reader has: a
field seen in one narrow scenario and a field seen in every scenario this
version ran are a different fact, and only the second is strong evidence a
disappearance means something. Rendering just the first occurrence would have
kept the ambiguity the caveat prose was apologizing for.

**The fix.** `FieldObservation` grew `scenarios: BTreeSet<String>` — every
scenario (bare directory name) whose evidence produced that field, populated
in `Visit::record` on every visit, not just the first. `sheet.rs`'s Fields
section renders it as a **scenario-group index**: every distinct scenario set
among a version's fields gets one `G<n>` line (`- \`G1\`: approval,
attachment, fresh-text, resume`), and each field line carries only the tag
(`` - `.type` `G3` ``) — two fields sharing five scenarios collapse to one
group line instead of five names printed twice. A reader now tells "the CLI
dropped this field" from "no scenario here exercised it" by checking whether
the tag's scenario names appear in the *other* sheet's own Scenarios section,
without opening the corpus. The header's and Fields section's caveat
paragraphs were cut accordingly — the apology clause in the header ("check
the Scenarios section of both sheets... it did not go away just because this
sheet makes it visible") is gone, 91 words down to 25, since the Fields
section now carries the actionable version of that instruction itself; the
Vocabulary section is unchanged and keeps its own local caveat, because it
has no equivalent per-value attribution.

