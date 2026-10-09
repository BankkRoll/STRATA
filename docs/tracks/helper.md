# Helper track: `strata-helper`

Owner paths: `crates/strata-helper/`, this file. Small additive protocol changes in
`crates/strata-ipc/` (listed below).

Status: the binary, both launch modes, every request handler, the app-side `HelperClient` and
the unelevated test suite are done. The paths that need elevation (raw volumes, USN FSCTLs,
service install, UAC) are implemented but verified only through the manual procedure below.

## Done (by SPEC section)

| SPEC | Item | Where |
|---|---|---|
| §4 | On-demand mode: `--pipe --client-image --client-pid [--parent-pid [<pid>]]`, launched via `launch_elevated`; exits on disconnect, `Shutdown`, 30 min idle, 60 s with no client, or parent exit | `args.rs`, `run.rs`, `server.rs`, `watch.rs` |
| §4 | Service mode (`--service`), install/uninstall (`--install-service --client-image`, `--uninstall-service`), per-user pipes, idle stop | `service.rs` |
| §4 | Startup hardening: `drop_privileges` keeps only `SeBackup`, `SeManageVolume`, `SeChangeNotify`; ref-counted privilege scopes around volume opens | `run.rs`, `privs.rs` |
| §4 | Client verified before any byte is read: `TrustPolicy::Signed` (release) / `DevUnsigned` (debug), plus PID binding (on-demand) or same-user + administrator (service) | `verify.rs` |
| §4 | Version handshake, rate limit, hostile frames: `strata-ipc` transport, exercised end to end | `tests/pipe.rs` |
| §4, §21 | Helper crash or disconnect → typed `ClientError::Disconnected`, `HelperClient::reconnect()` | `client.rs`, `tests/binary.rs` |
| §6.4 | `ScanVolume`: `strata-ntfs` scan → bounded channel → re-batching writer; `ScanProgress` on the interval, final `ScanProgress`, `ScanDone{stats}`; `include_metadata`; one scan at a time (`Busy`) | `ops/scan.rs` |
| §6.4 | Backpressure: bounded at every hop (scanner read-ahead, 4-batch channel, pipe, client 8-batch route); slow client stalls the scan instead of buffering | `ops/scan.rs`, `client.rs` |
| §10.1 | `QueryUsnJournal` (`None` when inactive), `ReadUsn` with `BytesToWaitFor` + helper-enforced timeout and cancel (overlapped + `CancelIoEx`), `CreateUsnJournal` | `ops/usn.rs` |
| §10.2 | `ReadRecords` via `NtfsVolume::read_record`, cached volume handle, reopen once when the MFT grew, sequence check → `missing` | `ops/records.rs` |
| §10.3 | Journal id changed / journal gone → `JournalChanged`; `from` purged → `JournalWrapped` | `ops/usn.rs` |
| §15.3 | `DeleteOnReboot` via `strata_clean::privileged::schedule_delete_on_reboot` (plain single-link files only) | `ops/delete.rs` |
| §15.7 | `PrivilegedDelete` via `verify_and_delete_by_id`; helper-level never-list check before `strata-clean`'s own; audit `Started` + outcome for every request | `ops/delete.rs`, `audit.rs` |
| §19 | Service install/uninstall entry points for the installer and Settings | `service.rs` |
| §23 M3 | Helper + IPC with security checks, streaming results | whole crate |
| §23 M13 | Service install/uninstall | `service.rs` (manual test below) |

## Protocol changes (`strata-ipc`, `PROTOCOL_VERSION` 1 → 2)

All additive except the two field additions; postcard is positional, hence the bump.

- `DeleteRequest.is_dir: bool` (strata-clean opens directories with different access).
- `Request::ReadUsn` gains `bytes_to_wait_for: u32`, `timeout_ms: u32`.
- New `Request::CreateUsnJournal { volume, maximum_size, allocation_delta }` → `UsnJournal(Some(info))`.
- New `Request::DeleteOnReboot(RebootDeleteRequest { file_ref, expected_path, expected_size, expected_mtime })` → `RebootScheduled { file_ref }`.
- `Response::Deleted` gains `summary: DeleteSummary { files, dirs, links, bytes }`.
- New `Response::Audit(AuditEntry { seq, time, client_pid, op, phase, volume, file_ref, path, detail })`,
  `AuditOp { ScanVolume, PrivilegedDelete, DeleteOnReboot, CreateUsnJournal }`,
  `AuditPhase { Started, Succeeded, Refused, Failed }`.
- New `ErrorCode::JournalChanged`, `ErrorCode::JournalWrapped` (appended).
- `security::TrustError::Unauthorized { reason }` (log-only, never on the wire);
  `security::is_sid_string` made public.

`docs/tracks/platform.md`'s message table describes v1; the list above is the delta.

Event order per request id: `Audit(Started)` → (scan: `ScanProgress`*/`ScanBatch`*) →
`Audit(outcome)` → final response (`ScanDone`, `Deleted`, `RebootScheduled`, `UsnJournal`) or
`Error`. Refusals before acting still produce `Started` + `Refused`.

Volume ids accepted by every request: `\\?\Volume{GUID}\` (trailing `\` optional), `X:`, `X:\`;
debug builds with `--image` also accept `strata-image`. Anything else → `BadRequest`.

## What the app backend calls

```rust
use strata_helper::client::{ClientConfig, ClientError, HelperClient, ScanEvent, UsnRead};
use strata_helper::HELPER_EXE;

let helper = strata_ipc::security::TrustPolicy::sibling_of_current_exe(HELPER_EXE)?;
// "Fast scan (admin)": one UAC prompt. Service mode: ClientConfig::service(helper).
let mut client = match HelperClient::connect(ClientConfig::on_demand(helper)) {
    Ok(c) => c,
    Err(ClientError::Declined) => { /* walker fallback + "Standard scan" banner */ }
    Err(ClientError::ServiceNotInstalled) => { /* fall back to on-demand */ }
    Err(e) => { /* show error */ }
};
client.welcome();                                   // elevated, capabilities, build
client.list_volumes()?;                             // Vec<VolumeInfo>
for ev in client.scan_volume(guid_path, ScanOptions::default())? {   // ScanStream
    match ev? {
        ScanEvent::Batch(records) => index.ingest(records),
        ScanEvent::Progress(p) => ui.progress(p),
        ScanEvent::Audit(a) => store.helper_audit(a),
        ScanEvent::Done(stats) => break,           // stats.cancelled
    }
}
// stream.cancel() or drop(stream) cancels; client.cancel(id) waits for the ack.
client.query_usn_journal(v)?;                       // Option<UsnJournalInfo>
client.create_usn_journal(v, 0, 0)?;                // after user confirmation; Audited<_>
let chunk = client.read_usn(&UsnRead { volume, journal_id, from, max_bytes: 1 << 20,
                                       bytes_to_wait_for: 1, wait: Duration::from_secs(5) })?;
strata_ntfs::parse_usn_buffer(&chunk.to_fsctl_buffer())?;   // next_usn = chunk.next_usn
client.read_records(v, refs)?;                      // RecordsReply { records, missing }
client.privileged_delete(DeleteRequest { .. })?;    // Audited<DeleteSummary>
client.delete_on_reboot(RebootDeleteRequest { .. })?;
client.ping()?; client.shutdown()?;
// On ClientError::Disconnected anywhere: banner, keep the index, offer client.reconnect().
// ClientError::Remote(RemoteError { code, message, audit }): store `audit` too.
// code JournalChanged | JournalWrapped → full rescan (SPEC §10.3).
```

Settings (§19): run `strata-helper.exe --install-service --client-image <Strata.exe>` /
`--uninstall-service` through `launch_elevated` and wait for the exit code (0 ok, 1 failure,
64 usage). `strata_helper::service::is_installed()` tells the UI which mode is available.

The client lives in the helper crate's library (`strata_helper::client`), not in `strata-ipc`:
it needs launch (UAC and service start) and the service pipe naming, which belong to the
helper. The app depends on `strata-helper` as a library.

## Trust model

On-demand:
1. App generates `session_pipe_name(user SID)` (128 random bits) and launches the helper with
   it, its own image path and PID.
2. Helper validates every argument, drops privileges, creates the pipe with
   `FIRST_PIPE_INSTANCE` and the user + SYSTEM DACL (medium label).
3. Only the launching PID (kernel-reported) running the expected image with the same signer as
   the helper is accepted (`LaunchedClientVerifier`). The app verifies the server the same way
   before sending `Hello`.

Service:
1. Installed by an administrator; config (binary path, `--client-image`) is admin-writable only.
   Service DACL: SYSTEM/administrators full, interactive users `CCLCSWRPLORC` (query + start).
2. The service reads `--client-image` from its own command line (SCM config); `StartService`
   arguments, which any interactive user controls, are ignored.
3. Every 2 s it enumerates active sessions (`WTSEnumerateSessionsW` + user name →
   `LookupAccountNameW`, so no `SeTcbPrivilege` is needed) and serves
   `\\.\pipe\strata-helper-svc-<SID>` per user. The name is well known so the app can find it.
4. Clients must be the configured app image (signer match), run as the pipe's user, and that
   user must be in `BUILTIN\Administrators` (deny-only in a filtered token counts). Service mode
   replaces an administrator's UAC consent; a standard user must not get SYSTEM-level raw reads
   (all file names on the volume) or deletes.
5. Squatting the well-known name only denies service: the app's server verification (image +
   signer of the installed helper) fails before any data is sent.
6. Stops after 5 min with no client connected and no requests.

Deletes run with ordinary administrator access checks (`SeRestorePrivilege` is dropped), so a
file that denies administrators stays undeletable rather than bypassed.

## Tests

`cargo test -p strata-helper -p strata-ipc`: all green, unelevated, stable across repeated runs.

| Suite | Count | Covers |
|---|---|---|
| unit | 28 | argument parsing (forms, duplicates, invalid pipe/PID/relative image, `--image` only in debug), volume-id grammar, error mapping, USN error typing and buffer split, unelevated USN query → `AccessDenied`, privilege ref-counting, audit numbering, cancel signals, PID-bound and service verifiers (current token: user SID, Administrators membership), service pipe names, service SDDL, session user → SID, unelevated install refused, parent watch |
| `tests/pipe.rs` | 14 | handshake/ping/volumes; scan of a 3,000-file image equals a direct `strata-ntfs` scan (no loss, no duplicates, batches ≤ batch size, progress, audit); metadata exclusion; slow client stalls the scan (backpressure) then cancel → `cancelled`, partial; second scan `Busy`; dropping a stream cancels; client disconnect mid-scan → helper loop returns `ClientDisconnected`; rate limit typed with `retry_after`; version mismatch typed (helper keeps serving); wrong PID refused; client refuses an unexpected server; `ReadRecords` current/stale/free, 70k refs `BadRequest`; malformed volume ids, image USN `NotSupported`, unelevated raw scan/USN `AccessDenied`; `Shutdown`; reconnect |
| `tests/delete.rs` | 4 | own temp file: wrong size / mtime / id / path refused (`Mismatch`, file intact, audit `Started`+`Refused`), match deletes (audit `Started`+`Succeeded`); recursive directory delete by id; never-list at helper level (real ids of `hosts`/`explorer.exe`, protected path claims with a harmless id incl. `C:\Users`, `C:\Windows\System32`, `C:\Program Files`, protected id with a harmless claim, bad volume, unpaired surrogates); delete-on-reboot refused for protected, directories, stale facts, `AccessDenied` unelevated |
| `tests/binary.rs` | 4 | real `strata-helper.exe` with `--image`: usage exit 64, scan then exit after disconnect, `taskkill` → `Disconnected` → `reconnect()` relaunches, parent death → exit 3 |
| doc | 4 | `parse_args`, `parse_volume_id`, `service_pipe_name`, client example (compile only) |

Test files live only in self-created `D:\strata-helper-tests\<unique>` (or `%TEMP%`) directories,
removed on drop. No request names a protected folder by its real id.

`cargo clippy -p strata-helper -p strata-ipc --all-targets -- -D warnings` and
`cargo clippy -p strata-helper --release --lib --bins -- -D warnings` are clean.
The integration tests need a debug build (`--image` and `Volumes::with_image` do not exist in
release).

## Manual test procedure (needs elevation)

Run from an elevated PowerShell on a test machine; build with
`cargo build -p strata-helper` (debug: `DevUnsigned` accepts the unsigned dev binaries).

1. **Raw scan + USN (on-demand path without UAC):** write a tiny client or use
   `HelperClient::connect(ClientConfig::spawn(helper, vec![]))` from an elevated test binary
   in the same folder. Expect `welcome().elevated`, `capabilities.mft_scan`. Scan `C:` and
   compare `ScanDone.records` with `strata-cli scan C:`. `query_usn_journal("C:")` → `Some`;
   `read_usn` from `next_usn` with `bytes_to_wait_for: 1, wait: 10 s`, create a file in another
   window → returns within a second with records that `parse_usn_buffer` decodes; with no
   activity → empty after 10 s. `read_usn` with a wrong `journal_id` → `JournalChanged`; with
   `from: first_usn - 1` → `JournalWrapped`.
2. **Create journal (on a VHDX, not C:):** mount a fresh NTFS VHDX, `fsutil usn deletejournal
   /d X:`, `query_usn_journal` → `None`, `create_usn_journal("X:", 0, 0)` → `Some` with 32 MiB.
3. **UAC:** from the unelevated app, `ClientConfig::on_demand(helper)`: consent → connected;
   decline → `ClientError::Declined`. Close the app → helper exits (Task Manager).
4. **Privileged delete:** as admin, create `C:\strata-manual\x.txt`, deny Users read on it,
   delete via `privileged_delete` → `Deleted`. Request `C:\Windows\System32\drivers\etc\hosts`
   → `Protected`, file intact.
5. **Delete on reboot:** schedule a scratch file → `RebootScheduled`;
   `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\PendingFileRenameOperations`
   lists it; remove the entry or reboot.
6. **Service:** `strata-helper.exe --install-service --client-image <path-to-app.exe>` →
   exit 0; `sc.exe sdshow StrataHelper` shows `SERVICE_SDDL`; from the unelevated app
   `ClientConfig::service(helper)` starts it and connects without UAC; a standard (non-admin)
   account's app is refused (`Rejected(Untrusted)`); after 5 idle minutes `sc.exe query
   StrataHelper` → STOPPED; `--uninstall-service` → exit 0 and the service is gone.
   Install twice → reconfigures (idempotent); uninstall when absent → exit 0.

## Decisions

- **Client in `strata_helper::client`, not `strata-ipc`.** Launch and service discovery are
  helper concerns; `strata-ipc` stays transport + protocol.
- **Audit as stream events (`Response::Audit`)**, not fields on final responses: refusals and
  failures need audit too, and a helper crash after `Started` leaves the app with a write-ahead
  record (no outcome → "interrupted").
- **Scans, journal creation and deletes are audited; reads are not** (`ReadUsn`/`ReadRecords`
  run every 250 ms per volume and would flood the log).
- **USN wait timeout enforced by the helper** (kernel `Timeout = 0`, overlapped wait on
  `[io event, cancel event]`, `CancelIoEx`, always reaped), because the kernel field's unit is
  documented inconsistently and the wait must also be cancellable.
- **One scan at a time per connection** (`Busy` for a second): a scan uses the disk's full
  sequential bandwidth and large buffers.
- **Progress `bytes_read` = position of the highest record delivered**: the scanner reports
  bytes read only at the end; the final `ScanProgress` carries the exact value.
- **`ScanStats.corrupt` = BAAD + torn + bad signature + malformed** (unreadable sectors are not
  "corrupt records").
- **Batches bounded by records (64–65,536) and ~16 MiB estimated size**, far below the 64 MiB
  frame limit even for 1,024-link records.
- **`SeRestorePrivilege` dropped at startup**; deletes use normal admin access checks.
- **Privilege scopes are reference counted** (token privileges are process-wide; per-request
  guards would disable a privilege under a concurrent request).
- **Never-list guard built lazily** on the first delete and reused (it enumerates volumes and
  profiles).
- **`DeleteOnReboot` consent minted in the helper** from the verified app's request: the
  `Consent` type is deliberately not serializable, and the app shows the prompt before sending.
- **Service user discovery via `WTSQuerySessionInformationW` + `LookupAccountNameW`**, not
  `WTSQueryUserToken`, so the service keeps no `SeTcbPrivilege`.
- **`--parent-pid` value optional**: alone it means the client PID (the launching app).
- **Release binary uses the Windows subsystem** (no console flash); results are exit codes.
- **On-demand exits on disconnect**; `reconnect()` relaunches (new UAC prompt). The pipe and
  image modes reconnect to the same helper.

## Blockers / not verified here

- Not elevated: raw volume scans, USN FSCTLs, journal creation, privilege enable/drop on the
  real token, delete-on-reboot success, service install/start/stop and UAC launch are
  implemented but only covered by the manual procedure above. Unelevated tests assert the
  typed `AccessDenied` paths.
- High-IL helper ↔ medium-IL app across the integrity boundary (the pipe label was verified by
  the platform track; the on-demand path needs the manual step 3).

## Change requests for other crates

1. `strata-clean`: `privileged::verify_and_delete_by_id` reports a by-id open of a freed file id
   as `CleanError::Os { code: 87 }` (→ `ErrorCode::Io`). A typed `NotFound` would let the app
   say "already gone":

   ```diff
   --- a/crates/strata-clean/src/privileged.rs
   +++ b/crates/strata-clean/src/privileged.rs
   @@ pub fn verify_and_delete_by_id(
        let probe = handle::open_by_id(
            &hint,
            u128::from(req.file_ref.0),
            ACCESS_READ_ATTRIBUTES,
            SHARE_ALL,
        )
   -    .map_err(|e| CleanError::from_io(&display, &e))?;
   +    .map_err(|e| match crate::error::win32_code(e.raw_os_error().unwrap_or(0)) {
   +        // ERROR_INVALID_PARAMETER: no file has this id any more.
   +        87 => CleanError::NotFound { path: display.clone() },
   +        _ => CleanError::from_io(&display, &e),
   +    })?;
   ```

2. `docs/tracks/platform.md` (platform track): update the message table to protocol v2 (see
   "Protocol changes").

No `strata-core` changes are needed.

## Next steps

1. App backend: wire `HelperClient` into the Tauri commands (scan into `strata-index`,
   `Response::Audit` into the `strata-store` audit table, Disconnected banner + reconnect,
   Declined → walker).
2. Run the manual procedure elevated; record real-volume scan throughput over the pipe in
   `docs/BENCHMARKS.md`.
3. Installer (M14): install the helper next to the app in Program Files and call
   `--install-service` when the user opts into service mode.
4. ETW (§11, M12) requests belong in this helper when that track starts (new protocol variants).
