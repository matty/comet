# Seeing a UI state without producing it for real

Reaching a surface by driving the app is slow, and the states worth reviewing are the slowest of
all: a nearly-full context window, a failing update, a resumed plan, an agent that errored. This
page is the standing answer to that problem, and the register of what is available.

**The mechanism is the `COMET_MOCK_*` family**, read by the mock harness
(`crates/harness/src/mock.rs`) and selected with `COMET_HARNESS=mock`. That choice is recorded in
`docs/debt/README.md` as D52's closure — option 3 on that row's page, taken deliberately over a
gallery route.

**Why these and not a gallery.** The knobs are *data-side*: they script provider events, which
then run through the real normalizer, the real fold, the real persisted document and the real
components. Two of D52's three constraints hold by construction — a preview built this way cannot
drift from what the app renders, and it cannot mask real data, because it only ever supplies
events where a real run would have supplied them. A gallery that constructs its own props
satisfies neither for free.

**What they are not.** A knob scripts a *provider*. Anything the provider is not the source of —
a steer boundary, an engine sweep, a device going offline — is out of reach here, and no value of
any variable below changes that. Where a state has that shape it is called out in the table.

## Running the app against them

```powershell
$env:COMET_HARNESS = "mock"
$env:COMET_MOCK_SUBAGENT = "1"
$env:COMET_MOCK_DELAY_MS = "1500"    # pace it slowly enough to watch
.\target\debug\comet.exe
```

Then start a **new** session and pick **Mock** in the agent picker. The harness is fixed per
session at creation, so an existing chat cannot be switched to the mock — the picker greys it out
and says so. The model picker defaults to the first row of the catalog, which is Fable on a real
harness: change it before sending (`AGENTS.md`, "Never test with Fable or Opus").

## The register

Every variable is off unless set, and treats `""` and `"0"` as off. Each one's own comment in
`mock.rs` is the detailed record — which capture it was shaped from, and which hard states it
exists to produce. Keep both in step.

| Variable | Value | Puts on screen |
| --- | --- | --- |
| `COMET_MOCK_SUBAGENT` | `1` | Four subagent cards: one completing with a multi-paragraph summary and all three counters, one failing with none, one cancelled, one still running. Also emits the parent's contentless `Agent` chip, so the card's suppression of it is exercised |
| `COMET_MOCK_CHECKLIST` | `1` | A plan card mid-flight: a completed step, a step stuck `InProgress`, and a step with no subject at all — the shape a resumed Claude run produces |
| `COMET_MOCK_APPROVAL` | `command`, `file-change`, `file-read`, `mcp`, `unknown`, `1` | The approval card in each of its request shapes, blocking mid-run. `1` is the file-change run 1.4 shipped; an unrecognized value falls back to it rather than disabling the knob |
| `COMET_MOCK_QUESTION` | `1` | The input chip and the composer's question panel |
| `COMET_MOCK_HANG` | `1` | A tool call that never resolves — the hung-tool line in the status strip above the composer |
| `COMET_MOCK_ERROR` | `1` | A scripted error part |
| `COMET_MOCK_CONTEXT` | `<percent>` | A context reading at that fill, for the composer's context disc |
| `COMET_MOCK_TABLE` | `1` | GFM tables in the transcript |
| `COMET_MOCK_CODE` | `1` | Rust and TypeScript code blocks, plus a multiline `Run` chip |
| `COMET_MOCK_MEND` | `1` | Link- and list-heavy prose whose half-streamed markers exercise the display mend |
| `COMET_MOCK_REPEAT` | `N` | Loops the script body, for a long transcript |
| `COMET_MOCK_DELAY_MS` | `N` | Paces the script. Raise it to watch a state arrive; `0` runs the whole script instantly |
| `COMET_MOCK_CHARS` | `N` | Re-chunks every text delta to N characters, so the pacing above applies to *characters* and delta boundaries land inside inline markers |

Knobs compose: `COMET_MOCK_SUBAGENT=1 COMET_MOCK_CHECKLIST=1` puts both new cards in one run.

## States no knob reaches

Recorded so the next person does not spend a session hunting for a value that does not exist.

**A subagent's `last seen running` card.** It needs a genuine steer boundary — the state exists
precisely because Comet never learns the outcome (`docs/debt/D57`). A steer becomes that boundary
only when the harness emits `AgentEvent::Steered`, which only the Claude and Codex adapters do.
The mock advertises `supports_steering: true` and then never drains `controls.steering`, so a
steer sent to it is dropped and the run simply ends; the engine's `Done` sweep then stamps the
agent `Cancelled` like any other unfinished one. Pinned instead by
`a_running_subagent_in_a_finished_entry_reads_last_seen_running` in `crates/ui/src/transcript.rs`.
Seeing it needs a real Claude run that delegates, steered mid-flight.

**A genuinely shorter resumed plan.** `COMET_MOCK_CHECKLIST` reproduces the *rows* a resumed run
produces, including the subject-less one, but a second message re-runs the same script rather than
resuming a server-held list, so both cards hold the same steps. Judging whether a short card reads
as a bug — D68's open question — still needs a real multi-run Claude chat.

## Other dev knobs

Not part of this family and not provider data, listed so the whole surface is in one place:
`COMET_OPEN_ROUTE`, `COMET_OPEN_DIALOG`, `COMET_OPEN_PICKER` jump straight to a surface;
`COMET_FORCE_GATE` renders the startup gate; `COMET_CONTEXT_DEMO` supplies a context reading
UI-side where there is no live one; `COMET_FRAME_STATS`, `COMET_SCROLL_TRACE`, `COMET_MOTION_SCALE`
and `COMET_NO_RENDER_CACHE` cover timing, animation and the transcript's flatten cache.
`.agents/rules/gpui-ui.md` carries the rendering-side detail.

## Adding one

1. Put it in `mock.rs` beside its siblings, gated on the same "unset, empty or `0` is off" test.
2. Its comment says **which recorded capture it was shaped from** and **which hard state it exists
   to produce** — a fixture whose every item completes cleanly never renders the state the surface
   was built for. `COMET_MOCK_CHECKLIST`'s comment is the worked example.
3. Add a row above.
4. `every_mock_knob_is_documented` in `mock.rs` pins steps 1 and 3 against each other, in both
   directions — an undocumented knob fails it, and so does a row for a knob that no longer exists.
   It is a source-text pin, so it cannot tell you the knob still *emits* anything; that is the
   surface's own row-level tests.
