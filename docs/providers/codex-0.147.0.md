# codex 0.147.0

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned) — but before reading a disappearance as the CLI dropping a capability, check the Scenarios section of both sheets. A field or a vocabulary value present in one version and absent in the other may mean that version's captures simply never exercised it, not that the CLI changed; the corpus's blind spot is absence, and it did not go away just because this sheet makes it visible.

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and its presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### fresh-text

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery

capture Codex initialize and paged model/list replies

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery-logged-out

capture Codex model discovery with an isolated empty Codex home

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery-neutral-cwd

capture Codex model discovery from a neutral working directory

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery-project-cwd

capture Codex model discovery from the selected project directory

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### resume

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### steer

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted. Read an absent path against the Scenarios section above before reading it as a claim about the wire format.

### To provider

- `.id`
- `.jsonrpc`
- `.method`
- `.params`
- `.params.approvalPolicy`
- `.params.approvalsReviewer`
- `.params.capabilities`
- `.params.capabilities.experimentalApi`
- `.params.clientInfo`
- `.params.clientInfo.name`
- `.params.clientInfo.title`
- `.params.clientInfo.version`
- `.params.cwd`
- `.params.effort`
- `.params.expectedTurnId`
- `.params.input`
- `.params.input[].text`
- `.params.input[].type`
- `.params.model`
- `.params.sandbox`
- `.params.sandboxPolicy`
- `.params.sandboxPolicy.networkAccess`
- `.params.sandboxPolicy.type`
- `.params.summary`
- `.params.threadId`

### From provider

- `.emittedAtMs`
- `.id`
- `.method`
- `.params`
- `.params.completedAtMs`
- `.params.delta`
- `.params.environmentId`
- `.params.error`
- `.params.failureReason`
- `.params.installationId`
- `.params.item`
- `.params.item.clientId`
- `.params.item.content`
- `.params.item.content[].text`
- `.params.item.content[].text_elements`
- `.params.item.content[].type`
- `.params.item.id`
- `.params.item.memoryCitation`
- `.params.item.phase`
- `.params.item.summary`
- `.params.item.text`
- `.params.item.type`
- `.params.itemId`
- `.params.name`
- `.params.rateLimits`
- `.params.rateLimits.credits`
- `.params.rateLimits.credits.balance`
- `.params.rateLimits.credits.hasCredits`
- `.params.rateLimits.credits.unlimited`
- `.params.rateLimits.individualLimit`
- `.params.rateLimits.limitId`
- `.params.rateLimits.limitName`
- `.params.rateLimits.planType`
- `.params.rateLimits.primary`
- `.params.rateLimits.primary.resetsAt`
- `.params.rateLimits.primary.usedPercent`
- `.params.rateLimits.primary.windowDurationMins`
- `.params.rateLimits.rateLimitReachedType`
- `.params.rateLimits.secondary`
- `.params.rateLimits.spendControlReached`
- `.params.serverName`
- `.params.startedAtMs`
- `.params.status`
- `.params.status.activeFlags`
- `.params.status.type`
- `.params.summaryIndex`
- `.params.thread`
- `.params.thread.agentNickname`
- `.params.thread.agentRole`
- `.params.thread.canAcceptDirectInput`
- `.params.thread.cliVersion`
- `.params.thread.createdAt`
- `.params.thread.cwd`
- `.params.thread.ephemeral`
- `.params.thread.extra`
- `.params.thread.forkedFromId`
- `.params.thread.gitInfo`
- `.params.thread.historyMode`
- `.params.thread.id`
- `.params.thread.modelProvider`
- `.params.thread.name`
- `.params.thread.parentThreadId`
- `.params.thread.path`
- `.params.thread.preview`
- `.params.thread.recencyAt`
- `.params.thread.section`
- `.params.thread.sectionEnteredAt`
- `.params.thread.sessionId`
- `.params.thread.source`
- `.params.thread.status`
- `.params.thread.status.type`
- `.params.thread.threadSource`
- `.params.thread.turns`
- `.params.thread.updatedAt`
- `.params.threadId`
- `.params.threadSettings`
- `.params.threadSettings.activePermissionProfile`
- `.params.threadSettings.approvalPolicy`
- `.params.threadSettings.approvalsReviewer`
- `.params.threadSettings.collaborationMode`
- `.params.threadSettings.collaborationMode.mode`
- `.params.threadSettings.collaborationMode.settings`
- `.params.threadSettings.collaborationMode.settings.developer_instructions`
- `.params.threadSettings.collaborationMode.settings.model`
- `.params.threadSettings.collaborationMode.settings.reasoning_effort`
- `.params.threadSettings.cwd`
- `.params.threadSettings.effort`
- `.params.threadSettings.model`
- `.params.threadSettings.modelProvider`
- `.params.threadSettings.multiAgentMode`
- `.params.threadSettings.personality`
- `.params.threadSettings.sandboxPolicy`
- `.params.threadSettings.sandboxPolicy.excludeSlashTmp`
- `.params.threadSettings.sandboxPolicy.excludeTmpdirEnvVar`
- `.params.threadSettings.sandboxPolicy.networkAccess`
- `.params.threadSettings.sandboxPolicy.type`
- `.params.threadSettings.sandboxPolicy.writableRoots`
- `.params.threadSettings.serviceTier`
- `.params.threadSettings.summary`
- `.params.tokenUsage`
- `.params.tokenUsage.last`
- `.params.tokenUsage.last.cacheWriteInputTokens`
- `.params.tokenUsage.last.cachedInputTokens`
- `.params.tokenUsage.last.inputTokens`
- `.params.tokenUsage.last.outputTokens`
- `.params.tokenUsage.last.reasoningOutputTokens`
- `.params.tokenUsage.last.totalTokens`
- `.params.tokenUsage.modelContextWindow`
- `.params.tokenUsage.total`
- `.params.tokenUsage.total.cacheWriteInputTokens`
- `.params.tokenUsage.total.cachedInputTokens`
- `.params.tokenUsage.total.inputTokens`
- `.params.tokenUsage.total.outputTokens`
- `.params.tokenUsage.total.reasoningOutputTokens`
- `.params.tokenUsage.total.totalTokens`
- `.params.turn`
- `.params.turn.completedAt`
- `.params.turn.durationMs`
- `.params.turn.error`
- `.params.turn.id`
- `.params.turn.items`
- `.params.turn.itemsView`
- `.params.turn.items[].id`
- `.params.turn.items[].memoryCitation`
- `.params.turn.items[].phase`
- `.params.turn.items[].text`
- `.params.turn.items[].type`
- `.params.turn.startedAt`
- `.params.turn.status`
- `.params.turnId`
- `.result`
- `.result.activePermissionProfile`
- `.result.approvalPolicy`
- `.result.approvalsReviewer`
- `.result.codexHome`
- `.result.cwd`
- `.result.data`
- `.result.data[].additionalSpeedTiers`
- `.result.data[].availabilityNux`
- `.result.data[].availabilityNux.message`
- `.result.data[].defaultReasoningEffort`
- `.result.data[].defaultServiceTier`
- `.result.data[].description`
- `.result.data[].displayName`
- `.result.data[].hidden`
- `.result.data[].id`
- `.result.data[].inputModalities`
- `.result.data[].isDefault`
- `.result.data[].model`
- `.result.data[].modelSpecialty`
- `.result.data[].serviceTiers`
- `.result.data[].serviceTiers[].description`
- `.result.data[].serviceTiers[].id`
- `.result.data[].serviceTiers[].name`
- `.result.data[].supportedReasoningEfforts`
- `.result.data[].supportedReasoningEfforts[].description`
- `.result.data[].supportedReasoningEfforts[].reasoningEffort`
- `.result.data[].supportsPersonality`
- `.result.data[].upgrade`
- `.result.data[].upgradeInfo`
- `.result.data[].upgradeInfo.migrationMarkdown`
- `.result.data[].upgradeInfo.model`
- `.result.data[].upgradeInfo.modelLink`
- `.result.data[].upgradeInfo.upgradeCopy`
- `.result.initialTurnsPage`
- `.result.instructionSources`
- `.result.itemsBackwardsCursor`
- `.result.model`
- `.result.modelProvider`
- `.result.multiAgentMode`
- `.result.nextCursor`
- `.result.platformFamily`
- `.result.platformOs`
- `.result.reasoningEffort`
- `.result.runtimeWorkspaceRoots`
- `.result.sandbox`
- `.result.sandbox.excludeSlashTmp`
- `.result.sandbox.excludeTmpdirEnvVar`
- `.result.sandbox.networkAccess`
- `.result.sandbox.type`
- `.result.sandbox.writableRoots`
- `.result.serviceTier`
- `.result.thread`
- `.result.thread.agentNickname`
- `.result.thread.agentRole`
- `.result.thread.canAcceptDirectInput`
- `.result.thread.cliVersion`
- `.result.thread.createdAt`
- `.result.thread.cwd`
- `.result.thread.ephemeral`
- `.result.thread.extra`
- `.result.thread.forkedFromId`
- `.result.thread.gitInfo`
- `.result.thread.historyMode`
- `.result.thread.id`
- `.result.thread.modelProvider`
- `.result.thread.name`
- `.result.thread.parentThreadId`
- `.result.thread.path`
- `.result.thread.preview`
- `.result.thread.recencyAt`
- `.result.thread.section`
- `.result.thread.sectionEnteredAt`
- `.result.thread.sessionId`
- `.result.thread.source`
- `.result.thread.status`
- `.result.thread.status.type`
- `.result.thread.threadSource`
- `.result.thread.turns`
- `.result.thread.turns[].completedAt`
- `.result.thread.turns[].durationMs`
- `.result.thread.turns[].error`
- `.result.thread.turns[].id`
- `.result.thread.turns[].items`
- `.result.thread.turns[].itemsView`
- `.result.thread.turns[].items[].clientId`
- `.result.thread.turns[].items[].content`
- `.result.thread.turns[].items[].content[].text`
- `.result.thread.turns[].items[].content[].text_elements`
- `.result.thread.turns[].items[].content[].type`
- `.result.thread.turns[].items[].id`
- `.result.thread.turns[].items[].memoryCitation`
- `.result.thread.turns[].items[].phase`
- `.result.thread.turns[].items[].text`
- `.result.thread.turns[].items[].type`
- `.result.thread.turns[].startedAt`
- `.result.thread.turns[].status`
- `.result.thread.updatedAt`
- `.result.turn`
- `.result.turn.completedAt`
- `.result.turn.durationMs`
- `.result.turn.error`
- `.result.turn.id`
- `.result.turn.items`
- `.result.turn.itemsView`
- `.result.turn.startedAt`
- `.result.turn.status`
- `.result.turnId`
- `.result.turnsBackwardsCursor`
- `.result.userAgent`

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

- `initialize`
- `initialized`
- `model/list`
- `thread/resume`
- `thread/start`
- `turn/start`
- `turn/steer`

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

- `account/rateLimits/updated`
- `item/agentMessage/delta`
- `item/completed`
- `item/reasoning/summaryPartAdded`
- `item/reasoning/summaryTextDelta`
- `item/started`
- `mcpServer/startupStatus/updated`
- `remoteControl/status/changed`
- `thread/goal/cleared`
- `thread/settings/updated`
- `thread/started`
- `thread/status/changed`
- `thread/tokenUsage/updated`
- `turn/completed`
- `turn/started`

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

(none observed)
