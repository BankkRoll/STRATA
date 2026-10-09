//! `IFileOperation` Recycle Bin deletes with a progress sink.
//!
//! Threading: `IFileOperation` only supports single-threaded apartments, so
//! [`recycle_paths_on_sta`] must run on a thread the caller owns and does
//! not otherwise use for COM; it initializes STA itself (with
//! `COINIT_DISABLE_OLE1DDE`) and uninitializes on return. `crate::recycle`
//! spawns a dedicated thread per batch, which makes the caller's apartment
//! irrelevant.

use std::io;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_ABORT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI,
    FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
    IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{HRESULT, PCWSTR, Ref, implement};

use crate::canon::CanonicalPath;
use crate::expect::CancelToken;

/// RAII COM apartment for the current thread.
struct ComApartment;

impl ComApartment {
    fn init_sta() -> io::Result<Self> {
        // SAFETY: initializes COM for this thread; balanced in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            // The thread is already MTA; IFileOperation would misbehave.
            return Err(io::Error::other("thread already initialized as MTA"));
        }
        hr.ok()?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// Per-item outcome reported by the sink.
#[derive(Debug, Clone, Default)]
pub(crate) struct ShellItemResult {
    /// `PostDeleteItem` HRESULT, or the shell-item creation failure.
    pub hr: Option<i32>,
    /// File-system path of the item now in the Recycle Bin (`$R...`).
    pub recycled_path: Option<String>,
    /// The Shell was about to delete permanently, so the sink stopped it.
    pub refused_permanent: bool,
    /// `PreDeleteItem` transfer flags, for diagnostics.
    pub pre_flags: Option<u32>,
}

#[derive(Debug, Default)]
struct SinkState {
    requested: Vec<Option<CanonicalPath>>,
    results: Vec<ShellItemResult>,
    unmatched_post: usize,
}

impl SinkState {
    fn index_of(&self, item: Option<&IShellItem>) -> Option<usize> {
        let path = item.and_then(fs_path)?;
        let c = CanonicalPath::parse(&path).ok()?;
        self.requested.iter().position(|r| r.as_ref() == Some(&c))
    }
}

#[implement(IFileOperationProgressSink)]
struct Sink {
    state: Arc<Mutex<SinkState>>,
    cancel: CancelToken,
}

fn fs_path(item: &IShellItem) -> Option<String> {
    // SAFETY: valid shell item; the returned string is freed below.
    let p = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    // SAFETY: `p` is a valid NUL-terminated string from the Shell.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: GetDisplayName allocates with the COM task allocator.
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hr: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreDeleteItem(&self, dwflags: u32, psiitem: Ref<IShellItem>) -> windows::core::Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = st.index_of(psiitem.as_ref());
        if let Some(i) = idx {
            st.results[i].pre_flags = Some(dwflags);
        }
        if self.cancel.is_cancelled() {
            return Err(E_ABORT.into());
        }
        // SECURITY: without this flag the Shell is about to destroy the item
        // instead of recycling it (too large, bin disabled). Never let that
        // happen silently; failing here stops the operation.
        if dwflags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            if let Some(i) = idx {
                st.results[i].refused_permanent = true;
            }
            return Err(E_ABORT.into());
        }
        Ok(())
    }

    fn PostDeleteItem(
        &self,
        _dwflags: u32,
        psiitem: Ref<IShellItem>,
        hrdelete: HRESULT,
        psinewlycreated: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recycled = psinewlycreated.as_ref().and_then(fs_path);
        match st.index_of(psiitem.as_ref()) {
            Some(i) => {
                st.results[i].hr = Some(hrdelete.0);
                st.results[i].recycled_path = recycled;
            }
            None => st.unmatched_post += 1,
        }
        Ok(())
    }

    fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Recycles `paths` (display or verbatim form, no NUL) in one operation.
///
/// Must run on a thread with no COM apartment yet (see the module docs).
/// Results are index-aligned with `paths`; an item with `hr == None` was
/// never attempted (the operation stopped early).
pub(crate) fn recycle_paths_on_sta(
    paths: &[String],
    cancel: &CancelToken,
) -> io::Result<Vec<ShellItemResult>> {
    let _com = ComApartment::init_sta()?;
    let state = Arc::new(Mutex::new(SinkState {
        requested: paths.iter().map(|p| CanonicalPath::parse(p).ok()).collect(),
        results: vec![ShellItemResult::default(); paths.len()],
        unmatched_post: 0,
    }));
    // SAFETY: creating the in-proc FileOperation object on this STA thread.
    let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }?;
    let flags = FILEOPERATION_FLAGS(
        FOFX_RECYCLEONDELETE.0
            | FOF_ALLOWUNDO.0
            | FOF_NOCONFIRMATION.0
            | FOF_NOCONFIRMMKDIR.0
            | FOF_NOERRORUI.0
            | FOF_SILENT.0
            | FOFX_EARLYFAILURE.0
            // Last-resort backstop: if the sink check ever misses a
            // would-be permanent delete, Windows asks instead of nuking.
            | FOF_WANTNUKEWARNING.0,
    );
    // SAFETY: valid COM object; plain flag setter.
    unsafe { op.SetOperationFlags(flags) }?;
    let sink: IFileOperationProgressSink = Sink {
        state: Arc::clone(&state),
        cancel: cancel.clone(),
    }
    .into();
    // SAFETY: valid objects; the cookie is released with Unadvise below.
    let cookie = unsafe { op.Advise(&sink) }?;

    let mut queued = 0usize;
    for (i, p) in paths.iter().enumerate() {
        let w = super::wide(p);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let item: windows::core::Result<IShellItem> =
            unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) };
        match item {
            // SAFETY: valid operation and shell item.
            Ok(item) => match unsafe { op.DeleteItem(&item, None) } {
                Ok(()) => queued += 1,
                Err(e) => lock(&state).results[i].hr = Some(e.code().0),
            },
            Err(e) => lock(&state).results[i].hr = Some(e.code().0),
        }
    }
    if queued > 0 {
        // Per-item outcomes come from the sink; the aggregate result only
        // says whether anything failed or was aborted.
        // SAFETY: valid operation object.
        let _ = unsafe { op.PerformOperations() };
    }
    // SAFETY: matching Advise above.
    let _ = unsafe { op.Unadvise(cookie) };
    drop(sink);
    drop(op);
    let results = lock(&state).results.clone();
    Ok(results)
}

fn lock(s: &Arc<Mutex<SinkState>>) -> std::sync::MutexGuard<'_, SinkState> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
