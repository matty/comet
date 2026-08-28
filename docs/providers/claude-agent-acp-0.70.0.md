# claude-agent-acp 0.70.0

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### session-discovery-claude-acp

the same ACP surface through claude-agent-acp, for the two-speaker diff

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\hermes\node\node.exe
<HOME>\AppData\Roaming\npm\node_modules\@agentclientprotocol/claude-agent-acp\dist\index.js
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: session-discovery-claude-acp

### To provider

- `.id` `G1`
- `.jsonrpc` `G1`
- `.method` `G1`
- `.params` `G1`
- `.params.clientCapabilities` `G1`
- `.params.clientCapabilities.fs` `G1`
- `.params.clientCapabilities.fs.readTextFile` `G1`
- `.params.clientCapabilities.fs.writeTextFile` `G1`
- `.params.clientCapabilities.terminal` `G1`
- `.params.clientInfo` `G1`
- `.params.clientInfo.name` `G1`
- `.params.clientInfo.title` `G1`
- `.params.clientInfo.version` `G1`
- `.params.cwd` `G1`
- `.params.mcpServers` `G1`
- `.params.protocolVersion` `G1`

### From provider

- `.id` `G1`
- `.jsonrpc` `G1`
- `.result` `G1`
- `.result._meta` `G1`
- `.result._meta.goal` `G1`
- `.result._meta.goal.actions` `G1`
- `.result._meta.goal.controlMethod` `G1`
- `.result._meta.goal.version` `G1`
- `.result._meta.jetbrains` `G1`
- `.result._meta.jetbrains.air` `G1`
- `.result._meta.jetbrains.air.capabilities` `G1`
- `.result._meta.jetbrains.air.version` `G1`
- `.result._meta.steering` `G1`
- `.result._meta.steering.supported` `G1`
- `.result.agentCapabilities` `G1`
- `.result.agentCapabilities._meta` `G1`
- `.result.agentCapabilities._meta.claudeCode` `G1`
- `.result.agentCapabilities._meta.claudeCode.promptQueueing` `G1`
- `.result.agentCapabilities.auth` `G1`
- `.result.agentCapabilities.auth.logout` `G1`
- `.result.agentCapabilities.loadSession` `G1`
- `.result.agentCapabilities.mcpCapabilities` `G1`
- `.result.agentCapabilities.mcpCapabilities.http` `G1`
- `.result.agentCapabilities.mcpCapabilities.sse` `G1`
- `.result.agentCapabilities.promptCapabilities` `G1`
- `.result.agentCapabilities.promptCapabilities.embeddedContext` `G1`
- `.result.agentCapabilities.promptCapabilities.image` `G1`
- `.result.agentCapabilities.providers` `G1`
- `.result.agentCapabilities.sessionCapabilities` `G1`
- `.result.agentCapabilities.sessionCapabilities.additionalDirectories` `G1`
- `.result.agentCapabilities.sessionCapabilities.close` `G1`
- `.result.agentCapabilities.sessionCapabilities.delete` `G1`
- `.result.agentCapabilities.sessionCapabilities.fork` `G1`
- `.result.agentCapabilities.sessionCapabilities.list` `G1`
- `.result.agentCapabilities.sessionCapabilities.resume` `G1`
- `.result.agentInfo` `G1`
- `.result.agentInfo.name` `G1`
- `.result.agentInfo.title` `G1`
- `.result.agentInfo.version` `G1`
- `.result.authMethods` `G1`
- `.result.configOptions` `G1`
- `.result.configOptions[].category` `G1`
- `.result.configOptions[].currentValue` `G1`
- `.result.configOptions[].description` `G1`
- `.result.configOptions[].id` `G1`
- `.result.configOptions[].name` `G1`
- `.result.configOptions[].options` `G1`
- `.result.configOptions[].options[].description` `G1`
- `.result.configOptions[].options[].name` `G1`
- `.result.configOptions[].options[].value` `G1`
- `.result.configOptions[].type` `G1`
- `.result.modes` `G1`
- `.result.modes.availableModes` `G1`
- `.result.modes.availableModes[].description` `G1`
- `.result.modes.availableModes[].id` `G1`
- `.result.modes.availableModes[].name` `G1`
- `.result.modes.currentModeId` `G1`
- `.result.protocolVersion` `G1`
- `.result.sessionId` `G1`

## Vocabulary

The observed value set for a small declared list of discriminator paths — not every field, only the ones whose values name what kind of thing a frame or a tool call is (`VOCABULARY_PATHS` in `crates/harness/src/capture/surface.rs`). Every path that const declares is listed under every direction, whether or not this version's scenarios put a scalar there. `(none observed)` means exactly that: no captured frame produced a value at that path in that direction, in this version's evidence — it is not a claim that the provider lacks the capability. Direction-keying itself is not a formality: a discriminator can carry a genuinely different vocabulary per direction, not merely an unevenly observed one — the value set one direction shows is not a subset of the other's, and a value native to one direction may never appear in the other at all. Reading a path's values without checking which direction produced them would silently merge two different discriminators into one.

### To provider

#### `.event.content_block.name`

(none observed)

#### `.event.type`

(none observed)

#### `.message.content[].name`

(none observed)

#### `.method`

- `initialize`
- `session/new`

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

(none observed)

### From provider

#### `.event.content_block.name`

(none observed)

#### `.event.type`

(none observed)

#### `.message.content[].name`

(none observed)

#### `.method`

(none observed)

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

(none observed)
