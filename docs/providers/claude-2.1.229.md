# claude 2.1.229

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### checklist

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 59

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

capture one bounded Claude run script

cwd: `<CWD>`
env: (none set)
tools: 59

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

### subagent

capture Claude delegating a Task/Agent subagent, including the resumed task_started sharing one task_id, patch-only task_updated, task_notification with usage, and background_tasks_changed populated-then-empty

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
haiku
--permission-mode
acceptEdits
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: checklist, checklist-resume
- `G2`: checklist, checklist-resume, subagent
- `G3`: checklist, subagent
- `G4`: subagent

### To provider

- `.message` `G2`
- `.message.content` `G2`
- `.message.content[].text` `G4`
- `.message.content[].type` `G4`
- `.message.role` `G2`
- `.parent_tool_use_id` `G1`
- `.type` `G2`

### From provider

- `.agents` `G2`
- `.analytics_disabled` `G2`
- `.apiKeySource` `G2`
- `.api_error_status` `G2`
- `.capabilities` `G2`
- `.claude_code_version` `G2`
- `.cwd` `G2`
- `.description` `G4`
- `.duration_api_ms` `G2`
- `.duration_ms` `G2`
- `.estimated_tokens` `G2`
- `.estimated_tokens_delta` `G2`
- `.event` `G2`
- `.event.content_block` `G2`
- `.event.content_block.caller` `G2`
- `.event.content_block.caller.type` `G2`
- `.event.content_block.id` `G2`
- `.event.content_block.input` `G2`
- `.event.content_block.name` `G2`
- `.event.content_block.signature` `G2`
- `.event.content_block.text` `G2`
- `.event.content_block.thinking` `G2`
- `.event.content_block.type` `G2`
- `.event.context_management` `G2`
- `.event.context_management.applied_edits` `G2`
- `.event.delta` `G2`
- `.event.delta.estimated_tokens` `G2`
- `.event.delta.partial_json` `G2`
- `.event.delta.signature` `G2`
- `.event.delta.stop_details` `G2`
- `.event.delta.stop_reason` `G2`
- `.event.delta.stop_sequence` `G2`
- `.event.delta.text` `G2`
- `.event.delta.thinking` `G2`
- `.event.delta.type` `G2`
- `.event.index` `G2`
- `.event.message` `G2`
- `.event.message.content` `G2`
- `.event.message.diagnostics` `G2`
- `.event.message.id` `G2`
- `.event.message.model` `G2`
- `.event.message.role` `G2`
- `.event.message.stop_details` `G2`
- `.event.message.stop_reason` `G2`
- `.event.message.stop_sequence` `G2`
- `.event.message.type` `G2`
- `.event.message.usage` `G2`
- `.event.message.usage.cache_creation` `G2`
- `.event.message.usage.cache_creation.ephemeral_1h_input_tokens` `G2`
- `.event.message.usage.cache_creation.ephemeral_5m_input_tokens` `G2`
- `.event.message.usage.cache_creation_input_tokens` `G2`
- `.event.message.usage.cache_read_input_tokens` `G2`
- `.event.message.usage.inference_geo` `G2`
- `.event.message.usage.input_tokens` `G2`
- `.event.message.usage.output_tokens` `G2`
- `.event.message.usage.service_tier` `G2`
- `.event.type` `G2`
- `.event.usage` `G2`
- `.event.usage.cache_creation_input_tokens` `G2`
- `.event.usage.cache_read_input_tokens` `G2`
- `.event.usage.input_tokens` `G2`
- `.event.usage.iterations` `G2`
- `.event.usage.iterations[].cache_creation` `G2`
- `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G2`
- `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G2`
- `.event.usage.iterations[].cache_creation_input_tokens` `G2`
- `.event.usage.iterations[].cache_read_input_tokens` `G2`
- `.event.usage.iterations[].input_tokens` `G2`
- `.event.usage.iterations[].output_tokens` `G2`
- `.event.usage.iterations[].type` `G2`
- `.event.usage.output_tokens` `G2`
- `.event.usage.output_tokens_details` `G2`
- `.event.usage.output_tokens_details.thinking_tokens` `G2`
- `.exit_code` `G3`
- `.fast_mode_disabled_reason` `G2`
- `.fast_mode_state` `G2`
- `.hook_event` `G3`
- `.hook_id` `G3`
- `.hook_name` `G3`
- `.is_error` `G2`
- `.last_tool_name` `G4`
- `.mcp_servers` `G2`
- `.mcp_servers[].name` `G1`
- `.mcp_servers[].status` `G1`
- `.memory_paths` `G2`
- `.memory_paths.auto` `G2`
- `.message` `G2`
- `.message.content` `G2`
- `.message.content[].caller` `G2`
- `.message.content[].caller.type` `G2`
- `.message.content[].content` `G2`
- `.message.content[].content[].text` `G4`
- `.message.content[].content[].tool_name` `G2`
- `.message.content[].content[].type` `G2`
- `.message.content[].id` `G2`
- `.message.content[].input` `G2`
- `.message.content[].input.activeForm` `G2`
- `.message.content[].input.content` `G4`
- `.message.content[].input.description` `G3`
- `.message.content[].input.file_path` `G4`
- `.message.content[].input.limit` `G4`
- `.message.content[].input.max_results` `G2`
- `.message.content[].input.message` `G4`
- `.message.content[].input.prompt` `G4`
- `.message.content[].input.query` `G2`
- `.message.content[].input.recipient` `G4`
- `.message.content[].input.run_in_background` `G4`
- `.message.content[].input.status` `G1`
- `.message.content[].input.subagent_type` `G4`
- `.message.content[].input.subject` `G3`
- `.message.content[].input.taskId` `G1`
- `.message.content[].input.to` `G4`
- `.message.content[].input.type` `G4`
- `.message.content[].name` `G2`
- `.message.content[].signature` `G2`
- `.message.content[].text` `G2`
- `.message.content[].thinking` `G2`
- `.message.content[].tool_use_id` `G2`
- `.message.content[].type` `G2`
- `.message.context_management` `G2`
- `.message.diagnostics` `G2`
- `.message.diagnostics.cache_miss_reason` `G4`
- `.message.diagnostics.cache_miss_reason.cache_missed_input_tokens` `G4`
- `.message.diagnostics.cache_miss_reason.type` `G4`
- `.message.id` `G2`
- `.message.model` `G2`
- `.message.role` `G2`
- `.message.stop_details` `G2`
- `.message.stop_reason` `G2`
- `.message.stop_sequence` `G2`
- `.message.type` `G2`
- `.message.usage` `G2`
- `.message.usage.cache_creation` `G2`
- `.message.usage.cache_creation.ephemeral_1h_input_tokens` `G2`
- `.message.usage.cache_creation.ephemeral_5m_input_tokens` `G2`
- `.message.usage.cache_creation_input_tokens` `G2`
- `.message.usage.cache_read_input_tokens` `G2`
- `.message.usage.inference_geo` `G2`
- `.message.usage.input_tokens` `G2`
- `.message.usage.output_tokens` `G2`
- `.message.usage.service_tier` `G2`
- `.model` `G2`
- `.modelUsage` `G2`
- `.modelUsage.{}.cacheCreationInputTokens` `G2`
- `.modelUsage.{}.cacheReadInputTokens` `G2`
- `.modelUsage.{}.canonicalModel` `G2`
- `.modelUsage.{}.contextWindow` `G2`
- `.modelUsage.{}.costUSD` `G2`
- `.modelUsage.{}.inputTokens` `G2`
- `.modelUsage.{}.maxOutputTokens` `G2`
- `.modelUsage.{}.outputTokens` `G2`
- `.modelUsage.{}.provider` `G2`
- `.modelUsage.{}.webSearchRequests` `G2`
- `.num_turns` `G2`
- `.outcome` `G3`
- `.output` `G3`
- `.output_file` `G4`
- `.output_style` `G2`
- `.parent_tool_use_id` `G2`
- `.patch` `G4`
- `.patch.end_time` `G4`
- `.patch.status` `G4`
- `.permissionMode` `G2`
- `.permission_denials` `G2`
- `.plugins` `G2`
- `.plugins[].name` `G2`
- `.plugins[].path` `G2`
- `.plugins[].source` `G2`
- `.plugins[].version` `G2`
- `.product_feedback_disabled` `G2`
- `.prompt` `G4`
- `.rate_limit_info` `G2`
- `.rate_limit_info.isUsingOverage` `G2`
- `.rate_limit_info.overageDisabledReason` `G2`
- `.rate_limit_info.overageStatus` `G2`
- `.rate_limit_info.rateLimitType` `G2`
- `.rate_limit_info.resetsAt` `G2`
- `.rate_limit_info.status` `G2`
- `.request_id` `G2`
- `.result` `G2`
- `.session_id` `G2`
- `.skills` `G2`
- `.slash_commands` `G2`
- `.status` `G2`
- `.stderr` `G3`
- `.stdout` `G3`
- `.stop_reason` `G2`
- `.subagent_type` `G4`
- `.subtype` `G2`
- `.summary` `G4`
- `.task_description` `G4`
- `.task_id` `G4`
- `.task_type` `G4`
- `.tasks` `G4`
- `.tasks[].description` `G4`
- `.tasks[].task_id` `G4`
- `.tasks[].task_type` `G4`
- `.terminal_reason` `G2`
- `.terminal_slash_commands` `G2`
- `.time_to_request_ms` `G2`
- `.timestamp` `G2`
- `.tool_use_id` `G4`
- `.tool_use_result` `G2`
- `.tool_use_result.agentId` `G4`
- `.tool_use_result.agentType` `G4`
- `.tool_use_result.content` `G4`
- `.tool_use_result.content[].text` `G4`
- `.tool_use_result.content[].type` `G4`
- `.tool_use_result.file` `G4`
- `.tool_use_result.file.content` `G4`
- `.tool_use_result.file.filePath` `G4`
- `.tool_use_result.file.numLines` `G4`
- `.tool_use_result.file.startLine` `G4`
- `.tool_use_result.file.totalLines` `G4`
- `.tool_use_result.matches` `G2`
- `.tool_use_result.message` `G4`
- `.tool_use_result.pin` `G4`
- `.tool_use_result.pin.id` `G4`
- `.tool_use_result.pin.name` `G4`
- `.tool_use_result.pin.ref` `G4`
- `.tool_use_result.prompt` `G4`
- `.tool_use_result.query` `G2`
- `.tool_use_result.resolvedModel` `G4`
- `.tool_use_result.resumedAgentId` `G4`
- `.tool_use_result.status` `G4`
- `.tool_use_result.statusChange` `G1`
- `.tool_use_result.statusChange.from` `G1`
- `.tool_use_result.statusChange.to` `G1`
- `.tool_use_result.success` `G2`
- `.tool_use_result.task` `G3`
- `.tool_use_result.task.id` `G3`
- `.tool_use_result.task.subject` `G3`
- `.tool_use_result.taskId` `G1`
- `.tool_use_result.toolStats` `G4`
- `.tool_use_result.toolStats.bashCount` `G4`
- `.tool_use_result.toolStats.editFileCount` `G4`
- `.tool_use_result.toolStats.linesAdded` `G4`
- `.tool_use_result.toolStats.linesRemoved` `G4`
- `.tool_use_result.toolStats.otherToolCount` `G4`
- `.tool_use_result.toolStats.readCount` `G4`
- `.tool_use_result.toolStats.searchCount` `G4`
- `.tool_use_result.totalDurationMs` `G4`
- `.tool_use_result.totalTokens` `G4`
- `.tool_use_result.totalToolUseCount` `G4`
- `.tool_use_result.total_deferred_tools` `G2`
- `.tool_use_result.type` `G4`
- `.tool_use_result.updatedFields` `G1`
- `.tool_use_result.usage` `G4`
- `.tool_use_result.usage.cache_creation` `G4`
- `.tool_use_result.usage.cache_creation.ephemeral_1h_input_tokens` `G4`
- `.tool_use_result.usage.cache_creation.ephemeral_5m_input_tokens` `G4`
- `.tool_use_result.usage.cache_creation_input_tokens` `G4`
- `.tool_use_result.usage.cache_read_input_tokens` `G4`
- `.tool_use_result.usage.inference_geo` `G4`
- `.tool_use_result.usage.input_tokens` `G4`
- `.tool_use_result.usage.iterations` `G4`
- `.tool_use_result.usage.iterations[].cache_creation` `G4`
- `.tool_use_result.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G4`
- `.tool_use_result.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G4`
- `.tool_use_result.usage.iterations[].cache_creation_input_tokens` `G4`
- `.tool_use_result.usage.iterations[].cache_read_input_tokens` `G4`
- `.tool_use_result.usage.iterations[].input_tokens` `G4`
- `.tool_use_result.usage.iterations[].output_tokens` `G4`
- `.tool_use_result.usage.iterations[].type` `G4`
- `.tool_use_result.usage.output_tokens` `G4`
- `.tool_use_result.usage.output_tokens_details` `G4`
- `.tool_use_result.usage.output_tokens_details.thinking_tokens` `G4`
- `.tool_use_result.usage.server_tool_use` `G4`
- `.tool_use_result.usage.server_tool_use.web_fetch_requests` `G4`
- `.tool_use_result.usage.server_tool_use.web_search_requests` `G4`
- `.tool_use_result.usage.service_tier` `G4`
- `.tool_use_result.usage.speed` `G4`
- `.tools` `G2`
- `.total_cost_usd` `G2`
- `.ttft_ms` `G2`
- `.ttft_stream_ms` `G2`
- `.type` `G2`
- `.usage` `G2`
- `.usage.cache_creation` `G2`
- `.usage.cache_creation.ephemeral_1h_input_tokens` `G2`
- `.usage.cache_creation.ephemeral_5m_input_tokens` `G2`
- `.usage.cache_creation_input_tokens` `G2`
- `.usage.cache_read_input_tokens` `G2`
- `.usage.duration_ms` `G4`
- `.usage.inference_geo` `G2`
- `.usage.input_tokens` `G2`
- `.usage.iterations` `G2`
- `.usage.iterations[].cache_creation` `G2`
- `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` `G2`
- `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` `G2`
- `.usage.iterations[].cache_creation_input_tokens` `G2`
- `.usage.iterations[].cache_read_input_tokens` `G2`
- `.usage.iterations[].input_tokens` `G2`
- `.usage.iterations[].output_tokens` `G2`
- `.usage.iterations[].type` `G2`
- `.usage.output_tokens` `G2`
- `.usage.output_tokens_details` `G2`
- `.usage.output_tokens_details.thinking_tokens` `G2`
- `.usage.server_tool_use` `G2`
- `.usage.server_tool_use.web_fetch_requests` `G2`
- `.usage.server_tool_use.web_search_requests` `G2`
- `.usage.service_tier` `G2`
- `.usage.speed` `G2`
- `.usage.tool_uses` `G4`
- `.usage.total_tokens` `G4`
- `.uuid` `G2`

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

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

- `user`

### From provider

#### `.event.content_block.name`

- `Agent`
- `Read`
- `SendMessage`
- `TaskCreate`
- `TaskUpdate`
- `ToolSearch`

#### `.event.type`

- `content_block_delta`
- `content_block_start`
- `content_block_stop`
- `message_delta`
- `message_start`
- `message_stop`

#### `.message.content[].name`

- `Agent`
- `Read`
- `SendMessage`
- `TaskCreate`
- `TaskUpdate`
- `ToolSearch`

#### `.method`

(none observed)

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

- `background_tasks_changed`
- `hook_response`
- `hook_started`
- `init`
- `status`
- `success`
- `task_notification`
- `task_progress`
- `task_started`
- `task_updated`
- `thinking_tokens`

#### `.type`

- `assistant`
- `rate_limit_event`
- `result`
- `stream_event`
- `system`
- `user`
