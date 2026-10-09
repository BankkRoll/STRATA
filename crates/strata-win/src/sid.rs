//! Security identifier (SID) conversions and account lookup.

use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{IsValidSid, LookupAccountSidW, PSID, SID_NAME_USE};
use windows::core::{PCWSTR, PWSTR};

use crate::error::{Context, Result, WinError};
use crate::handle::LocalBox;
use crate::wide::{WideCString, from_pwstr, from_wide_nul};

/// A SID parsed from its string form, owning its memory.
#[derive(Debug)]
pub struct OwnedSid(LocalBox);

impl OwnedSid {
    /// Parses `S-1-5-...`. Rejects malformed strings.
    pub fn parse(s: &str) -> Result<Self> {
        if s.contains('\0') {
            return Err(WinError::from_win32("ConvertStringSidToSidW", 1337));
        }
        let wide = WideCString::new(s);
        let mut sid = PSID::default();
        // SAFETY: `wide` is NUL-terminated; on success `sid` is LocalAlloc'd
        // and owned by the returned LocalBox.
        unsafe { ConvertStringSidToSidW(wide.as_pcwstr(), &mut sid) }
            .ctx("ConvertStringSidToSidW")?;
        // SAFETY: the pointer was allocated by ConvertStringSidToSidW.
        Ok(Self(unsafe { LocalBox::from_raw(sid.0) }))
    }

    /// Borrowed `PSID`, valid while `self` lives.
    #[must_use]
    pub fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr())
    }
}

/// Formats a SID as `S-1-5-...`.
///
/// # Safety
///
/// `sid` must point to a valid SID for the duration of the call.
pub(crate) unsafe fn sid_to_string(sid: PSID) -> Result<String> {
    // SAFETY: the caller guarantees `sid` is valid.
    if !unsafe { IsValidSid(sid) }.as_bool() {
        return Err(WinError::from_win32("IsValidSid", 1337));
    }
    let mut out = PWSTR::null();
    // SAFETY: `sid` is valid; `out` receives a LocalAlloc'd string.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }.ctx("ConvertSidToStringSidW")?;
    // SAFETY: `out` was allocated by the call above.
    let owned = unsafe { LocalBox::from_raw(out.0.cast()) };
    // SAFETY: `out` is a NUL-terminated string that `owned` keeps alive.
    let s = unsafe { from_pwstr(PCWSTR(out.0)) };
    drop(owned);
    Ok(s.to_string_lossy().into_owned())
}

/// Resolves a SID to `DOMAIN\name` (`LookupAccountSidW`). `None` for deleted
/// or unresolvable accounts.
///
/// # Safety
///
/// `sid` must point to a valid SID for the duration of the call.
pub(crate) unsafe fn lookup_account(sid: PSID) -> Option<String> {
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain_len = domain.len() as u32;
    let mut use_ = SID_NAME_USE::default();
    // SAFETY: buffers are writable for the lengths passed; `sid` is valid.
    let r = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut use_,
        )
    };
    r.ok()?;
    let name = from_wide_nul(&name).to_string_lossy().into_owned();
    let domain = from_wide_nul(&domain).to_string_lossy().into_owned();
    Some(if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    })
}

/// Resolves a SID string to `DOMAIN\name`.
#[must_use]
pub fn account_name(sid: &str) -> Option<String> {
    let sid = OwnedSid::parse(sid).ok()?;
    // SAFETY: `sid` owns a valid SID for the call.
    unsafe { lookup_account(sid.as_psid()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_sids_round_trip() {
        let sid = OwnedSid::parse("S-1-5-18").unwrap();
        // SAFETY: `sid` is valid for the call.
        assert_eq!(unsafe { sid_to_string(sid.as_psid()) }.unwrap(), "S-1-5-18");
        // NOTE: the account name is localized ("SYSTEM" / "Système"), so only
        // presence is asserted.
        assert!(account_name("S-1-5-18").is_some());
        assert!(OwnedSid::parse("not-a-sid").is_err());
        assert!(OwnedSid::parse("S-1-5-18\0x").is_err());
    }
}
