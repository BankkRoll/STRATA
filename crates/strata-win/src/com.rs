//! Scoped COM initialization for the calling thread.

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx,
    CoUninitialize,
};

use crate::error::{Result, WinError};

/// Keeps COM initialized on this thread until dropped.
///
/// If the thread was already initialized in the other apartment model
/// (`RPC_E_CHANGED_MODE`), COM is still usable, so this succeeds without
/// taking a reference and does not uninitialize on drop.
#[derive(Debug)]
pub(crate) struct ComApartment {
    owns_init: bool,
    // NOTE: COM initialization is per thread; the guard must not move threads.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ComApartment {
    /// Single-threaded apartment, as the shell expects.
    pub(crate) fn sta() -> Result<Self> {
        Self::init(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
    }

    /// Multithreaded apartment (WMI).
    pub(crate) fn mta() -> Result<Self> {
        Self::init(COINIT_MULTITHREADED)
    }

    fn init(model: COINIT) -> Result<Self> {
        // SAFETY: plain FFI call with no pointers; balanced by Drop.
        let hr = unsafe { CoInitializeEx(None, model) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self {
                owns_init: false,
                _not_send: std::marker::PhantomData,
            });
        }
        hr.ok().map_err(|e| WinError::new("CoInitializeEx", &e))?;
        Ok(Self {
            owns_init: true,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owns_init {
            // SAFETY: balances the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}
