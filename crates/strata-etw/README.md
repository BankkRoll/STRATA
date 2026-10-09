# strata-etw

Live file-activity attribution for Strata: which program wrote, created and deleted files, and where.

Activity tracking is opt-in and runs inside the elevated helper, because Windows only lets administrators start kernel trace sessions. An unelevated caller gets `EtwError::NotElevated` and no session is created. Activity data stays on the machine.

- **Session.** One real-time ETW session named `Strata-FileActivity` with the `Microsoft-Windows-Kernel-File` and `Microsoft-Windows-Kernel-Process` providers. A guard stops it on drop or panic, and a session left behind by a crash is taken over at start or stopped by `recover_orphaned_session()`.
- **Decoding.** Built-in payload layouts, checked against the manifests installed on the machine through TDH. Newer event versions are loaded from the manifest at start. Malformed payloads are counted and skipped, never panic.
- **Mapping.** Processes are keyed by (pid, start time), so a reused pid is never blamed for another program's writes. File objects and file keys map to names, and NT device paths become drive paths through an injected `DeviceMap`.
- **Aggregation.** Bytes written, files created and files deleted per process per directory, over now / last hour / today windows. `ActivityBatch` carries hourly rollups for `Store::record_activity` and last writers for `Store::set_last_writers`. `Tracker::evidence` yields prefix → app weights for `AppCatalog::add_evidence`.
- **Overhead guard.** Measures the consumer thread's CPU time. Above the cap (2% of the machine by default, sustained for 30 s), it decodes only 1 in N writes (N = 4, then 16, then 64) and scales their sizes by N. It also raises `suggest_disable`.

```rust
strata_etw::recover_orphaned_session()?;
let (monitor, rx) = Monitor::start(MonitorConfig::default(), DeviceMap::current()?, Arc::new(SystemClock))?;
for msg in rx {
    match msg {
        MonitorMessage::Batch(b) => { store.record_activity(&b.samples)?; store.set_last_writers(&b.last_writes)?; }
        MonitorMessage::Overhead(r) if r.suggest_disable => { /* offer to turn tracking off */ }
        MonitorMessage::Stopped { .. } => break,
        _ => {}
    }
}
```

`Monitor::clear()` together with `Store::clear_activity()` erases all activity data.

## Testing

`cargo test -p strata-etw` runs unelevated. It covers decoding of synthetic byte-level events, pid reuse, renames, deletes, reused file objects, device paths, hour-boundary rollups, sampling, evidence weights, session-guard logic, and the comparison of the built-in layouts with the installed manifests. Real sessions are tested from an elevated terminal with `cargo test -p strata-etw --test live_session -- --ignored --nocapture`. `cargo bench -p strata-etw` measures decode and pipeline throughput: the full pipeline costs about 1.1 µs of CPU per event, or about 1.1% of one core at 10,000 events/s.
