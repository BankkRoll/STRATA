# ETW activity track (`strata-etw`, M12)

Live process attribution (SPEC §11), the ETW evidence feed for app attribution
(§12.3), "last writer" (§13) and the activity settings (§19).

## Status

The library is complete and tested unelevated. A real kernel session has
**not** been run yet: this environment is not elevated (see Blockers). The
M12 acceptance item "attribution works for common apps" therefore needs one
elevated run of the procedure below. The helper/app wiring belongs to other
tracks (see "Wiring steps").

| SPEC §11 item | State | Where |
|---|---|---|
| Opt-in, helper-only real-time session | Done (`Monitor::start` refuses unelevated with `NotElevated`) | `monitor.rs` |
| Provider evaluated (Kernel-File vs NT Kernel Logger FileIo) | Done: Kernel-File + Kernel-Process | below |
| Unique fixed session name; stop a stale one on start | Done (`Strata-FileActivity`; `ALREADY_EXISTS` → stop → retry once) | `session.rs` |
| (pid → image, start time), pid reuse | Done: keyed by (pid, creation time); start/stop events + live snapshot fallback | `processes.rs` |
| File objects/keys → paths from create/name events | Done: `FileObject` from `Create`, `FileKey` from `NameCreate`/`NameDelete`, renames, bounded maps | `files.rs` |
| Device paths → drive paths | Done via injected `strata_win::path::DeviceMap` | `paths.rs` |
| Rolling windows: bytes written / created / deleted per process per directory | Done: now, last hour, since T (today) | `aggregate.rs` |
| Hourly rollups to SQLite, configurable retention | Done (shaped for `Store::record_activity`; retention stays in the store) | `aggregate.rs` |
| Activity view: top writers now / last hour / today | Done in memory (`top_writers`, `dir_totals`); store covers longer ranges | `monitor.rs` |
| "Who touched this": last writer and time | Done (`LastWrite` for `Store::set_last_writers`, follows renames) | `aggregate.rs` |
| Feeds app attribution | Done (`Evidence { prefix, app, weight }` for `AppCatalog::add_evidence`) | `evidence.rs` |
| Overhead guard: measure CPU, sample, suggest disabling | Done | `overhead.rs`, `monitor.rs` |
| Clean stop on exit; orphan cleanup on next launch | Done (RAII guard, `recover_orphaned_session`) | `session.rs` |
| Privacy: local only, clear with one click | Done (`Monitor::clear` + `Store::clear_activity`; nothing leaves the process except through the channel) | |
| Verified with a real elevated session | **Blocked** (not elevated) | `tests/live_session.rs` (ignored) |

## Provider choice and evidence

### Microsoft-Windows-Kernel-File (chosen) vs NT Kernel Logger FileIo

| | Kernel-File (manifest provider) | NT Kernel Logger, FileIo class |
|---|---|---|
| Session | Any named real-time session, shareable with other providers (Kernel-Process here) | The single `NT Kernel Logger` session, or one of 8 system loggers (Win8+) with `EVENT_TRACE_SYSTEM_LOGGER_MODE`; conflicts with Process Monitor/xperf/WPR that also use it |
| Process id | `EVENT_HEADER.ProcessId` is the issuing process | FileIo events carry only the thread id (`TTID`); you need Thread start/rundown events to map threads to processes |
| Filtering | Keywords per operation (`CREATE` 0x80, `WRITE` 0x200, `DELETE_PATH` 0x400, `RENAME_SETLINK_PATH` 0x800, `CREATE_NEW_FILE` 0x1000, `FILENAME` 0x10), so we subscribe only to what we count | `EVENT_TRACE_FLAG_FILE_IO` + `FILE_IO_INIT` enable every file operation (read, cleanup, close, query, dir enum...) |
| Deletes/renames | Dedicated `DeletePath` (26) and `RenamePath` (27) events with the path | `FileIo_Info` (SetInfo) without a path; you resolve it via `FileObject` |
| Creates | `CreateNewFile` (30) marks only creates that made a new file | Only `FileIo_Create` (no new-vs-existing outcome without `OpEnd`) |
| Metadata | Registered manifest, so TDH describes every version | MOF classes |

Evidence gathered on the dev machine (Windows 11, unelevated):

- `wevtutil gp Microsoft-Windows-Kernel-File /ge /gm` lists the keywords above
  and events 10-34. The keyword masks show `Create` (12) = `0xa0`
  (FILEIO|CREATE) and `Write` (16) = `0x220` (FILEIO|WRITE). So enabling
  CREATE and WRITE without FILEIO (0x20) leaves out the cleanup, close, read
  and query events (`MatchAnyKeyword`).
- `TdhGetManifestEventInformation` (no session, no elevation) returned every
  layout used. The test `tdh::builtin_layouts_match_installed_manifests`
  compares the built-in tables field for field. Kernel-Process start has
  versions 0-4: v3 inserted `ProcessSequenceNumber` and a variable-length
  `MandatoryLabel` SID before `ImageName`. Stop has versions 0-2, and its
  `ImageName` is an ANSI file name, not a path.
- `logman start Strata-Probe -p Microsoft-Windows-Kernel-File 0x200 -ets` →
  "Access is denied" unelevated, which confirms the elevation requirement.

### Direct `windows` bindings (chosen) vs `ferrisetw`

Checked on crates.io (2026-10-09):

- `ferrisetw` 1.2.0 is the latest release (2024-06-27; no release in more
  than two years). It depends on `windows ^0.57` (the workspace pins 0.62,
  which would mean a second `windows` build), `zerocopy 0.7`, `rand 0.8`,
  `num`, `memoffset`, `bitflags 1`. It decodes through a per-event TDH schema
  lookup.
- `ferrisetw2` 2.0.0 is a fork (first release 2026-09-18, 26 downloads),
  on `windows ^0.62`. Too new to depend on.
- The direct path needs only five control/consumer calls
  (`StartTraceW`, `EnableTraceEx2`, `ControlTraceW`, `OpenTraceW`,
  `ProcessTrace`/`CloseTrace`) and one TDH metadata call. All live in
  `ffi.rs` (about 440 lines including TDH and thread times). We get full control
  of the stale-session takeover and stop order, with no new dependencies
  (Cargo.lock only gains the crate itself).

**Decoding:** hand-walked layouts, not TDH per event. `TdhGetEventInformation`
+ `TdhGetProperty` re-resolve the schema and copy each property for every
event. The layout walk reads a few offsets: 188 ns/event for a `Write` in
the bench, still unoptimized. TDH remains the source of truth: layouts for
versions this build does not know are loaded from the installed manifest at
session start (`Decoder::with_installed_manifests`). Fields are looked up by
name, so a version that appends fields still decodes.

## Public API

```rust
// helper
strata_etw::recover_orphaned_session() -> Result<bool, EtwError>          // call first on helper start
Monitor::start(MonitorConfig, DeviceMap, Arc<dyn Clock>) -> Result<(Monitor, Receiver<MonitorMessage>), EtwError>
Monitor::{top_writers(Window, limit), dir_totals(Window, limit), evidence(since, &EvidenceConfig),
          stats(), manifest_errors(), set_devices(DeviceMap), clear(), stop() -> Result<SessionStats, _>}
MonitorMessage::{Batch(ActivityBatch), Overhead(OverheadReport), Stopped { reason, stats, session, panics }}
ActivityBatch { samples: Vec<strata_store::ActivitySample>, last_writes: Vec<strata_store::LastWrite> }
OverheadReport { cpu_percent: f32, sample_rate: u32, suggest_disable: bool }
Window::{Now, LastHour, Since(Timestamp)}      // Since(local midnight) = "today"
WriterSummary { image, counts: Counts, dirs }, DirTotal { image, dir, counts }
Counts { bytes_written, files_created, files_deleted }
Evidence { prefix, app /* image file name */, image, weight /* 0..=1 */, share }
MonitorConfig { session: SessionConfig, flush_interval (10 s), overhead_interval (5 s), overhead: OverheadConfig }
OverheadConfig { cap_percent (2.0), sustain_samples (6), max_sample_rate (64) }
EtwError::{NotElevated, Win32 { op, code }, Thread(_)}

// FFI-free pipeline (tests, benches, tools)
Tracker::new(Decoder, DeviceMap, Box<dyn ProcessSource>, Arc<dyn Clock>)
Tracker::{process_raw(&RawEvent), apply(&Event, scale), flush() -> ActivityBatch, top_writers, dir_totals,
          evidence, set_sample_rate, stats, clear}
session::{SessionGuard<C: TraceControl>, TraceControl, SessionConfig, recover_orphaned_session_with, SESSION_NAME}
decode::{Decoder, RawEvent, Event, EventKind, builtin_layouts}, tdh::{manifest_layout, load_layouts}
evidence::evidence(&[DirActivity], &EvidenceConfig), processes::{ProcessTable, ProcessSource, LiveProcesses}
```

All message and result types are `Serialize + Deserialize` for the helper → app pipe.

## Design notes

- **Session:** real-time, `ClientContext = 2` (system time), so event
  timestamps are FILETIMEs comparable with process creation times. Buffers
  are 64 KiB, 4-64 of them, with a 1 s flush timer. Providers:
  Kernel-Process keyword 0x10 (start/stop) and Kernel-File keywords
  `0x1E90`, level 4.
- **Shutdown order:** timer thread first (it samples the consumer thread's
  handle), then `ControlTraceW(STOP)` (so `ProcessTrace` returns), join the
  consumer, final flush, `Stopped`. If the stop fails, the consumer thread is
  detached instead of joined (no hang), the guard retries on drop, and the
  next launch's `recover_orphaned_session` catches anything left. A session
  stopped from outside is reported as `StopReason::SessionEnded(status)`.
- **Callback:** contains panics with `catch_unwind` (no unwinding into
  `ProcessTrace`) and counts them in `Stopped.panics`.
- **Counting rules:**
  - Bytes are `Write` events except paging I/O (`IRP_PAGING_IO` 0x2,
    `IRP_SYNCHRONOUS_PAGING_IO` 0x40). A cached write is attributed to the
    program that made it, and the lazy writer's System-process flush is not
    counted twice.
  - Created files are `CreateNewFile`. Deletes are `DeletePath` with a
    delete disposition (class 13: BOOLEAN; class 64: `FILE_DISPOSITION_DELETE`
    bit; undeletes are ignored) plus `Create` with `FILE_DELETE_ON_CLOSE`.
  - Last writer is every counted write and every created file.
- **Rename convention:** whichever name `RenamePath.FilePath` carries, the
  mapping ends on the new name. A different absolute path is taken as the new
  name. Equal to the current name means the old name, and the following
  `NameCreate` supplies the new one. A relative path is ignored. Pending
  last-writer entries move with the rename.
- **Process identity:** an event resolves to the process alive at its
  timestamp. Late events are allowed for 10 s after the stop. For pids with
  no start event, the live snapshot (`ProcessInfo::of`) is used only if that
  process started before the event, so a reused pid stays unattributed rather
  than misattributed. Failed lookups are not retried for 30 s. Pid 4 is
  `System`, pid 0 is ignored.
- **Memory bounds:**
  - File maps hold two generations (default 256 Ki entries per map).
  - Directories and processes are `Arc`s hashed by identity, with the
    interner swept at each flush.
  - Windows keep 61 minute buckets and 49 hour buckets.
  - Pending rollups and last writers are drained every flush (10 s).
- **Evidence weight:** `share × min(1, active_hours / 3)`, emitted only when
  share ≥ 0.6 and the directory saw ≥ 64 KiB of activity score (bytes +
  4 KiB per create/delete). It is computed only for directories actually
  written to, never for their ancestors. The image file name is the label
  (the classifier fuzzy-matches it). A sole writer across ≥ 3 distinct hours
  reaches 1.0 (high confidence in `add_evidence`); a single burst stays
  heuristic. `add_evidence` accumulates, so feed it once per freshly built
  catalog.
- **Overhead guard:**
  - The cap is a percentage of total machine CPU (Task Manager scale),
    taken from `activity.cpu_cap_percent`.
  - Sustained means all 6 samples over 5 s each (30 s). Then writes are
    sampled 1/4 → 1/16 → 1/64, with sizes scaled ×N, and `suggest_disable`
    is raised.
  - Creates, deletes, renames, name and process events are never sampled,
    because the maps depend on them.
  - It steps down when the projected cost at the lower rate (×4) is below
    half the cap for 30 s, and the suggestion clears at 1/1.

## Tests

`cargo test -p strata-etw` (unelevated): **51 run, all pass** (29 unit, 18
pipeline, 1 live, 3 doc). Two elevated tests are `#[ignore]`d.
`cargo clippy -p strata-etw --all-targets -- -D warnings` is clean.

| Suite | Covers |
|---|---|
| `layout` | field walking, 32/64-bit pointers, truncation at every length, opaque fields, unterminated strings, SIDs, TDH in-type mapping |
| `decode` | table covers every event, disposition rules |
| `tdh` | **built-in layouts == installed manifests** (TDH, no session needed), newer versions still carry needed fields, unknown event → `None` |
| `paths` | device → drive (case-insensitive, `Volume1` ≠ `Volume12`), `\??\`, `\Device\Mup` → UNC, unknown device kept, lone surrogates, dir interning and sweep |
| `processes` | pid reuse by time, lost stop events, snapshot fallback rejecting a reused pid, failed-lookup throttling, prune |
| `files` | reused file object, key over object, stale `NameDelete`, both rename conventions, bounded generations |
| `overhead` | sustained breach → 1/4 → 1/16 → 1/64 + suggestion, non-sustained ignored, no oscillation, step-down clears suggestion, sampler 1-in-N |
| `session` (fake control) | start/enable/stop order, stale takeover, enable failure stops the session, access denied → `NotElevated`, **panic while owning the guard stops it**, failed stop retried on drop, external stop, orphan recovery |
| `evidence` | weights by share and support, no dominant writer, minimum activity, creates/deletes count, case-insensitive dirs |
| `tests/pipeline.rs` | byte-level fixtures for every event kind (v0 and v1, 32-bit header, process start v4 with SID, stop v2), malformed payloads of every length, pid reuse, pre-existing processes, reused file object, file-key naming and release, device normalization, renames (both conventions, relative), deletes/creates/paging, **hour-boundary rollups with an injected clock**, late events, window expiry (now / hour / 48 h), **sampling**, **evidence weights**, clear, round trip through a real `Store` + JSON |
| `tests/live_session.rs` | unelevated: start refused, nothing left behind. `#[ignore]`, skip with a message when not elevated: real monitor attributes this test process's writes / creates / deletes / rename in its own temp dir; crash recovery (leaked guard) and stale-session takeover |

All fixtures are synthetic: made-up pids, pointers and paths (`C:\Users\me`,
`\Device\HarddiskVolume3`). No captures from a real machine.

## Overhead numbers

`cargo bench -p strata-etw` (release, one thread, `GetThreadTimes` CPU,
16 logical CPUs). The stream is 1M events: 85% writes, 5% opens, 5% new
files, 3% deletes, 2% renames, 64 processes, 4096 files in 256 dirs on 2
volumes, over 3 h of event time.

| Measure | Throughput | CPU/event | Consumer CPU at 10k events/s |
|---|---|---|---|
| Decode only (`Write` v1) | 5.0-5.3 M ev/s | 172-188 ns | 0.19% of a core (0.012% of the machine) |
| Full pipeline (decode + map + aggregate) | 0.71-0.93 M ev/s | 1.05-1.26 µs | **1.05-1.26% of a core (0.07-0.08% of the machine)** |
| Same with 1-in-4 write sampling | 1.58-1.68 M ev/s | 604-635 ns | 0.60-0.64% of a core |
| Flush (drain rollups after 1M events) | 0.9-1.0 ms | | |

At the default 2% machine cap (≈32% of one core here), the consumer reaches
the cap at about 250-300k events/s before sampling starts. This
measures the consumer only. The kernel-side cost of logging (in the writing
processes) needs the elevated run below (WPR/xperf or comparing a file-copy
benchmark with tracking on and off).

## Decisions

- 2026-10-09 — **Kernel-File + Kernel-Process manifest providers, not the NT
  Kernel Logger.** Process id in the header, per-operation keywords, explicit
  delete/rename/new-file events, and no contention for the single kernel
  logger (see the table above).
- 2026-10-09 — **Direct `windows` 0.62 bindings, not ferrisetw.** ferrisetw
  1.2.0 is unreleased since 2024 and pins `windows 0.57`. The fork is days
  old. Five FFI calls do not justify a second `windows` build.
- 2026-10-09 — **Hand-walked layouts validated by TDH, not TDH per event.**
  Faster, no allocation for writes, and a test fails if Windows changes a
  layout. Unknown future versions are loaded from TDH at start.
- 2026-10-09 — **Attribute writes at the IRP, skip paging I/O.** It
  attributes cached writes to the writing program and avoids double counting.
  Cost: writes through memory-mapped views (only visible as paging I/O from
  System) are not attributed.
- 2026-10-09 — **Unattributed rather than misattributed** when a pid cannot
  be tied to the process alive at the event time.
- 2026-10-09 — **Evidence only for directories actually written to.** One
  writer during a window says nothing about who owns `AppData\Local`.
- 2026-10-09 — **CPU cap as a percentage of total machine CPU** (Task Manager
  scale, matching the setting's meaning to users). The per-core figure is
  reported in the benches.
- 2026-10-09 — **Depend on `strata-store` for `ActivitySample`, `LastWrite`,
  `Timestamp`, `Clock` and `path_hash`.** Hashing must match the store
  bit for bit. Cost: the helper links bundled SQLite (it may use the store
  later anyway).
- 2026-10-09 — **Unbounded result channel.** Batches arrive every 10 s and
  overhead reports every 5 s. A bounded channel would have to drop or merge
  rollups when the helper stalls.

## Blockers

- **No elevated session in this environment.** These are untested against a
  live kernel:
  - real session start/stop;
  - that `ProcessTrace` returns promptly on `ControlTraceW(STOP)`;
  - that `RenamePath.FilePath` is one of the two conventions handled;
  - that `CreateNewFile` fires only on success;
  - whether the provider emits `NameCreate` rundown for files already open
    when tracking starts (if not, writes to those files count as
    `unmapped_writes` until they are reopened);
  - kernel-side overhead;
  - the M12 "attribution works for common apps" check.

  The logic is covered by fake-control and synthetic-stream tests, and the
  procedure below closes the gap.

## Manual elevated test procedure

From an **elevated** PowerShell in the repo, with no other ETW tool using
the name `Strata-FileActivity-Test`:

1. `cargo test -p strata-etw --test live_session -- --ignored --nocapture`
   - `live_session_attributes_this_process`: starts a monitor, writes 3 MiB
     to `%TEMP%\strata-etw-live-<pid>\download.part`, renames it to
     `download.bin`, creates and deletes `scratch.tmp`. It asserts that this
     process is the writer in that directory (bytes ≥ 3 MiB, ≥ 2 creates,
     ≥ 1 delete), that the last writer follows the rename, that the final
     message is `Stopped(Requested)`, and that the session is gone. It prints
     the observed totals and `TrackerStats` (check `unmapped_writes` and
     `unattributed`). It deletes only the directory it created.
   - `live_crash_recovery_and_stale_takeover`: leaks a session (simulated
     crash), checks that `recover_orphaned_session_with` stops it, then
     checks that a new start takes over a leaked session.
2. Crash check by hand:
   - Start a long run (`cargo test ... live_session_attributes_this_process`)
     and kill it with Task Manager during the 10 s wait.
   - `logman query -ets` lists `Strata-FileActivity-Test`. A further run of
     step 1 (or `recover_orphaned_session` in the helper) removes it.
   - `logman query -ets` no longer lists it.
3. Common apps (M12 acceptance), once wired into the helper:
   - Enable tracking.
   - Use a browser (download a file), a package manager
     (`npm install` / `cargo build`), and an app that writes to its own data
     folder.
   - Check that the Activity view lists each under its image, that "Who
     touched this" on the downloaded file names the browser, and that the
     classifier shows activity evidence for the app's data folder.
4. Overhead:
   - Copy a large tree (`robocopy` of a node_modules folder) with tracking
     off and then on, and compare the time.
   - Watch the `Overhead` messages: `cpu_percent` should stay under the cap,
     and `sample_rate` should be 1 under normal use.

Record results (no machine-specific paths) in this file.

## Wiring steps

### Helper (`strata-helper`)

1. At start, call `strata_etw::recover_orphaned_session()` and ignore
   `NotElevated`.
2. On `StartActivity { cpu_cap_percent }`:
   - Build `MonitorConfig { overhead: OverheadConfig { cap_percent, ..}, ..}`.
   - Call `Monitor::start(cfg, DeviceMap::current()?, Arc::new(SystemClock))`.
   - Forward each `MonitorMessage` as a pipe event (below).
   - Rebuild `DeviceMap` and call `set_devices` on `VolumeWatcher` events.
3. On `StopActivity`, client disconnect or helper exit, call
   `monitor.stop()`. Drop also stops the session.
4. `QueryActivity { window, limit }` → `monitor.top_writers` /
   `dir_totals`. `ClearActivity` → `monitor.clear()`.
   `ActivityEvidence { since }` → `monitor.evidence`.

### IPC (`strata-ipc`) — change request

`Response` derives `Eq` and must not depend on the store, so the protocol
carries plain mirror rows (the helper converts). Bump the version:

```diff
--- a/crates/strata-ipc/src/protocol.rs
+++ b/crates/strata-ipc/src/protocol.rs
@@
-pub const PROTOCOL_VERSION: u32 = 1;
+pub const PROTOCOL_VERSION: u32 = 2;
@@ pub struct Capabilities {
     pub privileged_delete: bool,
+    /// ETW activity tracking (elevated helper only).
+    pub activity: bool,
 }
@@ pub enum Request {
+    /// Start activity tracking; streams `ActivityBatch` / `ActivityHealth`
+    /// until `StopActivity`, ending with `ActivityStopped`.
+    StartActivity {
+        /// Sustained CPU cap, in hundredths of a percent of total CPU.
+        cpu_cap_centi_percent: u32,
+    },
+    /// Stop activity tracking.
+    StopActivity,
+    /// Forget in-memory activity in the helper.
+    ClearActivity,
+    /// Top writers from the helper's in-memory windows.
+    QueryActivity {
+        /// 0 = now, 1 = last hour, 2 = since `since_unix`.
+        window: u8,
+        /// UTC seconds, used when `window == 2`.
+        since_unix: i64,
+        /// Maximum rows.
+        limit: u32,
+    },
@@ pub enum Response {
+    /// Hourly rollup deltas and last writers to persist.
+    ActivityBatch {
+        /// (hour start UTC s, image, dir path hash, bytes, created, deleted).
+        rows: Vec<ActivityRow>,
+        /// Last writers.
+        last_writes: Vec<LastWriteRow>,
+    },
+    /// Overhead sample.
+    ActivityHealth {
+        /// Consumer CPU, hundredths of a percent of total CPU.
+        cpu_centi_percent: u32,
+        /// Writes sampled 1 in N.
+        sample_rate: u32,
+        /// Offer to disable tracking.
+        suggest_disable: bool,
+    },
+    /// Tracking ended (requested, or the session ended with a status).
+    ActivityStopped { requested: bool, status: u32, events_lost: u32 },
+    /// Reply to `QueryActivity`.
+    ActivityTop { writers: Vec<WriterRow> },
 }
+
+/// One hourly rollup delta.
+#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
+pub struct ActivityRow { pub hour: i64, pub image: String, pub dir_hash: u64,
+    pub bytes_written: u64, pub files_created: u64, pub files_deleted: u64 }
+/// One last writer.
+#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
+pub struct LastWriteRow { pub path_hash: u64, pub image: String, pub pid: Option<u32>, pub at: i64 }
+/// One top-writer row.
+#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
+pub struct WriterRow { pub image: String, pub bytes_written: u64,
+    pub files_created: u64, pub files_deleted: u64, pub dirs: u32 }
```

(Each new field also needs a rustdoc line, per the repo rules.) Conversion
in the helper: `ActivitySample { at, image, dir_hash, .. }` → `ActivityRow {
hour: at.0, .. }`, `LastWrite { at, .. }` → `LastWriteRow { at: at.0, .. }`,
`OverheadReport.cpu_percent` → `(cpu_percent * 100.0).round() as u32`.

### App (`src-tauri`)

1. When `settings.activity.enabled` turns on, launch/ask the helper:
   `StartActivity { cpu_cap_centi_percent: (cpu_cap_percent * 100.0) as u32 }`.
   On turn-off, send `StopActivity`.
2. `ActivityBatch` → on the store's background thread:
   - `store.record_activity(&rows→ActivitySample)`;
   - `store.set_last_writers(&rows→LastWrite)`.
3. `ActivityHealth { suggest_disable: true }` → non-blocking banner: "Activity
   tracking is using more CPU than your cap and is now sampling. [Turn off]".
4. Activity view:
   - now / last hour from `QueryActivity` (helper memory);
   - today and longer from `store.top_writers(since)` /
     `store.dir_writers(path_hash(dir), since, n)`. "Today" =
     `Timestamp(local midnight in UTC seconds)`.
5. Detail panel "Who touched this" (§13): `store.last_writer(path_hash(path))`.
6. Daily: `store.prune_activity(settings.activity.retention_days)` (already
   in the store track's notes).
7. Settings → Data → "Clear activity data":
   - `store.clear_activity()`;
   - `ClearActivity` to the helper if tracking is running.
8. Attribution (§12.3): after building `AppCatalog`, feed it once:
   `for e in evidence { catalog.add_evidence(&e.prefix, &e.app, e.weight) }`.
   Evidence comes from the helper (`monitor.evidence(now - 48 h, &Default)`,
   via an `ActivityEvidence` request shaped like `QueryActivity`). It can
   also be built from store rollups by passing `DirActivity` rows to
   `strata_etw::evidence::evidence`, though that needs a store query
   returning dir paths and active-hour counts (not provided by the store
   today; optional).

### Store (`strata-store`)

No change needed. The tables, `record_activity` (sums repeated flushes into
the hour), last-writer ordering, retention and clear already match.
Optional, for evidence over more than 48 h: a query returning
`(dir_hash, image, sum(counts), count(distinct hour))` since T.

### Classify (`strata-classify`)

No change needed. `add_evidence(prefix, app, weight)` matches `Evidence`.

## Notes

- Strata's own writes (the store's SQLite files, the helper's logs) are
  attributed like any other process. The UI may want to hide its own images.
- `Cargo.lock` only gains the `strata-etw` package entry (all dependencies
  already existed). No root `Cargo.toml` change: `members = ["crates/*"]`
  picks the crate up.
