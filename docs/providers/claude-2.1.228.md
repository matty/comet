# claude 2.1.228

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### approval

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 29

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
--include-partial-messages
--permission-prompt-tool
stdio
--model
claude-haiku-4-5-20251001
--effort
low
--permission-mode
default
```

### attachment

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 29

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
--include-partial-messages
--permission-prompt-tool
stdio
--model
claude-haiku-4-5-20251001
--effort
low
--permission-mode
acceptEdits
```

### command-discovery

capture Claude's cwd-scoped command initialize reply

cwd: `<CWD>`
env: (none set)
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

### fresh-text

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 29

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
--include-partial-messages
--permission-prompt-tool
stdio
--model
claude-haiku-4-5-20251001
--effort
low
--permission-mode
acceptEdits
```

### model-discovery

capture Claude's token-free model initialize reply

cwd: `<CWD>`
env: (none set)
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

### resume

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 29

```
<HOME>\.local\bin\claude.exe
--print
--input-format
stream-json
--output-format
stream-json
--verbose
--include-partial-messages
--permission-prompt-tool
stdio
--model
claude-haiku-4-5-20251001
--effort
low
--permission-mode
acceptEdits
--resume=<SESSION_1>
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: approval
- `G2`: approval, attachment, command-discovery, fresh-text
- `G3`: approval, attachment, command-discovery, fresh-text, model-discovery, resume
- `G4`: approval, attachment, command-discovery, fresh-text, resume
- `G5`: approval, attachment, fresh-text, resume
- `G6`: approval, attachment, resume
- `G7`: attachment
- `G8`: command-discovery, model-discovery

### To provider

- `.message` `G5`
- `.message.content` `G5`
- `.message.content[].source` `G7`
- `.message.content[].source.data` `G7`
- `.message.content[].source.media_type` `G7`
- `.message.content[].source.type` `G7`
- `.message.content[].text` `G7`
- `.message.content[].type` `G7`
- `.message.role` `G5`
- `.parent_tool_use_id` `G5`
- `.request` `G8`
- `.request.subtype` `G8`
- `.request_id` `G8`
- `.response` `G1`
- `.response.request_id` `G1`
- `.response.response` `G1`
- `.response.response.behavior` `G1`
- `.response.response.updatedInput` `G1`
- `.response.response.updatedInput.content` `G1`
- `.response.response.updatedInput.file_path` `G1`
- `.response.subtype` `G1`
- `.type` `G3`

### From provider

- `.agents` `G5`
- `.analytics_disabled` `G5`
- `.apiKeySource` `G5`
- `.api_error_status` `G5`
- `.capabilities` `G5`
- `.claude_code_version` `G5`
- `.cwd` `G5`
- `.duration_api_ms` `G5`
- `.duration_ms` `G5`
- `.event` `G5`
- `.event.content_block` `G5`
- `.event.content_block.id` `G6`
- `.event.content_block.input` `G6`
- `.event.content_block.name` `G6`
- `.event.content_block.signature` `G6`
- `.event.content_block.text` `G5`
- `.event.content_block.thinking` `G6`
- `.event.content_block.type` `G5`
- `.event.delta` `G5`
- `.event.delta.partial_json` `G6`
- `.event.delta.signature` `G6`
- `.event.delta.stop_reason` `G5`
- `.event.delta.stop_sequence` `G5`
- `.event.delta.text` `G5`
- `.event.delta.type` `G5`
- `.event.index` `G5`
- `.event.message` `G5`
- `.event.message.content` `G5`
- `.event.message.id` `G5`
- `.event.message.model` `G5`
- `.event.message.role` `G5`
- `.event.message.stop_reason` `G5`
- `.event.message.stop_sequence` `G5`
- `.event.message.type` `G5`
- `.event.message.usage` `G5`
- `.event.message.usage.input_tokens` `G5`
- `.event.message.usage.output_tokens` `G5`
- `.event.type` `G5`
- `.event.usage` `G5`
- `.event.usage.cache_creation_input_tokens` `G5`
- `.event.usage.cache_read_input_tokens` `G5`
- `.event.usage.input_tokens` `G5`
- `.event.usage.output_tokens` `G5`
- `.exit_code` `G2`
- `.fast_mode_disabled_reason` `G5`
- `.fast_mode_state` `G5`
- `.hook_event` `G2`
- `.hook_id` `G2`
- `.hook_name` `G2`
- `.isSynthetic` `G6`
- `.is_error` `G5`
- `.mcp_servers` `G5`
- `.memory_paths` `G5`
- `.memory_paths.auto` `G5`
- `.message` `G5`
- `.message.content` `G5`
- `.message.content[].content` `G6`
- `.message.content[].id` `G6`
- `.message.content[].input` `G6`
- `.message.content[].input.command` `G1`
- `.message.content[].input.content` `G1`
- `.message.content[].input.file_path` `G1`
- `.message.content[].input.skill` `G6`
- `.message.content[].is_error` `G1`
- `.message.content[].name` `G6`
- `.message.content[].signature` `G6`
- `.message.content[].text` `G5`
- `.message.content[].thinking` `G6`
- `.message.content[].tool_use_id` `G6`
- `.message.content[].type` `G5`
- `.message.context_management` `G5`
- `.message.id` `G5`
- `.message.model` `G5`
- `.message.role` `G5`
- `.message.stop_reason` `G5`
- `.message.stop_sequence` `G5`
- `.message.type` `G5`
- `.message.usage` `G5`
- `.message.usage.input_tokens` `G5`
- `.message.usage.output_tokens` `G5`
- `.model` `G5`
- `.modelUsage` `G5`
- `.modelUsage.{}.cacheCreationInputTokens` `G5`
- `.modelUsage.{}.cacheReadInputTokens` `G5`
- `.modelUsage.{}.canonicalModel` `G5`
- `.modelUsage.{}.contextWindow` `G5`
- `.modelUsage.{}.costUSD` `G5`
- `.modelUsage.{}.inputTokens` `G5`
- `.modelUsage.{}.maxOutputTokens` `G5`
- `.modelUsage.{}.outputTokens` `G5`
- `.modelUsage.{}.provider` `G5`
- `.modelUsage.{}.webSearchRequests` `G5`
- `.num_turns` `G5`
- `.outcome` `G2`
- `.output` `G2`
- `.output_style` `G5`
- `.parent_tool_use_id` `G5`
- `.permissionMode` `G5`
- `.permission_denials` `G5`
- `.plugins` `G5`
- `.plugins[].name` `G5`
- `.plugins[].path` `G5`
- `.plugins[].source` `G5`
- `.plugins[].version` `G5`
- `.product_feedback_disabled` `G5`
- `.request` `G1`
- `.request.description` `G1`
- `.request.display_name` `G1`
- `.request.input` `G1`
- `.request.input.content` `G1`
- `.request.input.file_path` `G1`
- `.request.permission_suggestions` `G1`
- `.request.permission_suggestions[].destination` `G1`
- `.request.permission_suggestions[].mode` `G1`
- `.request.permission_suggestions[].type` `G1`
- `.request.subtype` `G1`
- `.request.tool_name` `G1`
- `.request.tool_use_id` `G1`
- `.request_id` `G1`
- `.response` `G8`
- `.response.request_id` `G8`
- `.response.response` `G8`
- `.response.response.account` `G8`
- `.response.response.account.apiProvider` `G8`
- `.response.response.account.tokenSource` `G8`
- `.response.response.agents` `G8`
- `.response.response.agents[].description` `G8`
- `.response.response.agents[].model` `G8`
- `.response.response.agents[].name` `G8`
- `.response.response.available_output_styles` `G8`
- `.response.response.commands` `G8`
- `.response.response.commands[].aliases` `G8`
- `.response.response.commands[].argumentHint` `G8`
- `.response.response.commands[].description` `G8`
- `.response.response.commands[].name` `G8`
- `.response.response.current_permission_mode` `G8`
- `.response.response.fast_mode_disabled_reason` `G8`
- `.response.response.fast_mode_state` `G8`
- `.response.response.ide_rc_auto_enable_gate` `G8`
- `.response.response.models` `G8`
- `.response.response.models[].description` `G8`
- `.response.response.models[].displayName` `G8`
- `.response.response.models[].resolvedModel` `G8`
- `.response.response.models[].supportedEffortLevels` `G8`
- `.response.response.models[].supportsAdaptiveThinking` `G8`
- `.response.response.models[].supportsAutoMode` `G8`
- `.response.response.models[].supportsEffort` `G8`
- `.response.response.models[].supportsFastMode` `G8`
- `.response.response.models[].value` `G8`
- `.response.response.output_style` `G8`
- `.response.response.pid` `G8`
- `.response.response.remote_control_auto_enable` `G8`
- `.response.response.remote_control_auto_on_by_default` `G8`
- `.response.subtype` `G8`
- `.result` `G5`
- `.session_id` `G4`
- `.skills` `G5`
- `.slash_commands` `G5`
- `.status` `G5`
- `.stderr` `G2`
- `.stdout` `G2`
- `.stop_reason` `G5`
- `.subtype` `G4`
- `.terminal_reason` `G5`
- `.time_to_request_ms` `G5`
- `.timestamp` `G5`
- `.tool_use_result` `G6`
- `.tool_use_result.commandName` `G6`
- `.tool_use_result.content` `G1`
- `.tool_use_result.filePath` `G1`
- `.tool_use_result.interrupted` `G1`
- `.tool_use_result.isImage` `G1`
- `.tool_use_result.noOutputExpected` `G1`
- `.tool_use_result.originalFile` `G1`
- `.tool_use_result.stderr` `G1`
- `.tool_use_result.stdout` `G1`
- `.tool_use_result.structuredPatch` `G1`
- `.tool_use_result.success` `G6`
- `.tool_use_result.type` `G1`
- `.tool_use_result.userModified` `G1`
- `.tools` `G5`
- `.total_cost_usd` `G5`
- `.ttft_ms` `G5`
- `.ttft_stream_ms` `G5`
- `.type` `G3`
- `.usage` `G5`
- `.usage.cache_creation` `G5`
- `.usage.cache_creation.ephemeral_1h_input_tokens` `G5`
- `.usage.cache_creation.ephemeral_5m_input_tokens` `G5`
- `.usage.cache_creation_input_tokens` `G5`
- `.usage.cache_read_input_tokens` `G5`
- `.usage.inference_geo` `G5`
- `.usage.input_tokens` `G5`
- `.usage.iterations` `G5`
- `.usage.output_tokens` `G5`
- `.usage.output_tokens_details` `G5`
- `.usage.output_tokens_details.thinking_tokens` `G5`
- `.usage.server_tool_use` `G5`
- `.usage.server_tool_use.web_fetch_requests` `G5`
- `.usage.server_tool_use.web_search_requests` `G5`
- `.usage.service_tier` `G5`
- `.usage.speed` `G5`
- `.uuid` `G4`

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

(none observed)

#### `.request.subtype`

- `initialize`

#### `.response.subtype`

- `success`

#### `.subtype`

(none observed)

#### `.type`

- `control_request`
- `control_response`
- `user`

### From provider

#### `.event.content_block.name`

- `Bash`
- `Skill`
- `Write`

#### `.event.type`

- `content_block_delta`
- `content_block_start`
- `content_block_stop`
- `message_delta`
- `message_start`
- `message_stop`

#### `.message.content[].name`

- `Bash`
- `Skill`
- `Write`

#### `.method`

(none observed)

#### `.request.subtype`

- `can_use_tool`

#### `.response.subtype`

- `success`

#### `.subtype`

- `hook_response`
- `hook_started`
- `init`
- `status`
- `success`

#### `.type`

- `assistant`
- `control_request`
- `control_response`
- `result`
- `stream_event`
- `system`
- `user`
