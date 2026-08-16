# codex 0.147.0

Generated from the committed capture corpus — never from a live CLI, never from what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit.

This file reports only what the scenarios below actually produced. Diffing this sheet against another version's sheet is the version-change report (no differ is planned).

Two readings that argv makes tempting and both wrong: identical launch flags do not mean identical coverage — a frame or reply that depends on something actually happening during the run only appears when a run produced that trigger, so the same flag present in both versions' scenarios is not evidence the underlying event fired in both. And a field or value that is new in one version is not necessarily a new capability — it can be account or environment state that simply did not happen to occur during the other version's runs, not a wire-format change. Argv, cwd, env and scenario names narrow what to check; they do not settle it on their own.

## Scenarios

Every scenario this sheet's evidence is drawn from: the exact argv, working directory and configured environment Comet launched it with (redaction placeholders are the archive's, not this sheet's). A capability no scenario here exercises cannot appear in the sections below, whatever the wire format might otherwise support — this list is what makes that limit visible instead of silent. A distinct name is not proof of distinct coverage, either, and a matching argv is not proof of an identical launch: two scenarios can print the same argv and still set different environment variables, and a placeholder's presence in one scenario's env line and its absence from another's is real evidence a claim of identical launches must survive. Compare the whole block — argv, cwd and env together — before concluding two scenarios were launched identically, and compare it again before concluding two with the same purpose sentence tested the same thing — trusting the name or the purpose alone is not enough either way. Even a whole-block comparison is not a sufficiency test, though: a redaction placeholder cannot separate two scenarios whose real value redacted to the same token, so two blocks that read byte-identical in every field can still have been launched with genuinely different values underneath — the archive's placeholders prove a difference when they show one, never that there wasn't one when they don't.

### approval

capture a Codex run that answers file-change approval requests

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### approval-on-request

capture a Codex run that answers command-execution approval requests against an external target

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### auto

capture what Codex's auto_review reviewer puts on the wire

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### fresh-text

capture a plain Codex text turn

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### full-access

capture what Codex's danger-full-access sandbox puts on the wire

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### interruption

capture a Codex run interrupted mid-turn

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

capture a Codex run resuming an existing thread

cwd: `<CWD>`
env: (none set)
tools: (not observed)

```
<HOME>\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe
app-server
```

### steer

capture a Codex run receiving a mid-turn steering message

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

- `G1`: approval
- `G2`: approval, approval-on-request
- `G3`: approval, approval-on-request, auto
- `G4`: approval, approval-on-request, auto, fresh-text, full-access, interruption, model-discovery, model-discovery-logged-out, resume, steer
- `G5`: approval, approval-on-request, auto, fresh-text, full-access, interruption, resume, steer
- `G6`: approval, approval-on-request, auto, fresh-text, full-access, interruption, steer
- `G7`: approval, approval-on-request, auto, fresh-text, full-access, resume, steer
- `G8`: approval, approval-on-request, auto, fresh-text, interruption, resume, steer
- `G9`: approval, approval-on-request, auto, steer
- `G10`: approval-on-request
- `G11`: approval-on-request, auto, fresh-text, interruption, resume, steer
- `G12`: interruption
- `G13`: model-discovery
- `G14`: model-discovery, model-discovery-logged-out
- `G15`: model-discovery-logged-out
- `G16`: resume
- `G17`: steer

### To provider

- `.id` `G4`
- `.jsonrpc` `G4`
- `.method` `G4`
- `.params` `G4`
- `.params.approvalPolicy` `G5`
- `.params.approvalsReviewer` `G5`
- `.params.capabilities` `G4`
- `.params.capabilities.experimentalApi` `G4`
- `.params.clientInfo` `G4`
- `.params.clientInfo.name` `G4`
- `.params.clientInfo.title` `G4`
- `.params.clientInfo.version` `G4`
- `.params.cwd` `G5`
- `.params.effort` `G5`
- `.params.expectedTurnId` `G17`
- `.params.input` `G5`
- `.params.input[].text` `G5`
- `.params.input[].type` `G5`
- `.params.model` `G5`
- `.params.sandbox` `G5`
- `.params.sandboxPolicy` `G5`
- `.params.sandboxPolicy.networkAccess` `G11`
- `.params.sandboxPolicy.type` `G5`
- `.params.summary` `G5`
- `.params.threadId` `G5`
- `.params.turnId` `G12`
- `.result` `G2`
- `.result.decision` `G2`

### From provider

- `.emittedAtMs` `G4`
- `.id` `G4`
- `.method` `G4`
- `.params` `G4`
- `.params.availableDecisions` `G10`
- `.params.availableDecisions[].acceptWithExecpolicyAmendment` `G10`
- `.params.availableDecisions[].acceptWithExecpolicyAmendment.execpolicy_amendment` `G10`
- `.params.command` `G10`
- `.params.commandActions` `G10`
- `.params.commandActions[].command` `G10`
- `.params.commandActions[].type` `G10`
- `.params.completedAtMs` `G5`
- `.params.cwd` `G10`
- `.params.delta` `G7`
- `.params.diff` `G1`
- `.params.environmentId` `G4`
- `.params.error` `G5`
- `.params.failureReason` `G5`
- `.params.grantRoot` `G1`
- `.params.installationId` `G4`
- `.params.item` `G5`
- `.params.item.aggregatedOutput` `G3`
- `.params.item.changes` `G1`
- `.params.item.changes[].diff` `G1`
- `.params.item.changes[].kind` `G1`
- `.params.item.changes[].kind.type` `G1`
- `.params.item.changes[].path` `G1`
- `.params.item.clientId` `G5`
- `.params.item.command` `G3`
- `.params.item.commandActions` `G3`
- `.params.item.commandActions[].command` `G3`
- `.params.item.commandActions[].type` `G3`
- `.params.item.content` `G5`
- `.params.item.content[].text` `G5`
- `.params.item.content[].text_elements` `G5`
- `.params.item.content[].type` `G5`
- `.params.item.cwd` `G3`
- `.params.item.durationMs` `G3`
- `.params.item.exitCode` `G3`
- `.params.item.id` `G5`
- `.params.item.memoryCitation` `G7`
- `.params.item.phase` `G7`
- `.params.item.pluginId` `G3`
- `.params.item.processId` `G3`
- `.params.item.scriptPath` `G3`
- `.params.item.source` `G3`
- `.params.item.status` `G3`
- `.params.item.summary` `G9`
- `.params.item.text` `G7`
- `.params.item.type` `G5`
- `.params.itemId` `G7`
- `.params.name` `G5`
- `.params.proposedExecpolicyAmendment` `G10`
- `.params.rateLimits` `G7`
- `.params.rateLimits.credits` `G7`
- `.params.rateLimits.credits.balance` `G7`
- `.params.rateLimits.credits.hasCredits` `G7`
- `.params.rateLimits.credits.unlimited` `G7`
- `.params.rateLimits.individualLimit` `G7`
- `.params.rateLimits.limitId` `G7`
- `.params.rateLimits.limitName` `G7`
- `.params.rateLimits.planType` `G7`
- `.params.rateLimits.primary` `G7`
- `.params.rateLimits.primary.resetsAt` `G7`
- `.params.rateLimits.primary.usedPercent` `G7`
- `.params.rateLimits.primary.windowDurationMins` `G7`
- `.params.rateLimits.rateLimitReachedType` `G7`
- `.params.rateLimits.secondary` `G7`
- `.params.rateLimits.spendControlReached` `G7`
- `.params.reason` `G2`
- `.params.requestId` `G2`
- `.params.serverName` `G4`
- `.params.startedAtMs` `G5`
- `.params.status` `G4`
- `.params.status.activeFlags` `G5`
- `.params.status.type` `G5`
- `.params.summaryIndex` `G9`
- `.params.thread` `G6`
- `.params.thread.agentNickname` `G6`
- `.params.thread.agentRole` `G6`
- `.params.thread.canAcceptDirectInput` `G6`
- `.params.thread.cliVersion` `G6`
- `.params.thread.createdAt` `G6`
- `.params.thread.cwd` `G6`
- `.params.thread.ephemeral` `G6`
- `.params.thread.extra` `G6`
- `.params.thread.forkedFromId` `G6`
- `.params.thread.gitInfo` `G6`
- `.params.thread.historyMode` `G6`
- `.params.thread.id` `G6`
- `.params.thread.modelProvider` `G6`
- `.params.thread.name` `G6`
- `.params.thread.parentThreadId` `G6`
- `.params.thread.path` `G6`
- `.params.thread.preview` `G6`
- `.params.thread.recencyAt` `G6`
- `.params.thread.section` `G6`
- `.params.thread.sectionEnteredAt` `G6`
- `.params.thread.sessionId` `G6`
- `.params.thread.source` `G6`
- `.params.thread.status` `G6`
- `.params.thread.status.type` `G6`
- `.params.thread.threadSource` `G6`
- `.params.thread.turns` `G6`
- `.params.thread.updatedAt` `G6`
- `.params.threadId` `G5`
- `.params.threadSettings` `G5`
- `.params.threadSettings.activePermissionProfile` `G5`
- `.params.threadSettings.approvalPolicy` `G5`
- `.params.threadSettings.approvalsReviewer` `G5`
- `.params.threadSettings.collaborationMode` `G5`
- `.params.threadSettings.collaborationMode.mode` `G5`
- `.params.threadSettings.collaborationMode.settings` `G5`
- `.params.threadSettings.collaborationMode.settings.developer_instructions` `G5`
- `.params.threadSettings.collaborationMode.settings.model` `G5`
- `.params.threadSettings.collaborationMode.settings.reasoning_effort` `G5`
- `.params.threadSettings.cwd` `G5`
- `.params.threadSettings.effort` `G5`
- `.params.threadSettings.model` `G5`
- `.params.threadSettings.modelProvider` `G5`
- `.params.threadSettings.multiAgentMode` `G5`
- `.params.threadSettings.personality` `G5`
- `.params.threadSettings.sandboxPolicy` `G5`
- `.params.threadSettings.sandboxPolicy.excludeSlashTmp` `G11`
- `.params.threadSettings.sandboxPolicy.excludeTmpdirEnvVar` `G11`
- `.params.threadSettings.sandboxPolicy.networkAccess` `G8`
- `.params.threadSettings.sandboxPolicy.type` `G5`
- `.params.threadSettings.sandboxPolicy.writableRoots` `G11`
- `.params.threadSettings.serviceTier` `G5`
- `.params.threadSettings.summary` `G5`
- `.params.tokenUsage` `G7`
- `.params.tokenUsage.last` `G7`
- `.params.tokenUsage.last.cacheWriteInputTokens` `G7`
- `.params.tokenUsage.last.cachedInputTokens` `G7`
- `.params.tokenUsage.last.inputTokens` `G7`
- `.params.tokenUsage.last.outputTokens` `G7`
- `.params.tokenUsage.last.reasoningOutputTokens` `G7`
- `.params.tokenUsage.last.totalTokens` `G7`
- `.params.tokenUsage.modelContextWindow` `G7`
- `.params.tokenUsage.total` `G7`
- `.params.tokenUsage.total.cacheWriteInputTokens` `G7`
- `.params.tokenUsage.total.cachedInputTokens` `G7`
- `.params.tokenUsage.total.inputTokens` `G7`
- `.params.tokenUsage.total.outputTokens` `G7`
- `.params.tokenUsage.total.reasoningOutputTokens` `G7`
- `.params.tokenUsage.total.totalTokens` `G7`
- `.params.turn` `G5`
- `.params.turn.completedAt` `G5`
- `.params.turn.durationMs` `G5`
- `.params.turn.error` `G5`
- `.params.turn.id` `G5`
- `.params.turn.items` `G5`
- `.params.turn.itemsView` `G5`
- `.params.turn.items[].id` `G7`
- `.params.turn.items[].memoryCitation` `G7`
- `.params.turn.items[].phase` `G7`
- `.params.turn.items[].text` `G7`
- `.params.turn.items[].type` `G7`
- `.params.turn.startedAt` `G5`
- `.params.turn.status` `G5`
- `.params.turnId` `G5`
- `.result` `G4`
- `.result.activePermissionProfile` `G5`
- `.result.approvalPolicy` `G5`
- `.result.approvalsReviewer` `G5`
- `.result.codexHome` `G4`
- `.result.cwd` `G5`
- `.result.data` `G14`
- `.result.data[].additionalSpeedTiers` `G14`
- `.result.data[].availabilityNux` `G14`
- `.result.data[].availabilityNux.message` `G15`
- `.result.data[].defaultReasoningEffort` `G14`
- `.result.data[].defaultServiceTier` `G14`
- `.result.data[].description` `G14`
- `.result.data[].displayName` `G14`
- `.result.data[].hidden` `G14`
- `.result.data[].id` `G14`
- `.result.data[].inputModalities` `G14`
- `.result.data[].isDefault` `G14`
- `.result.data[].model` `G14`
- `.result.data[].modelSpecialty` `G14`
- `.result.data[].serviceTiers` `G14`
- `.result.data[].serviceTiers[].description` `G14`
- `.result.data[].serviceTiers[].id` `G14`
- `.result.data[].serviceTiers[].name` `G14`
- `.result.data[].supportedReasoningEfforts` `G14`
- `.result.data[].supportedReasoningEfforts[].description` `G14`
- `.result.data[].supportedReasoningEfforts[].reasoningEffort` `G14`
- `.result.data[].supportsPersonality` `G14`
- `.result.data[].upgrade` `G14`
- `.result.data[].upgradeInfo` `G14`
- `.result.data[].upgradeInfo.migrationMarkdown` `G13`
- `.result.data[].upgradeInfo.model` `G13`
- `.result.data[].upgradeInfo.modelLink` `G13`
- `.result.data[].upgradeInfo.upgradeCopy` `G13`
- `.result.initialTurnsPage` `G16`
- `.result.instructionSources` `G5`
- `.result.itemsBackwardsCursor` `G16`
- `.result.model` `G5`
- `.result.modelProvider` `G5`
- `.result.multiAgentMode` `G5`
- `.result.nextCursor` `G14`
- `.result.platformFamily` `G4`
- `.result.platformOs` `G4`
- `.result.reasoningEffort` `G5`
- `.result.runtimeWorkspaceRoots` `G5`
- `.result.sandbox` `G5`
- `.result.sandbox.excludeSlashTmp` `G11`
- `.result.sandbox.excludeTmpdirEnvVar` `G11`
- `.result.sandbox.networkAccess` `G8`
- `.result.sandbox.type` `G5`
- `.result.sandbox.writableRoots` `G11`
- `.result.serviceTier` `G5`
- `.result.thread` `G5`
- `.result.thread.agentNickname` `G5`
- `.result.thread.agentRole` `G5`
- `.result.thread.canAcceptDirectInput` `G5`
- `.result.thread.cliVersion` `G5`
- `.result.thread.createdAt` `G5`
- `.result.thread.cwd` `G5`
- `.result.thread.ephemeral` `G5`
- `.result.thread.extra` `G5`
- `.result.thread.forkedFromId` `G5`
- `.result.thread.gitInfo` `G5`
- `.result.thread.historyMode` `G5`
- `.result.thread.id` `G5`
- `.result.thread.modelProvider` `G5`
- `.result.thread.name` `G5`
- `.result.thread.parentThreadId` `G5`
- `.result.thread.path` `G5`
- `.result.thread.preview` `G5`
- `.result.thread.recencyAt` `G5`
- `.result.thread.section` `G5`
- `.result.thread.sectionEnteredAt` `G5`
- `.result.thread.sessionId` `G5`
- `.result.thread.source` `G5`
- `.result.thread.status` `G5`
- `.result.thread.status.type` `G5`
- `.result.thread.threadSource` `G5`
- `.result.thread.turns` `G5`
- `.result.thread.turns[].completedAt` `G16`
- `.result.thread.turns[].durationMs` `G16`
- `.result.thread.turns[].error` `G16`
- `.result.thread.turns[].id` `G16`
- `.result.thread.turns[].items` `G16`
- `.result.thread.turns[].itemsView` `G16`
- `.result.thread.turns[].items[].clientId` `G16`
- `.result.thread.turns[].items[].content` `G16`
- `.result.thread.turns[].items[].content[].text` `G16`
- `.result.thread.turns[].items[].content[].text_elements` `G16`
- `.result.thread.turns[].items[].content[].type` `G16`
- `.result.thread.turns[].items[].id` `G16`
- `.result.thread.turns[].items[].memoryCitation` `G16`
- `.result.thread.turns[].items[].phase` `G16`
- `.result.thread.turns[].items[].text` `G16`
- `.result.thread.turns[].items[].type` `G16`
- `.result.thread.turns[].startedAt` `G16`
- `.result.thread.turns[].status` `G16`
- `.result.thread.updatedAt` `G5`
- `.result.turn` `G5`
- `.result.turn.completedAt` `G5`
- `.result.turn.durationMs` `G5`
- `.result.turn.error` `G5`
- `.result.turn.id` `G5`
- `.result.turn.items` `G5`
- `.result.turn.itemsView` `G5`
- `.result.turn.startedAt` `G5`
- `.result.turn.status` `G5`
- `.result.turnId` `G17`
- `.result.turnsBackwardsCursor` `G16`
- `.result.userAgent` `G4`

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
- `turn/interrupt`
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
- `item/commandExecution/requestApproval`
- `item/completed`
- `item/fileChange/requestApproval`
- `item/reasoning/summaryPartAdded`
- `item/reasoning/summaryTextDelta`
- `item/started`
- `mcpServer/startupStatus/updated`
- `remoteControl/status/changed`
- `serverRequest/resolved`
- `thread/goal/cleared`
- `thread/settings/updated`
- `thread/started`
- `thread/status/changed`
- `thread/tokenUsage/updated`
- `turn/completed`
- `turn/diff/updated`
- `turn/started`

#### `.request.subtype`

(none observed)

#### `.response.subtype`

(none observed)

#### `.subtype`

(none observed)

#### `.type`

(none observed)
