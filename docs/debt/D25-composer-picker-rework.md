# D25 — the composer and picker surfaces need a rework

**The ruling, 2026-08-10, during 1.8's rendered check: this is known, accepted,
and explicitly not 1.8's job.** Recorded so the next slice inherits the decision
rather than rediscovering the symptom and quietly working around it again.

What has accumulated on two surfaces that were not designed for it:

- **The composer's shared input serves four roles** — prompt, wizard answer,
  deny note, and now a note whose label changes per provider — with draft
  parking (`park_draft_for_note`) as the mechanism that keeps them from
  destroying each other. That is a workaround holding, not a design.
- **The composer is also the approval surface**: 1.5's decision row *replaces*
  the composer, which is what makes the send block real, and the status strip
  above it carries the blocked-turn line. Three concerns in one 60px band.
- **The picker is one merged model+traits popover** that now also owns the
  permission axis. 1.8's Permissions chips wrap to a second line at the current
  width, and the section's height changes with the active mode's caption — both
  correct per the layout-vs-paint rule, both a sign the container is full.
- **Two of 1.5's rendered-check findings were "the same thing is on screen
  twice" and "two unrelated states paint identically"** — 0.2a found the same
  pair. A surface producing that class of finding every time it grows is
  telling you something about the surface.

**Why it is not fixed here.** A redesign is a piece of work with its own spec,
its own rendered checks and its own review; folding it into a slice whose job
was to make an existing surface reachable would have meant shipping a redesign
nobody specified. The controls all work — this is about how they read together.

**How to apply.** Nothing to do on the current plan. The trigger is the *next*
slice that wants to add another control to the composer or the picker (2.3's
image-modality gate and 2.4's slash-command menu are both candidates): cost the
rework at that point rather than finding a fifth place to put something.
