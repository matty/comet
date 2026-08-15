# D73 — History diffs do not have a pinned source revision

The syntax-highlighting sync intentionally deferred upstream commit
`d903bd7332b822d4ad1c76313af96dca017e398c` (`feat(ui): highlight history
diffs`). Comet currently highlights working-tree diffs in the Changes surface;
there is no local history-diff surface to which that commit can be adapted.

This is a missing prerequisite, not a decision that history diffs should remain
plain. Upstream's implementation assumes a selected, pinned commit and a
history diff model. Porting only its paint changes would either create a dead
path or invent that product surface inside an upstream-sync slice.

## Closure condition

After Comet gains a history graph with a selected/pinned revision and a history
diff view, run the upstream sync helper for `d903bd7` and adapt its syntax
highlighting to that local surface. Close this item in the same change.
