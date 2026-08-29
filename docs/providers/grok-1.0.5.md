# grok 1.0.5

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### run-grok

capture a plain Grok text turn, including its session/update command push

cwd: none set — the launch inherits the recorder's
env: (none set)
tools: (not observed)

```
<HOME>\.grok\bin\grok.exe
--no-auto-update
agent
--no-leader
stdio
```

### session-discovery-grok

the same ACP surface from Grok Build, the first ground-up ACP agent

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\.grok\bin\grok.exe
--no-auto-update
agent
--no-leader
stdio
```

### steer-grok

capture a Grok run receiving a queued steer, delivered as the next session/prompt on the same session (Grok advertises no in-turn steering extension)

cwd: none set — the launch inherits the recorder's
env: (none set)
tools: (not observed)

```
<HOME>\.grok\bin\grok.exe
--no-auto-update
agent
--no-leader
stdio
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: run-grok, session-discovery-grok, steer-grok
- `G2`: run-grok, steer-grok
- `G3`: steer-grok

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
- `.params.prompt` `G2`
- `.params.prompt[].text` `G2`
- `.params.prompt[].type` `G2`
- `.params.protocolVersion` `G1`
- `.params.sessionId` `G2`

### From provider

- `.id` `G1`
- `.jsonrpc` `G1`
- `.method` `G1`
- `.params` `G1`
- `.params._meta` `G1`
- `.params._meta.agentTimestampMs` `G1`
- `.params._meta.chunkId` `G2`
- `.params._meta.eventId` `G1`
- `.params._meta.promptId` `G2`
- `.params._meta.streamStartMs` `G2`
- `.params._meta.totalTokens` `G1`
- `.params._meta.turnStartMs` `G2`
- `.params._meta.updateParams` `G1`
- `.params._meta.updateParams.commandsCount` `G1`
- `.params._meta.updateParams.kind` `G3`
- `.params._meta.updateParams.status` `G3`
- `.params._meta.updateParams.title` `G3`
- `.params._meta.updateParams.toolCallId` `G3`
- `.params._meta.updateType` `G1`
- `.params.agentResult` `G2`
- `.params.allow_access` `G1`
- `.params.announcements` `G1`
- `.params.announcements[].cta` `G1`
- `.params.announcements[].cta.caption` `G1`
- `.params.announcements[].cta.label` `G1`
- `.params.announcements[].cta.url` `G1`
- `.params.announcements[].dismissible` `G1`
- `.params.announcements[].expires_at` `G1`
- `.params.announcements[].id` `G1`
- `.params.announcements[].message` `G1`
- `.params.announcements[].persistent` `G1`
- `.params.announcements[].severity` `G1`
- `.params.announcements[].title` `G1`
- `.params.announcements[].updated_at` `G1`
- `.params.auto_permission_mode_enabled` `G1`
- `.params.availableModels` `G1`
- `.params.availableModels[]._meta` `G1`
- `.params.availableModels[]._meta.agentType` `G1`
- `.params.availableModels[]._meta.reasoningEffort` `G1`
- `.params.availableModels[]._meta.reasoningEfforts` `G1`
- `.params.availableModels[]._meta.reasoningEfforts[].default` `G1`
- `.params.availableModels[]._meta.reasoningEfforts[].description` `G1`
- `.params.availableModels[]._meta.reasoningEfforts[].id` `G1`
- `.params.availableModels[]._meta.reasoningEfforts[].label` `G1`
- `.params.availableModels[]._meta.reasoningEfforts[].value` `G1`
- `.params.availableModels[]._meta.supportsReasoningEffort` `G1`
- `.params.availableModels[]._meta.totalContextTokens` `G1`
- `.params.availableModels[].description` `G1`
- `.params.availableModels[].modelId` `G1`
- `.params.availableModels[].name` `G1`
- `.params.campaigns` `G1`
- `.params.collapsed_edit_blocks` `G1`
- `.params.consent_gate` `G1`
- `.params.currentModelId` `G1`
- `.params.elapsedMs` `G1`
- `.params.entries` `G2`
- `.params.entries[].id` `G2`
- `.params.entries[].kind` `G2`
- `.params.entries[].position` `G2`
- `.params.entries[].text` `G2`
- `.params.entries[].version` `G2`
- `.params.gate_label` `G1`
- `.params.gate_message` `G1`
- `.params.gate_url` `G1`
- `.params.gen` `G1`
- `.params.group_tool_verbs` `G1`
- `.params.mcpServers` `G1`
- `.params.mcpToolCount` `G1`
- `.params.permission_mode` `G1`
- `.params.privacy_banner_reshow_days` `G1`
- `.params.privacy_notice_rollout` `G1`
- `.params.promptId` `G2`
- `.params.removed` `G2`
- `.params.runningKind` `G2`
- `.params.runningPromptId` `G2`
- `.params.runningText` `G2`
- `.params.sessionId` `G1`
- `.params.session_picker_grouped` `G1`
- `.params.sharing_enabled` `G1`
- `.params.show_resolved_model` `G1`
- `.params.slash_command_tags` `G1`
- `.params.stopReason` `G2`
- `.params.subscription_tier_display` `G1`
- `.params.subscription_watch_interval_secs` `G1`
- `.params.tips` `G1`
- `.params.update` `G1`
- `.params.update._meta` `G1`
- `.params.update._meta.modelId` `G2`
- `.params.update._meta.promptIndex` `G2`
- `.params.update._meta.tools` `G1`
- `.params.update._meta.x\.ai/tool` `G3`
- `.params.update._meta.x\.ai/tool.input` `G3`
- `.params.update._meta.x\.ai/tool.input.directory` `G3`
- `.params.update._meta.x\.ai/tool.input.limit` `G3`
- `.params.update._meta.x\.ai/tool.input.offset` `G3`
- `.params.update._meta.x\.ai/tool.input.path` `G3`
- `.params.update._meta.x\.ai/tool.input.pattern` `G3`
- `.params.update._meta.x\.ai/tool.kind` `G3`
- `.params.update._meta.x\.ai/tool.label` `G3`
- `.params.update._meta.x\.ai/tool.name` `G3`
- `.params.update._meta.x\.ai/tool.namespace` `G3`
- `.params.update._meta.x\.ai/tool.read_only` `G3`
- `.params.update._meta.x\.ai/tool.version` `G3`
- `.params.update.arguments_delta` `G3`
- `.params.update.availableCommands` `G1`
- `.params.update.availableCommands[]._meta` `G1`
- `.params.update.availableCommands[]._meta.bareName` `G1`
- `.params.update.availableCommands[]._meta.path` `G1`
- `.params.update.availableCommands[]._meta.pluginName` `G1`
- `.params.update.availableCommands[]._meta.qualifiedName` `G1`
- `.params.update.availableCommands[]._meta.scope` `G1`
- `.params.update.availableCommands[].description` `G1`
- `.params.update.availableCommands[].input` `G1`
- `.params.update.availableCommands[].input.hint` `G1`
- `.params.update.availableCommands[].name` `G1`
- `.params.update.content` `G2`
- `.params.update.content.text` `G2`
- `.params.update.content.type` `G2`
- `.params.update.content[].content` `G3`
- `.params.update.content[].content.text` `G3`
- `.params.update.content[].content.type` `G3`
- `.params.update.content[].type` `G3`
- `.params.update.kind` `G3`
- `.params.update.locations` `G3`
- `.params.update.locations[].line` `G3`
- `.params.update.locations[].path` `G3`
- `.params.update.name` `G3`
- `.params.update.prompt_id` `G2`
- `.params.update.rawInput` `G3`
- `.params.update.rawInput.path` `G3`
- `.params.update.rawInput.pattern` `G3`
- `.params.update.rawOutput` `G3`
- `.params.update.rawOutput.Content` `G3`
- `.params.update.rawOutput.Content.absolute_root_path` `G3`
- `.params.update.rawOutput.Content.content` `G3`
- `.params.update.rawOutput.FileContent` `G3`
- `.params.update.rawOutput.FileContent.absolute_path` `G3`
- `.params.update.rawOutput.FileContent.content` `G3`
- `.params.update.rawOutput.FileContent.content_concise` `G3`
- `.params.update.rawOutput.FileContent.limit` `G3`
- `.params.update.rawOutput.FileContent.offset` `G3`
- `.params.update.rawOutput.FileContent.raw_output` `G3`
- `.params.update.rawOutput.FileContent.total_lines` `G3`
- `.params.update.rawOutput.content` `G3`
- `.params.update.rawOutput.exit_code` `G3`
- `.params.update.rawOutput.file_matches` `G3`
- `.params.update.rawOutput.file_matches[].matches` `G3`
- `.params.update.rawOutput.file_matches[].matches[].content` `G3`
- `.params.update.rawOutput.file_matches[].matches[].line_number` `G3`
- `.params.update.rawOutput.file_matches[].path` `G3`
- `.params.update.rawOutput.match_count` `G3`
- `.params.update.rawOutput.result_count` `G3`
- `.params.update.rawOutput.stderr` `G3`
- `.params.update.rawOutput.stdout` `G3`
- `.params.update.rawOutput.type` `G3`
- `.params.update.sessionUpdate` `G1`
- `.params.update.session_summary` `G3`
- `.params.update.signature` `G2`
- `.params.update.status` `G3`
- `.params.update.stop_reason` `G2`
- `.params.update.title` `G3`
- `.params.update.toolCallId` `G3`
- `.params.update.tool_call_id` `G3`
- `.params.update.tool_index` `G3`
- `.params.update.usage` `G2`
- `.params.update.usage.apiDurationMs` `G2`
- `.params.update.usage.cacheCreationTokens` `G2`
- `.params.update.usage.cache_creation_input_tokens` `G2`
- `.params.update.usage.cache_read_input_tokens` `G2`
- `.params.update.usage.cachedReadTokens` `G2`
- `.params.update.usage.costUsdTicks` `G2`
- `.params.update.usage.inputTokens` `G2`
- `.params.update.usage.input_tokens` `G2`
- `.params.update.usage.modelCalls` `G2`
- `.params.update.usage.modelUsage` `G2`
- `.params.update.usage.modelUsage.{}.apiDurationMs` `G2`
- `.params.update.usage.modelUsage.{}.cacheCreationTokens` `G2`
- `.params.update.usage.modelUsage.{}.cachedReadTokens` `G2`
- `.params.update.usage.modelUsage.{}.costUsdTicks` `G2`
- `.params.update.usage.modelUsage.{}.inputTokens` `G2`
- `.params.update.usage.modelUsage.{}.modelCalls` `G2`
- `.params.update.usage.modelUsage.{}.outputTokens` `G2`
- `.params.update.usage.modelUsage.{}.reasoningTokens` `G2`
- `.params.update.usage.modelUsage.{}.totalTokens` `G2`
- `.params.update.usage.numTurns` `G2`
- `.params.update.usage.outputTokens` `G2`
- `.params.update.usage.output_tokens` `G2`
- `.params.update.usage.reasoningTokens` `G2`
- `.params.update.usage.reasoning_tokens` `G2`
- `.params.update.usage.totalTokens` `G2`
- `.params.upserted` `G2`
- `.params.upserted[].activity` `G2`
- `.params.upserted[].cwd` `G2`
- `.params.upserted[].isWorktree` `G2`
- `.params.upserted[].lastChangeUnixMs` `G2`
- `.params.upserted[].modelId` `G2`
- `.params.upserted[].origin` `G2`
- `.params.upserted[].origin.kind` `G2`
- `.params.upserted[].reasoningEffort` `G2`
- `.params.upserted[].resident` `G2`
- `.params.upserted[].sessionId` `G2`
- `.params.upserted[].title` `G2`
- `.params.upserted[].yolo` `G2`
- `.result` `G1`
- `.result._meta` `G1`
- `.result._meta.agentId` `G1`
- `.result._meta.agentInstanceId` `G1`
- `.result._meta.agentVersion` `G1`
- `.result._meta.availableCommands` `G1`
- `.result._meta.availableCommands[].description` `G1`
- `.result._meta.availableCommands[].input` `G1`
- `.result._meta.availableCommands[].input.hint` `G1`
- `.result._meta.availableCommands[].name` `G1`
- `.result._meta.cachedReadTokens` `G2`
- `.result._meta.cancelRewind` `G1`
- `.result._meta.codebaseIndexed` `G1`
- `.result._meta.currentWorkingDirectory` `G1`
- `.result._meta.defaultAuthMethodId` `G1`
- `.result._meta.feedbackEnabled` `G1`
- `.result._meta.gitRoot` `G1`
- `.result._meta.grokShell` `G1`
- `.result._meta.hostname` `G1`
- `.result._meta.inputTokens` `G2`
- `.result._meta.isGitRepo` `G1`
- `.result._meta.mcpApps` `G1`
- `.result._meta.mcpServers` `G1`
- `.result._meta.metadata` `G1`
- `.result._meta.modelId` `G2`
- `.result._meta.modelState` `G1`
- `.result._meta.modelState.availableModels` `G1`
- `.result._meta.modelState.availableModels[]._meta` `G1`
- `.result._meta.modelState.availableModels[]._meta.agentType` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEffort` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts[].default` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts[].description` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts[].id` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts[].label` `G1`
- `.result._meta.modelState.availableModels[]._meta.reasoningEfforts[].value` `G1`
- `.result._meta.modelState.availableModels[]._meta.supportsReasoningEffort` `G1`
- `.result._meta.modelState.availableModels[]._meta.totalContextTokens` `G1`
- `.result._meta.modelState.availableModels[].description` `G1`
- `.result._meta.modelState.availableModels[].modelId` `G1`
- `.result._meta.modelState.availableModels[].name` `G1`
- `.result._meta.modelState.currentModelId` `G1`
- `.result._meta.outputTokens` `G2`
- `.result._meta.promptId` `G2`
- `.result._meta.reasoningTokens` `G2`
- `.result._meta.requestId` `G2`
- `.result._meta.sessionId` `G2`
- `.result._meta.sessionRecap` `G1`
- `.result._meta.showNonGitWarning` `G1`
- `.result._meta.totalTokens` `G2`
- `.result._meta.usage` `G2`
- `.result._meta.usage.apiDurationMs` `G2`
- `.result._meta.usage.cacheCreationTokens` `G2`
- `.result._meta.usage.cachedReadTokens` `G2`
- `.result._meta.usage.costUsdTicks` `G2`
- `.result._meta.usage.inputTokens` `G2`
- `.result._meta.usage.modelCalls` `G2`
- `.result._meta.usage.modelUsage` `G2`
- `.result._meta.usage.modelUsage.{}.apiDurationMs` `G2`
- `.result._meta.usage.modelUsage.{}.cacheCreationTokens` `G2`
- `.result._meta.usage.modelUsage.{}.cachedReadTokens` `G2`
- `.result._meta.usage.modelUsage.{}.costUsdTicks` `G2`
- `.result._meta.usage.modelUsage.{}.inputTokens` `G2`
- `.result._meta.usage.modelUsage.{}.modelCalls` `G2`
- `.result._meta.usage.modelUsage.{}.outputTokens` `G2`
- `.result._meta.usage.modelUsage.{}.reasoningTokens` `G2`
- `.result._meta.usage.modelUsage.{}.totalTokens` `G2`
- `.result._meta.usage.numTurns` `G2`
- `.result._meta.usage.outputTokens` `G2`
- `.result._meta.usage.reasoningTokens` `G2`
- `.result._meta.usage.totalTokens` `G2`
- `.result._meta.voiceMode` `G1`
- `.result._meta.x\.ai/mcp/sdk` `G1`
- `.result._meta.x\.ai/pluginDirs` `G1`
- `.result._meta.x\.ai/schedulerBackgroundLoops` `G1`
- `.result._meta.x\.ai/sessionConfig` `G1`
- `.result._meta.x\.ai/sessionConfig.options` `G1`
- `.result._meta.x\.ai/sessionConfig.options[].category` `G1`
- `.result._meta.x\.ai/sessionConfig.options[].description` `G1`
- `.result._meta.x\.ai/sessionConfig.options[].id` `G1`
- `.result._meta.x\.ai/sessionConfig.options[].label` `G1`
- `.result._meta.x\.ai/sessionConfig.options[].selected` `G1`
- `.result._meta.x\.ai/sessionDetail` `G1`
- `.result._meta.x\.ai/sessionDetail.currentModelId` `G1`
- `.result._meta.x\.ai/sessionDetail.cwd` `G1`
- `.result._meta.x\.ai/sessionDetail.kind` `G1`
- `.result._meta.x\.ai/sessionDetail.sessionId` `G1`
- `.result.agentCapabilities` `G1`
- `.result.agentCapabilities._meta` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities.toolOverrides` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities.toolOverrides.x_keyword_search` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities.toolOverrides.x_semantic_search` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities.toolOverrides.x_thread_fetch` `G1`
- `.result.agentCapabilities._meta.x\.ai/capabilities.toolOverrides.x_user_search` `G1`
- `.result.agentCapabilities._meta.x\.ai/fs_notify` `G1`
- `.result.agentCapabilities._meta.x\.ai/hooks` `G1`
- `.result.agentCapabilities._meta.x\.ai/hooks.blockingEvents` `G1`
- `.result.agentCapabilities._meta.x\.ai/hooks.decisions` `G1`
- `.result.agentCapabilities._meta.x\.ai/hooks.stopSignals` `G1`
- `.result.agentCapabilities.auth` `G1`
- `.result.agentCapabilities.loadSession` `G1`
- `.result.agentCapabilities.mcpCapabilities` `G1`
- `.result.agentCapabilities.mcpCapabilities.http` `G1`
- `.result.agentCapabilities.mcpCapabilities.sse` `G1`
- `.result.agentCapabilities.promptCapabilities` `G1`
- `.result.agentCapabilities.promptCapabilities.audio` `G1`
- `.result.agentCapabilities.promptCapabilities.embeddedContext` `G1`
- `.result.agentCapabilities.promptCapabilities.image` `G1`
- `.result.agentCapabilities.sessionCapabilities` `G1`
- `.result.agentCapabilities.sessionCapabilities.close` `G1`
- `.result.agentCapabilities.sessionCapabilities.list` `G1`
- `.result.agentCapabilities.sessionCapabilities.resume` `G1`
- `.result.authMethods` `G1`
- `.result.authMethods[].description` `G1`
- `.result.authMethods[].id` `G1`
- `.result.authMethods[].name` `G1`
- `.result.models` `G1`
- `.result.models.availableModels` `G1`
- `.result.models.availableModels[]._meta` `G1`
- `.result.models.availableModels[]._meta.agentType` `G1`
- `.result.models.availableModels[]._meta.reasoningEffort` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts[].default` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts[].description` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts[].id` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts[].label` `G1`
- `.result.models.availableModels[]._meta.reasoningEfforts[].value` `G1`
- `.result.models.availableModels[]._meta.supportsReasoningEffort` `G1`
- `.result.models.availableModels[]._meta.totalContextTokens` `G1`
- `.result.models.availableModels[].description` `G1`
- `.result.models.availableModels[].modelId` `G1`
- `.result.models.availableModels[].name` `G1`
- `.result.models.currentModelId` `G1`
- `.result.protocolVersion` `G1`
- `.result.sessionId` `G1`
- `.result.stopReason` `G2`

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

- `initialize`
- `session/new`
- `session/prompt`

#### `.params.update.sessionUpdate`

(none observed)

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

- `_x.ai/announcements/update`
- `_x.ai/mcp/servers_updated`
- `_x.ai/mcp_initialized`
- `_x.ai/models/update`
- `_x.ai/queue/changed`
- `_x.ai/session/prompt_complete`
- `_x.ai/session_notification`
- `_x.ai/sessions/changed`
- `_x.ai/settings/update`
- `session/update`

#### `.params.update.sessionUpdate`

- `agent_message_chunk`
- `agent_thought_chunk`
- `available_commands_update`
- `interaction_resolved`
- `pending_interaction`
- `response_completed`
- `session_info_update`
- `session_summary_generated`
- `tool_call`
- `tool_call_delta_chunk`
- `tool_call_update`
- `turn_completed`
- `user_message_chunk`

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

(none observed)
