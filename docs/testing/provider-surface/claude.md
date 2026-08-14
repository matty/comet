# claude: the provider surface

**Generated. Do not edit.** Rebuilt from the promoted corpus, the decode sources, and
`crates/harness/tests/corpus/dispositions.json` by
`cargo test -p comet-harness --test capture_corpus surface_report`, which fails when this
file is stale. Decisions belong in `dispositions.json`, not here.

Every field the promoted evidence observes, with what Comet does about it. Values are
printed only where the value's own grammar makes that safe; prose is withheld and a
redacted value reports its kind. A `consumed` row marked *derived* is a name match in the
decode sources, which proves something mentions the field, never that a value reaches a
user.

## What the client reports (stdout, stderr)

### Unknown - 196 fields

Nobody has decided. This is the backlog.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.uuid` | string | 642 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | redacted: machine_id | - |
| `.event.index` | number | 364 | 2.1.228, 2.1.229 | claude/2.1.228/approval #8 | - | - |
| `.event.delta.partial_json` | string | 147 | 2.1.228, 2.1.229 | claude/2.1.228/approval #13 | redacted: assistant_prose; `` | - |
| `.estimated_tokens` | number | 76 | 2.1.229 | claude/2.1.229/checklist #8 | - | - |
| `.estimated_tokens_delta` | number | 76 | 2.1.229 | claude/2.1.229/checklist #8 | - | - |
| `.event.delta.estimated_tokens` | null | 63 | 2.1.229 | claude/2.1.229/checklist #9 | - | - |
| `.message.context_management` | null | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | - |
| `.message.stop_reason` | null | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | - |
| `.message.stop_sequence` | null | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | - |
| `.event.content_block` | object | 50 | 2.1.228, 2.1.229 | claude/2.1.228/approval #8 | - | - |
| `.message.diagnostics` | null/object | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | - |
| `.message.stop_details` | null | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | - |
| `.message.usage.cache_creation` | object | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | - |
| `.message.usage.cache_creation.ephemeral_1h_input_tokens` | number | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | - |
| `.message.usage.cache_creation.ephemeral_5m_input_tokens` | number | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | - |
| `.message.usage.inference_geo` | string | 36 | 2.1.229 | claude/2.1.229/checklist #18 | `not_available` | - |
| `.message.usage.service_tier` | string | 36 | 2.1.229 | claude/2.1.229/checklist #18 | `standard` | - |
| `.ttft_ms` | number | 29 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | - |
| `.response.response.models[].supportsAdaptiveThinking` | bool | 24 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.models[].supportsAutoMode` | bool | 24 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.models[].supportsEffort` | bool | 24 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.event.delta.stop_reason` | string | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | `end_turn` \| `tool_use` | - |
| `.event.delta.stop_sequence` | null | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | - |
| `.event.message.stop_reason` | null | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | - |
| `.event.message.stop_sequence` | null | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | - |
| `.message.content[].signature` | string | 21 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | redacted: claude_thinking_signature | - |
| `.plugins[].source` | string | 21 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `frontend-design@claude-plugins-official` \| `skill-creator@claude-plugins-official` \| `superpowers@claude-plugins-official` | - |
| `.event.content_block.signature` | string | 20 | 2.1.228, 2.1.229 | claude/2.1.228/approval #8 | `` | - |
| `.event.delta.signature` | string | 20 | 2.1.228, 2.1.229 | claude/2.1.228/approval #9 | redacted: claude_thinking_signature | - |
| `.fast_mode_disabled_reason` | string | 14 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `sdk_opt_in_required` | - |
| `.fast_mode_state` | string | 14 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `off` | - |
| `.message.content[].caller` | object | 14 | 2.1.229 | claude/2.1.229/checklist #25 | - | - |
| `.event.content_block.caller` | object | 13 | 2.1.229 | claude/2.1.229/checklist #20 | - | - |
| `.event.context_management` | object | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.context_management.applied_edits` | array | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.delta.stop_details` | null | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.message.diagnostics` | null | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | - |
| `.event.message.stop_details` | null | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | - |
| `.event.message.usage.cache_creation` | object | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | - |
| `.event.message.usage.cache_creation.ephemeral_1h_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | - |
| `.event.message.usage.cache_creation.ephemeral_5m_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | - |
| `.event.message.usage.inference_geo` | string | 13 | 2.1.229 | claude/2.1.229/checklist #6 | `not_available` | - |
| `.event.message.usage.service_tier` | string | 13 | 2.1.229 | claude/2.1.229/checklist #6 | `standard` | - |
| `.event.usage.iterations` | array | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.usage.iterations[].cache_creation` | object | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.usage.output_tokens_details` | object | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.event.usage.output_tokens_details.thinking_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | - |
| `.hook_event` | string | 12 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | `SessionStart` | - |
| `.hook_id` | string | 12 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | redacted: tool_use_id | - |
| `.hook_name` | string | 12 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | `SessionStart:startup` | - |
| `.response.response.models[].supportsFastMode` | bool | 8 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.analytics_disabled` | bool | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.apiKeySource` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `none` | - |
| `.api_error_status` | null | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.capabilities` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.claude_code_version` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `2.1.228` \| `2.1.229` | - |
| `.duration_api_ms` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.mcp_servers` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.memory_paths` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.memory_paths.auto` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | redacted: claude_memory_path | - |
| `.modelUsage.{}.cacheCreationInputTokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.cacheReadInputTokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.canonicalModel` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `claude-haiku-4-5` | - |
| `.modelUsage.{}.costUSD` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.inputTokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.maxOutputTokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.outputTokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.modelUsage.{}.provider` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `firstParty` | - |
| `.modelUsage.{}.webSearchRequests` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.num_turns` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.output_style` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `default` | - |
| `.permissionMode` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `acceptEdits` \| `default` | - |
| `.permission_denials` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.plugins` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.plugins[].version` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `6.2.0` \| `6.3.0` | - |
| `.product_feedback_disabled` | bool | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.skills` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.slash_commands` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | - |
| `.stop_reason` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `end_turn` | - |
| `.terminal_reason` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `completed` | - |
| `.time_to_request_ms` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.total_cost_usd` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.ttft_stream_ms` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.cache_creation` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.cache_creation.ephemeral_1h_input_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.cache_creation.ephemeral_5m_input_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.inference_geo` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `` \| `not_available` | - |
| `.usage.iterations` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.output_tokens_details` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.output_tokens_details.thinking_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.server_tool_use` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.server_tool_use.web_fetch_requests` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.server_tool_use.web_search_requests` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | - |
| `.usage.service_tier` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `standard` | - |
| `.usage.speed` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | `standard` | - |
| `.exit_code` | number | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #4 | - | - |
| `.output` | string | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #4 | redacted: provider_prose | - |
| `.stderr` | string | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #4 | redacted: provider_prose; `` | - |
| `.stdout` | string | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #4 | redacted: provider_prose | - |
| `.task_description` | string | 5 | 2.1.229 | claude/2.1.229/subagent #118 | redacted: assistant_prose | - |
| `.message.content[].input.taskId` | string | 4 | 2.1.229 | claude/2.1.229/checklist #88 | `1` \| `2` | - |
| `.response.response.account` | object | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.account.apiProvider` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `firstParty` | - |
| `.response.response.account.tokenSource` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `ANTHROPIC_AUTH_TOKEN` \| `none` | - |
| `.response.response.available_output_styles` | array | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.current_permission_mode` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `auto` | - |
| `.response.response.fast_mode_disabled_reason` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `sdk_opt_in_required` | - |
| `.response.response.fast_mode_state` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `off` | - |
| `.response.response.ide_rc_auto_enable_gate` | bool | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.output_style` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `default` | - |
| `.response.response.pid` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: machine_id | - |
| `.response.response.remote_control_auto_enable` | bool | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.response.response.remote_control_auto_on_by_default` | bool | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | - |
| `.tool_use_result.statusChange.from` | string | 4 | 2.1.229 | claude/2.1.229/checklist #93 | `in_progress` \| `pending` | - |
| `.tool_use_result.statusChange.to` | string | 4 | 2.1.229 | claude/2.1.229/checklist #93 | `completed` \| `in_progress` | - |
| `.tool_use_result.taskId` | string | 4 | 2.1.229 | claude/2.1.229/checklist #93 | `1` \| `2` | - |
| `.tool_use_result.updatedFields` | array | 4 | 2.1.229 | claude/2.1.229/checklist #93 | - | - |
| `.isSynthetic` | bool | 3 | 2.1.228 | claude/2.1.228/approval #26 | - | - |
| `.message.content[].input.activeForm` | string | 3 | 2.1.229 | claude/2.1.229/checklist #88 | _withheld_ | - |
| `.message.content[].input.file_path` | string | 3 | 2.1.228, 2.1.229 | claude/2.1.228/approval #100 | `<CWD>\README.md` \| `<CWD>\capture-marker.txt` | - |
| `.message.content[].input.max_results` | number | 3 | 2.1.229 | claude/2.1.229/checklist #25 | - | - |
| `.message.content[].input.query` | string | 3 | 2.1.229 | claude/2.1.229/checklist #25 | `select:TaskCreate` \| `select:TaskUpdate`; _withheld_ | - |
| `.message.content[].input.skill` | string | 3 | 2.1.228 | claude/2.1.228/approval #23 | `superpowers:using-superpowers` | - |
| `.message.content[].input.subject` | string | 3 | 2.1.229 | claude/2.1.229/checklist #55 | _withheld_ | - |
| `.rate_limit_info.isUsingOverage` | bool | 3 | 2.1.229 | claude/2.1.229/checklist #30 | - | - |
| `.rate_limit_info.overageDisabledReason` | string | 3 | 2.1.229 | claude/2.1.229/checklist #30 | `org_level_disabled` | - |
| `.rate_limit_info.overageStatus` | string | 3 | 2.1.229 | claude/2.1.229/checklist #30 | `rejected` | - |
| `.rate_limit_info.resetsAt` | number | 3 | 2.1.229 | claude/2.1.229/checklist #30 | - | - |
| `.terminal_slash_commands` | array | 3 | 2.1.229 | claude/2.1.229/checklist #4 | - | - |
| `.tool_use_result.commandName` | string | 3 | 2.1.228 | claude/2.1.228/approval #25 | `superpowers:using-superpowers` | - |
| `.tool_use_result.matches` | array | 3 | 2.1.229 | claude/2.1.229/checklist #27 | - | - |
| `.tool_use_result.query` | string | 3 | 2.1.229 | claude/2.1.229/checklist #27 | `select:TaskCreate` \| `select:TaskUpdate`; _withheld_ | - |
| `.tool_use_result.task.subject` | string | 3 | 2.1.229 | claude/2.1.229/checklist #64 | _withheld_ | - |
| `.tool_use_result.total_deferred_tools` | number | 3 | 2.1.229 | claude/2.1.229/checklist #27 | - | - |
| `.usage.iterations[].cache_creation` | object | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | - |
| `.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | - |
| `.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | - |
| `.message.diagnostics.cache_miss_reason` | object | 2 | 2.1.229 | claude/2.1.229/subagent #198 | - | - |
| `.message.diagnostics.cache_miss_reason.cache_missed_input_tokens` | number | 2 | 2.1.229 | claude/2.1.229/subagent #198 | - | - |
| `.output_file` | string | 2 | 2.1.229 | claude/2.1.229/subagent #125 | `<TEMP>\claude\<CWD>\<SESSION_ID_1>\tasks\<TOOL_USE_ID_5>.output` | - |
| `.patch.end_time` | number | 2 | 2.1.229 | claude/2.1.229/subagent #124 | - | - |
| `.task_type` | string | 2 | 2.1.229 | claude/2.1.229/subagent #116 | `local_agent` | - |
| `.last_tool_name` | string | 1 | 2.1.229 | claude/2.1.229/subagent #121 | `Read` | - |
| `.message.content[].input.limit` | number | 1 | 2.1.229 | claude/2.1.229/subagent #207 | - | - |
| `.message.content[].input.recipient` | string | 1 | 2.1.229 | claude/2.1.229/subagent #171 | redacted: tool_use_id | - |
| `.message.content[].input.run_in_background` | bool | 1 | 2.1.229 | claude/2.1.229/subagent #115 | - | - |
| `.message.content[].input.to` | string | 1 | 2.1.229 | claude/2.1.229/subagent #171 | redacted: tool_use_id | - |
| `.request.display_name` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `Write` | - |
| `.request.input.file_path` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `<CWD>\capture-marker.txt` | - |
| `.request.permission_suggestions` | array | 1 | 2.1.228 | claude/2.1.228/approval #102 | - | - |
| `.request.permission_suggestions[].destination` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `session` | - |
| `.request.permission_suggestions[].mode` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `acceptEdits` | - |
| `.tasks[].task_type` | string | 1 | 2.1.229 | claude/2.1.229/subagent #173 | `local_agent` | - |
| `.tool_use_result.agentType` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `general-purpose` | - |
| `.tool_use_result.file` | object | 1 | 2.1.229 | claude/2.1.229/subagent #209 | - | - |
| `.tool_use_result.file.filePath` | string | 1 | 2.1.229 | claude/2.1.229/subagent #209 | `<CWD>\README.md` | - |
| `.tool_use_result.file.numLines` | number | 1 | 2.1.229 | claude/2.1.229/subagent #209 | - | - |
| `.tool_use_result.file.startLine` | number | 1 | 2.1.229 | claude/2.1.229/subagent #209 | - | - |
| `.tool_use_result.file.totalLines` | number | 1 | 2.1.229 | claude/2.1.229/subagent #209 | - | - |
| `.tool_use_result.filePath` | string | 1 | 2.1.228 | claude/2.1.228/approval #104 | `<CWD>\capture-marker.txt` | - |
| `.tool_use_result.interrupted` | bool | 1 | 2.1.228 | claude/2.1.228/approval #56 | - | - |
| `.tool_use_result.isImage` | bool | 1 | 2.1.228 | claude/2.1.228/approval #56 | - | - |
| `.tool_use_result.noOutputExpected` | bool | 1 | 2.1.228 | claude/2.1.228/approval #56 | - | - |
| `.tool_use_result.originalFile` | null | 1 | 2.1.228 | claude/2.1.228/approval #104 | - | - |
| `.tool_use_result.pin` | object | 1 | 2.1.229 | claude/2.1.229/subagent #175 | - | - |
| `.tool_use_result.pin.ref` | string | 1 | 2.1.229 | claude/2.1.229/subagent #175 | `18303e` | - |
| `.tool_use_result.resumedAgentId` | string | 1 | 2.1.229 | claude/2.1.229/subagent #175 | redacted: tool_use_id | - |
| `.tool_use_result.stderr` | string | 1 | 2.1.228 | claude/2.1.228/approval #56 | `` | - |
| `.tool_use_result.stdout` | string | 1 | 2.1.228 | claude/2.1.228/approval #56 | `capture` | - |
| `.tool_use_result.structuredPatch` | array | 1 | 2.1.228 | claude/2.1.228/approval #104 | - | - |
| `.tool_use_result.toolStats` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.bashCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.editFileCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.linesAdded` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.linesRemoved` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.otherToolCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.readCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.toolStats.searchCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.cache_creation` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.cache_creation.ephemeral_1h_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.cache_creation.ephemeral_5m_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.inference_geo` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `not_available` | - |
| `.tool_use_result.usage.iterations` | array | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.iterations[].cache_creation` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.iterations[].cache_creation.ephemeral_1h_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.iterations[].cache_creation.ephemeral_5m_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.output_tokens_details` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.output_tokens_details.thinking_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.server_tool_use` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.server_tool_use.web_fetch_requests` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.server_tool_use.web_search_requests` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | - |
| `.tool_use_result.usage.service_tier` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `standard` | - |
| `.tool_use_result.usage.speed` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `standard` | - |
| `.tool_use_result.userModified` | bool | 1 | 2.1.228 | claude/2.1.228/approval #104 | - | - |

### Deferred - 2 fields

Worth building; each names its debt row.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.agents` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | **D31** - Nothing needs it: `agentType` arrives per-run on `task_started`, and these entries' `description` fields are long model-facing prose rather than card copy. The row records why it stays unread, not work queued. |
| `.response.response.agents` | array | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | **D31** - The same catalogue on the initialize reply; same decision as `.agents`. |

### Consumed - 170 fields

Something in Comet names this field.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.type` | string | 647 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | `assistant` \| `control_request` \| `control_response` \| `rate_limit_event` \| `result` \| `stream_event` \| `system` \| `user` | _derived_ |
| `.session_id` | string | 642 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | redacted: session_id | _derived_ |
| `.parent_tool_use_id` | null/string | 506 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | redacted: tool_use_id | _derived_ |
| `.event` | object | 430 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.type` | string | 430 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | `content_block_delta` \| `content_block_start` \| `content_block_stop` \| `message_delta` \| `message_start` \| `message_stop` | _derived_ |
| `.event.delta` | object | 286 | 2.1.228, 2.1.229 | claude/2.1.228/approval #9 | - | _derived_ |
| `.event.delta.type` | string | 264 | 2.1.228, 2.1.229 | claude/2.1.228/approval #9 | `input_json_delta` \| `signature_delta` \| `text_delta` \| `thinking_delta` | _derived_ |
| `.response.response.commands[].argumentHint` | string | 168 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: provider_prose; `` | _derived_ |
| `.response.response.commands[].description` | string | 168 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: provider_prose | _derived_ |
| `.response.response.commands[].name` | string | 168 | 2.1.228 | claude/2.1.228/command-discovery #5 | `commit-pr` \| `deep-research` \| `find-skills` \| `gpui-ui` \| `superpowers:brainstorming` \| `superpowers:dispatching-parallel-agents` \| `sync-upstream` \| `verify`; (more) | _derived_ |
| `.subtype` | string | 133 | 2.1.228, 2.1.229 | claude/2.1.228/approval #3 | `hook_response` \| `hook_started` \| `init` \| `status` \| `success` \| `task_progress` \| `task_started` \| `thinking_tokens`; (more) | _derived_ |
| `.message` | object | 76 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | _derived_ |
| `.message.content` | array | 76 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | _derived_ |
| `.message.content[].type` | string | 76 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | `text` \| `thinking` \| `tool_result` \| `tool_use` | _derived_ |
| `.message.role` | string | 76 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | `assistant` \| `user` | _derived_ |
| `.timestamp` | string | 76 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | `2026-08-12T12:28:48.310Z` \| `2026-08-12T12:28:48.612Z` \| `2026-08-12T12:28:48.619Z` \| `2026-08-12T12:28:48.620Z` \| `2026-08-12T12:28:52.217Z` \| `2026-08-12T12:28:52.632Z` \| `2026-08-12T12:28:52.862Z` \| `2026-08-12T12:28:53.583Z`; (more) | _derived_ |
| `.event.delta.thinking` | string | 63 | 2.1.229 | claude/2.1.229/checklist #9 | redacted: assistant_prose | _derived_ |
| `.message.id` | string | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | redacted: claude_message_id | _derived_ |
| `.message.model` | string | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | `claude-haiku-4-5-20251001` | _derived_ |
| `.message.type` | string | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | `message` | _derived_ |
| `.message.usage` | object | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | _derived_ |
| `.message.usage.input_tokens` | number | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | _derived_ |
| `.message.usage.output_tokens` | number | 53 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | - | _derived_ |
| `.event.content_block.type` | string | 50 | 2.1.228, 2.1.229 | claude/2.1.228/approval #8 | `text` \| `thinking` \| `tool_use` | _derived_ |
| `.response.response.commands[].aliases` | array | 44 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.request_id` | string | 37 | 2.1.228, 2.1.229 | claude/2.1.228/approval #102 | redacted: claude_request_id | _derived_ |
| `.message.usage.cache_creation_input_tokens` | number | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | _derived_ |
| `.message.usage.cache_read_input_tokens` | number | 36 | 2.1.229 | claude/2.1.229/checklist #18 | - | _derived_ |
| `.event.delta.text` | string | 34 | 2.1.228, 2.1.229 | claude/2.1.228/approval #36 | redacted: assistant_prose | _derived_ |
| `.response.response.models[].description` | string | 28 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: provider_prose | _derived_ |
| `.response.response.models[].displayName` | string | 28 | 2.1.228 | claude/2.1.228/command-discovery #5 | `Fable` \| `Haiku` \| `Sonnet` \| `gpt-5.6-sol`; _withheld_ | _derived_ |
| `.response.response.models[].resolvedModel` | string | 28 | 2.1.228 | claude/2.1.228/command-discovery #5 | `claude-fable-5[1m]` \| `claude-haiku-4-5-20251001` \| `claude-opus-5[1m]` \| `claude-sonnet-5` \| `claude-sonnet-5[1m]` \| `gpt-5.6-sol` | _derived_ |
| `.response.response.models[].value` | string | 28 | 2.1.228 | claude/2.1.228/command-discovery #5 | `claude-fable-5[1m]` \| `default` \| `gpt-5.6-sol` \| `haiku` \| `opus[1m]` \| `sonnet` \| `sonnet[1m]` | _derived_ |
| `.response.response.models[].supportedEffortLevels` | array | 24 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.status` | string | 24 | 2.1.228, 2.1.229 | claude/2.1.228/approval #6 | `completed` \| `requesting` | _derived_ |
| `.event.message` | object | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.message.content` | array | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.message.id` | string | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | redacted: claude_message_id | _derived_ |
| `.event.message.model` | string | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | `claude-haiku-4-5-20251001` | _derived_ |
| `.event.message.role` | string | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | `assistant` | _derived_ |
| `.event.message.type` | string | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | `message` | _derived_ |
| `.event.message.usage` | object | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.message.usage.input_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.message.usage.output_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #7 | - | _derived_ |
| `.event.usage` | object | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | _derived_ |
| `.event.usage.cache_creation_input_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | _derived_ |
| `.event.usage.cache_read_input_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | _derived_ |
| `.event.usage.input_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | _derived_ |
| `.event.usage.output_tokens` | number | 22 | 2.1.228, 2.1.229 | claude/2.1.228/approval #27 | - | _derived_ |
| `.message.content[].thinking` | string | 21 | 2.1.228, 2.1.229 | claude/2.1.228/approval #10 | redacted: assistant_prose | _derived_ |
| `.plugins[].name` | string | 21 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `frontend-design` \| `skill-creator` \| `superpowers` | _derived_ |
| `.plugins[].path` | string | 21 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | _withheld_ | _derived_ |
| `.event.content_block.thinking` | string | 20 | 2.1.228, 2.1.229 | claude/2.1.228/approval #8 | redacted: assistant_prose; `` | _derived_ |
| `.response.response.agents[].description` | string | 20 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: provider_prose | _derived_ |
| `.response.response.agents[].name` | string | 20 | 2.1.228 | claude/2.1.228/command-discovery #5 | `Explore` \| `Plan` \| `claude` \| `general-purpose` \| `statusline-setup` | _derived_ |
| `.message.content[].content` | array/string | 19 | 2.1.228, 2.1.229 | claude/2.1.228/approval #25 | redacted: user_text | _derived_ |
| `.message.content[].id` | string | 19 | 2.1.228, 2.1.229 | claude/2.1.228/approval #23 | redacted: tool_use_id | _derived_ |
| `.message.content[].input` | object | 19 | 2.1.228, 2.1.229 | claude/2.1.228/approval #23 | - | _derived_ |
| `.message.content[].name` | string | 19 | 2.1.228, 2.1.229 | claude/2.1.228/approval #23 | `Agent` \| `Bash` \| `Read` \| `Skill` \| `TaskCreate` \| `TaskUpdate` \| `ToolSearch` \| `Write`; (more) | _derived_ |
| `.message.content[].tool_use_id` | string | 19 | 2.1.228, 2.1.229 | claude/2.1.228/approval #25 | redacted: tool_use_id | _derived_ |
| `.event.content_block.id` | string | 18 | 2.1.228, 2.1.229 | claude/2.1.228/approval #12 | redacted: tool_use_id | _derived_ |
| `.event.content_block.input` | object | 18 | 2.1.228, 2.1.229 | claude/2.1.228/approval #12 | - | _derived_ |
| `.event.content_block.name` | string | 18 | 2.1.228, 2.1.229 | claude/2.1.228/approval #12 | `Agent` \| `Bash` \| `SendMessage` \| `Skill` \| `TaskCreate` \| `TaskUpdate` \| `ToolSearch` \| `Write`; (more) | _derived_ |
| `.tool_use_result` | object | 18 | 2.1.228, 2.1.229 | claude/2.1.228/approval #25 | - | _derived_ |
| `.message.content[].text` | string | 17 | 2.1.228, 2.1.229 | claude/2.1.228/approval #26 | redacted: assistant_prose, user_text | _derived_ |
| `.message.content[].caller.type` | string | 14 | 2.1.229 | claude/2.1.229/checklist #25 | `direct` | _derived_ |
| `.event.content_block.caller.type` | string | 13 | 2.1.229 | claude/2.1.229/checklist #20 | `direct` | _derived_ |
| `.event.message.usage.cache_creation_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | _derived_ |
| `.event.message.usage.cache_read_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #6 | - | _derived_ |
| `.event.usage.iterations[].cache_creation_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | _derived_ |
| `.event.usage.iterations[].cache_read_input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | _derived_ |
| `.event.usage.iterations[].input_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | _derived_ |
| `.event.usage.iterations[].output_tokens` | number | 13 | 2.1.229 | claude/2.1.229/checklist #28 | - | _derived_ |
| `.event.usage.iterations[].type` | string | 13 | 2.1.229 | claude/2.1.229/checklist #28 | `message` | _derived_ |
| `.event.content_block.text` | string | 12 | 2.1.228, 2.1.229 | claude/2.1.228/approval #35 | `` | _derived_ |
| `.usage` | object | 10 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.subagent_type` | string | 8 | 2.1.229 | claude/2.1.229/subagent #116 | `general-purpose` | _derived_ |
| `.tool_use_result.success` | bool | 8 | 2.1.228, 2.1.229 | claude/2.1.228/approval #25 | - | _derived_ |
| `.cwd` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | redacted: cwd_path | _derived_ |
| `.duration_ms` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.is_error` | bool | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.message.content[].content[].type` | string | 7 | 2.1.229 | claude/2.1.229/checklist #27 | `text` \| `tool_reference` | _derived_ |
| `.model` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | `claude-haiku-4-5-20251001` | _derived_ |
| `.modelUsage` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.modelUsage.{}.contextWindow` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.result` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | redacted: assistant_prose | _derived_ |
| `.task_id` | string | 7 | 2.1.229 | claude/2.1.229/subagent #116 | redacted: tool_use_id | _derived_ |
| `.tools` | array | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #5 | - | _derived_ |
| `.usage.cache_creation_input_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.usage.cache_read_input_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.usage.input_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.usage.output_tokens` | number | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #116 | - | _derived_ |
| `.outcome` | string | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #4 | `success` | _derived_ |
| `.tool_use_id` | string | 5 | 2.1.229 | claude/2.1.229/subagent #116 | redacted: tool_use_id | _derived_ |
| `.mcp_servers[].name` | string | 4 | 2.1.229 | claude/2.1.229/checklist #4 | _withheld_ | _derived_ |
| `.mcp_servers[].status` | string | 4 | 2.1.229 | claude/2.1.229/checklist #4 | `connected` \| `needs-auth` | _derived_ |
| `.message.content[].content[].tool_name` | string | 4 | 2.1.229 | claude/2.1.229/checklist #27 | `TaskCreate` \| `TaskUpdate` | _derived_ |
| `.message.content[].input.description` | string | 4 | 2.1.229 | claude/2.1.229/checklist #55 | redacted: assistant_prose; _withheld_ | _derived_ |
| `.message.content[].input.status` | string | 4 | 2.1.229 | claude/2.1.229/checklist #88 | `completed` \| `in_progress` | _derived_ |
| `.response` | object | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.response.request_id` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | redacted: claude_request_id | _derived_ |
| `.response.response` | object | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.response.response.agents[].model` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `sonnet` | _derived_ |
| `.response.response.commands` | array | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.response.response.models` | array | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | - | _derived_ |
| `.response.subtype` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #5 | `success` | _derived_ |
| `.tool_use_result.statusChange` | object | 4 | 2.1.229 | claude/2.1.229/checklist #93 | - | _derived_ |
| `.description` | string | 3 | 2.1.229 | claude/2.1.229/subagent #116 | redacted: assistant_prose | _derived_ |
| `.message.content[].content[].text` | string | 3 | 2.1.229 | claude/2.1.229/subagent #126 | redacted: user_text | _derived_ |
| `.rate_limit_info` | object | 3 | 2.1.229 | claude/2.1.229/checklist #30 | - | _derived_ |
| `.rate_limit_info.rateLimitType` | string | 3 | 2.1.229 | claude/2.1.229/checklist #30 | `five_hour` | _derived_ |
| `.rate_limit_info.status` | string | 3 | 2.1.229 | claude/2.1.229/checklist #30 | `allowed` | _derived_ |
| `.tool_use_result.task` | object | 3 | 2.1.229 | claude/2.1.229/checklist #64 | - | _derived_ |
| `.tool_use_result.task.id` | string | 3 | 2.1.229 | claude/2.1.229/checklist #64 | `1` \| `2` | _derived_ |
| `.usage.duration_ms` | number | 3 | 2.1.229 | claude/2.1.229/subagent #121 | - | _derived_ |
| `.usage.iterations[].cache_creation_input_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | _derived_ |
| `.usage.iterations[].cache_read_input_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | _derived_ |
| `.usage.iterations[].input_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | _derived_ |
| `.usage.iterations[].output_tokens` | number | 3 | 2.1.229 | claude/2.1.229/checklist #118 | - | _derived_ |
| `.usage.iterations[].type` | string | 3 | 2.1.229 | claude/2.1.229/checklist #118 | `message` | _derived_ |
| `.usage.tool_uses` | number | 3 | 2.1.229 | claude/2.1.229/subagent #121 | - | _derived_ |
| `.usage.total_tokens` | number | 3 | 2.1.229 | claude/2.1.229/subagent #121 | - | _derived_ |
| `.message.content[].input.content` | string | 2 | 2.1.228, 2.1.229 | claude/2.1.228/approval #100 | redacted: assistant_prose; _withheld_ | _derived_ |
| `.message.diagnostics.cache_miss_reason.type` | string | 2 | 2.1.229 | claude/2.1.229/subagent #198 | `tools_changed` | _derived_ |
| `.patch` | object | 2 | 2.1.229 | claude/2.1.229/subagent #124 | - | _derived_ |
| `.patch.status` | string | 2 | 2.1.229 | claude/2.1.229/subagent #124 | `completed` | _derived_ |
| `.prompt` | string | 2 | 2.1.229 | claude/2.1.229/subagent #116 | redacted: user_text | _derived_ |
| `.summary` | string | 2 | 2.1.229 | claude/2.1.229/subagent #125 | redacted: assistant_prose | _derived_ |
| `.tasks` | array | 2 | 2.1.229 | claude/2.1.229/subagent #173 | - | _derived_ |
| `.tool_use_result.content` | array/string | 2 | 2.1.228, 2.1.229 | claude/2.1.228/approval #104 | redacted: user_text | _derived_ |
| `.tool_use_result.type` | string | 2 | 2.1.228, 2.1.229 | claude/2.1.228/approval #104 | `create` \| `text` | _derived_ |
| `.message.content[].input.command` | string | 1 | 2.1.228 | claude/2.1.228/approval #52 | _withheld_ | _derived_ |
| `.message.content[].input.message` | string | 1 | 2.1.229 | claude/2.1.229/subagent #171 | _withheld_ | _derived_ |
| `.message.content[].input.prompt` | string | 1 | 2.1.229 | claude/2.1.229/subagent #115 | redacted: user_text | _derived_ |
| `.message.content[].input.subagent_type` | string | 1 | 2.1.229 | claude/2.1.229/subagent #115 | `general-purpose` | _derived_ |
| `.message.content[].input.type` | string | 1 | 2.1.229 | claude/2.1.229/subagent #171 | `message` | _derived_ |
| `.message.content[].is_error` | bool | 1 | 2.1.228 | claude/2.1.228/approval #56 | - | _derived_ |
| `.request` | object | 1 | 2.1.228 | claude/2.1.228/approval #102 | - | _derived_ |
| `.request.description` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `capture-marker.txt` | _derived_ |
| `.request.input` | object | 1 | 2.1.228 | claude/2.1.228/approval #102 | - | _derived_ |
| `.request.input.content` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | _withheld_ | _derived_ |
| `.request.permission_suggestions[].type` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `setMode` | _derived_ |
| `.request.subtype` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `can_use_tool` | _derived_ |
| `.request.tool_name` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | `Write` | _derived_ |
| `.request.tool_use_id` | string | 1 | 2.1.228 | claude/2.1.228/approval #102 | redacted: tool_use_id | _derived_ |
| `.tasks[].description` | string | 1 | 2.1.229 | claude/2.1.229/subagent #173 | redacted: assistant_prose | _derived_ |
| `.tasks[].task_id` | string | 1 | 2.1.229 | claude/2.1.229/subagent #173 | redacted: tool_use_id | _derived_ |
| `.tool_use_result.agentId` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | redacted: tool_use_id | _derived_ |
| `.tool_use_result.content[].text` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | redacted: user_text | _derived_ |
| `.tool_use_result.content[].type` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `text` | _derived_ |
| `.tool_use_result.file.content` | string | 1 | 2.1.229 | claude/2.1.229/subagent #209 | redacted: user_text | _derived_ |
| `.tool_use_result.message` | string | 1 | 2.1.229 | claude/2.1.229/subagent #175 | _withheld_ | _derived_ |
| `.tool_use_result.pin.id` | string | 1 | 2.1.229 | claude/2.1.229/subagent #175 | redacted: tool_use_id | _derived_ |
| `.tool_use_result.pin.name` | string | 1 | 2.1.229 | claude/2.1.229/subagent #175 | redacted: tool_use_id | _derived_ |
| `.tool_use_result.prompt` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | redacted: user_text | _derived_ |
| `.tool_use_result.resolvedModel` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `claude-haiku-4-5-20251001` | _derived_ |
| `.tool_use_result.status` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `completed` | _derived_ |
| `.tool_use_result.totalDurationMs` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.totalTokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.totalToolUseCount` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage` | object | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.cache_creation_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.cache_read_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.iterations[].cache_creation_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.iterations[].cache_read_input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.iterations[].input_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.iterations[].output_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |
| `.tool_use_result.usage.iterations[].type` | string | 1 | 2.1.229 | claude/2.1.229/subagent #126 | `message` | _derived_ |
| `.tool_use_result.usage.output_tokens` | number | 1 | 2.1.229 | claude/2.1.229/subagent #126 | - | _derived_ |

## How Comet drives the client (stdin)

### Unknown - 4 fields

Nobody has decided. This is the backlog.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.message.content[].source` | object | 1 | 2.1.228 | claude/2.1.228/attachment #1 | - | - |
| `.response.response.behavior` | string | 1 | 2.1.228 | claude/2.1.228/approval #103 | `allow` | - |
| `.response.response.updatedInput` | object | 1 | 2.1.228 | claude/2.1.228/approval #103 | - | - |
| `.response.response.updatedInput.file_path` | string | 1 | 2.1.228 | claude/2.1.228/approval #103 | `<CWD>\capture-marker.txt` | - |

### Consumed - 18 fields

Something in Comet names this field.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.type` | string | 12 | 2.1.228, 2.1.229 | claude/2.1.228/approval #1 | `control_request` \| `control_response` \| `user` | _derived_ |
| `.message` | object | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #1 | - | _derived_ |
| `.message.content` | array/string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #1 | redacted: user_text | _derived_ |
| `.message.role` | string | 7 | 2.1.228, 2.1.229 | claude/2.1.228/approval #1 | `user` | _derived_ |
| `.parent_tool_use_id` | null | 6 | 2.1.228, 2.1.229 | claude/2.1.228/approval #1 | - | _derived_ |
| `.request` | object | 4 | 2.1.228 | claude/2.1.228/command-discovery #1 | - | _derived_ |
| `.request.subtype` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #1 | `initialize` | _derived_ |
| `.request_id` | string | 4 | 2.1.228 | claude/2.1.228/command-discovery #1 | redacted: claude_request_id | _derived_ |
| `.message.content[].type` | string | 3 | 2.1.228, 2.1.229 | claude/2.1.228/attachment #1 | `image` \| `text` | _derived_ |
| `.message.content[].text` | string | 2 | 2.1.228, 2.1.229 | claude/2.1.228/attachment #1 | redacted: user_text | _derived_ |
| `.message.content[].source.data` | string | 1 | 2.1.228 | claude/2.1.228/attachment #1 | redacted: attachment_bytes | _derived_ |
| `.message.content[].source.media_type` | string | 1 | 2.1.228 | claude/2.1.228/attachment #1 | `image/png` | _derived_ |
| `.message.content[].source.type` | string | 1 | 2.1.228 | claude/2.1.228/attachment #1 | `base64` | _derived_ |
| `.response` | object | 1 | 2.1.228 | claude/2.1.228/approval #103 | - | _derived_ |
| `.response.request_id` | string | 1 | 2.1.228 | claude/2.1.228/approval #103 | redacted: claude_request_id | _derived_ |
| `.response.response` | object | 1 | 2.1.228 | claude/2.1.228/approval #103 | - | _derived_ |
| `.response.response.updatedInput.content` | string | 1 | 2.1.228 | claude/2.1.228/approval #103 | _withheld_ | _derived_ |
| `.response.subtype` | string | 1 | 2.1.228 | claude/2.1.228/approval #103 | `success` | _derived_ |

