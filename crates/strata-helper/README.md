# strata-helper

The elevated half of Strata. The unelevated app asks it, over a secured named pipe
(`strata-ipc`), for the work that needs administrator rights: raw NTFS MFT scans streamed in
batches, USN change-journal queries, blocking reads and creation, MFT record re-reads, and
deletes that it re-verifies by file id against the never-delete list before acting. It also runs
ETW file-activity tracking (which process wrote where), streaming hourly rollups and last writers
to the app and answering top-writer and attribution-evidence queries from memory. Every
privileged action produces audit records that the app stores.

The library also holds `client::HelperClient`, which the app uses to launch, connect to and
call the helper. It is `Send + Sync` and can be shared across threads. `start_activity` returns
an `ActivityStream` of `ActivityEvent`s (dropping it stops tracking); `stop_activity`,
`clear_activity`, `query_activity` and `activity_evidence` act on the same connection's
session. One tracking session runs per helper process, and only an elevated helper can start it.

## Modes

```text
strata-helper --pipe <name> --client-image <app.exe> --client-pid <pid> [--parent-pid [<pid>]]
strata-helper --install-service --client-image <app.exe>     (elevated)
strata-helper --uninstall-service                            (elevated)
strata-helper --service --client-image <app.exe>             (run by the Service Control Manager)
```

- **On demand:** the app starts the helper through UAC with a random pipe name. Only that app
  process may connect. The helper exits when the app disconnects, sends `Shutdown`, stays idle
  for 30 minutes, or exits.
- **Service:** an on-demand `LocalSystem` service that interactive users may start. It serves
  `\\.\pipe\strata-helper-svc-<user SID>` for each signed-in user, accepts only the configured
  app image run by a local administrator, and stops itself after 5 idle minutes.

Exit codes: 0 success, 1 failure, 3 the launching app exited, 64 bad arguments.

## Security

The pipe DACL admits only the user and SYSTEM. Clients are verified by image path and
Authenticode signer before the helper reads any of their bytes. The protocol version must
match, requests are rate-limited, and volume ids follow a strict grammar. Privileges outside
`SeBackupPrivilege`, `SeManageVolumePrivilege` and `SeChangeNotifyPrivilege` are removed at
startup, and the first two are enabled only while a volume is being opened.

## Testing

`cargo test -p strata-helper` runs unelevated. Debug builds accept `--image <file>`, which
serves a synthetic NTFS image under the volume id `strata-image`. The tests use it to drive the
real request loop over real named pipes.
