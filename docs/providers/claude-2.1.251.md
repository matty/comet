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

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: edit

### To provider

- `.message` `G1`
- `.message.content` `G1`
- `.message.role` `G1`
- `.parent_tool_use_id` `G1`
- `.type` `G1`

### From provider

- `.agents` `G1`
- `.analytics_disabled` `G1`
- `.apiKeySource` `G1`
- `.api_error_status` `G1`
- `.capabilities` `G1`
- `.claude_code_version` `G1`
- `.cwd` `G1`
- `.duration_api_ms` `G1`
- `.duration_ms` `G1`
- `.estimated_tokens` `G1`
- `.estimated_tokens_delta` `G1`
- `.event` `G1`
- `.event.content_block` `G1`
- `.event.content_block.caller` `G1`
- `.event.content_block.caller.type` `G1`
- `.event.content_block.id` `G1`
- `.event.content_block.input` `G1`
- `.event.content_block.name` `G1`
- `.event.content_block.signature` `G1`
- `.event.content_block.text` `G1`
- `.event.content_block.thinking` `G1`
- `.event.content_block.type` `G1`
- `.event.context_management` `G1`
- `.event.context_management.applied_edits` `G1`
- `.event.delta` `G1`
- `.event.delta.estimated_tokens` `G1`
- `.event.delta.partial_json` `G1`
- `.event.delta.signature` `G1`
- `.event.delta.stop_details` `G1`
- `.event.delta.stop_reason` `G1`
- `.event.delta.stop_sequence` `G1`
- `.event.delta.text` `G1`
- `.event.delta.thinking` `G1`
- `.event.delta.type` `G1`
- `.event.index` `G1`
- `.event.message` `G1`
- `.event.message.content` `G1`
- `.event.message.diagnostics` `G1`
- `.event.message.id` `G1`
- `.event.message.model` `G1`
- `.event.message.role` `G1`
- `.event.message.stop_details` `G1`
- `.event.message.stop_reason` `G1`
- `.event.message.stop_sequence` `G1`
- `.event.message.type` `G1`
- `.event.message.usage` `G1`
- `.event.message.usage.cache_creation` `G1`
- `.event.message.usage.cache_creation.ephemeral_1h_input_tokens` `G1`
- `.event.message.usage.cache_creation.ephemeral_5m_input_tokens` `G1`
- `.event.message.usage.cache_creation_input_tokens` `G1`
- `.event.message.usage.cache_read_input_tokens` `G1`
- `.event.message.usage.inference_geo` `G1`
- `.event.message.usage.input_tokens` `G1`
- `.event.message.usage.output_tokens` `G1`
- `.event.message.usage.service_tier` `G1`
- `.event.type` `G1`
- `.event.usage` `G1`
- `.event.usage.cache_creation_input_tokens` `G1`
- `.event.usage.cache_read_input_tokens` `G1`
- `.event.usage.input_tokens` `G1`
- `.event.usage.iterations` `G1`
- `.event.usage.iterations[].cache_creation` `G1`
- `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G1`
- `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G1`
- `.event.usage.iterations[].cache_creation_input_tokens` `G1`
- `.event.usage.iterations[].cache_read_input_tokens` `G1`
- `.event.usage.iterations[].input_tokens` `G1`
- `.event.usage.iterations[].output_tokens` `G1`
- `.event.usage.iterations[].type` `G1`
- `.event.usage.output_tokens` `G1`
- `.event.usage.output_tokens_details` `G1`
- `.event.usage.output_tokens_details.thinking_tokens` `G1`
- `.fast_mode_disabled_reason` `G1`
- `.fast_mode_state` `G1`
- `.is_error` `G1`
- `.mcp_servers` `G1`
- `.mcp_servers[].name` `G1`
- `.mcp_servers[].status` `G1`
- `.memory_paths` `G1`
- `.memory_paths.auto` `G1`
- `.message` `G1`
- `.message.content` `G1`
- `.message.content[].caller` `G1`
- `.message.content[].caller.type` `G1`
- `.message.content[].content` `G1`
- `.message.content[].id` `G1`
- `.message.content[].input` `G1`
- `.message.content[].input.file_path` `G1`
- `.message.content[].input.new_string` `G1`
- `.message.content[].input.old_string` `G1`
- `.message.content[].input.replace_all` `G1`
- `.message.content[].name` `G1`
- `.message.content[].signature` `G1`
- `.message.content[].text` `G1`
- `.message.content[].thinking` `G1`
- `.message.content[].tool_use_id` `G1`
- `.message.content[].type` `G1`
- `.message.context_management` `G1`
- `.message.diagnostics` `G1`
- `.message.id` `G1`
- `.message.model` `G1`
- `.message.role` `G1`
- `.message.stop_details` `G1`
- `.message.stop_reason` `G1`
- `.message.stop_sequence` `G1`
- `.message.type` `G1`
- `.message.usage` `G1`
- `.message.usage.cache_creation` `G1`
- `.message.usage.cache_creation.ephemeral_1h_input_tokens` `G1`
- `.message.usage.cache_creation.ephemeral_5m_input_tokens` `G1`
- `.message.usage.cache_creation_input_tokens` `G1`
- `.message.usage.cache_read_input_tokens` `G1`
- `.message.usage.inference_geo` `G1`
- `.message.usage.input_tokens` `G1`
- `.message.usage.output_tokens` `G1`
- `.message.usage.service_tier` `G1`
- `.messaging_socket_path` `G1`
- `.model` `G1`
- `.modelUsage` `G1`
- `.modelUsage.{}.cacheCreationInputTokens` `G1`
- `.modelUsage.{}.cacheReadInputTokens` `G1`
- `.modelUsage.{}.canonicalModel` `G1`
- `.modelUsage.{}.contextWindow` `G1`
- `.modelUsage.{}.costBasis` `G1`
- `.modelUsage.{}.costUSD` `G1`
- `.modelUsage.{}.inputTokens` `G1`
- `.modelUsage.{}.maxOutputTokens` `G1`
- `.modelUsage.{}.outputTokens` `G1`
- `.modelUsage.{}.provider` `G1`
- `.modelUsage.{}.webSearchRequests` `G1`
- `.num_turns` `G1`
- `.output_style` `G1`
- `.parent_tool_use_id` `G1`
- `.permissionMode` `G1`
- `.permission_denials` `G1`
- `.plugins` `G1`
- `.powershell_path` `G1`
- `.product_feedback_disabled` `G1`
- `.queued_turn_count` `G1`
- `.rate_limit_info` `G1`
- `.rate_limit_info.isUsingOverage` `G1`
- `.rate_limit_info.overageDisabledReason` `G1`
- `.rate_limit_info.overageStatus` `G1`
- `.rate_limit_info.rateLimitType` `G1`
- `.rate_limit_info.resetsAt` `G1`
- `.rate_limit_info.status` `G1`
- `.rate_limit_info.unifiedWindows` `G1`
- `.rate_limit_info.unifiedWindows.five_hour` `G1`
- `.rate_limit_info.unifiedWindows.five_hour.resetsAt` `G1`
- `.rate_limit_info.unifiedWindows.five_hour.utilization` `G1`
- `.rate_limit_info.unifiedWindows.seven_day` `G1`
- `.rate_limit_info.unifiedWindows.seven_day.resetsAt` `G1`
- `.rate_limit_info.unifiedWindows.seven_day.utilization` `G1`
- `.request_id` `G1`
- `.result` `G1`
- `.session_id` `G1`
- `.skills` `G1`
- `.slash_commands` `G1`
- `.status` `G1`
- `.stop_reason` `G1`
- `.subagent_stats` `G1`
- `.subagent_stats.by_type` `G1`
- `.subagent_stats.completed` `G1`
- `.subagent_stats.failed` `G1`
- `.subagent_stats.killed` `G1`
- `.subagent_stats.killed.parent` `G1`
- `.subagent_stats.killed.system` `G1`
- `.subagent_stats.killed.user` `G1`
- `.subagent_stats.max_depth` `G1`
- `.subagent_stats.refused` `G1`
- `.subagent_stats.refused.budget` `G1`
- `.subagent_stats.refused.concurrency_limit` `G1`
- `.subagent_stats.refused.depth_limit` `G1`
- `.subagent_stats.requested` `G1`
- `.subagent_stats.requested.background` `G1`
- `.subagent_stats.requested.foreground` `G1`
- `.subagent_stats.requested.unset` `G1`
- `.subagent_stats.spawned` `G1`
- `.subagent_stats.spawned_by_subagents` `G1`
- `.subagent_stats.started_in_background` `G1`
- `.subtype` `G1`
- `.terminal_reason` `G1`
- `.terminal_slash_commands` `G1`
- `.time_to_request_ms` `G1`
- `.timestamp` `G1`
- `.tool_use_result` `G1`
- `.tool_use_result.file` `G1`
- `.tool_use_result.file.content` `G1`
- `.tool_use_result.file.filePath` `G1`
- `.tool_use_result.file.numLines` `G1`
- `.tool_use_result.file.startLine` `G1`
- `.tool_use_result.file.totalLines` `G1`
- `.tool_use_result.filePath` `G1`
- `.tool_use_result.newString` `G1`
- `.tool_use_result.oldString` `G1`
- `.tool_use_result.originalFile` `G1`
- `.tool_use_result.replaceAll` `G1`
- `.tool_use_result.structuredPatch` `G1`
- `.tool_use_result.structuredPatch[].lines` `G1`
- `.tool_use_result.structuredPatch[].newLines` `G1`
- `.tool_use_result.structuredPatch[].newStart` `G1`
- `.tool_use_result.structuredPatch[].oldLines` `G1`
- `.tool_use_result.structuredPatch[].oldStart` `G1`
- `.tool_use_result.type` `G1`
- `.tool_use_result.userModified` `G1`
- `.tools` `G1`
- `.total_cost_usd` `G1`
- `.ttft_ms` `G1`
- `.ttft_stream_ms` `G1`
- `.type` `G1`
- `.usage` `G1`
- `.usage.cache_creation` `G1`
- `.usage.cache_creation.ephemeral_1h_input_tokens` `G1`
- `.usage.cache_creation.ephemeral_5m_input_tokens` `G1`
- `.usage.cache_creation_input_tokens` `G1`
- `.usage.cache_read_input_tokens` `G1`
- `.usage.inference_geo` `G1`
- `.usage.input_tokens` `G1`
- `.usage.iterations` `G1`
- `.usage.iterations[].cache_creation` `G1`
- `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G1`
- `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G1`
- `.usage.iterations[].cache_creation_input_tokens` `G1`
- `.usage.iterations[].cache_read_input_tokens` `G1`
- `.usage.iterations[].input_tokens` `G1`
- `.usage.iterations[].output_tokens` `G1`
- `.usage.iterations[].type` `G1`
- `.usage.output_tokens` `G1`
- `.usage.output_tokens_details` `G1`
- `.usage.output_tokens_details.thinking_tokens` `G1`
- `.usage.server_tool_use` `G1`
- `.usage.server_tool_use.web_fetch_requests` `G1`
- `.usage.server_tool_use.web_search_requests` `G1`
- `.usage.service_tier` `G1`
- `.usage.speed` `G1`
- `.uuid` `G1`

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
