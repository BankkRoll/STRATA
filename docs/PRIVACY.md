# User data and privacy

Strata collects no telemetry. Scan results, file names and paths never leave your machine.

## What is stored, and where

Strata's data lives under `%LOCALAPPDATA%\app.strata.desktop\`, named after the app identifier.

| Data | Location | Contents |
|---|---|---|
| `state.db` | `store\` | Settings, and the undo/audit log of every cleanup (paths, sizes, method, time) that Recycle Bin restores depend on |
| `history.db` | `store\` | Folder-size snapshots over time, activity tracking data (if enabled), the duplicate finder's hash cache |
| `update-state.json` | the folder itself | Version numbers of the last installed update, used to offer a rollback |
| WebView2 profile | the folder itself | The embedded browser's own data. The UI is bundled with the app; no remote web content is loaded |
| User rule packs | Your user rules folder (Settings → Rules) | Rule packs you write yourself |

Snapshots store aggregate sizes of folders above a minimum size (16 MB by default), not
individual files. Snapshots are thinned to one per week after 30 days and deleted after 90 days
by default (Settings → Data & privacy).

## Activity tracking is opt-in

Activity tracking is off by default (Settings → Activity tracking). It needs administrator
rights. When on, Strata records which programs write, create and delete files, aggregated per
program, per folder and per hour, plus the last program that wrote each path. The data stays in
`history.db` and is deleted after the retention period (30 days by default). Tracing is throttled
to stay under a CPU cap (2% by default).

## Network access

| Request | When | What is sent |
|---|---|---|
| Update check | 20 seconds after launch, then daily, in release builds configured with an updater key | A request to GitHub for the release manifest (`latest.json`). No identifiers, scan data or paths |
| Update download | When a newer release is found | The signed installer is downloaded from GitHub Releases and installs when you exit Strata |
| Rollback | Only if a just-installed update fails to start and you confirm the prompt | The previous version's manifest and installer are downloaded from GitHub Releases |
| WebView2 bootstrapper | During installation, only if the WebView2 Runtime is missing | Microsoft's bootstrapper downloads the runtime from Microsoft |

**Report an issue** (in Settings) opens a new GitHub issue in your browser, pre-filled with the
Strata version, Windows build and architecture. Strata itself sends nothing; you review and submit
the issue yourself, and it never includes paths or file names.

Nothing else in Strata makes network requests. The signature checks between the app and its
helper run without revocation checking, so they never contact certificate servers. Scanning a
network share you enable reads that share, as any file browser would.

## Removing data

| Data | How |
|---|---|
| Old snapshots | Lower "Keep snapshots for" in Settings → Data & privacy; older snapshots are deleted at the next daily maintenance |
| Activity data | Turn off activity tracking, or lower "Keep activity for" in Settings → Activity tracking |
| Everything | Uninstall and tick **Delete the application data**, or delete `%LOCALAPPDATA%\app.strata.desktop\` while Strata is closed |

Deleting `state.db` also deletes the undo log; items already in the Recycle Bin can then only be
restored from the Windows Recycle Bin.

## Uninstalling

Strata installs with an NSIS setup program (`.exe`). The uninstaller:

- removes the program files and Start menu entries;
- stops and removes the `StrataHelper` service, if service mode was installed;
- stops the `Strata-FileActivity` ETW session, if one is running;
- removes the "Start with Windows" entries.

Your data folders stay unless you tick **Delete the application data**, which removes
`%LOCALAPPDATA%\app.strata.desktop\` and `%APPDATA%\app.strata.desktop\`. Files you moved to the
Recycle Bin stay there.
