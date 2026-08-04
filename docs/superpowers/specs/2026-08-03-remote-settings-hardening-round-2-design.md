# Remote Settings Hardening Round 2 Design

## Goal

Close the remaining lifecycle, concurrency, recovery, secret-presentation, and endpoint-validation gaps in desktop remote settings without widening the LAN administration boundary.

## Durable add ownership

`AppState` owns a `RemoteAddCoordinator` with exactly one in-flight operation and a retrievable terminal public result. `RemoteConnectionsPage` moves the decoded zeroizing secret into the coordinator and never owns the pair-and-persist task. The task runs the entire pair then local `PUT_REMOTE` sequence from `Context<AppState>`, preventing page navigation, page replacement, or page drop from opening a cancellation window after remote trust succeeds.

A second submission is rejected while the coordinator is in flight. A newly attached page observes the same coordinator state and can render success, pairing failure, or partial success. A persistence failure after trust returns `PartialSuccess` containing public remote details, recovery instructions, and the local save error; it is also logged without secret material. App shutdown may abort the operation only because the process/runtime ends. If shutdown occurs after remote trust but before persistence completes, this is an acknowledged process-exit residual risk rather than a recoverable in-process page lifecycle.

## Destructive identity copy

Remove and revoke confirmations include friendly name plus stable server ID. Duplicate friendly names therefore produce distinguishable confirmation copy. Existing consequence text and cancel-before-RPC behavior remain unchanged.

## Watch ownership and recovery

Every remote/trusted watch subscribe attempt obtains the current local `RpcClient` from `AppState`; no loop captures an engine client across attempts. Engine replacement therefore changes the client used on the next retry. Watch backoff counts subscription failures, immediate closes, and malformed frames. Subscribe success alone does not reset it. The first valid decoded snapshot resets the sequence to 250 ms; otherwise delays grow 250, 500, 1000, 2000, and cap at 4000 ms. Page-owned task handles continue to cancel loops and stream receivers on page drop.

## Secret input and presentation

Remote pairing input uses a dedicated single-line `SecretInput` entity backed by `Zeroizing<String>`, never `ComposerInput`. Text replacement, typing, paste, clear, and drop operate on that buffer. Rendering produces only bullet/group separator text, so no plaintext is copied into framework-owned render strings. Debug output is redacted.

The server-generated pairing secret remains in zeroizing state and is masked by default. A `Copy Secret` action copies from a short-lived zeroizing source buffer into the system clipboard without logging or storing another UI plaintext copy. The UI explicitly warns that the copied value remains in the system clipboard until replaced. Copy-status state contains no secret and expires. No reveal mode is added.

`InstallationRemotePairer` moves/copies the decoded array directly into `Zeroizing<[u8; 16]>` at the narrow Task 4 API boundary; no unprotected intermediate array is retained by UI state.

## Endpoint validation

Bracketed hosts must parse as `std::net::Ipv6Addr`. Unbracketed IP literals are accepted where unambiguous. DNS hosts use explicit label rules: total length at most 253, labels 1–63 ASCII characters, alphanumeric edges, and only ASCII alphanumeric or hyphen internally. Whitespace, empty labels, underscores, leading/trailing hyphens, malformed IPv4-like values, and ambiguous bare IPv6 are rejected.

## Verification

Strict TDD adds focused failures before each production change:

- gated pair success followed by dropped page observer still reaches `PUT_REMOTE` and retains partial recovery;
- a concurrent second add is rejected and a reattached observer sees terminal state;
- duplicate-name revoke confirmations differ by stable server ID;
- swapping the local-client provider changes the client used on retry;
- close/malformed cycles accumulate capped backoff and a valid frame resets it;
- secret debug/render are redacted, clear/drop zeroize, and copy status is bounded and contains no plaintext;
- malformed IPv6 and DNS label cases are rejected.

Affected protocol, RPC, client, engine, and UI suites run after focused tests, followed by formatting, diff validation, and strict clippy attempts with baseline blockers recorded accurately.
