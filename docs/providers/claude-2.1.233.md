# claude 2.1.233

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### approval

capture a Claude run that answers Bash and Write approval requests

cwd: `<CWD>`
env: (none set)
tools: 62

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

capture a Claude run with an inlined image attachment

cwd: `<CWD>`
env: (none set)
tools: 62

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

### auto

capture what Claude's auto permission mode puts on the wire

cwd: `<CWD>`
env: (none set)
tools: 62

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
auto
```

### checklist

capture a Claude run driving TaskCreate/TaskUpdate

cwd: `<CWD>`
env: (none set)
tools: 62

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

### checklist-resume

capture a Claude run resuming and continuing a checklist from a second process

cwd: `<CWD>`
env: (none set)
tools: 62

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

capture a plain Claude text turn

cwd: `<CWD>`
env: (none set)
tools: 62

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

### full-access

capture what Claude's bypassPermissions mode puts on the wire

cwd: `<CWD>`
env: (none set)
tools: 35

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
bypassPermissions
--dangerously-skip-permissions
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

capture a Claude run resuming an existing session

cwd: `<CWD>`
env: (none set)
tools: 62

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
- `G2`: approval, attachment, auto, checklist, checklist-resume, command-discovery, fresh-text, full-access, model-discovery, resume
- `G3`: approval, attachment, auto, checklist, checklist-resume, command-discovery, fresh-text, full-access, resume
- `G4`: approval, attachment, auto, checklist, checklist-resume, fresh-text, full-access, resume
- `G5`: approval, attachment, auto, checklist, checklist-resume, fresh-text, resume
- `G6`: approval, attachment, auto, checklist, command-discovery, fresh-text, full-access
- `G7`: approval, checklist
- `G8`: approval, checklist, checklist-resume
- `G9`: attachment
- `G10`: checklist
- `G11`: checklist, checklist-resume
- `G12`: command-discovery
- `G13`: command-discovery, model-discovery
- `G14`: model-discovery

### To provider

- `.message` `G4`
- `.message.content` `G4`
- `.message.content[].source` `G9`
- `.message.content[].source.data` `G9`
- `.message.content[].source.media_type` `G9`
- `.message.content[].source.type` `G9`
- `.message.content[].text` `G9`
- `.message.content[].type` `G9`
- `.message.role` `G4`
- `.parent_tool_use_id` `G4`
- `.request` `G13`
- `.request.subtype` `G13`
- `.request_id` `G13`
- `.response` `G1`
- `.response.request_id` `G1`
- `.response.response` `G1`
- `.response.response.behavior` `G1`
- `.response.response.updatedInput` `G1`
- `.response.response.updatedInput.content` `G1`
- `.response.response.updatedInput.file_path` `G1`
- `.response.subtype` `G1`
- `.type` `G2`

### From provider

- `.agents` `G4`
- `.analytics_disabled` `G4`
- `.apiKeySource` `G4`
- `.api_error_status` `G4`
- `.capabilities` `G4`
- `.claude_code_version` `G4`
- `.cwd` `G4`
- `.duration_api_ms` `G4`
- `.duration_ms` `G4`
- `.estimated_tokens` `G4`
- `.estimated_tokens_delta` `G4`
- `.event` `G4`
- `.event.content_block` `G4`
- `.event.content_block.caller` `G8`
- `.event.content_block.caller.type` `G8`
- `.event.content_block.id` `G8`
- `.event.content_block.input` `G8`
- `.event.content_block.name` `G8`
- `.event.content_block.signature` `G4`
- `.event.content_block.text` `G4`
- `.event.content_block.thinking` `G4`
- `.event.content_block.type` `G4`
- `.event.context_management` `G4`
- `.event.context_management.applied_edits` `G4`
- `.event.delta` `G4`
- `.event.delta.estimated_tokens` `G4`
- `.event.delta.partial_json` `G8`
- `.event.delta.signature` `G4`
- `.event.delta.stop_details` `G4`
- `.event.delta.stop_reason` `G4`
- `.event.delta.stop_sequence` `G4`
- `.event.delta.text` `G4`
- `.event.delta.thinking` `G4`
- `.event.delta.type` `G4`
- `.event.index` `G4`
- `.event.message` `G4`
- `.event.message.content` `G4`
- `.event.message.diagnostics` `G4`
- `.event.message.id` `G4`
- `.event.message.model` `G4`
- `.event.message.role` `G4`
- `.event.message.stop_details` `G4`
- `.event.message.stop_reason` `G4`
- `.event.message.stop_sequence` `G4`
- `.event.message.type` `G4`
- `.event.message.usage` `G4`
- `.event.message.usage.cache_creation` `G4`
- `.event.message.usage.cache_creation.ephemeral_1h_input_tokens` `G4`
- `.event.message.usage.cache_creation.ephemeral_5m_input_tokens` `G4`
- `.event.message.usage.cache_creation_input_tokens` `G4`
- `.event.message.usage.cache_read_input_tokens` `G4`
- `.event.message.usage.inference_geo` `G4`
- `.event.message.usage.input_tokens` `G4`
- `.event.message.usage.output_tokens` `G4`
- `.event.message.usage.service_tier` `G4`
- `.event.type` `G4`
- `.event.usage` `G4`
- `.event.usage.cache_creation_input_tokens` `G4`
- `.event.usage.cache_read_input_tokens` `G4`
- `.event.usage.input_tokens` `G4`
- `.event.usage.iterations` `G4`
- `.event.usage.iterations[].cache_creation` `G4`
- `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G4`
- `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G4`
- `.event.usage.iterations[].cache_creation_input_tokens` `G4`
- `.event.usage.iterations[].cache_read_input_tokens` `G4`
- `.event.usage.iterations[].input_tokens` `G4`
- `.event.usage.iterations[].output_tokens` `G4`
- `.event.usage.iterations[].type` `G4`
- `.event.usage.output_tokens` `G4`
- `.event.usage.output_tokens_details` `G4`
- `.event.usage.output_tokens_details.thinking_tokens` `G4`
- `.exit_code` `G6`
- `.fast_mode_disabled_reason` `G4`
- `.fast_mode_state` `G4`
- `.hook_event` `G6`
- `.hook_id` `G6`
- `.hook_name` `G6`
- `.is_error` `G4`
- `.mcp_servers` `G4`
- `.mcp_servers[].name` `G5`
- `.mcp_servers[].status` `G5`
- `.memory_paths` `G4`
- `.memory_paths.auto` `G4`
- `.message` `G4`
- `.message.content` `G4`
- `.message.content[].caller` `G8`
- `.message.content[].caller.type` `G8`
- `.message.content[].content` `G8`
- `.message.content[].content[].tool_name` `G11`
- `.message.content[].content[].type` `G11`
- `.message.content[].id` `G8`
- `.message.content[].input` `G8`
- `.message.content[].input.activeForm` `G11`
- `.message.content[].input.command` `G1`
- `.message.content[].input.content` `G1`
- `.message.content[].input.description` `G7`
- `.message.content[].input.file_path` `G1`
- `.message.content[].input.max_results` `G11`
- `.message.content[].input.query` `G11`
- `.message.content[].input.status` `G11`
- `.message.content[].input.subject` `G10`
- `.message.content[].input.taskId` `G11`
- `.message.content[].is_error` `G1`
- `.message.content[].name` `G8`
- `.message.content[].signature` `G4`
- `.message.content[].text` `G4`
- `.message.content[].thinking` `G4`
- `.message.content[].tool_use_id` `G8`
- `.message.content[].type` `G4`
- `.message.context_management` `G4`
- `.message.diagnostics` `G4`
- `.message.id` `G4`
- `.message.model` `G4`
- `.message.role` `G4`
- `.message.stop_details` `G4`
- `.message.stop_reason` `G4`
- `.message.stop_sequence` `G4`
- `.message.type` `G4`
- `.message.usage` `G4`
- `.message.usage.cache_creation` `G4`
- `.message.usage.cache_creation.ephemeral_1h_input_tokens` `G4`
- `.message.usage.cache_creation.ephemeral_5m_input_tokens` `G4`
- `.message.usage.cache_creation_input_tokens` `G4`
- `.message.usage.cache_read_input_tokens` `G4`
- `.message.usage.inference_geo` `G4`
- `.message.usage.input_tokens` `G4`
- `.message.usage.output_tokens` `G4`
- `.message.usage.service_tier` `G4`
- `.model` `G4`
- `.modelUsage` `G4`
- `.modelUsage.{}.cacheCreationInputTokens` `G4`
- `.modelUsage.{}.cacheReadInputTokens` `G4`
- `.modelUsage.{}.canonicalModel` `G4`
- `.modelUsage.{}.contextWindow` `G4`
- `.modelUsage.{}.costUSD` `G4`
- `.modelUsage.{}.inputTokens` `G4`
- `.modelUsage.{}.maxOutputTokens` `G4`
- `.modelUsage.{}.outputTokens` `G4`
- `.modelUsage.{}.provider` `G4`
- `.modelUsage.{}.webSearchRequests` `G4`
- `.num_turns` `G4`
- `.outcome` `G6`
- `.output` `G6`
- `.output_style` `G4`
- `.parent_tool_use_id` `G4`
- `.permissionMode` `G4`
- `.permission_denials` `G4`
- `.plugins` `G4`
- `.plugins[].name` `G4`
- `.plugins[].path` `G4`
- `.plugins[].source` `G4`
- `.plugins[].version` `G4`
- `.product_feedback_disabled` `G4`
- `.rate_limit_info` `G4`
- `.rate_limit_info.isUsingOverage` `G4`
- `.rate_limit_info.overageDisabledReason` `G4`
- `.rate_limit_info.overageStatus` `G4`
- `.rate_limit_info.rateLimitType` `G4`
- `.rate_limit_info.resetsAt` `G4`
- `.rate_limit_info.status` `G4`
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
- `.request_id` `G4`
- `.response` `G13`
- `.response.request_id` `G13`
- `.response.response` `G13`
- `.response.response.account` `G13`
- `.response.response.account.apiProvider` `G13`
- `.response.response.account.email` `G12`
- `.response.response.account.organization` `G12`
- `.response.response.account.subscriptionType` `G12`
- `.response.response.account.tokenSource` `G14`
- `.response.response.agents` `G13`
- `.response.response.agents[].description` `G13`
- `.response.response.agents[].model` `G13`
- `.response.response.agents[].name` `G13`
- `.response.response.available_output_styles` `G13`
- `.response.response.commands` `G13`
- `.response.response.commands[].aliases` `G13`
- `.response.response.commands[].argumentHint` `G13`
- `.response.response.commands[].description` `G13`
- `.response.response.commands[].name` `G13`
- `.response.response.current_permission_mode` `G13`
- `.response.response.fast_mode_disabled_reason` `G13`
- `.response.response.fast_mode_state` `G13`
- `.response.response.ide_rc_auto_enable_gate` `G13`
- `.response.response.models` `G13`
- `.response.response.models[].description` `G13`
- `.response.response.models[].displayName` `G13`
- `.response.response.models[].resolvedModel` `G13`
- `.response.response.models[].supportedEffortLevels` `G13`
- `.response.response.models[].supportsAdaptiveThinking` `G13`
- `.response.response.models[].supportsAutoMode` `G13`
- `.response.response.models[].supportsEffort` `G13`
- `.response.response.models[].supportsFastMode` `G13`
- `.response.response.models[].value` `G13`
- `.response.response.output_style` `G13`
- `.response.response.pid` `G13`
- `.response.response.remote_control_auto_enable` `G13`
- `.response.response.remote_control_auto_on_by_default` `G13`
- `.response.subtype` `G13`
- `.result` `G4`
- `.session_id` `G3`
- `.skills` `G4`
- `.slash_commands` `G4`
- `.status` `G4`
- `.stderr` `G6`
- `.stdout` `G6`
- `.stop_reason` `G4`
- `.subtype` `G3`
- `.terminal_reason` `G4`
- `.terminal_slash_commands` `G4`
- `.time_to_request_ms` `G4`
- `.timestamp` `G4`
- `.tool_use_result` `G8`
- `.tool_use_result.content` `G1`
- `.tool_use_result.filePath` `G1`
- `.tool_use_result.interrupted` `G1`
- `.tool_use_result.isImage` `G1`
- `.tool_use_result.matches` `G11`
- `.tool_use_result.noOutputExpected` `G1`
- `.tool_use_result.originalFile` `G1`
- `.tool_use_result.query` `G11`
- `.tool_use_result.statusChange` `G11`
- `.tool_use_result.statusChange.from` `G11`
- `.tool_use_result.statusChange.to` `G11`
- `.tool_use_result.stderr` `G1`
- `.tool_use_result.stdout` `G1`
- `.tool_use_result.structuredPatch` `G1`
- `.tool_use_result.success` `G11`
- `.tool_use_result.task` `G10`
- `.tool_use_result.task.id` `G10`
- `.tool_use_result.task.subject` `G10`
- `.tool_use_result.taskId` `G11`
- `.tool_use_result.total_deferred_tools` `G11`
- `.tool_use_result.type` `G1`
- `.tool_use_result.updatedFields` `G11`
- `.tool_use_result.userModified` `G1`
- `.tools` `G4`
- `.total_cost_usd` `G4`
- `.ttft_ms` `G4`
- `.ttft_stream_ms` `G4`
- `.type` `G2`
- `.usage` `G4`
- `.usage.cache_creation` `G4`
- `.usage.cache_creation.ephemeral_1h_input_tokens` `G4`
- `.usage.cache_creation.ephemeral_5m_input_tokens` `G4`
- `.usage.cache_creation_input_tokens` `G4`
- `.usage.cache_read_input_tokens` `G4`
- `.usage.inference_geo` `G4`
- `.usage.input_tokens` `G4`
- `.usage.iterations` `G4`
- `.usage.iterations[].cache_creation` `G4`
- `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G4`
- `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G4`
- `.usage.iterations[].cache_creation_input_tokens` `G4`
- `.usage.iterations[].cache_read_input_tokens` `G4`
- `.usage.iterations[].input_tokens` `G4`
- `.usage.iterations[].output_tokens` `G4`
- `.usage.iterations[].type` `G4`
- `.usage.output_tokens` `G4`
- `.usage.output_tokens_details` `G4`
- `.usage.output_tokens_details.thinking_tokens` `G4`
- `.usage.server_tool_use` `G4`
- `.usage.server_tool_use.web_fetch_requests` `G4`
- `.usage.server_tool_use.web_search_requests` `G4`
- `.usage.service_tier` `G4`
- `.usage.speed` `G4`
- `.uuid` `G3`

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
- `TaskCreate`
- `TaskUpdate`
- `ToolSearch`
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
- `TaskCreate`
- `TaskUpdate`
- `ToolSearch`
- `Write`

#### `.method`

(none observed)

#### `.params.update.sessionUpdate`

(none observed)

#### `.request.subtype`

- `can_use_tool`

#### `.request.tool_name`

- `Write`

#### `.response.subtype`

- `success`

#### `.subtype`

- `hook_response`
- `hook_started`
- `init`
- `status`
- `success`
- `thinking_tokens`

#### `.type`

- `assistant`
- `control_request`
- `control_response`
- `rate_limit_event`
- `result`
- `stream_event`
- `system`
- `user`
