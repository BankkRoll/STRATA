# Platform track: `strata-win` + `strata-ipc`

Status: both crates complete as library code with tests. The helper binary
(`strata-helper`) is not part of this track yet; see "Next steps".

## What's done (by SPEC section)

| SPEC | Item | Where |
|---|---|---|
| §3 | Shared Win32 layer, `windows` 0.62, RAII handles, `// SAFETY:` on every `unsafe` | `crates/strata-win` |
| §4 | `is_elevated`, `elevation_type`, `enable_privilege` → `PrivilegeGuard`, `drop_privileges` | `strata_win::process` |
| §4 | `launch_elevated` (`ShellExecuteExW` + `runas`, `Declined` vs `Failed`) | `strata_win::process` |
| §4 | Pipe DACL (user + SYSTEM), medium integrity label, unguessable name, peer verification, version handshake, rate limit, typed `Disconnected` | `crates/strata-ipc` |
| §5 | Volume discovery: GUID paths, all mount paths, folder mounts with parent volume, fs/serial/label/flags, cluster size, sizes, system volume, BitLocker, Dev Drive, network drives, `scanner_choice` | `strata_win::volume` |
| §5 | Hot-plug watcher (message-only window, re-enumerate + diff) | `strata_win::watcher` |
| §6.4 | `ScanBatch { records: Vec<ScanRecord> }` streamed in binary frames | `strata_ipc::protocol`, `frame` |
| §7.5 | VSS shadow storage via WMI `Win32_ShadowStorage`, `Unavailable` when unelevated | `strata_win::shadow` |
| §10.1 | `QueryUsnJournal` / `ReadUsn` messages (raw record bytes) | `strata_ipc::protocol` |
| §11 | `DeviceMap` / `device_to_dos_path` (`\Device\HarddiskVolumeN\x` → `C:\x`, folder mounts, `\Device\Mup`) and `ProcessInfo` (pid + start time identity) | `strata_win::path`, `process` |
| §12.1 | Known folders via `SHGetKnownFolderPath` (+ `GetTempPath2W`), every profile from `ProfileList`, hive-or-derived with provenance | `strata_win::known` |
| §13 | `NtfsDisableLastAccessUpdate` decoding (incl. `0x8000000x` system-managed forms) | `strata_win::last_access` |
| §15.7 | `PrivilegedDelete { volume, file_ref, expected_path, expected_size, expected_mtime }` | `strata_ipc::protocol` |
| §21 | BitLocker locked, removable yanked (watcher `Removed`), network (opt-in, distinct kind), multiple profiles, non-English (API only, no English names except documented on-disk fallback), redirected folders (API reports OneDrive/`D:` locations) | as above |

## Verified on the dev machine (Windows 11, unelevated)

Machine-specific values (GUIDs, SIDs, account and profile names) are deliberately
omitted. What was observed:

- **Volumes:** the system NTFS volume, a second NTFS data volume, a letterless NTFS
  recovery partition and the FAT32 EFI partition were all enumerated, with mount
  paths, filesystem, label, cluster size, sizes, system flag and USN support.
  BitLocker reported `not_encrypted` for lettered volumes and `unknown` for the
  letterless ones (the shell property needs a parsing path). No folder mounts or
  network drives existed, so `nested_in` and `network_drives()` are covered by
  unit tests with synthetic data.
- **Known folders:** every machine and current-user folder resolved via the API.
  Documents, Desktop and Pictures were redirected to OneDrive (Folder Backup) and
  the API reported the redirected locations. `ProfileList` also lists
  S-1-5-18/19/20, which are skipped. With no second user profile on the machine,
  the other-profile path is covered by `known::tests::derived_profile_layout`
  (not-loaded hive → derived layout) and the expansion tests.
- `last_access_policy()` → `Enabled { system_managed: true }` (registry
  `0x80000002`; `fsutil` reports `2`).
- `shadow_storage()` → `Unavailable { hresult: 0x80041014, needs_elevation: true }`
  (the VSS provider requires elevation).
- `elevation_type()` → `Limited` (split admin token, unelevated).
- `dev_drive_state` on NTFS → the FSCTL succeeds only with an all-ones
  `FlagMask` (narrower masks give `ERROR_INVALID_PARAMETER`), flags 0.

## Wire protocol

### Framing (byte-exact)

```text
offset  size     field
0       4        len         u32 little-endian = 5 + payload length
4       1        kind        1 Hello, 2 Welcome, 3 Reject, 4 Request, 5 Response
5       4        request_id  u32 little-endian
9       len - 5  payload     postcard encoding of the kind's type
```

- `5 <= len <= MAX_FRAME_LEN` (64 MiB). Outside that range the frame is
  rejected before its body is read (`TooShort` / `Oversized`).
- The payload must decode to exactly `len - 5` bytes (`TrailingBytes`
  otherwise). Unknown kinds and undecodable payloads are errors. Any framing
  error poisons the connection (the stream cannot be resynchronized).
- Example: `Request::Ping` with id `0x01020304` is
  `06 00 00 00 04 04 03 02 01 00`.

### Messages (`PROTOCOL_VERSION = 1`)

Handshake (request id 0):

- client → `Hello { protocol, build, client_pid }`
- helper → `Welcome { protocol, helper_build, elevated, capabilities { mft_scan, usn_journal, read_records, privileged_delete } }`
  or `Reject(VersionMismatch { helper, client } | Untrusted | PidMismatch | Malformed)`

Requests (client-chosen non-zero id):

| Request | Responses (same id) |
|---|---|
| `Ping` | `Pong` |
| `ListVolumes` | `Volumes { volumes: Vec<VolumeInfo> }` |
| `ScanVolume { volume, options: ScanOptions { batch_size, progress_interval_ms, include_metadata } }` | `ScanProgress { records, bytes_read, bytes_total }`*, `ScanBatch { records: Vec<ScanRecord> }`*, then `ScanDone { stats: ScanStats { records, corrupt, elapsed_ms, cancelled } }` |
| `Cancel { request_id }` | `CancelAck { was_running }` (with the cancel's id); the target ends with `ScanDone { cancelled: true }` |
| `QueryUsnJournal { volume }` | `UsnJournal(Option<UsnJournalInfo>)` (`None` = journal inactive) |
| `ReadUsn { volume, journal_id, from, max_bytes }` | `UsnRecords { next_usn, raw }` (raw `USN_RECORD_V2/V3/V4` bytes, parsed by `strata-ntfs`) |
| `ReadRecords { volume, file_refs }` | `Records { records, missing }` |
| `PrivilegedDelete(DeleteRequest { volume, file_ref, expected_path (UTF-16), expected_size, expected_mtime })` | `Deleted { file_ref }` |
| `Shutdown` | `ShuttingDown` |

Any request may instead get `Error(ErrorReply { code, message, retry_after_ms })`
with `code` ∈ `BadRequest, UnknownVolume, NotSupported, AccessDenied,
NotFound, Mismatch, Protected, RateLimited, Cancelled, Busy, Io, Internal`.
Postcard identifies variants by position, so any change to these types must
bump `PROTOCOL_VERSION`.

## Security model

1. **Name.** `session_pipe_name(sid)` → `\\.\pipe\strata-helper-<SID>-<128-bit
   BCryptGenRandom hex>`, passed on the helper's command line and validated by
   `is_valid_pipe_name` before use.
2. **Creation.** `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE`
   (a pre-existing/squatted name fails), `PIPE_REJECT_REMOTE_CLIENTS`, byte
   mode, overlapped, **one instance** reused across reconnects so the name
   never disappears and a second client gets `ERROR_PIPE_BUSY`.
3. **DACL** (`pipe_sddl`):
   `D:P(A;;0x120183;;;<user>)(A;;GA;;;SY)(A;;RC;;;OW)S:(ML;;NW;;;ME)`.
   The user gets read/write data + attributes only: no `FILE_APPEND_DATA`
   (= `FILE_CREATE_PIPE_INSTANCE`), no `WRITE_DAC`. `OWNER RIGHTS` is limited
   to `READ_CONTROL`. The **medium mandatory label** is needed because a
   high-IL creator's default label would block the medium-IL app from
   writing; low-IL/AppContainer processes stay locked out. The SID is
   validated before interpolation (no SDDL injection).
4. **Peer verification before parsing anything** (`PeerVerifier`):
   `GetNamedPipeClientProcessId` → `ProcessInfo::of` (image + start time from
   one handle) → `verify_signature`. `TrustPolicy::Signed` requires the
   expected image path (case-insensitive, final-path canonicalized) and a
   trusted signature with the same subject + issuer as our own.
   `TrustPolicy::DevUnsigned` (accepts two unsigned binaries with matching
   paths) exists only under `cfg(debug_assertions)`. The client can verify
   the server the same way (`ClientOptions::server_verifier`,
   `GetNamedPipeServerProcessId`) and opens the pipe with
   `SECURITY_IDENTIFICATION` so an impostor server cannot impersonate it.
5. **Handshake.** Version must match exactly; `Hello.client_pid` must equal
   the kernel-reported PID. Rejections are sent, then the server lingers up
   to 2 s for the client to read them before disconnecting.
6. **Rate limit.** Token bucket per connection (default burst 200, 100/s).
   Over-limit requests are answered with `ErrorCode::RateLimited` +
   `retry_after_ms`; both sides surface `IpcError::RateLimited`.
7. **Hostile frames.** Non-request frames after the handshake, request id 0,
   malformed or oversized frames all close the connection.
8. **Disconnects.** Broken pipe / no data / not connected map to
   `IpcError::Disconnected` on both ends, for the "helper disconnected"
   banner and reconnect (§4).

The helper itself must still validate every request (§15.7: reopen by file
id, compare path/size/mtime, never-list, no reparse traversal); the transport
only guarantees who is talking.

## Transport choice

Overlapped I/O behind blocking calls (`run_overlapped`): each read/write is
issued overlapped, then waited on with an optional timeout; on timeout it is
cancelled with `CancelIoEx` and **always** reaped with
`GetOverlappedResult(.., TRUE)` before its buffer is released. This gives
full duplex on one connection (a synchronous pipe handle serializes I/O, so a
blocked reader would stall the scan stream) and timeouts, without an async
runtime. Reads and writes each have their own mutex, so frames never
interleave and `ServerConnection` can be shared across threads.

## Public API (main entry points)

`strata-win`:

- `volume::{discover_volumes, DiscoveryOptions, VolumeInfo, scanner_choice, ScannerKind, DriveKind, FileSystemKind, BitLockerState, DevDriveState, NestedMount, volume_guid_paths, system_volume_guid, local_volume_info, network_drives, wnet_connection, shell_bitlocker_state, dev_drive_state}`
- `watcher::{VolumeWatcher, WatcherOptions, VolumeEvent, diff_volumes, letters_from_unit_mask}`
- `known::{resolve_known_folders, known_folders, ResolvedFolders, ProfileFolders, FolderSource, HiveAccess, profile_list, ProfileEntry, known_folder_path, temp_dir, expand_for_profile, is_user_profile_sid}`
- `process::{is_elevated, elevation_type, ElevationType, current_user, UserAccount, Token, enable_privilege, PrivilegeGuard, PrivilegeError, drop_privileges, SE_BACKUP, SE_RESTORE, SE_MANAGE_VOLUME, SE_CHANGE_NOTIFY, launch_elevated, ElevatedChild, LaunchError, ProcessInfo, process_image_path, process_start_time, quote_arg, join_args}`
- `signature::{verify_signature, verify_signature_with, VerifyOptions, Revocation, SignatureStatus, SignatureKind, SignerInfo, same_signer, same_signer_files}`
- `path::{to_verbatim, strip_verbatim, final_path, final_path_of, open_for_attributes, eq_ignore_case, volume_guid_for_mount_point, mount_points_for_volume, volume_mount_root, query_dos_device, DeviceMap, device_to_dos_path}`
- `last_access::{last_access_policy, LastAccessPolicy}`, `shadow::{shadow_storage, ShadowStorageReport, ShadowStorage, parse_volume_ref}`, `sid::{OwnedSid, account_name}`
- `WinError` (serializable: op, HRESULT, message), `OwnedHandle`

`strata-ipc`:

- `protocol::*` (above), `frame::{encode_frame, decode_frame, peek_len, FrameKind, FrameError, MAX_FRAME_LEN}`
- `pipe::{PipeServer, ServerConfig, ServerConnection, PipeClient, ClientOptions}`
- `security::{pipe_sddl, SecurityDescriptor, session_pipe_name, is_valid_pipe_name, PeerIdentity, PeerVerifier, TrustPolicy, TrustError, USER_PIPE_ACCESS, CLIENT_PIPE_ACCESS}`
- `rate::{RateLimit, TokenBucket}`, `IpcError`

## Tests

| Crate | Unit | Doc | Notes |
|---|---|---|---|
| `strata-win` | 43 | 14 | real-machine tests assert invariants only (CI-safe) |
| `strata-ipc` | 34 | 5 | 17 end-to-end tests on real named pipes; 3 proptests (4096 cases each) |

`strata-ipc` pipe tests: handshake + ping, version mismatch (typed on both
ends), untrusted client image (then the pipe is reused), client refusing an
unexpected server, busy second client + reconnect, accept/connect timeouts,
1M-record stream, cancel mid-stream (full duplex), client and server
disconnect mid-stream, rate limiting (5 of 10 refused with `retry_after`),
malformed frames (unknown kind, bad payload, short length), oversized frame
rejected before reading the body, handshake frames after the handshake,
DACL + label read back with `GetSecurityInfo`, `GENERIC_WRITE` open denied,
squatting a second instance fails, invalid pipe names refused. The pipe tests
also pass with `--release`, where `DevUnsigned` does not exist and the tests
inject an image-path-only verifier.

## Benchmarks (release, `cargo bench -p strata-ipc`)

Encode + decode of 100k `ScanRecord`s (one name link, 4 times, sizes), 10
rounds:

| Codec | Bytes/record | Encode | Decode |
|---|---|---|---|
| **postcard 1.1.3** | 85.8 | 170 ns/record (504 MB/s) | 280 ns/record (307 MB/s) |
| bincode 2.0.1 (standard) | 91.0 | 103 ns/record (882 MB/s) | 263 ns/record (345 MB/s) |

Named pipe, 1M records in 8192-record batches, one process both ends
(includes cloning, encoding and decoding): **0.313 s → 3.20 M records/s,
279 MB/s**. The debug-build test streams the same 1M records at ~0.1 M
records/s (unoptimized serde).

## Decisions

- **postcard over bincode.** Decode (the app side) is within 6%, frames are
  6% smaller, postcard's 1.x wire format is specified and stable, and
  bincode's 3.0.0 release is a `compile_error!` tombstone (project ended), so
  depending on 2.x is a dead end. Encode is slower (+67 ns/record ≈ 70 ms per
  1M records) but runs in the helper alongside MFT parsing, well under the
  §2 budget.
- **Overlapped I/O, not one thread per direction on a synchronous handle**
  (see "Transport choice").
- **Single pipe instance, reused** instead of a listener that pre-creates the
  next instance: no create-instance right is needed by anyone, so the DACL
  can withhold it from the user.
- **Signer match is subject + issuer, not leaf thumbprint**, because Azure
  Trusted Signing rotates leaf certificates every few days.
- **Signer extraction from WinVerifyTrust state** (`WTHelperProvDataFromStateData`)
  instead of `CryptQueryObject`/`CryptMsgGetParam`, because it works for
  catalog-signed files too. Embedded vs catalog is decided from the PE
  certificate table, since `WTD_CHOICE_FILE` also consults catalogs on
  Windows 8+.
- **Revocation off by default** in `verify_signature` (configurable) so the
  handshake never blocks on the network; offline CRL lookups would otherwise
  fail verification.
- **Hot-plug via `GUID_DEVINTERFACE_VOLUME` notifications + re-enumeration
  diff.** Message-only windows do not receive `DBT_DEVTYP_VOLUME` broadcasts;
  the diff also catches folder mounts, letter changes and BitLocker unlocks,
  with a 1.5 s settle rescan for late drive-letter assignment.
- **BitLocker via the shell property store** (`GetPropertyStore` +
  `PropVariantToInt32`); `VT_EMPTY` (no protection, seen on Home) maps to 0 =
  not encrypted, matching Explorer. `GetVolumeInformationW` failing with an
  FVE facility error also marks the volume locked.
- **Dev Drive query only on ReFS** (Dev Drives are always ReFS), with an
  all-ones `FlagMask`.
- **`GetTempPath2W` resolved at runtime** so the crate still loads on Windows
  10 builds without it.
- **Network drives opt-in** (`DiscoveryOptions::include_network`) because a
  dead share blocks for the SMB timeout; and the UI process should enumerate
  them, since an elevated helper sees a different logon session's mappings.

## Blockers / needs elevation or other hardware to verify

- `enable_privilege(SE_BACKUP/SE_RESTORE/SE_MANAGE_VOLUME)` success path and
  `drop_privileges` on the real process token: needs an elevated run (the
  logic is tested on a duplicated token and with `SeTimeZonePrivilege`).
- `launch_elevated`: never called in tests (it would show a UAC prompt); the
  declined/failed classification is unit-tested.
- `shadow_storage()` `Available` path: needs elevation.
- High-IL server ↔ medium-IL client across the integrity boundary: tests run
  both ends at medium. The label is verified by reading it back.
- BitLocker `Unlocked`/`Locked` values and Dev Drive positive detection: no
  encrypted volume or ReFS Dev Drive on this machine. The shell value table
  (1/3/4/5/8 unlocked, 6 locked, 0/2 not encrypted) comes from observed
  Explorer behavior, not documentation.
- Other users' hives (`HiveAccess::Read`): needs a second signed-in user or
  elevation.
- Folder mounts and network drives: none on this machine.

## Core change requests (`strata-core`)

1. Carry known-folder provenance in the core type so consumers (the
   never-list, the "scanning another user's profile requires elevation" label,
   §21) can tell derived paths from authoritative ones without depending on
   `strata-win`:

```diff
--- a/crates/strata-core/src/known.rs
+++ b/crates/strata-core/src/known.rs
@@
+/// Where a resolved per-user folder path came from.
+#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
+#[serde(rename_all = "snake_case")]
+pub enum FolderSource {
+    /// `SHGetKnownFolderPath` / `GetTempPath2W` (authoritative).
+    Api,
+    /// The user's `User Shell Folders` registry value.
+    Hive,
+    /// `ProfileList\<SID>\ProfileImagePath`.
+    ProfileList,
+    /// Derived from the profile path; may be wrong if redirected.
+    Derived,
+}
+
 /// Known folders of one user profile.
 #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
 pub struct UserFolders {
@@
     /// Resolved per-user folders.
     pub folders: HashMap<KnownFolder, PathBuf>,
+    /// Provenance of each entry in `folders`; missing means `Api`.
+    #[serde(default)]
+    pub sources: HashMap<KnownFolder, FolderSource>,
 }
```

   After it lands, `strata_win::known::FolderSource` becomes a re-export and
   `ProfileFolders.sources` moves into `UserFolders`.

## Next steps for the helper binary (`strata-helper`)

1. `main`: parse `--pipe <name> --app <path>`; reject unless
   `is_valid_pipe_name`. Call `drop_privileges(&[SE_BACKUP, SE_RESTORE,
   SE_MANAGE_VOLUME, SE_CHANGE_NOTIFY])` first thing.
2. `PipeServer::create(ServerConfig { user_sid: <interactive user>, verifier:
   TrustPolicy::signed(app_path) (DevUnsigned in debug), elevated:
   is_elevated(), capabilities, .. })`. In on-demand mode the user SID is the
   helper's own token user; in service mode it must come from the
   interactive session (`WTSQueryUserToken`).
3. Accept loop: `accept(None)` → per connection, one reader thread
   dispatching `recv_request` and worker threads sending on the shared
   `ServerConnection`; track in-flight ids for `Cancel`; exit on `Shutdown`
   or after an idle timeout with no client.
4. `ScanVolume`: `enable_privilege(SE_BACKUP)` for the scan, run
   `strata-ntfs`, send `ScanBatch` every `batch_size` records and
   `ScanProgress` on the interval, `ScanDone` at the end.
5. `QueryUsnJournal`/`ReadUsn`/`ReadRecords`: FSCTLs + `OpenFileById` through
   `strata-ntfs`; `PrivilegedDelete` through `strata-clean`'s validated path
   with the never-list, under `enable_privilege(SE_RESTORE)` only for the call.
6. App side: `launch_elevated(helper, ["--pipe", name, "--app", exe])` →
   `PipeClient::connect` with `server_verifier =
   TrustPolicy::signed(helper)`; on `IpcError::Disconnected` show the banner
   and offer reconnect; on `LaunchError::Declined` fall back to the walker.
