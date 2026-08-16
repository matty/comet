# codex 0.147.0

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### fresh-text

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery

capture Codex initialize and paged model/list replies

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### model-discovery-logged-out

capture Codex model discovery with an isolated empty Codex home

cwd: `<CWD>`
env: `CODEX_HOME=<CODEX_HOME>`
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### resume

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### steer

capture one bounded Codex run script

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

## Fields

Every dotted path observed on the wire for this provider and version, split by the direction it travelled — `To provider` is what Comet sends, `From provider` is what the provider sends back — one path per line, sorted, each tagged with the scenario group (below) that produced it. A field missing from this version's list is only evidence the CLI dropped it if the scenarios that group names are also present in the other version's own Scenarios section — a group made only of scenarios this version's Scenarios section doesn't have means the field was simply never exercised here, not removed.

### Scenario groups

- `G1`: fresh-text, model-discovery, model-discovery-logged-out, resume, steer
- `G2`: fresh-text, resume, steer
- `G3`: fresh-text, steer
- `G4`: model-discovery
- `G5`: model-discovery, model-discovery-logged-out
- `G6`: model-discovery-logged-out
- `G7`: resume
- `G8`: steer

### To provider

- `.id` `G1`
- `.jsonrpc` `G1`
- `.method` `G1`
- `.params` `G1`
- `.params.approvalPolicy` `G2`
- `.params.approvalsReviewer` `G2`
- `.params.capabilities` `G1`
- `.params.capabilities.experimentalApi` `G1`
- `.params.clientInfo` `G1`
- `.params.clientInfo.name` `G1`
- `.params.clientInfo.title` `G1`
- `.params.clientInfo.version` `G1`
- `.params.cwd` `G2`
- `.params.effort` `G2`
- `.params.expectedTurnId` `G8`
- `.params.input` `G2`
- `.params.input[].text` `G2`
- `.params.input[].type` `G2`
- `.params.model` `G2`
- `.params.sandbox` `G2`
- `.params.sandboxPolicy` `G2`
- `.params.sandboxPolicy.networkAccess` `G2`
- `.params.sandboxPolicy.type` `G2`
- `.params.summary` `G2`
- `.params.threadId` `G2`

### From provider

- `.emittedAtMs` `G1`
- `.id` `G1`
- `.method` `G1`
- `.params` `G1`
- `.params.completedAtMs` `G2`
- `.params.delta` `G2`
- `.params.environmentId` `G1`
- `.params.error` `G2`
- `.params.failureReason` `G2`
- `.params.installationId` `G1`
- `.params.item` `G2`
- `.params.item.clientId` `G2`
- `.params.item.content` `G2`
- `.params.item.content[].text` `G2`
- `.params.item.content[].text_elements` `G2`
- `.params.item.content[].type` `G2`
- `.params.item.id` `G2`
- `.params.item.memoryCitation` `G2`
- `.params.item.phase` `G2`
- `.params.item.summary` `G8`
- `.params.item.text` `G2`
- `.params.item.type` `G2`
- `.params.itemId` `G2`
- `.params.name` `G2`
- `.params.rateLimits` `G2`
- `.params.rateLimits.credits` `G2`
- `.params.rateLimits.credits.balance` `G2`
- `.params.rateLimits.credits.hasCredits` `G2`
- `.params.rateLimits.credits.unlimited` `G2`
- `.params.rateLimits.individualLimit` `G2`
- `.params.rateLimits.limitId` `G2`
- `.params.rateLimits.limitName` `G2`
- `.params.rateLimits.planType` `G2`
- `.params.rateLimits.primary` `G2`
- `.params.rateLimits.primary.resetsAt` `G2`
- `.params.rateLimits.primary.usedPercent` `G2`
- `.params.rateLimits.primary.windowDurationMins` `G2`
- `.params.rateLimits.rateLimitReachedType` `G2`
- `.params.rateLimits.secondary` `G2`
- `.params.rateLimits.spendControlReached` `G2`
- `.params.serverName` `G1`
- `.params.startedAtMs` `G2`
- `.params.status` `G1`
- `.params.status.activeFlags` `G2`
- `.params.status.type` `G2`
- `.params.summaryIndex` `G8`
- `.params.thread` `G3`
- `.params.thread.agentNickname` `G3`
- `.params.thread.agentRole` `G3`
- `.params.thread.canAcceptDirectInput` `G3`
- `.params.thread.cliVersion` `G3`
- `.params.thread.createdAt` `G3`
- `.params.thread.cwd` `G3`
- `.params.thread.ephemeral` `G3`
- `.params.thread.extra` `G3`
- `.params.thread.forkedFromId` `G3`
- `.params.thread.gitInfo` `G3`
- `.params.thread.historyMode` `G3`
- `.params.thread.id` `G3`
- `.params.thread.modelProvider` `G3`
- `.params.thread.name` `G3`
- `.params.thread.parentThreadId` `G3`
- `.params.thread.path` `G3`
- `.params.thread.preview` `G3`
- `.params.thread.recencyAt` `G3`
- `.params.thread.section` `G3`
- `.params.thread.sectionEnteredAt` `G3`
- `.params.thread.sessionId` `G3`
- `.params.thread.source` `G3`
- `.params.thread.status` `G3`
- `.params.thread.status.type` `G3`
- `.params.thread.threadSource` `G3`
- `.params.thread.turns` `G3`
- `.params.thread.updatedAt` `G3`
- `.params.threadId` `G2`
- `.params.threadSettings` `G2`
- `.params.threadSettings.activePermissionProfile` `G2`
- `.params.threadSettings.approvalPolicy` `G2`
- `.params.threadSettings.approvalsReviewer` `G2`
- `.params.threadSettings.collaborationMode` `G2`
- `.params.threadSettings.collaborationMode.mode` `G2`
- `.params.threadSettings.collaborationMode.settings` `G2`
- `.params.threadSettings.collaborationMode.settings.developer_instructions` `G2`
- `.params.threadSettings.collaborationMode.settings.model` `G2`
- `.params.threadSettings.collaborationMode.settings.reasoning_effort` `G2`
- `.params.threadSettings.cwd` `G2`
- `.params.threadSettings.effort` `G2`
- `.params.threadSettings.model` `G2`
- `.params.threadSettings.modelProvider` `G2`
- `.params.threadSettings.multiAgentMode` `G2`
- `.params.threadSettings.personality` `G2`
- `.params.threadSettings.sandboxPolicy` `G2`
- `.params.threadSettings.sandboxPolicy.excludeSlashTmp` `G2`
- `.params.threadSettings.sandboxPolicy.excludeTmpdirEnvVar` `G2`
- `.params.threadSettings.sandboxPolicy.networkAccess` `G2`
- `.params.threadSettings.sandboxPolicy.type` `G2`
- `.params.threadSettings.sandboxPolicy.writableRoots` `G2`
- `.params.threadSettings.serviceTier` `G2`
- `.params.threadSettings.summary` `G2`
- `.params.tokenUsage` `G2`
- `.params.tokenUsage.last` `G2`
- `.params.tokenUsage.last.cacheWriteInputTokens` `G2`
- `.params.tokenUsage.last.cachedInputTokens` `G2`
- `.params.tokenUsage.last.inputTokens` `G2`
- `.params.tokenUsage.last.outputTokens` `G2`
- `.params.tokenUsage.last.reasoningOutputTokens` `G2`
- `.params.tokenUsage.last.totalTokens` `G2`
- `.params.tokenUsage.modelContextWindow` `G2`
- `.params.tokenUsage.total` `G2`
- `.params.tokenUsage.total.cacheWriteInputTokens` `G2`
- `.params.tokenUsage.total.cachedInputTokens` `G2`
- `.params.tokenUsage.total.inputTokens` `G2`
- `.params.tokenUsage.total.outputTokens` `G2`
- `.params.tokenUsage.total.reasoningOutputTokens` `G2`
- `.params.tokenUsage.total.totalTokens` `G2`
- `.params.turn` `G2`
- `.params.turn.completedAt` `G2`
- `.params.turn.durationMs` `G2`
- `.params.turn.error` `G2`
- `.params.turn.id` `G2`
- `.params.turn.items` `G2`
- `.params.turn.itemsView` `G2`
- `.params.turn.items[].id` `G2`
- `.params.turn.items[].memoryCitation` `G2`
- `.params.turn.items[].phase` `G2`
- `.params.turn.items[].text` `G2`
- `.params.turn.items[].type` `G2`
- `.params.turn.startedAt` `G2`
- `.params.turn.status` `G2`
- `.params.turnId` `G2`
- `.result` `G1`
- `.result.activePermissionProfile` `G2`
- `.result.approvalPolicy` `G2`
- `.result.approvalsReviewer` `G2`
- `.result.codexHome` `G1`
- `.result.cwd` `G2`
- `.result.data` `G5`
- `.result.data[].additionalSpeedTiers` `G5`
- `.result.data[].availabilityNux` `G5`
- `.result.data[].availabilityNux.message` `G6`
- `.result.data[].defaultReasoningEffort` `G5`
- `.result.data[].defaultServiceTier` `G5`
- `.result.data[].description` `G5`
- `.result.data[].displayName` `G5`
- `.result.data[].hidden` `G5`
- `.result.data[].id` `G5`
- `.result.data[].inputModalities` `G5`
- `.result.data[].isDefault` `G5`
- `.result.data[].model` `G5`
- `.result.data[].modelSpecialty` `G5`
- `.result.data[].serviceTiers` `G5`
- `.result.data[].serviceTiers[].description` `G5`
- `.result.data[].serviceTiers[].id` `G5`
- `.result.data[].serviceTiers[].name` `G5`
- `.result.data[].supportedReasoningEfforts` `G5`
- `.result.data[].supportedReasoningEfforts[].description` `G5`
- `.result.data[].supportedReasoningEfforts[].reasoningEffort` `G5`
- `.result.data[].supportsPersonality` `G5`
- `.result.data[].upgrade` `G5`
- `.result.data[].upgradeInfo` `G5`
- `.result.data[].upgradeInfo.migrationMarkdown` `G4`
- `.result.data[].upgradeInfo.model` `G4`
- `.result.data[].upgradeInfo.modelLink` `G4`
- `.result.data[].upgradeInfo.upgradeCopy` `G4`
- `.result.initialTurnsPage` `G7`
- `.result.instructionSources` `G2`
- `.result.itemsBackwardsCursor` `G7`
- `.result.model` `G2`
- `.result.modelProvider` `G2`
- `.result.multiAgentMode` `G2`
- `.result.nextCursor` `G5`
- `.result.platformFamily` `G1`
- `.result.platformOs` `G1`
- `.result.reasoningEffort` `G2`
- `.result.runtimeWorkspaceRoots` `G2`
- `.result.sandbox` `G2`
- `.result.sandbox.excludeSlashTmp` `G2`
- `.result.sandbox.excludeTmpdirEnvVar` `G2`
- `.result.sandbox.networkAccess` `G2`
- `.result.sandbox.type` `G2`
- `.result.sandbox.writableRoots` `G2`
- `.result.serviceTier` `G2`
- `.result.thread` `G2`
- `.result.thread.agentNickname` `G2`
- `.result.thread.agentRole` `G2`
- `.result.thread.canAcceptDirectInput` `G2`
- `.result.thread.cliVersion` `G2`
- `.result.thread.createdAt` `G2`
- `.result.thread.cwd` `G2`
- `.result.thread.ephemeral` `G2`
- `.result.thread.extra` `G2`
- `.result.thread.forkedFromId` `G2`
- `.result.thread.gitInfo` `G2`
- `.result.thread.historyMode` `G2`
- `.result.thread.id` `G2`
- `.result.thread.modelProvider` `G2`
- `.result.thread.name` `G2`
- `.result.thread.parentThreadId` `G2`
- `.result.thread.path` `G2`
- `.result.thread.preview` `G2`
- `.result.thread.recencyAt` `G2`
- `.result.thread.section` `G2`
- `.result.thread.sectionEnteredAt` `G2`
- `.result.thread.sessionId` `G2`
- `.result.thread.source` `G2`
- `.result.thread.status` `G2`
- `.result.thread.status.type` `G2`
- `.result.thread.threadSource` `G2`
- `.result.thread.turns` `G2`
- `.result.thread.turns[].completedAt` `G7`
- `.result.thread.turns[].durationMs` `G7`
- `.result.thread.turns[].error` `G7`
- `.result.thread.turns[].id` `G7`
- `.result.thread.turns[].items` `G7`
- `.result.thread.turns[].itemsView` `G7`
- `.result.thread.turns[].items[].clientId` `G7`
- `.result.thread.turns[].items[].content` `G7`
- `.result.thread.turns[].items[].content[].text` `G7`
- `.result.thread.turns[].items[].content[].text_elements` `G7`
- `.result.thread.turns[].items[].content[].type` `G7`
- `.result.thread.turns[].items[].id` `G7`
- `.result.thread.turns[].items[].memoryCitation` `G7`
- `.result.thread.turns[].items[].phase` `G7`
- `.result.thread.turns[].items[].text` `G7`
- `.result.thread.turns[].items[].type` `G7`
- `.result.thread.turns[].startedAt` `G7`
- `.result.thread.turns[].status` `G7`
- `.result.thread.updatedAt` `G2`
- `.result.turn` `G2`
- `.result.turn.completedAt` `G2`
- `.result.turn.durationMs` `G2`
- `.result.turn.error` `G2`
- `.result.turn.id` `G2`
- `.result.turn.items` `G2`
- `.result.turn.itemsView` `G2`
- `.result.turn.startedAt` `G2`
- `.result.turn.status` `G2`
- `.result.turnId` `G8`
- `.result.turnsBackwardsCursor` `G7`
- `.result.userAgent` `G1`

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
