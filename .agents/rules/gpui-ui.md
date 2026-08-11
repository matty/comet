# Writing UI code in `crates/ui`

`crates/ui` is the gpui viewport: shell, sidebar, transcript, composer, terminal, diff pane.

Start by reading the module doc comment of the file you're changing. In this crate those
headers are not decoration — several of them record a bug that was expensive to find and the
structural reason the current shape prevents it. `theme.rs`, `frost.rs`, `motion.rs`,
`edge_fade.rs`, and `attachments.rs` are the dense ones.

## Numbers are layout, colors are paint

Layout constants live in `theme.rs` as plain numbers — `SPACE_XS/SM/MD/LG`, `PANEL_RADIUS`,
`CONTROL_RADIUS`, `BUBBLE_RADIUS`, `HEADER_HEIGHT`, `TITLEBAR_HEIGHT`,
`STATUS_STRIP_HEIGHT`. None of them may vary by appearance. The reason is that layout runs
once and paint runs per-appearance: a size that depends on the palette means switching
light/dark reflows the window, which is both visibly wrong and a whole class of bugs that
only reproduce after a theme toggle. If you find yourself wanting a different padding in
light mode, the real problem is usually a contrast issue you should fix in the color.

Colors come from `Theme::of(cx)` or the free helpers — `ink(alpha)` for fills,
`hairline(alpha)` for 1px edges, plus `wash`, `neutral`, `scrim`, `band`,
`glass_selected_bg`, `card_selected_bg`. A new color belongs in `theme.rs`, derived from the
oklch neutral scale via `oklch()` or `grey()`, because that's the only place both appearances
are defined together and the contrast test can see it.

Two literals in the crate are deliberate and not precedent: the brand orange in `icons.rs`
(a fixed logo color, not a theme tone) and the ANSI conversion in `terminal/view.rs` (the
palette belongs to the terminal protocol, not to us). Anything else that hardcodes a color is
a bug.

## Light is designed, not inverted

Don't add a light value by mirroring lightness — `theme.rs`'s header walks through why that
produces the washed-out look, and it's worth reading before you touch a token. The short
version, and the three traps in order of how often they bite:

1. **Surface order flips meaning.** Dark raises a surface by getting lighter; light chrome
   recedes by getting *darker*, and the content panel is the white one.
2. **Elevation reverses.** A faint white wash reads as "raised" on dark. Its literal
   translation — a faint black wash on white — reads as *recessed*; the composer looked like
   a dent. Light lifts with white plus a border and a shadow.
3. **Accents must move down the scale.** The dark palette's 400-level accents are picked for
   contrast against near-black and land at 2–4:1 on white, failing WCAG AA. Light uses the
   600-level sibling at the same hue to restore the ratio the dark token had.

Fill alphas carry over unchanged (`INK_FILL_SCALE`); only hairlines scale
(`INK_HAIRLINE_SCALE`) so a 1px edge survives a bright surround. Text tokens are paired so
each light token lands within ~0.5 of its dark counterpart's contrast ratio — enforced by
`text_contrast_is_paired_across_appearances` in `theme.rs`'s test module. Add new text tokens
to that test rather than eyeballing them.

## Anything that caches paint must key on `theme_generation()`

`theme_generation()` bumps on every appearance change. Caches that hold shaped text or
resolved colors across frames have to notice — `markdown::render::RenderCache` stores the
generation it was shaped under and `sync_palette` drops everything when it moves. A cache
that skips this looks correct until someone toggles the theme, then paints the old palette
until the content changes. If you add cross-frame caching, key it the same way.
`COMET_NO_RENDER_CACHE=1` bypasses the transcript's flatten cache to isolate exactly this.

## Fork-only primitives

These exist only in the pinned `wingleeio/zed` gpui rev, which is why the rev can't move to
the crates.io release:

- `Window::with_edge_fade` — per-primitive scroll-edge fades (`edge_fade.rs`).
- `Window::paint_backdrop_blur` — behind `frosted()` in `frost.rs`.
- `ImageSource::evict` — frees sprite-atlas tiles. `remove_asset` alone leaks them; see
  `attachments.rs` and `flush_evicted`.

Popovers and dialogs go through `frosted()` so the entire card subtree paints inside one
scene layer. This isn't stylistic: with per-primitive bounds-tree ordering, a hover repaint
somewhere else could reassign the card's quads *below* the blur, which intermittently
snapshotted and blurred away washes, dividers, and borders. Inside one layer the order is
structural — blur, shadow, tint, border, rows, text. `popover.rs` and `rail.rs` call it with
`frosted(12.0, 16.0, card)`; match the corner radius to the card's rounding.

## Motion

Use the `motion.rs` helpers instead of hand-rolling animations; its header carries the full
catalog with exact durations and easings (`fade-in`, `menu-in`, `dialog-in`, `comet-pulse`,
and the rest), and CSS `cubic-bezier()` is evaluated exactly by `CubicBezier`. Reduced motion
is honored automatically by every `with_animation` element, so you get it for free by using
them.

gpui has no `div` scale transform at the pinned rev — only `svg` transformations — so
`menu-in`/`dialog-in` approximate their scale component with fade plus translate. That's a
known limitation, not an oversight; don't "fix" it by bumping the rev. translateY is a
relative-position `top` inset, which taffy applies after layout, so siblings never move.

## Invalidation that stops one layer short

A handler that resets state and then reads something derived from it gets the *post*-reset
answer. The picker's Retry row did exactly this: it set `harnesses` to `Loadable::Idle` and
then called `effective_harness`, whose last fallback reads the first row of that list. On the
first-run path — no explicit pick, no chat config, nothing remembered — it answered `None`, the
forced refetch never ran, and the reload behind it asked without `force` and got the cached
failure straight back. Retry looked like it worked and changed nothing. **Read derived values
before you invalidate what they derive from.**

The same shape has a second half: an invalidation is only as deep as its last layer. Clearing a
`Loadable` slot re-arms the UI, not the engine, and a `force` flag that reaches a harness whose
own cache ignores it clears nothing at all. When something is cached at more than one level,
follow the invalidation to whoever actually holds the value and prove it there — a test at the
top layer passes while the bottom one keeps serving the stale answer.

## Driving the UI while you work

`COMET_HARNESS=mock` with the `COMET_MOCK_*` variables runs the UI without a real agent CLI.
`COMET_OPEN_ROUTE`, `COMET_OPEN_DIALOG`, and `COMET_OPEN_PICKER` jump straight to a surface
instead of clicking there. `COMET_FRAME_STATS=1`, `COMET_SCROLL_TRACE=1`, and
`COMET_MOTION_SCALE` cover timing and animation; `COMET_GPU_STATS=1` is implemented in the
gpui fork rather than this repo and prints renderer memory including device memory.
