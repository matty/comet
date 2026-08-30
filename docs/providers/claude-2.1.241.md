# claude 2.1.241

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### command-discovery

capture Claude's cwd-scoped command initialize reply

cwd: `<CWD>`
env: `CLAUDE_CONFIG_DIR=<CLAUDE_CONFIG_DIR>`
tools: (not observed)

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
```

### model-discovery

capture Claude's token-free model initialize reply

cwd: `<CWD>`
env: `CLAUDE_CONFIG_DIR=<CLAUDE_CONFIG_DIR>`
tools: (not observed)

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
--bare
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: command-discovery
- `G2`: command-discovery, model-discovery
- `G3`: model-discovery

### To provider

- `.request` `G2`
- `.request.subtype` `G2`
- `.request_id` `G2`
- `.type` `G2`

### From provider

- `.response` `G2`
- `.response.request_id` `G2`
- `.response.response` `G2`
- `.response.response.account` `G2`
- `.response.response.account.apiProvider` `G2`
- `.response.response.account.email` `G1`
- `.response.response.account.subscriptionType` `G1`
- `.response.response.account.tokenSource` `G3`
- `.response.response.agents` `G2`
- `.response.response.agents[].description` `G2`
- `.response.response.agents[].model` `G2`
- `.response.response.agents[].name` `G2`
- `.response.response.available_output_styles` `G2`
- `.response.response.commands` `G2`
- `.response.response.commands[].aliases` `G2`
- `.response.response.commands[].argumentHint` `G2`
- `.response.response.commands[].description` `G2`
- `.response.response.commands[].name` `G2`
- `.response.response.current_permission_mode` `G2`
- `.response.response.fast_mode_disabled_reason` `G2`
- `.response.response.fast_mode_state` `G2`
- `.response.response.ide_rc_auto_enable_gate` `G2`
- `.response.response.models` `G2`
- `.response.response.models[].description` `G2`
- `.response.response.models[].displayName` `G2`
- `.response.response.models[].resolvedModel` `G2`
- `.response.response.models[].supportedEffortLevels` `G2`
- `.response.response.models[].supportsAdaptiveThinking` `G2`
- `.response.response.models[].supportsAutoMode` `G2`
- `.response.response.models[].supportsEffort` `G2`
- `.response.response.models[].supportsFastMode` `G2`
- `.response.response.models[].value` `G2`
- `.response.response.output_style` `G2`
- `.response.response.pid` `G2`
- `.response.response.remote_control_auto_enable` `G2`
- `.response.response.remote_control_auto_on_by_default` `G2`
- `.response.response.session_state` `G2`
- `.response.subtype` `G2`
- `.type` `G2`

## Vocabulary

The observed value set for a small declared list of discriminator paths — not every field, only the ones whose values name what kind of thing a frame or a tool call is (`VOCABULARY_PATHS` in `crates/capture/src/surface.rs`). Every path that const declares is listed under every direction, whether or not this version's scenarios put a scalar there. `(none observed)` means exactly that: no captured frame produced a value at that path in that direction, in this version's evidence — it is not a claim that the provider lacks the capability. Direction-keying itself is not a formality: a discriminator can carry a genuinely different vocabulary per direction, not merely an unevenly observed one — the value set one direction shows is not a subset of the other's, and a value native to one direction may never appear in the other at all. Reading a path's values without checking which direction produced them would silently merge two different discriminators into one.

### To provider

#### `.event.content_block.name`

(none observed)

#### `.event.type`

(none observed)

#### `.message.content[].name`

(none observed)

#### `.method`

(none observed)

#### `.params.update.sessionUpdate`

(none observed)

#### `.request.subtype`

- `initialize`

#### `.request.tool_name`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

- `control_request`

### From provider

#### `.event.content_block.name`

(none observed)

#### `.event.type`

(none observed)

#### `.message.content[].name`

(none observed)

#### `.method`

(none observed)

#### `.params.update.sessionUpdate`

(none observed)

#### `.request.subtype`

(none observed)

#### `.request.tool_name`

(none observed)

#### `.response.subtype`

- `success`

#### `.subtype`

(none observed)

#### `.type`

- `control_response`
