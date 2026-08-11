# D45 — provider lifecycle faults are scripted one at a time

The fakes cover ordinary completion, protocol interruption and a process that
wedges after a turn starts. They cannot currently express most failures around
that path:

- initialize, thread setup or turn start never answers;
- the child exits after writing a useful stderr tail;
- stdout closes halfway through a frame;
- stdin breaks while Comet writes a decision or steer;
- a response is duplicated, delayed until after cancellation or arrives among
  unrelated notifications;
- a provider emits a tool call and keeps the stream alive without a result.

D5 added a mock-harness reproducer for the user-visible hung-tool state. It did
not give either provider subprocess fake a way to reproduce the wire and process
conditions that lead to it.

**Why this is debt.** Each new lifecycle regression currently needs another
custom scenario or binary, and the suite has no common invariant saying that all
failure modes settle within a bound, emit one terminal outcome, resolve pending
interaction and reap the child.

Fix shape: give the native fakes reusable fault actions such as `delay`, `hang`,
`close_stdout`, `close_stdin`, `stderr_then_exit`, `partial_line`, `duplicate` and
`late_reply`. Keep provider-specific state machines, but share the process and
JSONL mechanics. Add one focused test per fault and falsify each independently;
a large combinatorial matrix can wait for D48.
