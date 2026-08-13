# D65 — plan mode is not a fifth `RuntimeMode`

**Status:** open, unowned. A slice, not a corner of one.

## The question this answers

Phase 4's remainder spec asked slice 4.3 to decide whether `plan` becomes a fifth
`RuntimeMode`. It does not, and this page records why so nobody re-derives it from the fact that
the CLI accepts the flag.

`--permission-mode plan` **is** real. Slice 1.2 mapped four modes (`default`, `acceptEdits`,
`auto`, `bypassPermissions`) and left `plan` out, and the capture rig drove it successfully.
Adding a fifth enum value would take minutes.

It would also be wrong, because the mode is not the feature.

## What plan mode actually needs, from the capture

Recorded 2026-08-13, Claude Code 2.1.229 —
`captures/2026-08-13-plan-todo-subagent.md` §3, run 3:

1. **`requires_user_interaction` decoded off `can_use_tool`.** It is on the wire and Comet reads
   it nowhere. It is the field separating "may I run this command?" from "here is a plan, answer
   it", and Comet's approval surface (1.5–1.7) treats every `can_use_tool` alike.

2. **`ExitPlanMode` intercepted by tool name.** Its `input.plan` is the entire plan as markdown.
   Today it renders as a generic permission card with a full markdown document stuffed into it.

3. **A real answer channel for `AskUserQuestion`.** This is the decisive one. The rig replied
   `{"behavior":"allow"}` — the textbook approval response — and the tool_result came back:

   > *"The user did not answer the questions."*

   **Allowing an interaction tool is not answering it.** An approve/deny surface is structurally
   the wrong control, so this cannot be delivered by extending the approval cards.

Two smaller findings from the same run: the model writes its plan to
`~/.claude/plans/plan-<slug>.md` with **no `can_use_tool` request at all** (plan mode permits
writes under that directory silently), and only the later write into the working directory was
gated.

## Why the two-axis split is right

The parent design already anticipated this with a second axis,
`ProviderInteractionMode = default | plan`, orthogonal to the permission axis. Phase 1 never
built it.

t3code, independently, implements plan mode as its own channel twice over: `ExitPlanMode` is
intercepted by tool name and emitted as `turn.proposed.completed { planMarkdown }` rather than
as an approval (`ClaudeAdapter.ts:2909`), and `AskUserQuestion` routes to a dedicated
`user-input.requested` / `user-input.resolved` pair (`:3914`) whose comment reads *"plan mode
relies on this heavily"*.

Two implementations reaching the same shape, and the wire evidence above, all say the same
thing: plan mode is an interaction channel that happens to be entered by a permission flag.

## Asymmetry, stated up front

**Codex has no plan mode.** Whatever this becomes is Claude-only, the same shape as 3.2's
Update-button asymmetry and 4.2's subagent events. A later slice must not tidy it into a fake
symmetry.

## Related

- **D14** — the deny note vanishing from the UI; same surface, different gap.
- **D68** — the checklist's own scoping decision, taken in the same slice.
