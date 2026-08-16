# claude 2.1.229

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned) — but before reading a disappearance as the CLI dropping a capability, check the Scenarios section of both sheets. A field or a vocabulary value present in one version and absent in the other may mean that version's captures simply never exercised it, not that the CLI changed; the corpus's blind spot is absence, and it did not go away just because this sheet makes it visible.

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from, with the exact argv Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either: two scenarios below with the same argv were launched identically whatever their purpose sentences say, and two with the same purpose sentence can still differ in argv — compare the argv itself before concluding two scenarios tested different things, rather than trusting the name or the purpose alone.

### checklist

capture one bounded Claude run script

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

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted. Read an absent path against the Scenarios section above before reading it as a claim about the wire format.

### To provider

- `.message`
- `.message.content`
- `.message.content[].text`
- `.message.content[].type`
- `.message.role`
- `.parent_tool_use_id`
- `.type`

### From provider

- `.agents`
- `.analytics_disabled`
- `.apiKeySource`
- `.api_error_status`
- `.capabilities`
- `.claude_code_version`
- `.cwd`
- `.description`
- `.duration_api_ms`
- `.duration_ms`
- `.estimated_tokens`
- `.estimated_tokens_delta`
- `.event`
- `.event.content_block`
- `.event.content_block.caller`
- `.event.content_block.caller.type`
- `.event.content_block.id`
- `.event.content_block.input`
- `.event.content_block.name`
- `.event.content_block.signature`
- `.event.content_block.text`
- `.event.content_block.thinking`
- `.event.content_block.type`
- `.event.context_management`
- `.event.context_management.applied_edits`
- `.event.delta`
- `.event.delta.estimated_tokens`
- `.event.delta.partial_json`
- `.event.delta.signature`
- `.event.delta.stop_details`
- `.event.delta.stop_reason`
- `.event.delta.stop_sequence`
- `.event.delta.text`
- `.event.delta.thinking`
- `.event.delta.type`
- `.event.index`
- `.event.message`
- `.event.message.content`
- `.event.message.diagnostics`
- `.event.message.id`
- `.event.message.model`
- `.event.message.role`
- `.event.message.stop_details`
- `.event.message.stop_reason`
- `.event.message.stop_sequence`
- `.event.message.type`
- `.event.message.usage`
- `.event.message.usage.cache_creation`
- `.event.message.usage.cache_creation.ephemeral_1h_input_tokens`
- `.event.message.usage.cache_creation.ephemeral_5m_input_tokens`
- `.event.message.usage.cache_creation_input_tokens`
- `.event.message.usage.cache_read_input_tokens`
- `.event.message.usage.inference_geo`
- `.event.message.usage.input_tokens`
- `.event.message.usage.output_tokens`
- `.event.message.usage.service_tier`
- `.event.type`
- `.event.usage`
- `.event.usage.cache_creation_input_tokens`
- `.event.usage.cache_read_input_tokens`
- `.event.usage.input_tokens`
- `.event.usage.iterations`
- `.event.usage.iterations[].cache_creation`
- `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens`
- `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens`
- `.event.usage.iterations[].cache_creation_input_tokens`
- `.event.usage.iterations[].cache_read_input_tokens`
- `.event.usage.iterations[].input_tokens`
- `.event.usage.iterations[].output_tokens`
- `.event.usage.iterations[].type`
- `.event.usage.output_tokens`
- `.event.usage.output_tokens_details`
- `.event.usage.output_tokens_details.thinking_tokens`
- `.exit_code`
- `.fast_mode_disabled_reason`
- `.fast_mode_state`
- `.hook_event`
- `.hook_id`
- `.hook_name`
- `.is_error`
- `.last_tool_name`
- `.mcp_servers`
- `.mcp_servers[].name`
- `.mcp_servers[].status`
- `.memory_paths`
- `.memory_paths.auto`
- `.message`
- `.message.content`
- `.message.content[].caller`
- `.message.content[].caller.type`
- `.message.content[].content`
- `.message.content[].content[].text`
- `.message.content[].content[].tool_name`
- `.message.content[].content[].type`
- `.message.content[].id`
- `.message.content[].input`
- `.message.content[].input.activeForm`
- `.message.content[].input.content`
- `.message.content[].input.description`
- `.message.content[].input.file_path`
- `.message.content[].input.limit`
- `.message.content[].input.max_results`
- `.message.content[].input.message`
- `.message.content[].input.prompt`
- `.message.content[].input.query`
- `.message.content[].input.recipient`
- `.message.content[].input.run_in_background`
- `.message.content[].input.status`
- `.message.content[].input.subagent_type`
- `.message.content[].input.subject`
- `.message.content[].input.taskId`
- `.message.content[].input.to`
- `.message.content[].input.type`
- `.message.content[].name`
- `.message.content[].signature`
- `.message.content[].text`
- `.message.content[].thinking`
- `.message.content[].tool_use_id`
- `.message.content[].type`
- `.message.context_management`
- `.message.diagnostics`
- `.message.diagnostics.cache_miss_reason`
- `.message.diagnostics.cache_miss_reason.cache_missed_input_tokens`
- `.message.diagnostics.cache_miss_reason.type`
- `.message.id`
- `.message.model`
- `.message.role`
- `.message.stop_details`
- `.message.stop_reason`
- `.message.stop_sequence`
- `.message.type`
- `.message.usage`
- `.message.usage.cache_creation`
- `.message.usage.cache_creation.ephemeral_1h_input_tokens`
- `.message.usage.cache_creation.ephemeral_5m_input_tokens`
- `.message.usage.cache_creation_input_tokens`
- `.message.usage.cache_read_input_tokens`
- `.message.usage.inference_geo`
- `.message.usage.input_tokens`
- `.message.usage.output_tokens`
- `.message.usage.service_tier`
- `.model`
- `.modelUsage`
- `.modelUsage.{}.cacheCreationInputTokens`
- `.modelUsage.{}.cacheReadInputTokens`
- `.modelUsage.{}.canonicalModel`
- `.modelUsage.{}.contextWindow`
- `.modelUsage.{}.costUSD`
- `.modelUsage.{}.inputTokens`
- `.modelUsage.{}.maxOutputTokens`
- `.modelUsage.{}.outputTokens`
- `.modelUsage.{}.provider`
- `.modelUsage.{}.webSearchRequests`
- `.num_turns`
- `.outcome`
- `.output`
- `.output_file`
- `.output_style`
- `.parent_tool_use_id`
- `.patch`
- `.patch.end_time`
- `.patch.status`
- `.permissionMode`
- `.permission_denials`
- `.plugins`
- `.plugins[].name`
- `.plugins[].path`
- `.plugins[].source`
- `.plugins[].version`
- `.product_feedback_disabled`
- `.prompt`
- `.rate_limit_info`
- `.rate_limit_info.isUsingOverage`
- `.rate_limit_info.overageDisabledReason`
- `.rate_limit_info.overageStatus`
- `.rate_limit_info.rateLimitType`
- `.rate_limit_info.resetsAt`
- `.rate_limit_info.status`
- `.request_id`
- `.result`
- `.session_id`
- `.skills`
- `.slash_commands`
- `.status`
- `.stderr`
- `.stdout`
- `.stop_reason`
- `.subagent_type`
- `.subtype`
- `.summary`
- `.task_description`
- `.task_id`
- `.task_type`
- `.tasks`
- `.tasks[].description`
- `.tasks[].task_id`
- `.tasks[].task_type`
- `.terminal_reason`
- `.terminal_slash_commands`
- `.time_to_request_ms`
- `.timestamp`
- `.tool_use_id`
- `.tool_use_result`
- `.tool_use_result.agentId`
- `.tool_use_result.agentType`
- `.tool_use_result.content`
- `.tool_use_result.content[].text`
- `.tool_use_result.content[].type`
- `.tool_use_result.file`
- `.tool_use_result.file.content`
- `.tool_use_result.file.filePath`
- `.tool_use_result.file.numLines`
- `.tool_use_result.file.startLine`
- `.tool_use_result.file.totalLines`
- `.tool_use_result.matches`
- `.tool_use_result.message`
- `.tool_use_result.pin`
- `.tool_use_result.pin.id`
- `.tool_use_result.pin.name`
- `.tool_use_result.pin.ref`
- `.tool_use_result.prompt`
- `.tool_use_result.query`
- `.tool_use_result.resolvedModel`
- `.tool_use_result.resumedAgentId`
- `.tool_use_result.status`
- `.tool_use_result.statusChange`
- `.tool_use_result.statusChange.from`
- `.tool_use_result.statusChange.to`
- `.tool_use_result.success`
- `.tool_use_result.task`
- `.tool_use_result.task.id`
- `.tool_use_result.task.subject`
- `.tool_use_result.taskId`
- `.tool_use_result.toolStats`
- `.tool_use_result.toolStats.bashCount`
- `.tool_use_result.toolStats.editFileCount`
- `.tool_use_result.toolStats.linesAdded`
- `.tool_use_result.toolStats.linesRemoved`
- `.tool_use_result.toolStats.otherToolCount`
- `.tool_use_result.toolStats.readCount`
- `.tool_use_result.toolStats.searchCount`
- `.tool_use_result.totalDurationMs`
- `.tool_use_result.totalTokens`
- `.tool_use_result.totalToolUseCount`
- `.tool_use_result.total_deferred_tools`
- `.tool_use_result.type`
- `.tool_use_result.updatedFields`
- `.tool_use_result.usage`
- `.tool_use_result.usage.cache_creation`
- `.tool_use_result.usage.cache_creation.ephemeral_1h_input_tokens`
- `.tool_use_result.usage.cache_creation.ephemeral_5m_input_tokens`
- `.tool_use_result.usage.cache_creation_input_tokens`
- `.tool_use_result.usage.cache_read_input_tokens`
- `.tool_use_result.usage.inference_geo`
- `.tool_use_result.usage.input_tokens`
- `.tool_use_result.usage.iterations`
- `.tool_use_result.usage.iterations[].cache_creation`
- `.tool_use_result.usage.iterations[].cache_creation.ephemeral_1h_input_tokens`
- `.tool_use_result.usage.iterations[].cache_creation.ephemeral_5m_input_tokens`
- `.tool_use_result.usage.iterations[].cache_creation_input_tokens`
- `.tool_use_result.usage.iterations[].cache_read_input_tokens`
- `.tool_use_result.usage.iterations[].input_tokens`
- `.tool_use_result.usage.iterations[].output_tokens`
- `.tool_use_result.usage.iterations[].type`
- `.tool_use_result.usage.output_tokens`
- `.tool_use_result.usage.output_tokens_details`
- `.tool_use_result.usage.output_tokens_details.thinking_tokens`
- `.tool_use_result.usage.server_tool_use`
- `.tool_use_result.usage.server_tool_use.web_fetch_requests`
- `.tool_use_result.usage.server_tool_use.web_search_requests`
- `.tool_use_result.usage.service_tier`
- `.tool_use_result.usage.speed`
- `.tools`
- `.total_cost_usd`
- `.ttft_ms`
- `.ttft_stream_ms`
- `.type`
- `.usage`
- `.usage.cache_creation`
- `.usage.cache_creation.ephemeral_1h_input_tokens`
- `.usage.cache_creation.ephemeral_5m_input_tokens`
- `.usage.cache_creation_input_tokens`
- `.usage.cache_read_input_tokens`
- `.usage.duration_ms`
- `.usage.inference_geo`
- `.usage.input_tokens`
- `.usage.iterations`
- `.usage.iterations[].cache_creation`
- `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens`
- `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens`
- `.usage.iterations[].cache_creation_input_tokens`
- `.usage.iterations[].cache_read_input_tokens`
- `.usage.iterations[].input_tokens`
- `.usage.iterations[].output_tokens`
- `.usage.iterations[].type`
- `.usage.output_tokens`
- `.usage.output_tokens_details`
- `.usage.output_tokens_details.thinking_tokens`
- `.usage.server_tool_use`
- `.usage.server_tool_use.web_fetch_requests`
- `.usage.server_tool_use.web_search_requests`
- `.usage.service_tier`
- `.usage.speed`
- `.usage.tool_uses`
- `.usage.total_tokens`
- `.uuid`

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
