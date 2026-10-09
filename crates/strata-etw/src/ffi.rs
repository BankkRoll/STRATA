//! Every Win32 call of the crate (the only module with `unsafe`).
//!
//! - [`Win32Control`]: `StartTraceW` / `EnableTraceEx2` / `ControlTraceW`
//!   behind [`TraceControl`], used through [`crate::session::SessionGuard`].
//! - [`RealtimeConsumer`]: `OpenTraceW` + `ProcessTrace` + `CloseTrace` with
//!   an event-record callback that hands each event to a [`Tracker`].
//! - [`manifest_layout`] wraps `TdhGetManifestEventInformation`.
//! - [`thread_cpu_time`] wraps `GetThreadTimes` for the overhead guard.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use strata_core::FileTime;
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, FILETIME, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_DESCRIPTOR, EVENT_HEADER_FLAG_32_BIT_HEADER, EVENT_PROPERTY_INFO, EVENT_RECORD,
    EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE,
    ProcessTrace, PropertyParamCount, PropertyParamFixedCount, PropertyParamFixedLength,
    PropertyParamLength, PropertyStruct, StartTraceW, TRACE_EVENT_INFO,
    TdhGetManifestEventInformation, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Threading::GetThreadTimes;
use windows::core::{GUID, PCWSTR, PWSTR};

use crate::decode::RawEvent;
use crate::error::{EtwError, code};
use crate::layout::{Field, FieldType, Layout, Provider};
use crate::session::{ProviderSpec, SessionConfig, SessionStats, TraceControl};
use crate::tracker::Tracker;
use crate::{KERNEL_FILE_PROVIDER, KERNEL_PROCESS_PROVIDER};

// -----------------------------------------------------------------------------
// Trace control
// -----------------------------------------------------------------------------

/// `WNODE_HEADER.ClientContext = 2`: system-time timestamps, so event times
/// are FILETIMEs directly comparable with process creation times.
const CLOCK_SYSTEM_TIME: u32 = 2;

/// Room after `EVENT_TRACE_PROPERTIES` for the logger name and log file
/// name that `ControlTraceW` copies back (1024 UTF-16 units each).
const NAME_ROOM: usize = 2 * 1024 * 2;

/// An `EVENT_TRACE_PROPERTIES` followed by name storage, 8-byte aligned.
struct Properties {
    buf: Vec<u64>,
}

impl Properties {
    fn new() -> Self {
        let size = size_of::<EVENT_TRACE_PROPERTIES>() + NAME_ROOM;
        let mut p = Self {
            buf: vec![0; size.div_ceil(8)],
        };
        let head = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let props = p.props();
        props.Wnode.BufferSize = size as u32;
        props.LoggerNameOffset = head;
        props.LogFileNameOffset = head + (NAME_ROOM / 2) as u32;
        p
    }

    fn props(&mut self) -> &mut EVENT_TRACE_PROPERTIES {
        // SAFETY: the buffer is 8-byte aligned, zero-initialized (a valid
        // EVENT_TRACE_PROPERTIES) and larger than the struct.
        unsafe { &mut *self.buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>() }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn status(e: WIN32_ERROR) -> Result<(), u32> {
    if e == ERROR_SUCCESS { Ok(()) } else { Err(e.0) }
}

/// The real [`TraceControl`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32Control;

impl TraceControl for Win32Control {
    fn start(&mut self, cfg: &SessionConfig) -> Result<u64, u32> {
        let mut p = Properties::new();
        let props = p.props();
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.Wnode.ClientContext = CLOCK_SYSTEM_TIME;
        props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        props.BufferSize = cfg.buffer_kb;
        props.MinimumBuffers = cfg.min_buffers;
        props.MaximumBuffers = cfg.max_buffers;
        props.FlushTimer = cfg.flush_secs;
        props.LogFileNameOffset = 0;
        let name = wide(&cfg.name);
        let mut handle = CONTROLTRACE_HANDLE::default();
        // SAFETY: `name` is NUL-terminated; `p` is a properties block with
        // room for the logger name at LoggerNameOffset, as StartTraceW
        // requires.
        let r = unsafe { StartTraceW(&mut handle, PCWSTR(name.as_ptr()), p.props()) };
        status(r).map(|()| handle.Value)
    }

    fn enable(&mut self, handle: u64, provider: &ProviderSpec) -> Result<(), u32> {
        let guid = GUID::from_u128(provider.guid);
        // SAFETY: plain FFI with a valid GUID pointer and no parameters block.
        let r = unsafe {
            EnableTraceEx2(
                CONTROLTRACE_HANDLE { Value: handle },
                &guid,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                provider.level,
                provider.keywords,
                0,
                0,
                None,
            )
        };
        status(r)
    }

    fn stop(&mut self, name: &str) -> Result<SessionStats, u32> {
        let mut p = Properties::new();
        let n = wide(name);
        // SAFETY: `n` is NUL-terminated and `p` has room for both names.
        let r = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(n.as_ptr()),
                p.props(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        status(r)?;
        let props = p.props();
        Ok(SessionStats {
            events_lost: props.EventsLost,
            realtime_buffers_lost: props.RealTimeBuffersLost,
        })
    }

    fn exists(&mut self, name: &str) -> Result<bool, u32> {
        let mut p = Properties::new();
        let n = wide(name);
        // SAFETY: as in `stop`.
        let r = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(n.as_ptr()),
                p.props(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        match status(r) {
            Ok(()) => Ok(true),
            Err(code::NOT_FOUND) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// -----------------------------------------------------------------------------
// Real-time consumer
// -----------------------------------------------------------------------------

/// State shared with the event callback.
#[derive(Debug)]
pub struct CallbackState {
    /// The pipeline.
    pub tracker: Arc<Mutex<Tracker>>,
    /// Events whose processing panicked (the panic is contained).
    pub panics: AtomicU64,
    /// Set once a panic has been seen, for the monitor to report.
    pub poisoned: AtomicBool,
}

/// An open real-time trace. [`RealtimeConsumer::run`] blocks in
/// `ProcessTrace` until the session stops.
#[derive(Debug)]
pub struct RealtimeConsumer {
    handle: PROCESSTRACE_HANDLE,
    /// Keeps the callback state alive at a stable address while the trace
    /// can still call back.
    state: Arc<CallbackState>,
    _name: Vec<u16>,
}

// SAFETY: the handle is an opaque kernel token usable from any thread, and
// the callback state is `Sync`.
unsafe impl Send for RealtimeConsumer {}

const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;

impl RealtimeConsumer {
    /// Opens the real-time session `name`.
    ///
    /// # Errors
    ///
    /// [`EtwError::Win32`] (`OpenTraceW`).
    pub fn open(name: &str, state: Arc<CallbackState>) -> Result<Self, EtwError> {
        let mut name_w = wide(name);
        let mut log = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name_w.as_mut_ptr()),
            ..Default::default()
        };
        log.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        log.Context = Arc::as_ptr(&state).cast_mut().cast();
        // SAFETY: `log` is fully initialized; the logger name and the context
        // outlive the trace (both are owned by the returned consumer, and
        // `run`/drop close the trace before they are freed).
        let handle = unsafe { OpenTraceW(&mut log) };
        if handle.Value == INVALID_PROCESSTRACE_HANDLE {
            let e = windows::core::Error::from_thread();
            return Err(EtwError::win32("OpenTraceW", (e.code().0 as u32) & 0xFFFF));
        }
        Ok(Self {
            handle,
            state,
            _name: name_w,
        })
    }

    /// Delivers events until the session stops, then closes the trace.
    /// Returns the `ProcessTrace` status (0 or `ERROR_CANCELLED` on a normal
    /// stop).
    pub fn run(mut self) -> u32 {
        // SAFETY: the handle is open; ProcessTrace blocks this thread and
        // calls `on_event` with our context.
        let r = unsafe { ProcessTrace(&[self.handle], None, None) };
        self.close();
        r.0
    }

    fn close(&mut self) {
        if self.handle.Value != INVALID_PROCESSTRACE_HANDLE {
            // SAFETY: closes the handle opened in `open`, exactly once.
            unsafe {
                let _ = CloseTrace(self.handle);
            }
            self.handle.Value = INVALID_PROCESSTRACE_HANDLE;
        }
    }

    /// The shared callback state.
    #[must_use]
    pub fn state(&self) -> &Arc<CallbackState> {
        &self.state
    }
}

impl Drop for RealtimeConsumer {
    fn drop(&mut self) {
        self.close();
    }
}

fn provider_of(g: &GUID) -> Option<Provider> {
    if *g == KERNEL_FILE_PROVIDER {
        Some(Provider::KernelFile)
    } else if *g == KERNEL_PROCESS_PROVIDER {
        Some(Provider::KernelProcess)
    } else {
        None
    }
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    // SAFETY: ETW passes a valid record for the duration of the callback.
    let Some(rec) = (unsafe { rec.as_ref() }) else {
        return;
    };
    // SAFETY: UserContext points into the `Arc<CallbackState>` the consumer
    // owns; it is alive until the trace is closed, after ProcessTrace returns.
    let Some(state) = (unsafe { rec.UserContext.cast::<CallbackState>().as_ref() }) else {
        return;
    };
    let h = &rec.EventHeader;
    let Some(provider) = provider_of(&h.ProviderId) else {
        return;
    };
    let data: &[u8] = if rec.UserData.is_null() || rec.UserDataLength == 0 {
        &[]
    } else {
        // SAFETY: ETW guarantees UserDataLength readable bytes at UserData
        // for the duration of the callback.
        unsafe {
            std::slice::from_raw_parts(rec.UserData.cast::<u8>(), usize::from(rec.UserDataLength))
        }
    };
    let pointer_size = if u32::from(h.Flags) & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    };
    let raw = RawEvent {
        provider,
        id: h.EventDescriptor.Id,
        version: h.EventDescriptor.Version,
        pid: h.ProcessId,
        timestamp: FileTime(h.TimeStamp as u64),
        pointer_size,
        data,
    };
    // NOTE: a panic must not unwind into ProcessTrace (undefined behavior
    // across the FFI boundary); contain it and count it.
    let r = catch_unwind(AssertUnwindSafe(|| {
        state
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .process_raw(&raw);
    }));
    if r.is_err() {
        state.panics.fetch_add(1, Ordering::Relaxed);
        state.poisoned.store(true, Ordering::Relaxed);
    }
}

// -----------------------------------------------------------------------------
// Thread CPU time
// -----------------------------------------------------------------------------

/// Kernel + user CPU time consumed so far by the thread behind `thread`
/// (a raw thread handle with query access, e.g. from a `JoinHandle`).
///
/// # Errors
///
/// [`EtwError::Win32`] (`GetThreadTimes`).
pub fn thread_cpu_time(thread: isize) -> Result<Duration, EtwError> {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the caller passes a live thread handle; all out-pointers are
    // valid FILETIMEs.
    unsafe { GetThreadTimes(HANDLE(thread as *mut _), &mut c, &mut e, &mut k, &mut u) }
        .map_err(|err| EtwError::win32("GetThreadTimes", (err.code().0 as u32) & 0xFFFF))?;
    let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Ok(Duration::from_nanos(
        (ticks(k) + ticks(u)).saturating_mul(100),
    ))
}

// -----------------------------------------------------------------------------
// TDH manifest metadata
// -----------------------------------------------------------------------------

/// TDH status `ERROR_NOT_FOUND`: the manifest has no such event version.
const ERROR_NOT_FOUND: u32 = 1168;

/// Reads the top-level fields of one event version from the installed
/// manifest. `Ok(None)` when the manifest does not define that version.
///
/// # Errors
///
/// The TDH status code for any other failure.
pub fn manifest_layout(provider: &GUID, id: u16, version: u8) -> Result<Option<Layout>, u32> {
    let desc = EVENT_DESCRIPTOR {
        Id: id,
        Version: version,
        ..Default::default()
    };
    let mut size = 0u32;
    // SAFETY: a null buffer with size 0 asks for the required size only.
    let st = unsafe { TdhGetManifestEventInformation(provider, &desc, None, &mut size) };
    match st {
        s if s == ERROR_INSUFFICIENT_BUFFER.0 => {}
        ERROR_NOT_FOUND => return Ok(None),
        s if s == ERROR_SUCCESS.0 => return Ok(Some(Layout::default())),
        s => return Err(s),
    }
    // NOTE: u64 elements keep the buffer 8-byte aligned for TRACE_EVENT_INFO.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
    // SAFETY: `buf` holds at least `size` writable, suitably aligned bytes.
    let st = unsafe { TdhGetManifestEventInformation(provider, &desc, Some(info), &mut size) };
    if st == ERROR_NOT_FOUND {
        return Ok(None);
    }
    if st != ERROR_SUCCESS.0 {
        return Err(st);
    }
    let bytes = (size as usize).min(buf.len() * 8);
    // SAFETY: TDH filled a TRACE_EVENT_INFO at the start of `buf`.
    let count = unsafe { (*info).TopLevelPropertyCount } as usize;
    // SAFETY: the property array starts inside the struct and TDH sized the
    // buffer for `PropertyCount >= TopLevelPropertyCount` entries.
    let props =
        unsafe { (&raw const (*info).EventPropertyInfoArray).cast::<EVENT_PROPERTY_INFO>() };
    let base = buf.as_ptr().cast::<u8>();
    // SAFETY: `base` points at `bytes` initialized bytes owned by `buf`.
    let all = unsafe { std::slice::from_raw_parts(base, bytes) };
    let variable = PropertyStruct.0
        | PropertyParamCount.0
        | PropertyParamLength.0
        | PropertyParamFixedCount.0
        | PropertyParamFixedLength.0;
    let mut fields = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `i < TopLevelPropertyCount`, within the array TDH wrote.
        let p = unsafe { &*props.add(i) };
        let name = utf16z_at(all, p.NameOffset as usize).unwrap_or_default();
        // SAFETY: union reads of plain integers; every bit pattern is valid.
        let (in_type, count_v, length_v) = unsafe {
            (
                p.Anonymous1.nonStructType.InType,
                p.Anonymous2.count,
                p.Anonymous3.length,
            )
        };
        let mut ty = FieldType::from_tdh_intype(in_type);
        let fixed_string =
            matches!(ty, FieldType::UnicodeString | FieldType::AnsiString) && length_v != 0;
        if p.Flags.0 & variable != 0 || count_v > 1 || fixed_string {
            ty = FieldType::Opaque;
        }
        fields.push(Field {
            name: name.into(),
            ty,
        });
    }
    Ok(Some(Layout { fields }))
}

fn utf16z_at(all: &[u8], offset: usize) -> Option<String> {
    let units: Vec<u16> = all
        .get(offset..)?
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}
