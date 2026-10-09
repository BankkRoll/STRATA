//! Access tokens: elevation, the current user's SID, and privileges.
//!
//! The helper enables `SeBackupPrivilege` / `SeManageVolumePrivilege` (and
//! `SeRestorePrivilege` only for deletes) for exactly as long as it needs them
//! through [`PrivilegeGuard`], and permanently removes every other privilege
//! at startup with [`drop_privileges`].

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount,
    GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeNameW, LookupPrivilegeValueW, PSID,
    SE_PRIVILEGE_ENABLED, SE_PRIVILEGE_ENABLED_BY_DEFAULT, SE_PRIVILEGE_REMOVED,
    SecurityImpersonation, TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenElevation,
    TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited, TokenIntegrityLevel,
    TokenPrimary, TokenPrivileges, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

use crate::error::{Context, Result, WinError};
use crate::handle::OwnedHandle;
use crate::wide::{WideCString, from_wide_nul};

/// `SeBackupPrivilege`: read any file regardless of ACLs (raw volume reads).
pub const SE_BACKUP: &str = "SeBackupPrivilege";
/// `SeRestorePrivilege`: write/delete any file regardless of ACLs.
pub const SE_RESTORE: &str = "SeRestorePrivilege";
/// `SeManageVolumePrivilege`: volume maintenance (USN journal management).
pub const SE_MANAGE_VOLUME: &str = "SeManageVolumePrivilege";
/// `SeChangeNotifyPrivilege`: bypass traverse checking. Every token has it
/// and removing it breaks path traversal, so the helper keeps it.
pub const SE_CHANGE_NOTIFY: &str = "SeChangeNotifyPrivilege";

/// What kind of token a UAC-enabled admin has (`TokenElevationType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElevationType {
    /// UAC disabled, or a standard user (no split token).
    Default,
    /// The elevated half of a split token.
    Full,
    /// The filtered (unelevated) half of a split admin token.
    Limited,
}

/// One privilege in a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeState {
    /// Privilege name, e.g. `SeBackupPrivilege`.
    pub name: String,
    /// Whether the privilege is enabled in the token.
    pub enabled: bool,
    /// Enabled by default when the token is created.
    pub enabled_by_default: bool,
}

/// An access token handle.
#[derive(Debug)]
pub struct Token(OwnedHandle);

impl Token {
    /// Opens the current process's primary token.
    pub fn current_process(access: TOKEN_ACCESS_MASK) -> Result<Self> {
        let mut h = HANDLE::default();
        // SAFETY: the pseudo handle is always valid; `h` is an out-pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) }.ctx("OpenProcessToken")?;
        // SAFETY: OpenProcessToken returned a handle we now own.
        unsafe { OwnedHandle::from_raw(h) }
            .map(Self)
            .ok_or_else(|| WinError::from_win32("OpenProcessToken", 6))
    }

    /// Wraps an owned token handle.
    #[must_use]
    pub fn from_handle(h: OwnedHandle) -> Self {
        Self(h)
    }

    /// The raw handle.
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0.raw()
    }

    /// A primary-token copy with full access. Changes to the copy (such as
    /// [`Token::remove_privileges_except`]) do not affect the original.
    pub fn duplicate(&self) -> Result<Self> {
        let mut h = HANDLE::default();
        // SAFETY: `self` is a live token opened with TOKEN_DUPLICATE; `h` is
        // an out-pointer.
        unsafe {
            DuplicateTokenEx(
                self.raw(),
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut h,
            )
        }
        .ctx("DuplicateTokenEx")?;
        // SAFETY: DuplicateTokenEx returned a handle we now own.
        unsafe { OwnedHandle::from_raw(h) }
            .map(Self)
            .ok_or_else(|| WinError::from_win32("DuplicateTokenEx", 6))
    }

    /// Raw `GetTokenInformation` output, in an 8-byte-aligned buffer so the
    /// pointer-bearing structs it contains can be read in place.
    fn info(&self, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>> {
        let mut needed = 0u32;
        // SAFETY: size query with no buffer; `needed` is an out-pointer. The
        // expected ERROR_INSUFFICIENT_BUFFER is ignored.
        let _ = unsafe { GetTokenInformation(self.raw(), class, None, 0, &mut needed) };
        if needed == 0 {
            return Err(WinError::last("GetTokenInformation"));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` provides `needed` writable bytes.
        unsafe {
            GetTokenInformation(
                self.raw(),
                class,
                Some(buf.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
        }
        .ctx("GetTokenInformation")?;
        Ok(buf)
    }

    /// Whether the token is elevated (`TokenElevation`).
    pub fn is_elevated(&self) -> Result<bool> {
        let buf = self.info(TokenElevation)?;
        // SAFETY: the buffer holds a TOKEN_ELEVATION and is suitably aligned.
        let e = unsafe { &*buf.as_ptr().cast::<TOKEN_ELEVATION>() };
        Ok(e.TokenIsElevated != 0)
    }

    /// The token's elevation type.
    pub fn elevation_type(&self) -> Result<ElevationType> {
        let buf = self.info(TokenElevationType)?;
        // SAFETY: the buffer holds a TOKEN_ELEVATION_TYPE (an i32).
        let t = unsafe { *buf.as_ptr().cast::<TOKEN_ELEVATION_TYPE>() };
        Ok(if t == TokenElevationTypeFull {
            ElevationType::Full
        } else if t == TokenElevationTypeLimited {
            ElevationType::Limited
        } else {
            ElevationType::Default
        })
    }

    /// The user SID as a string (`S-1-5-21-...`).
    pub fn user_sid(&self) -> Result<String> {
        let buf = self.info(TokenUser)?;
        // SAFETY: the buffer holds a TOKEN_USER whose SID pointer points
        // into the same buffer, which outlives this call.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        // SAFETY: as above; the SID is valid while `buf` lives.
        unsafe { crate::sid::sid_to_string(user.User.Sid) }
    }

    /// The user account as `DOMAIN\name`, if resolvable.
    pub fn user_account(&self) -> Result<Option<String>> {
        let buf = self.info(TokenUser)?;
        // SAFETY: see `user_sid`.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        // SAFETY: the SID lives in `buf`.
        Ok(unsafe { crate::sid::lookup_account(user.User.Sid) })
    }

    /// Mandatory integrity level RID (`0x2000` medium, `0x3000` high,
    /// `0x4000` system).
    pub fn integrity_level(&self) -> Result<u32> {
        let buf = self.info(TokenIntegrityLevel)?;
        // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points
        // into the buffer.
        let label = unsafe { &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
        let sid: PSID = label.Label.Sid;
        // SAFETY: `sid` is valid; the count pointer is valid for reads.
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        if count == 0 {
            return Err(WinError::from_win32("GetSidSubAuthorityCount", 1337));
        }
        // SAFETY: index `count - 1` is within the SID's sub-authorities.
        Ok(unsafe { *GetSidSubAuthority(sid, u32::from(count) - 1) })
    }

    /// Every privilege present in the token.
    pub fn privileges(&self) -> Result<Vec<PrivilegeState>> {
        let buf = self.info(TokenPrivileges)?;
        let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
        parse_privileges(&bytes)
            .into_iter()
            .map(|(luid, attrs)| {
                Ok(PrivilegeState {
                    name: privilege_name(luid)?,
                    enabled: attrs & SE_PRIVILEGE_ENABLED.0 != 0,
                    enabled_by_default: attrs & SE_PRIVILEGE_ENABLED_BY_DEFAULT.0 != 0,
                })
            })
            .collect()
    }

    /// Whether the named privilege is present and enabled.
    pub fn privilege_enabled(&self, name: &str) -> Result<Option<bool>> {
        Ok(self
            .privileges()?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .map(|p| p.enabled))
    }

    /// Permanently removes (`SE_PRIVILEGE_REMOVED`) every privilege except
    /// `keep`. Returns the names removed. Removed privileges cannot be
    /// re-enabled for the lifetime of the token.
    pub fn remove_privileges_except(&self, keep: &[&str]) -> Result<Vec<String>> {
        let to_remove: Vec<PrivilegeState> = self
            .privileges()?
            .into_iter()
            .filter(|p| !keep.iter().any(|k| k.eq_ignore_ascii_case(&p.name)))
            .collect();
        if to_remove.is_empty() {
            return Ok(Vec::new());
        }
        let entries: Vec<(LUID, u32)> = to_remove
            .iter()
            .map(|p| Ok((privilege_luid(&p.name)?, SE_PRIVILEGE_REMOVED.0)))
            .collect::<Result<_>>()?;
        let buf = build_privileges(&entries);
        // SAFETY: `buf` is a correctly laid out TOKEN_PRIVILEGES with
        // `entries.len()` elements, 4-byte aligned as the struct requires.
        unsafe {
            AdjustTokenPrivileges(
                self.raw(),
                false,
                Some(buf.as_ptr().cast::<TOKEN_PRIVILEGES>()),
                0,
                None,
                None,
            )
        }
        .ctx("AdjustTokenPrivileges")?;
        Ok(to_remove.into_iter().map(|p| p.name).collect())
    }
}

/// Parses a `TOKEN_PRIVILEGES` buffer into (LUID, attributes) pairs.
/// Bounds-checked: a short buffer yields only the complete entries.
fn parse_privileges(bytes: &[u8]) -> Vec<(LUID, u32)> {
    let read_u32 = |o: usize| -> Option<u32> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    };
    let Some(count) = read_u32(0) else {
        return Vec::new();
    };
    (0..count as usize)
        .map_while(|i| {
            let o = 4 + i * 12;
            Some((
                LUID {
                    LowPart: read_u32(o)?,
                    HighPart: read_u32(o + 4)? as i32,
                },
                read_u32(o + 8)?,
            ))
        })
        .collect()
}

/// Lays out a variable-length `TOKEN_PRIVILEGES` (count, then 12-byte
/// LUID_AND_ATTRIBUTES entries) in a u32 buffer for alignment.
fn build_privileges(entries: &[(LUID, u32)]) -> Vec<u32> {
    const _: () = assert!(std::mem::size_of::<LUID_AND_ATTRIBUTES>() == 12);
    let mut buf = Vec::with_capacity(1 + entries.len() * 3);
    buf.push(entries.len() as u32);
    for (luid, attrs) in entries {
        buf.extend([luid.LowPart, luid.HighPart as u32, *attrs]);
    }
    buf
}

fn privilege_luid(name: &str) -> Result<LUID> {
    let wide = WideCString::new(name);
    let mut luid = LUID::default();
    // SAFETY: `wide` is NUL-terminated; `luid` is an out-pointer.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), wide.as_pcwstr(), &mut luid) }
        .ctx("LookupPrivilegeValueW")?;
    Ok(luid)
}

fn privilege_name(luid: LUID) -> Result<String> {
    let mut buf = [0u16; 128];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` units; `luid` is a valid LUID.
    unsafe {
        LookupPrivilegeNameW(
            PCWSTR::null(),
            &luid,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    }
    .ctx("LookupPrivilegeNameW")?;
    Ok(from_wide_nul(&buf).to_string_lossy().into_owned())
}

/// Whether the current process is elevated.
///
/// # Example
///
/// ```
/// let elevated = strata_win::process::is_elevated().unwrap();
/// println!("elevated: {elevated}");
/// ```
pub fn is_elevated() -> Result<bool> {
    Token::current_process(TOKEN_QUERY)?.is_elevated()
}

/// The current process's elevation type.
pub fn elevation_type() -> Result<ElevationType> {
    Token::current_process(TOKEN_QUERY)?.elevation_type()
}

/// The current user: SID string and `DOMAIN\name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAccount {
    /// `S-1-5-21-...`.
    pub sid: String,
    /// `DOMAIN\name`, when resolvable.
    pub account: Option<String>,
}

/// The user that owns the current process token.
pub fn current_user() -> Result<UserAccount> {
    let t = Token::current_process(TOKEN_QUERY)?;
    Ok(UserAccount {
        sid: t.user_sid()?,
        account: t.user_account()?,
    })
}

/// Why a privilege could not be enabled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivilegeError {
    /// The token does not hold the privilege (e.g. not elevated, or removed).
    #[error("privilege {0} is not held by this token")]
    NotHeld(String),
    /// A Win32 call failed.
    #[error(transparent)]
    Win(#[from] WinError),
}

/// Enables a privilege on the process token until dropped, then restores
/// its previous state.
///
/// Privilege state is per token, so the change is visible to every thread of
/// the process while the guard lives.
#[derive(Debug)]
pub struct PrivilegeGuard {
    token: Token,
    name: String,
    /// Previous state (count + entries); empty when nothing changed.
    previous: Vec<u32>,
}

impl PrivilegeGuard {
    /// The privilege this guard enabled.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Enables `name` (e.g. [`SE_BACKUP`]) for the guard's lifetime.
///
/// # Example
///
/// ```
/// use strata_win::process::{enable_privilege, PrivilegeError, SE_BACKUP};
/// match enable_privilege(SE_BACKUP) {
///     Ok(_guard) => { /* read raw volume while the guard lives */ }
///     Err(PrivilegeError::NotHeld(_)) => { /* unelevated: fall back */ }
///     Err(e) => panic!("{e}"),
/// }
/// ```
pub fn enable_privilege(name: &str) -> std::result::Result<PrivilegeGuard, PrivilegeError> {
    let token = Token::current_process(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY | TOKEN_DUPLICATE)?;
    let luid = privilege_luid(name)?;
    let new = build_privileges(&[(luid, SE_PRIVILEGE_ENABLED.0)]);
    let mut previous = vec![0u32; 1 + 3];
    let mut returned = 0u32;
    // SAFETY: `new` and `previous` are TOKEN_PRIVILEGES layouts with room for
    // one entry each; sizes are passed in bytes.
    unsafe {
        AdjustTokenPrivileges(
            token.raw(),
            false,
            Some(new.as_ptr().cast::<TOKEN_PRIVILEGES>()),
            (previous.len() * 4) as u32,
            Some(previous.as_mut_ptr().cast::<TOKEN_PRIVILEGES>()),
            Some(&mut returned),
        )
    }
    .ctx("AdjustTokenPrivileges")?;
    // NOTE: AdjustTokenPrivileges reports success even when the privilege is
    // absent; the real outcome is in the last error.
    // SAFETY: reads the thread's last-error value.
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(PrivilegeError::NotHeld(name.to_owned()));
    }
    Ok(PrivilegeGuard {
        token,
        name: name.to_owned(),
        previous,
    })
}

impl Drop for PrivilegeGuard {
    fn drop(&mut self) {
        // PrivilegeCount == 0 means the privilege was already enabled.
        if self.previous.first().copied().unwrap_or(0) == 0 {
            return;
        }
        // SAFETY: `previous` holds the TOKEN_PRIVILEGES written by the
        // enabling call; restoring it is a plain adjustment.
        unsafe {
            let _ = AdjustTokenPrivileges(
                self.token.raw(),
                false,
                Some(self.previous.as_ptr().cast::<TOKEN_PRIVILEGES>()),
                0,
                None,
                None,
            );
        }
    }
}

/// Permanently removes every privilege of the current process except
/// `except`, so the helper holds only the privileges it uses. Returns the
/// removed names.
///
/// The helper calls this once at startup with
/// `[SE_BACKUP, SE_RESTORE, SE_MANAGE_VOLUME, SE_CHANGE_NOTIFY]`.
pub fn drop_privileges(except: &[&str]) -> Result<Vec<String>> {
    Token::current_process(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?.remove_privileges_except(except)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that change or compare the process token: privilege
    /// state is process-wide, so a concurrent enable shows up in another
    /// test's comparison.
    static PROCESS_TOKEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn privilege_buffer_round_trip() {
        let e = [
            (
                LUID {
                    LowPart: 17,
                    HighPart: 0,
                },
                2,
            ),
            (
                LUID {
                    LowPart: 9,
                    HighPart: -1,
                },
                4,
            ),
        ];
        let buf = build_privileges(&e);
        let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let parsed = parse_privileges(&bytes);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].0.HighPart, -1);
        assert_eq!(parsed[1].1, 4);
        assert_eq!(parse_privileges(&bytes[..10]).len(), 0);
        assert!(parse_privileges(&[]).is_empty());
        let mut lying = bytes.clone();
        lying[0] = 200;
        assert_eq!(parse_privileges(&lying).len(), 2);
    }

    #[test]
    fn current_token_queries() {
        let t = Token::current_process(TOKEN_QUERY).unwrap();
        let elevated = t.is_elevated().unwrap();
        let ty = t.elevation_type().unwrap();
        if ty == ElevationType::Full {
            assert!(elevated);
        }
        if ty == ElevationType::Limited {
            assert!(!elevated);
        }
        let il = t.integrity_level().unwrap();
        assert!(il >= 0x1000, "{il:#x}");
        assert_eq!(elevated, il >= 0x3000);
        let user = current_user().unwrap();
        assert!(user.sid.starts_with("S-1-5-"), "{}", user.sid);
        let privs = t.privileges().unwrap();
        assert!(
            privs
                .iter()
                .any(|p| p.name == SE_CHANGE_NOTIFY && p.enabled)
        );
    }

    #[test]
    fn unheld_privilege_is_reported() {
        if is_elevated().unwrap() {
            return;
        }
        // NOTE: SeTcbPrivilege is never granted to interactive users.
        match enable_privilege("SeTcbPrivilege") {
            Err(PrivilegeError::NotHeld(n)) => assert_eq!(n, "SeTcbPrivilege"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            enable_privilege("SeNoSuchPrivilege"),
            Err(PrivilegeError::Win(_))
        ));
    }

    #[test]
    fn guard_enables_then_restores() {
        let _serial = PROCESS_TOKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // NOTE: SeTimeZonePrivilege is held but disabled by default for
        // standard and admin users alike.
        let t = Token::current_process(TOKEN_QUERY).unwrap();
        let Some(before) = t.privilege_enabled("SeTimeZonePrivilege").unwrap() else {
            return;
        };
        {
            let g = enable_privilege("SeTimeZonePrivilege").unwrap();
            assert_eq!(g.name(), "SeTimeZonePrivilege");
            assert_eq!(
                t.privilege_enabled("SeTimeZonePrivilege").unwrap(),
                Some(true)
            );
        }
        assert_eq!(
            t.privilege_enabled("SeTimeZonePrivilege").unwrap(),
            Some(before)
        );
    }

    #[test]
    fn removal_on_duplicate_token_leaves_process_untouched() {
        let _serial = PROCESS_TOKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = Token::current_process(TOKEN_QUERY | TOKEN_DUPLICATE).unwrap();
        let before = t.privileges().unwrap();
        let dup = t.duplicate().unwrap();
        let removed = dup.remove_privileges_except(&[SE_CHANGE_NOTIFY]).unwrap();
        assert_eq!(removed.len(), before.len() - 1);
        let after: Vec<_> = dup
            .privileges()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(after, vec![SE_CHANGE_NOTIFY.to_owned()]);
        assert_eq!(t.privileges().unwrap(), before);
        assert!(
            dup.remove_privileges_except(&[SE_CHANGE_NOTIFY])
                .unwrap()
                .is_empty()
        );
    }
}
