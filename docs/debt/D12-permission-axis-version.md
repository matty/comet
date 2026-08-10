# D12 — the permission axis crosses RPC with no version signal

`PROTOCOL_VERSION` is still `1`. Slice 1.1 added `runtimeMode` and 1.2 removed
`autoApprove`, both additively, so a pre-1.1 daemon and a post-1.2 client both
advertise `1` and pair successfully. The older peer ignores the unknown
`runtimeMode` key and defaults the absent `autoApprove` to `false`, so the run
executes under whatever that build's default was — not the mode the caller asked
for.

**Inert today, on two independent grounds**, which is why 1.2 did not bump:

- Nothing user-reachable can produce a non-default mode. `DraftConfig.runtime_mode`
  has no writer (`crates/ui/src/pickers.rs:67-69`) and `apply_owned_fields`
  preserves the row's existing mode (`:183`).
- The direction of the loss is a restriction, never an escalation. On the older
  peer Claude runs `--permission-mode default` instead of `bypassPermissions`, and
  Codex is unaffected either way because `approvalPolicy` is a pinned literal
  `"never"` on both `thread/start` and `turn/start`
  (`crates/harness/src/codex/mod.rs:434,448,534`), so no approval request ever
  arrives for the accept-outright branch to answer.

**Owner: 1.8.** The moment a user can select a mode, a mixed same-version pair
becomes a silent permission downgrade on a remote run — the user picks
`approval-required`, the older host runs the default and writes. A version bump is
the right instrument there: it refuses the pairing outright
(`RemoteConnectionState::IncompatibleVersion`) instead of running the session under
a mode nobody chose.

Note the asymmetry with the usual bump rule in `PROGRESS.md` ("a new field stays
additive"). That rule is about *decodability*. This is about a field whose absence
is indistinguishable from a deliberate value — the same trap
`.agents/rules/optional-wire-fields.md` names, one layer up.
