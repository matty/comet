# claude 2.1.251

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### edit

capture a Claude run that edits an existing file with Edit

cwd: `<CWD>`
env: `CLAUDE_CONFIG_DIR=<CLAUDE_CONFIG_DIR>`
tools: 65

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

### edit-create

capture a Claude Edit call with an empty old_string against a path that has never existed (D132)

cwd: `<CWD>`
env: (none set)
tools: 65

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

### edit-noop

capture a Claude Edit call with old_string absent and new_string empty, on a file that exists and has been read (D17's degenerate case)

cwd: `<CWD>`
env: (none set)
tools: 65

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

### write-overwrite

capture a Claude Write call that overwrites an existing file's content (D18)

cwd: `<CWD>`
env: (none set)
tools: 65

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

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: edit, edit-create
- `G2`: edit, edit-create, edit-noop
- `G3`: edit, edit-create, edit-noop, write-overwrite
- `G4`: edit, edit-create, write-overwrite
- `G5`: edit, edit-noop, write-overwrite
- `G6`: edit-create, edit-noop, write-overwrite
- `G7`: edit-noop
- `G8`: write-overwrite

### To provider

- `.message` `G3`
- `.message.content` `G3`
- `.message.role` `G3`
- `.parent_tool_use_id` `G3`
- `.type` `G3`

### From provider

- `.agents` `G3`
- `.analytics_disabled` `G3`
- `.apiKeySource` `G3`
- `.api_error_status` `G3`
- `.capabilities` `G3`
- `.claude_code_version` `G3`
- `.cwd` `G3`
- `.duration_api_ms` `G3`
- `.duration_ms` `G3`
- `.estimated_tokens` `G3`
- `.estimated_tokens_delta` `G3`
- `.event` `G3`
- `.event.content_block` `G3`
- `.event.content_block.caller` `G3`
- `.event.content_block.caller.type` `G3`
- `.event.content_block.id` `G3`
- `.event.content_block.input` `G3`
- `.event.content_block.name` `G3`
- `.event.content_block.signature` `G3`
- `.event.content_block.text` `G3`
- `.event.content_block.thinking` `G3`
- `.event.content_block.type` `G3`
- `.event.context_management` `G3`
- `.event.context_management.applied_edits` `G3`
- `.event.delta` `G3`
- `.event.delta.estimated_tokens` `G3`
- `.event.delta.partial_json` `G3`
- `.event.delta.signature` `G3`
- `.event.delta.stop_details` `G3`
- `.event.delta.stop_reason` `G3`
- `.event.delta.stop_sequence` `G3`
- `.event.delta.text` `G3`
- `.event.delta.thinking` `G3`
- `.event.delta.type` `G3`
- `.event.index` `G3`
- `.event.message` `G3`
- `.event.message.content` `G3`
- `.event.message.diagnostics` `G3`
- `.event.message.id` `G3`
- `.event.message.model` `G3`
- `.event.message.role` `G3`
- `.event.message.stop_details` `G3`
- `.event.message.stop_reason` `G3`
- `.event.message.stop_sequence` `G3`
- `.event.message.type` `G3`
- `.event.message.usage` `G3`
- `.event.message.usage.cache_creation` `G3`
- `.event.message.usage.cache_creation.ephemeral_1h_input_tokens` `G3`
- `.event.message.usage.cache_creation.ephemeral_5m_input_tokens` `G3`
- `.event.message.usage.cache_creation_input_tokens` `G3`
- `.event.message.usage.cache_read_input_tokens` `G3`
- `.event.message.usage.inference_geo` `G3`
- `.event.message.usage.input_tokens` `G3`
- `.event.message.usage.output_tokens` `G3`
- `.event.message.usage.service_tier` `G3`
- `.event.type` `G3`
- `.event.usage` `G3`
- `.event.usage.cache_creation_input_tokens` `G3`
- `.event.usage.cache_read_input_tokens` `G3`
- `.event.usage.input_tokens` `G3`
- `.event.usage.iterations` `G3`
- `.event.usage.iterations[].cache_creation` `G3`
- `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G3`
- `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G3`
- `.event.usage.iterations[].cache_creation_input_tokens` `G3`
- `.event.usage.iterations[].cache_read_input_tokens` `G3`
- `.event.usage.iterations[].input_tokens` `G3`
- `.event.usage.iterations[].output_tokens` `G3`
- `.event.usage.iterations[].type` `G3`
- `.event.usage.output_tokens` `G3`
- `.event.usage.output_tokens_details` `G3`
- `.event.usage.output_tokens_details.thinking_tokens` `G3`
- `.exit_code` `G6`
- `.fast_mode_disabled_reason` `G3`
- `.fast_mode_state` `G3`
- `.hook_event` `G6`
- `.hook_id` `G6`
- `.hook_name` `G6`
- `.is_error` `G3`
- `.mcp_servers` `G3`
- `.mcp_servers[].name` `G3`
- `.mcp_servers[].status` `G3`
- `.memory_paths` `G3`
- `.memory_paths.auto` `G3`
- `.message` `G3`
- `.message.content` `G3`
- `.message.content[].caller` `G3`
- `.message.content[].caller.type` `G3`
- `.message.content[].content` `G3`
- `.message.content[].id` `G3`
- `.message.content[].input` `G3`
- `.message.content[].input.content` `G8`
- `.message.content[].input.file_path` `G3`
- `.message.content[].input.new_string` `G2`
- `.message.content[].input.old_string` `G1`
- `.message.content[].input.replace_all` `G1`
- `.message.content[].is_error` `G7`
- `.message.content[].name` `G3`
- `.message.content[].signature` `G3`
- `.message.content[].text` `G3`
- `.message.content[].thinking` `G3`
- `.message.content[].tool_use_id` `G3`
- `.message.content[].type` `G3`
- `.message.context_management` `G3`
- `.message.diagnostics` `G3`
- `.message.id` `G3`
- `.message.model` `G3`
- `.message.role` `G3`
- `.message.stop_details` `G3`
- `.message.stop_reason` `G3`
- `.message.stop_sequence` `G3`
- `.message.type` `G3`
- `.message.usage` `G3`
- `.message.usage.cache_creation` `G3`
- `.message.usage.cache_creation.ephemeral_1h_input_tokens` `G3`
- `.message.usage.cache_creation.ephemeral_5m_input_tokens` `G3`
- `.message.usage.cache_creation_input_tokens` `G3`
- `.message.usage.cache_read_input_tokens` `G3`
- `.message.usage.inference_geo` `G3`
- `.message.usage.input_tokens` `G3`
- `.message.usage.output_tokens` `G3`
- `.message.usage.service_tier` `G3`
- `.messaging_socket_path` `G3`
- `.model` `G3`
- `.modelUsage` `G3`
- `.modelUsage.{}.cacheCreationInputTokens` `G3`
- `.modelUsage.{}.cacheReadInputTokens` `G3`
- `.modelUsage.{}.canonicalModel` `G3`
- `.modelUsage.{}.contextWindow` `G3`
- `.modelUsage.{}.costBasis` `G3`
- `.modelUsage.{}.costUSD` `G3`
- `.modelUsage.{}.inputTokens` `G3`
- `.modelUsage.{}.maxOutputTokens` `G3`
- `.modelUsage.{}.outputTokens` `G3`
- `.modelUsage.{}.provider` `G3`
- `.modelUsage.{}.webSearchRequests` `G3`
- `.num_turns` `G3`
- `.outcome` `G6`
- `.output` `G6`
- `.output_style` `G3`
- `.parent_tool_use_id` `G3`
- `.permissionMode` `G3`
- `.permission_denials` `G3`
- `.plugins` `G3`
- `.plugins[].name` `G6`
- `.plugins[].path` `G6`
- `.plugins[].source` `G6`
- `.plugins[].version` `G6`
- `.powershell_path` `G3`
- `.product_feedback_disabled` `G3`
- `.queued_turn_count` `G3`
- `.rate_limit_info` `G3`
- `.rate_limit_info.isUsingOverage` `G3`
- `.rate_limit_info.overageDisabledReason` `G3`
- `.rate_limit_info.overageStatus` `G3`
- `.rate_limit_info.rateLimitType` `G3`
- `.rate_limit_info.resetsAt` `G3`
- `.rate_limit_info.status` `G3`
- `.rate_limit_info.unifiedWindows` `G3`
- `.rate_limit_info.unifiedWindows.five_hour` `G3`
- `.rate_limit_info.unifiedWindows.five_hour.resetsAt` `G3`
- `.rate_limit_info.unifiedWindows.five_hour.utilization` `G3`
- `.rate_limit_info.unifiedWindows.seven_day` `G3`
- `.rate_limit_info.unifiedWindows.seven_day.resetsAt` `G3`
- `.rate_limit_info.unifiedWindows.seven_day.utilization` `G3`
- `.request_id` `G3`
- `.result` `G3`
- `.session_id` `G3`
- `.skills` `G3`
- `.slash_commands` `G3`
- `.status` `G3`
- `.stderr` `G6`
- `.stdout` `G6`
- `.stop_reason` `G3`
- `.subagent_stats` `G3`
- `.subagent_stats.by_type` `G3`
- `.subagent_stats.completed` `G3`
- `.subagent_stats.failed` `G3`
- `.subagent_stats.killed` `G3`
- `.subagent_stats.killed.parent` `G3`
- `.subagent_stats.killed.system` `G3`
- `.subagent_stats.killed.user` `G3`
- `.subagent_stats.max_depth` `G3`
- `.subagent_stats.refused` `G3`
- `.subagent_stats.refused.budget` `G3`
- `.subagent_stats.refused.concurrency_limit` `G3`
- `.subagent_stats.refused.depth_limit` `G3`
- `.subagent_stats.requested` `G3`
- `.subagent_stats.requested.background` `G3`
- `.subagent_stats.requested.foreground` `G3`
- `.subagent_stats.requested.unset` `G3`
- `.subagent_stats.spawned` `G3`
- `.subagent_stats.spawned_by_subagents` `G3`
- `.subagent_stats.started_in_background` `G3`
- `.subtype` `G3`
- `.terminal_reason` `G3`
- `.terminal_slash_commands` `G3`
- `.time_to_request_ms` `G3`
- `.timestamp` `G3`
- `.tool_use_result` `G3`
- `.tool_use_result.content` `G8`
- `.tool_use_result.file` `G5`
- `.tool_use_result.file.content` `G5`
- `.tool_use_result.file.filePath` `G5`
- `.tool_use_result.file.numLines` `G5`
- `.tool_use_result.file.startLine` `G5`
- `.tool_use_result.file.totalLines` `G5`
- `.tool_use_result.filePath` `G4`
- `.tool_use_result.newString` `G1`
- `.tool_use_result.oldString` `G1`
- `.tool_use_result.originalFile` `G4`
- `.tool_use_result.replaceAll` `G1`
- `.tool_use_result.structuredPatch` `G4`
- `.tool_use_result.structuredPatch[].lines` `G4`
- `.tool_use_result.structuredPatch[].newLines` `G4`
- `.tool_use_result.structuredPatch[].newStart` `G4`
- `.tool_use_result.structuredPatch[].oldLines` `G4`
- `.tool_use_result.structuredPatch[].oldStart` `G4`
- `.tool_use_result.type` `G5`
- `.tool_use_result.userModified` `G4`
- `.tools` `G3`
- `.total_cost_usd` `G3`
- `.ttft_ms` `G3`
- `.ttft_stream_ms` `G3`
- `.type` `G3`
- `.usage` `G3`
- `.usage.cache_creation` `G3`
- `.usage.cache_creation.ephemeral_1h_input_tokens` `G3`
- `.usage.cache_creation.ephemeral_5m_input_tokens` `G3`
- `.usage.cache_creation_input_tokens` `G3`
- `.usage.cache_read_input_tokens` `G3`
- `.usage.inference_geo` `G3`
- `.usage.input_tokens` `G3`
- `.usage.iterations` `G3`
- `.usage.iterations[].cache_creation` `G3`
- `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G3`
- `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G3`
- `.usage.iterations[].cache_creation_input_tokens` `G3`
- `.usage.iterations[].cache_read_input_tokens` `G3`
- `.usage.iterations[].input_tokens` `G3`
- `.usage.iterations[].output_tokens` `G3`
- `.usage.iterations[].type` `G3`
- `.usage.output_tokens` `G3`
- `.usage.output_tokens_details` `G3`
- `.usage.output_tokens_details.thinking_tokens` `G3`
- `.usage.server_tool_use` `G3`
- `.usage.server_tool_use.web_fetch_requests` `G3`
- `.usage.server_tool_use.web_search_requests` `G3`
- `.usage.service_tier` `G3`
- `.usage.speed` `G3`
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

(none observed)

#### `.request.tool_name`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

- `user`

### From provider

#### `.event.content_block.name`

- `Edit`
- `Read`
- `Write`

#### `.event.type`

- `content_block_delta`
- `content_block_start`
- `content_block_stop`
- `message_delta`
- `message_start`
- `message_stop`

#### `.message.content[].name`

- `Edit`
- `Read`
- `Write`

#### `.method`

(none observed)

#### `.params.update.sessionUpdate`

(none observed)

#### `.request.subtype`

(none observed)

#### `.request.tool_name`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

- `hook_response`
- `hook_started`
- `init`
- `status`
- `success`
- `thinking_tokens`

#### `.type`

- `assistant`
- `rate_limit_event`
- `result`
- `stream_event`
- `system`
- `user`
