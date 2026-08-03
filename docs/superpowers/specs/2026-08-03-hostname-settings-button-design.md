# Hostname Device Name and Settings Button Design

## Goal

Make a new Comet installation identify the local machine by its system hostname
instead of an `unknown-*` placeholder, and replace the obsolete bottom-left
account control with a direct Settings button.

## Device-name resolution

The engine resolves the default local device name in this order:

1. A non-empty `COMET_DEVICE_NAME` override.
2. Windows `COMPUTERNAME`.
3. `HOSTNAME`.
4. A non-empty `/etc/hostname` value.
5. The existing `unknown-device` last-resort sentinel.

Values are trimmed and empty values are ignored. Resolution is isolated behind
a testable helper whose environment and hostname-file inputs can be supplied by
tests without mutating the process environment.

Workspace startup continues to preserve deliberate user renames. When the
existing local device row has an empty name or a known generated sentinel
(`unknown-default` or `unknown-device`), startup repairs it with the resolved
system hostname. Any other existing name remains unchanged.

The same resolved name is used for the local workspace device row and
`ServerHello`, so local and paired clients see one consistent machine name.

## Bottom-left navigation

The normal desktop sidebar no longer renders an avatar, device/account name,
Alpha label, email, dropdown, or sign-out affordance. Its bottom control is one
full-width row containing the existing settings gear icon and the label
`Settings`.

Clicking the row directly opens Settings at the Devices section. It uses the
existing settings route and navigation history behavior. The account-menu open
state, dismissal timestamp, popover construction, and related rendering inputs
are removed when they no longer have another caller.

## Compatibility and scope

- Agent-provider accounts remain available inside Settings > Accounts.
- Remote configuration, listening, pairing, and trust behavior do not change.
- Existing user-selected device names are not overwritten.
- The sidebar layout and styling reuse the existing row/icon/theme patterns; no
  broader navigation redesign is included.

## Verification

Automated tests cover:

- explicit override precedence;
- Windows `COMPUTERNAME` fallback;
- Unix-style `HOSTNAME` and hostname-file fallback;
- trimming and empty-value handling;
- repair of `unknown-default` and `unknown-device`;
- preservation of a deliberate custom device name;
- direct navigation from the bottom Settings row to Settings > Devices;
- absence of the obsolete account-menu behavior from the rendered sidebar
  model/source boundary.

After focused tests pass, rebuild and relaunch the Windows desktop app. Confirm
the embedded engine starts, the sidebar shows only the labeled Settings row,
and the local device displays the Windows hostname.
