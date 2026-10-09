//! Synthetic event fixtures, encoded byte for byte from the manifest
//! layouts (see `strata_etw::decode`). Every path, pid and pointer is made
//! up; nothing here comes from a real capture.

#![allow(dead_code)]

use std::sync::Arc;

use strata_core::FileTime;
use strata_etw::decode::{Decoder, RawEvent, file_id, process_id};
use strata_etw::layout::Provider;
use strata_etw::processes::ProcessSource;
use strata_etw::tracker::Tracker;
use strata_store::{ManualClock, Timestamp};
use strata_win::path::DeviceMap;

/// Little-endian payload builder.
#[derive(Debug, Clone)]
pub struct Payload {
    pub bytes: Vec<u8>,
    pub ptr: usize,
}

impl Payload {
    pub fn new(ptr: usize) -> Self {
        Self {
            bytes: Vec::new(),
            ptr,
        }
    }
    pub fn ptr(mut self, v: u64) -> Self {
        if self.ptr == 4 {
            self.bytes.extend((v as u32).to_le_bytes());
        } else {
            self.bytes.extend(v.to_le_bytes());
        }
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.bytes.extend(v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.bytes.extend(v.to_le_bytes());
        self
    }
    pub fn wstr(mut self, s: &str) -> Self {
        for u in s.encode_utf16().chain([0]) {
            self.bytes.extend(u.to_le_bytes());
        }
        self
    }
    pub fn astr(mut self, s: &str) -> Self {
        self.bytes.extend(s.as_bytes());
        self.bytes.push(0);
        self
    }
    /// A SID with `n` sub-authorities (S-1-16-12288 style labels have 1).
    pub fn sid(mut self, n: u8) -> Self {
        self.bytes.extend([1, n, 0, 0, 0, 0, 0, 16]);
        for i in 0..n {
            self.bytes.extend((0x3000u32 + u32::from(i)).to_le_bytes());
        }
        self
    }
}

/// An owned synthetic event.
#[derive(Debug, Clone)]
pub struct Ev {
    pub provider: Provider,
    pub id: u16,
    pub version: u8,
    pub pid: u32,
    pub ts: FileTime,
    pub ptr: usize,
    pub data: Vec<u8>,
}

impl Ev {
    pub fn raw(&self) -> RawEvent<'_> {
        RawEvent {
            provider: self.provider,
            id: self.id,
            version: self.version,
            pid: self.pid,
            timestamp: self.ts,
            pointer_size: self.ptr,
            data: &self.data,
        }
    }
}

pub fn ft(secs: i64) -> FileTime {
    FileTime::from_unix_secs(secs)
}

/// 2026-10-09T10:00:00Z.
pub const T0: i64 = 1_791_540_000;

fn file(id: u16, version: u8, pid: u32, at: i64, p: Payload) -> Ev {
    Ev {
        provider: Provider::KernelFile,
        id,
        version,
        pid,
        ts: ft(at),
        ptr: p.ptr,
        data: p.bytes,
    }
}

pub fn create(pid: u32, at: i64, fo: u64, options: u32, name: &str) -> Ev {
    let p = Payload::new(8)
        .ptr(0xFFFF_0000_0000_1000)
        .ptr(fo)
        .u32(4242)
        .u32(options)
        .u32(0x80)
        .u32(7)
        .wstr(name);
    file(file_id::CREATE, 1, pid, at, p)
}

pub fn create_new(pid: u32, at: i64, fo: u64, name: &str) -> Ev {
    let mut e = create(pid, at, fo, 2 << 24, name);
    e.id = file_id::CREATE_NEW_FILE;
    e
}

pub fn name_create(at: i64, key: u64, name: &str) -> Ev {
    file(
        file_id::NAME_CREATE,
        0,
        4,
        at,
        Payload::new(8).ptr(key).wstr(name),
    )
}

pub fn name_delete(at: i64, key: u64, name: &str) -> Ev {
    let mut e = name_create(at, key, name);
    e.id = file_id::NAME_DELETE;
    e
}

pub fn write_ptr(pid: u32, at: i64, fo: u64, key: u64, size: u32, flags: u32, ptr: usize) -> Ev {
    let p = Payload::new(ptr)
        .u64(4096)
        .ptr(0xFFFF_0000_0000_2000)
        .ptr(fo)
        .ptr(key)
        .u32(4243)
        .u32(size)
        .u32(flags)
        .u32(0);
    file(file_id::WRITE, 1, pid, at, p)
}

pub fn write(pid: u32, at: i64, fo: u64, key: u64, size: u32) -> Ev {
    write_ptr(pid, at, fo, key, size, 0, 8)
}

#[allow(clippy::too_many_arguments)]
fn path_event(
    id: u16,
    pid: u32,
    at: i64,
    fo: u64,
    key: u64,
    class: u32,
    extra: u64,
    path: &str,
) -> Ev {
    let p = Payload::new(8)
        .ptr(0xFFFF_0000_0000_3000)
        .ptr(fo)
        .ptr(key)
        .ptr(extra)
        .u32(4244)
        .u32(class)
        .wstr(path);
    file(id, 1, pid, at, p)
}

pub fn delete_path(pid: u32, at: i64, fo: u64, key: u64, delete: bool, path: &str) -> Ev {
    path_event(
        file_id::DELETE_PATH,
        pid,
        at,
        fo,
        key,
        13,
        u64::from(delete),
        path,
    )
}

pub fn delete_path_ex(pid: u32, at: i64, fo: u64, key: u64, flags: u64, path: &str) -> Ev {
    path_event(file_id::DELETE_PATH, pid, at, fo, key, 64, flags, path)
}

pub fn rename_path(pid: u32, at: i64, fo: u64, key: u64, path: &str) -> Ev {
    path_event(file_id::RENAME_PATH, pid, at, fo, key, 10, 0, path)
}

/// Kernel-Process start, version 4 (with the variable-length label SID).
pub fn proc_start(pid: u32, created: i64, image: &str) -> Ev {
    let p = Payload::new(8)
        .u32(pid)
        .u64(77)
        .u64(ft(created).0)
        .u32(1)
        .u64(76)
        .u32(1)
        .u32(0)
        .u32(3)
        .u32(0)
        .sid(1)
        .wstr(image)
        .u32(0)
        .u32(0)
        .wstr("")
        .wstr("")
        .u32(0);
    Ev {
        provider: Provider::KernelProcess,
        id: process_id::START,
        version: 4,
        pid,
        ts: ft(created),
        ptr: 8,
        data: p.bytes,
    }
}

/// Kernel-Process stop, version 2.
pub fn proc_stop(pid: u32, created: i64, at: i64) -> Ev {
    let p = Payload::new(8)
        .u32(pid)
        .u64(77)
        .u64(ft(created).0)
        .u64(ft(at).0)
        .u32(0)
        .u32(3)
        .u32(10)
        .u64(1 << 20)
        .u64(2 << 20)
        .u64(123_456)
        .u32(1)
        .u32(2)
        .u32(3)
        .u32(4)
        .u32(5)
        .astr("tool.exe");
    Ev {
        provider: Provider::KernelProcess,
        id: process_id::STOP,
        version: 2,
        pid,
        ts: ft(at),
        ptr: 8,
        data: p.bytes,
    }
}

/// A process source that knows nothing (every process must have a start
/// event) unless entries are added.
#[derive(Debug, Default)]
pub struct Snapshot(pub Vec<(u32, String, FileTime)>);

impl ProcessSource for Snapshot {
    fn lookup(&mut self, pid: u32) -> Option<(String, FileTime)> {
        self.0
            .iter()
            .find(|e| e.0 == pid)
            .map(|e| (e.1.clone(), e.2))
    }
}

pub fn devices() -> DeviceMap {
    DeviceMap::from_entries([
        (r"\Device\HarddiskVolume3".to_owned(), r"C:\".into()),
        (r"\Device\HarddiskVolume5".to_owned(), r"D:\".into()),
    ])
}

pub fn tracker_at(now: i64, snapshot: Snapshot) -> (Tracker, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(Timestamp(now)));
    let t = Tracker::new(
        Decoder::default(),
        devices(),
        Box::new(snapshot),
        clock.clone(),
    );
    (t, clock)
}

pub fn feed(t: &mut Tracker, events: &[Ev]) {
    for e in events {
        t.process_raw(&e.raw());
    }
}
