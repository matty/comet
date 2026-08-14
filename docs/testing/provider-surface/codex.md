# codex: the provider surface

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

### Unknown - 153 fields

Nobody has decided. This is the backlog.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.emittedAtMs` | number | 104 | 0.147.0 | codex/0.147.0/fresh-text #3 | - | - |
| `.params.threadId` | string | 91 | 0.147.0 | codex/0.147.0/fresh-text #8 | redacted: thread_id | - |
| `.params.turnId` | string | 41 | 0.147.0 | codex/0.147.0/fresh-text #25 | redacted: turn_id | - |
| `.params.failureReason` | null | 33 | 0.147.0 | codex/0.147.0/fresh-text #8 | - | - |
| `.result.data[].additionalSpeedTiers` | array | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].availabilityNux` | null/object | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].defaultServiceTier` | null | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].modelSpecialty` | null | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].serviceTiers` | array | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].supportsPersonality` | bool | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.params.item` | object | 18 | 0.147.0 | codex/0.147.0/fresh-text #25 | - | - |
| `.params.itemId` | string | 18 | 0.147.0 | codex/0.147.0/fresh-text #28 | redacted: tool_use_id | - |
| `.params.completedAtMs` | number | 9 | 0.147.0 | codex/0.147.0/fresh-text #26 | - | - |
| `.params.startedAtMs` | number | 9 | 0.147.0 | codex/0.147.0/fresh-text #25 | - | - |
| `.params.item.clientId` | null | 8 | 0.147.0 | codex/0.147.0/fresh-text #25 | - | - |
| `.params.item.content[].text_elements` | array | 8 | 0.147.0 | codex/0.147.0/fresh-text #25 | - | - |
| `.params.item.memoryCitation` | null | 8 | 0.147.0 | codex/0.147.0/fresh-text #27 | - | - |
| `.params.item.phase` | string | 8 | 0.147.0 | codex/0.147.0/fresh-text #27 | `final_answer` | - |
| `.params.environmentId` | null | 7 | 0.147.0 | codex/0.147.0/fresh-text #3 | - | - |
| `.params.installationId` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #3 | redacted: machine_id | - |
| `.params.serverName` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #3 | redacted: machine_id | - |
| `.result.codexHome` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #2 | redacted: codex_home_path; `<HOME>\.codex` | - |
| `.result.platformFamily` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #2 | `windows` | - |
| `.result.platformOs` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #2 | `windows` | - |
| `.result.userAgent` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #2 | _withheld_ | - |
| `.params.turn.completedAt` | null/number | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | - |
| `.params.turn.durationMs` | null/number | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | - |
| `.params.turn.items` | array | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | - |
| `.params.turn.itemsView` | string | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | `notLoaded` \| `summary` | - |
| `.params.turn.startedAt` | number | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | - |
| `.result.data[].upgradeInfo.modelLink` | null | 6 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.result.data[].upgradeInfo.upgradeCopy` | null | 6 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | - |
| `.params.tokenUsage` | object | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.last.cacheWriteInputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.last.cachedInputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.last.inputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.last.outputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.last.reasoningOutputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.modelContextWindow` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total` | object | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total.cacheWriteInputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total.cachedInputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total.inputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total.outputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.tokenUsage.total.reasoningOutputTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | - |
| `.params.rateLimits.credits` | object | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.credits.balance` | string | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | `0` | - |
| `.params.rateLimits.credits.hasCredits` | bool | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.credits.unlimited` | bool | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.individualLimit` | null | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.limitId` | string | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | `codex` | - |
| `.params.rateLimits.limitName` | null | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.planType` | string | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | `prolite` | - |
| `.params.rateLimits.primary` | object | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.primary.resetsAt` | number | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.primary.windowDurationMins` | number | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.rateLimitReachedType` | null | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.secondary` | null | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.rateLimits.spendControlReached` | null | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | - |
| `.params.status.activeFlags` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #17 | - | - |
| `.params.threadSettings` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.activePermissionProfile` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.approvalPolicy` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `on-request` | - |
| `.params.threadSettings.approvalsReviewer` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `user` | - |
| `.params.threadSettings.collaborationMode` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.collaborationMode.mode` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `default` | - |
| `.params.threadSettings.collaborationMode.settings` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.collaborationMode.settings.developer_instructions` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.collaborationMode.settings.reasoning_effort` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `low` | - |
| `.params.threadSettings.effort` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `low` | - |
| `.params.threadSettings.modelProvider` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `openai` | - |
| `.params.threadSettings.multiAgentMode` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `explicitRequestOnly` | - |
| `.params.threadSettings.personality` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `pragmatic` | - |
| `.params.threadSettings.sandboxPolicy` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.sandboxPolicy.excludeSlashTmp` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.sandboxPolicy.excludeTmpdirEnvVar` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.sandboxPolicy.networkAccess` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.threadSettings.sandboxPolicy.writableRoots` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | - | - |
| `.params.turn.items[].memoryCitation` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #33 | - | - |
| `.params.turn.items[].phase` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #33 | `final_answer` | - |
| `.result.activePermissionProfile` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.approvalPolicy` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `on-request` | - |
| `.result.approvalsReviewer` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `user` | - |
| `.result.instructionSources` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.modelProvider` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `openai` | - |
| `.result.multiAgentMode` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `explicitRequestOnly` | - |
| `.result.runtimeWorkspaceRoots` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.sandbox` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.sandbox.excludeSlashTmp` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.sandbox.excludeTmpdirEnvVar` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.sandbox.networkAccess` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.sandbox.writableRoots` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.agentNickname` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.agentRole` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.canAcceptDirectInput` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.cliVersion` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `0.147.0` | - |
| `.result.thread.createdAt` | number | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.ephemeral` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.extra` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.forkedFromId` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.gitInfo` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.historyMode` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `legacy` | - |
| `.result.thread.modelProvider` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `openai` | - |
| `.result.thread.parentThreadId` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.preview` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | ``; _withheld_ | - |
| `.result.thread.recencyAt` | number | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.section` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.sectionEnteredAt` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.sessionId` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | redacted: session_id | - |
| `.result.thread.source` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `vscode` | - |
| `.result.thread.threadSource` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.turns` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.thread.updatedAt` | number | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | - |
| `.result.turn.completedAt` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | - |
| `.result.turn.durationMs` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | - |
| `.result.turn.items` | array | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | - |
| `.result.turn.itemsView` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | `notLoaded` | - |
| `.result.turn.startedAt` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | - |
| `.params.summaryIndex` | number | 2 | 0.147.0 | codex/0.147.0/steer #43 | - | - |
| `.params.thread.agentNickname` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.agentRole` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.canAcceptDirectInput` | bool | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.cliVersion` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `0.147.0` | - |
| `.params.thread.createdAt` | number | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.ephemeral` | bool | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.extra` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.forkedFromId` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.gitInfo` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.historyMode` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `legacy` | - |
| `.params.thread.modelProvider` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `openai` | - |
| `.params.thread.parentThreadId` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.preview` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `` | - |
| `.params.thread.recencyAt` | number | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.section` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.sectionEnteredAt` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.sessionId` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | redacted: session_id | - |
| `.params.thread.source` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `vscode` | - |
| `.params.thread.threadSource` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.turns` | array | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.params.thread.updatedAt` | number | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | - |
| `.result.initialTurnsPage` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.itemsBackwardsCursor` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].completedAt` | number | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].durationMs` | number | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].items` | array | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].itemsView` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | `full` | - |
| `.result.thread.turns[].items[].clientId` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].items[].content[].text_elements` | array | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].items[].memoryCitation` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.thread.turns[].items[].phase` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | `final_answer` | - |
| `.result.thread.turns[].startedAt` | number | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |
| `.result.turnId` | string | 1 | 0.147.0 | codex/0.147.0/steer #20 | redacted: turn_id | - |
| `.result.turnsBackwardsCursor` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | - |

### Deferred - 5 fields

Worth building; each names its debt row.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.result.data[].defaultReasoningEffort` | string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | `high` \| `low` \| `medium` | **D36** - Needs a declared default on `comet_proto::Model`, whose ladder carries none, so it costs a `PROTOCOL_VERSION` question. Read that constant's doc comment on whether an absent value is safe for an older peer before deciding. |
| `.result.data[].isDefault` | bool | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | **D72** - Let the live answer order the merged catalog in `codex/catalog.rs`, so `pickers::default_model`'s first-row rule lands on the model Codex actually names. Carrying it on `comet_proto::Model` instead would cost a `PROTOCOL_VERSION` question. Latent today: the curated list already leads with `gpt-5.6-sol`, so the two agree by coincidence. |
| `.result.data[].upgrade` | null/string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | `gpt-5.6-luna` \| `gpt-5.6-terra` | **D35** - A replacement model id. `AgentEvent::Notice` already exists as a surface, so no proto change is needed; the open question is whether a catalog reply is a legitimate producer for a notice. |
| `.result.data[].upgradeInfo` | null/object | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | **D35** - The object carrying `migrationMarkdown`; same decision as `.upgrade`. |
| `.result.data[].upgradeInfo.migrationMarkdown` | string | 6 | 0.147.0 | codex/0.147.0/model-discovery #6 | _withheld_ | **D35** - Provider-written deprecation copy, so it is prose and the walker withholds its value. Showing it means routing a catalog reply into `AgentEvent::Notice`. |

### Consumed - 82 fields

Something in Comet names this field.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.result.data[].supportedReasoningEfforts[].description` | string | 124 | 0.147.0 | codex/0.147.0/model-discovery #6 | redacted: provider_prose | _derived_ |
| `.result.data[].supportedReasoningEfforts[].reasoningEffort` | string | 124 | 0.147.0 | codex/0.147.0/model-discovery #6 | `high` \| `low` \| `max` \| `medium` \| `ultra` \| `xhigh` | _derived_ |
| `.method` | string | 104 | 0.147.0 | codex/0.147.0/fresh-text #3 | `item/completed` \| `item/started` \| `mcpServer/startupStatus/updated` \| `remoteControl/status/changed` \| `thread/settings/updated` \| `thread/started` \| `thread/status/changed` \| `turn/started`; (more) | _derived_ |
| `.params` | object | 104 | 0.147.0 | codex/0.147.0/fresh-text #3 | - | _derived_ |
| `.params.status` | object/string | 47 | 0.147.0 | codex/0.147.0/fresh-text #3 | `cancelled` \| `disabled` \| `ready` \| `starting` | _derived_ |
| `.params.error` | null | 33 | 0.147.0 | codex/0.147.0/fresh-text #8 | - | _derived_ |
| `.params.name` | string | 33 | 0.147.0 | codex/0.147.0/fresh-text #8 | redacted: codex_mcp_server_name | _derived_ |
| `.result.data[].description` | string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | redacted: provider_prose | _derived_ |
| `.result.data[].displayName` | string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | `GPT-5.2` \| `GPT-5.3-Codex-Spark` \| `GPT-5.4` \| `GPT-5.4-Mini` \| `GPT-5.5` \| `GPT-5.6-Luna` \| `GPT-5.6-Sol` \| `GPT-5.6-Terra` | _derived_ |
| `.result.data[].hidden` | bool | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | _derived_ |
| `.result.data[].id` | string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | `gpt-5.2` \| `gpt-5.3-codex-spark` \| `gpt-5.4` \| `gpt-5.4-mini` \| `gpt-5.5` \| `gpt-5.6-luna` \| `gpt-5.6-sol` \| `gpt-5.6-terra` | _derived_ |
| `.result.data[].inputModalities` | array | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | _derived_ |
| `.result.data[].model` | string | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | `gpt-5.2` \| `gpt-5.3-codex-spark` \| `gpt-5.4` \| `gpt-5.4-mini` \| `gpt-5.5` \| `gpt-5.6-luna` \| `gpt-5.6-sol` \| `gpt-5.6-terra` | _derived_ |
| `.result.data[].supportedReasoningEfforts` | array | 26 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | _derived_ |
| `.result.data[].serviceTiers[].description` | string | 19 | 0.147.0 | codex/0.147.0/model-discovery #6 | redacted: provider_prose | _derived_ |
| `.result.data[].serviceTiers[].id` | string | 19 | 0.147.0 | codex/0.147.0/model-discovery #6 | `priority` | _derived_ |
| `.result.data[].serviceTiers[].name` | string | 19 | 0.147.0 | codex/0.147.0/model-discovery #6 | `Fast` | _derived_ |
| `.id` | number/string | 18 | 0.147.0 | codex/0.147.0/fresh-text #2 | redacted: codex_rpc_id | _derived_ |
| `.params.item.id` | string | 18 | 0.147.0 | codex/0.147.0/fresh-text #25 | redacted: tool_use_id | _derived_ |
| `.params.item.type` | string | 18 | 0.147.0 | codex/0.147.0/fresh-text #25 | `agentMessage` \| `reasoning` \| `userMessage` | _derived_ |
| `.result` | object | 18 | 0.147.0 | codex/0.147.0/fresh-text #2 | - | _derived_ |
| `.params.delta` | string | 17 | 0.147.0 | codex/0.147.0/fresh-text #28 | redacted: assistant_prose | _derived_ |
| `.params.item.content` | array | 10 | 0.147.0 | codex/0.147.0/fresh-text #25 | - | _derived_ |
| `.params.item.content[].text` | string | 8 | 0.147.0 | codex/0.147.0/fresh-text #25 | redacted: user_text | _derived_ |
| `.params.item.content[].type` | string | 8 | 0.147.0 | codex/0.147.0/fresh-text #25 | `text` | _derived_ |
| `.params.item.text` | string | 8 | 0.147.0 | codex/0.147.0/fresh-text #27 | redacted: assistant_prose | _derived_ |
| `.params.status.type` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #17 | `active` \| `idle` | _derived_ |
| `.params.turn` | object | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | _derived_ |
| `.params.turn.error` | null | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | - | _derived_ |
| `.params.turn.id` | string | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | redacted: turn_id | _derived_ |
| `.params.turn.status` | string | 6 | 0.147.0 | codex/0.147.0/fresh-text #18 | `completed` \| `inProgress` | _derived_ |
| `.result.data[].upgradeInfo.model` | string | 6 | 0.147.0 | codex/0.147.0/model-discovery #6 | `gpt-5.6-luna` \| `gpt-5.6-terra` | _derived_ |
| `.params.tokenUsage.last` | object | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | _derived_ |
| `.params.tokenUsage.last.totalTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | _derived_ |
| `.params.tokenUsage.total.totalTokens` | number | 5 | 0.147.0 | codex/0.147.0/fresh-text #30 | - | _derived_ |
| `.params.rateLimits` | object | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | _derived_ |
| `.params.rateLimits.primary.usedPercent` | number | 4 | 0.147.0 | codex/0.147.0/fresh-text #31 | - | _derived_ |
| `.result.data` | array | 4 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | _derived_ |
| `.result.nextCursor` | null | 4 | 0.147.0 | codex/0.147.0/model-discovery #6 | - | _derived_ |
| `.params.threadSettings.collaborationMode.settings.model` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `gpt-5.6-luna` | _derived_ |
| `.params.threadSettings.cwd` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | redacted: cwd_path | _derived_ |
| `.params.threadSettings.model` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `gpt-5.6-luna` | _derived_ |
| `.params.threadSettings.sandboxPolicy.type` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `workspaceWrite` | _derived_ |
| `.params.threadSettings.serviceTier` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `default` | _derived_ |
| `.params.threadSettings.summary` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #16 | `auto` | _derived_ |
| `.params.turn.items[].id` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #33 | redacted: tool_use_id | _derived_ |
| `.params.turn.items[].text` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #33 | redacted: assistant_prose | _derived_ |
| `.params.turn.items[].type` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #33 | `agentMessage` | _derived_ |
| `.result.cwd` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | redacted: cwd_path | _derived_ |
| `.result.model` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `gpt-5.6-luna` | _derived_ |
| `.result.reasoningEffort` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `xhigh` | _derived_ |
| `.result.sandbox.type` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `workspaceWrite` | _derived_ |
| `.result.serviceTier` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `default` | _derived_ |
| `.result.thread` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | _derived_ |
| `.result.thread.cwd` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | redacted: cwd_path | _derived_ |
| `.result.thread.id` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | redacted: thread_id | _derived_ |
| `.result.thread.name` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | _derived_ |
| `.result.thread.path` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | redacted: codex_thread_path | _derived_ |
| `.result.thread.status` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | - | _derived_ |
| `.result.thread.status.type` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #6 | `idle` | _derived_ |
| `.result.turn` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | _derived_ |
| `.result.turn.error` | null | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | - | _derived_ |
| `.result.turn.id` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | redacted: turn_id | _derived_ |
| `.result.turn.status` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #11 | `inProgress` | _derived_ |
| `.params.item.summary` | array | 2 | 0.147.0 | codex/0.147.0/steer #42 | - | _derived_ |
| `.params.thread` | object | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | _derived_ |
| `.params.thread.cwd` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | redacted: cwd_path | _derived_ |
| `.params.thread.id` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | redacted: thread_id | _derived_ |
| `.params.thread.name` | null | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | _derived_ |
| `.params.thread.path` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | redacted: codex_thread_path | _derived_ |
| `.params.thread.status` | object | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | - | _derived_ |
| `.params.thread.status.type` | string | 2 | 0.147.0 | codex/0.147.0/fresh-text #7 | `idle` | _derived_ |
| `.result.data[].availabilityNux.message` | string | 2 | 0.147.0 | codex/0.147.0/model-discovery-logged-out #6 | redacted: provider_prose | _derived_ |
| `.result.thread.turns[].items[].id` | string | 2 | 0.147.0 | codex/0.147.0/resume #9 | redacted: tool_use_id | _derived_ |
| `.result.thread.turns[].items[].type` | string | 2 | 0.147.0 | codex/0.147.0/resume #9 | `agentMessage` \| `userMessage` | _derived_ |
| `.result.thread.turns[].error` | null | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | _derived_ |
| `.result.thread.turns[].id` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | redacted: turn_id | _derived_ |
| `.result.thread.turns[].items[].content` | array | 1 | 0.147.0 | codex/0.147.0/resume #9 | - | _derived_ |
| `.result.thread.turns[].items[].content[].text` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | redacted: user_text | _derived_ |
| `.result.thread.turns[].items[].content[].type` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | `text` | _derived_ |
| `.result.thread.turns[].items[].text` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | redacted: assistant_prose | _derived_ |
| `.result.thread.turns[].status` | string | 1 | 0.147.0 | codex/0.147.0/resume #9 | `completed` | _derived_ |

## How Comet drives the client (stdin)

### Unknown - 14 fields

Nobody has decided. This is the backlog.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.jsonrpc` | string | 25 | 0.147.0 | codex/0.147.0/fresh-text #1 | `2.0` | - |
| `.params.capabilities` | object | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | - | - |
| `.params.capabilities.experimentalApi` | bool | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | - | - |
| `.params.clientInfo` | object | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | - | - |
| `.params.clientInfo.title` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | `Comet` | - |
| `.params.clientInfo.version` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | `0.1.15` | - |
| `.params.approvalPolicy` | string | 6 | 0.147.0 | codex/0.147.0/fresh-text #5 | `on-request` | - |
| `.params.threadId` | string | 5 | 0.147.0 | codex/0.147.0/fresh-text #10 | redacted: thread_id | - |
| `.params.approvalsReviewer` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #5 | `user` | - |
| `.params.effort` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #10 | `low` | - |
| `.params.sandbox` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #5 | `workspace-write` | - |
| `.params.sandboxPolicy` | object | 3 | 0.147.0 | codex/0.147.0/fresh-text #10 | - | - |
| `.params.sandboxPolicy.networkAccess` | bool | 3 | 0.147.0 | codex/0.147.0/fresh-text #10 | - | - |
| `.params.expectedTurnId` | string | 1 | 0.147.0 | codex/0.147.0/steer #19 | redacted: turn_id | - |

### Consumed - 11 fields

Something in Comet names this field.

| field | type | n | versions | first seen | values | note |
| --- | --- | --- | --- | --- | --- | --- |
| `.method` | string | 25 | 0.147.0 | codex/0.147.0/fresh-text #1 | `initialize` \| `initialized` \| `model/list` \| `thread/resume` \| `thread/start` \| `turn/start` \| `turn/steer` | _derived_ |
| `.id` | string | 18 | 0.147.0 | codex/0.147.0/fresh-text #1 | redacted: codex_rpc_id | _derived_ |
| `.params` | object | 18 | 0.147.0 | codex/0.147.0/fresh-text #1 | - | _derived_ |
| `.params.clientInfo.name` | string | 7 | 0.147.0 | codex/0.147.0/fresh-text #1 | `comet-native` | _derived_ |
| `.params.model` | string | 6 | 0.147.0 | codex/0.147.0/fresh-text #5 | `gpt-5.6-luna` | _derived_ |
| `.params.input` | array | 4 | 0.147.0 | codex/0.147.0/fresh-text #10 | - | _derived_ |
| `.params.input[].text` | string | 4 | 0.147.0 | codex/0.147.0/fresh-text #10 | redacted: user_text | _derived_ |
| `.params.input[].type` | string | 4 | 0.147.0 | codex/0.147.0/fresh-text #10 | `text` | _derived_ |
| `.params.cwd` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #5 | redacted: cwd_path | _derived_ |
| `.params.sandboxPolicy.type` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #10 | `workspaceWrite` | _derived_ |
| `.params.summary` | string | 3 | 0.147.0 | codex/0.147.0/fresh-text #10 | `auto` | _derived_ |

