# D47 — scripted runs have no live drift canary

The ignored real-CLI tests exercise token-free model and command discovery. No
live turn checks the run protocols. Approval requests, steering, notices, item
lifecycles, interruption, errors and terminal mapping therefore depend on
scripted fakes plus historical captures.

**Why this is debt.** The fakes preserve the protocol shape known when they were
written. They cannot report that a provider renamed a method, moved a field or
changed event ordering until a user reaches that path. D8–D10 describe how some
of that drift is also easy to diagnose poorly once it arrives.

This should not become an ordinary PR test. It needs installed CLIs,
authentication, network access and provider capacity; exact prose and timing are
outside Comet's control.

Fix shape: provide an explicit local or scheduled canary that runs one minimal
turn per provider, records a sanitized transcript and asserts broad invariants:
startup, one content event, no unknown/unparseable frames, and one terminal
outcome. Discovery can remain the token-free first stage. Any Claude turn must
select Haiku or Sonnet, never Fable or Opus. A canary failure reports possible
protocol drift for review; it does not automatically rewrite the fake corpus.
