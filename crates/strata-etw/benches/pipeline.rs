//! Decode and pipeline throughput on synthetic streams:
//! `cargo bench -p strata-etw`.
//!
//! 1. Decode only: Kernel-File `Write` v1 payloads through `Decoder::decode`.
//! 2. Full pipeline (`Tracker::process_raw`: filter, sample, decode, map,
//!    aggregate) on a realistic mix: 85% writes, 5% opens, 5% new files,
//!    3% deletes, 2% renames, over 64 processes, 4096 files in 256
//!    directories on two volumes, spanning three hours of event time.
//! 3. The same stream with 1-in-4 write sampling.
//!
//! CPU is the benchmark thread's own kernel + user time (`GetThreadTimes`),
//! which is what the overhead guard measures on the consumer thread. The
//! "cost at 10k events/s" column is the share of one core the consumer would
//! use at that event rate.

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::time::Instant;

use common::*;
use strata_etw::decode::Decoder;
use strata_etw::ffi::thread_cpu_time;

/// `GetCurrentThread()` pseudo-handle.
const CURRENT_THREAD: isize = -2;

const EVENTS: usize = 1_000_000;

fn stream() -> Vec<Ev> {
    let mut evs = Vec::with_capacity(EVENTS + 64);
    for p in 0..64u32 {
        evs.push(proc_start(
            1000 + p,
            T0 - 10,
            &format!(r"\Device\HarddiskVolume3\Apps\Vendor{p}\app{p}.exe"),
        ));
    }
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let name = |f: u64| {
        let vol = if f.is_multiple_of(4) { 5 } else { 3 };
        format!(
            r"\Device\HarddiskVolume{vol}\Data\dir{:03}\file{f:05}.bin",
            f % 256
        )
    };
    for f in 0..4096u64 {
        let pid = 1000 + (f % 64) as u32;
        evs.push(create(pid, T0, 0x1_0000 + f, 0, &name(f)));
        evs.push(name_create(T0, 0x9_0000 + f, &name(f)));
    }
    for i in 0..EVENTS as u64 {
        let at = T0 + (i * 3 * 3600 / EVENTS as u64) as i64;
        let f = rnd() % 4096;
        let pid = 1000 + (f % 64) as u32;
        let fo = 0x1_0000 + f;
        let key = 0x9_0000 + f;
        let ev = match rnd() % 100 {
            0..85 => write(pid, at, fo, key, 4096 + (rnd() % 60_000) as u32),
            85..90 => create(pid, at, fo, 0, &name(f)),
            90..95 => create_new(pid, at, fo, &name(f)),
            95..98 => delete_path(pid, at, fo, key, true, &name(f)),
            _ => rename_path(pid, at, fo, key, &name((f + 1) % 4096)),
        };
        evs.push(ev);
    }
    evs
}

fn report(label: &str, n: usize, wall: f64, cpu: f64) {
    let per_event_ns = cpu / n as f64 * 1e9;
    let core_at_10k = per_event_ns * 10_000.0 / 1e9 * 100.0;
    let cpus = std::thread::available_parallelism().map_or(1, |c| c.get()) as f64;
    println!(
        "{label:<34} {:>7.2} M ev/s  {per_event_ns:>6.0} ns/ev CPU  at 10k ev/s: {core_at_10k:.2}% of a core = {:.3}% of this {cpus}-CPU machine",
        n as f64 / wall / 1e6,
        core_at_10k / cpus,
    );
}

fn timed(f: impl FnOnce()) -> (f64, f64) {
    let c0 = thread_cpu_time(CURRENT_THREAD).unwrap();
    let t0 = Instant::now();
    f();
    let wall = t0.elapsed().as_secs_f64();
    let cpu = (thread_cpu_time(CURRENT_THREAD).unwrap() - c0).as_secs_f64();
    (wall, cpu)
}

fn main() {
    let d = Decoder::default();
    let writes: Vec<Ev> = (0..EVENTS as u64)
        .map(|i| write(1000, T0, i % 4096, i % 977, 4096))
        .collect();
    for round in 0..3 {
        let (wall, cpu) = timed(|| {
            for e in &writes {
                black_box(d.decode(black_box(&e.raw())).ok());
            }
        });
        report(
            &format!("decode Write v1 (round {round})"),
            writes.len(),
            wall,
            cpu,
        );
    }

    let evs = stream();
    for (rate, label) in [
        (1, "pipeline, full fidelity"),
        (4, "pipeline, 1-in-4 write sampling"),
    ] {
        for round in 0..3 {
            let (mut t, _) = tracker_at(T0 + 3 * 3600, Snapshot::default());
            t.set_sample_rate(rate);
            let (wall, cpu) = timed(|| feed(&mut t, &evs));
            report(&format!("{label} (round {round})"), evs.len(), wall, cpu);
            let s = t.stats();
            assert_eq!(s.decode_errors, 0);
            if round == 2 {
                let (wall, cpu) = timed(|| {
                    black_box(t.flush());
                });
                println!(
                    "  flush: {:.1} ms wall, {:.1} ms CPU; stats {s:?}",
                    wall * 1e3,
                    cpu * 1e3
                );
            }
        }
    }
}
