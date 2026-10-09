//! Client verification policies (SPEC §4 pipe security).
//!
//! Both build on `strata_ipc::security::TrustPolicy` (image path +
//! Authenticode signer, checked before a single byte of the client is read)
//! and add what each launch mode knows about its legitimate client:
//!
//! - [`LaunchedClientVerifier`] (on-demand): only the exact process that
//!   launched the helper (`--client-pid`) may connect.
//! - [`ServiceClientVerifier`] (service): the client must run as the user the
//!   pipe was created for, and that user must be a member of the local
//!   Administrators group (even with a UAC-filtered token). Service mode
//!   replaces the per-launch UAC consent of an administrator; it must never
//!   give a standard user SYSTEM-level raw volume reads or deletes.

use std::path::PathBuf;
use std::sync::Arc;

use strata_ipc::security::{PeerIdentity, PeerVerifier, TrustError, TrustPolicy};
use strata_win::process::Token;
use strata_win::{OwnedHandle, WinError};
use windows::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, TOKEN_GROUPS, TOKEN_QUERY, TokenGroups,
    WinBuiltinAdministratorsSid,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The trust policy for `client_image`: `Signed` in release builds,
/// `DevUnsigned` (two unsigned binaries with matching paths, or a signed
/// pair) in debug builds.
///
/// # Errors
///
/// The current executable's path cannot be determined.
pub fn trust_policy(client_image: impl Into<PathBuf>) -> Result<TrustPolicy, WinError> {
    #[cfg(debug_assertions)]
    {
        TrustPolicy::dev_unsigned(client_image)
    }
    #[cfg(not(debug_assertions))]
    {
        TrustPolicy::signed(client_image)
    }
}

/// On-demand mode: the trust policy plus the PID of the launching app.
#[derive(Debug)]
pub struct LaunchedClientVerifier {
    policy: Arc<dyn PeerVerifier>,
    client_pid: u32,
}

impl LaunchedClientVerifier {
    /// Accepts only `client_pid`, and only if `policy` accepts it too.
    #[must_use]
    pub fn new(policy: Arc<dyn PeerVerifier>, client_pid: u32) -> Self {
        Self { policy, client_pid }
    }
}

impl PeerVerifier for LaunchedClientVerifier {
    fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError> {
        // SECURITY: the PID comes from GetNamedPipeClientProcessId (kernel),
        // so another instance of the app cannot use this helper.
        if peer.pid != self.client_pid {
            return Err(TrustError::Unauthorized {
                reason: format!(
                    "client pid {} is not the launching process {}",
                    peer.pid, self.client_pid
                ),
            });
        }
        self.policy.verify(peer)
    }
}

/// Service mode: the trust policy plus account checks on the client token.
#[derive(Debug)]
pub struct ServiceClientVerifier {
    policy: Arc<dyn PeerVerifier>,
    user_sid: String,
}

impl ServiceClientVerifier {
    /// Accepts clients that `policy` accepts, running as `user_sid`, whose
    /// user is an administrator.
    #[must_use]
    pub fn new(policy: Arc<dyn PeerVerifier>, user_sid: impl Into<String>) -> Self {
        Self {
            policy,
            user_sid: user_sid.into(),
        }
    }
}

impl PeerVerifier for ServiceClientVerifier {
    fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError> {
        self.policy.verify(peer)?;
        let account = TokenAccount::of_process(peer.pid).map_err(TrustError::Inspect)?;
        if !account.user_sid.eq_ignore_ascii_case(&self.user_sid) {
            return Err(TrustError::Unauthorized {
                reason: "client runs as a different user than the pipe serves".into(),
            });
        }
        if !account.administrators_member {
            return Err(TrustError::Unauthorized {
                reason: "service mode serves administrators only".into(),
            });
        }
        Ok(())
    }
}

/// Who a process runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAccount {
    /// The token user's SID string.
    pub user_sid: String,
    /// Whether `BUILTIN\Administrators` is in the token's groups, enabled or
    /// deny-only (a UAC-filtered administrator counts).
    pub administrators_member: bool,
}

impl TokenAccount {
    /// Reads the primary token of process `pid`.
    ///
    /// # Errors
    ///
    /// The process or its token cannot be opened or queried.
    pub fn of_process(pid: u32) -> Result<Self, WinError> {
        // SAFETY: plain OpenProcess; the handle is owned below.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|e| WinError::new("OpenProcess", &e))?;
        // SAFETY: fresh process handle.
        let process = unsafe { OwnedHandle::from_raw(h) }
            .ok_or_else(|| WinError::from_win32("OpenProcess", 87))?;
        let mut token = windows::Win32::Foundation::HANDLE::default();
        // SAFETY: `process` is live; `token` receives a handle we own.
        unsafe { OpenProcessToken(process.raw(), TOKEN_QUERY, &mut token) }
            .map_err(|e| WinError::new("OpenProcessToken", &e))?;
        // SAFETY: fresh token handle.
        let token = unsafe { OwnedHandle::from_raw(token) }
            .ok_or_else(|| WinError::from_win32("OpenProcessToken", 87))?;
        let administrators_member = has_administrators_group(&token)?;
        let user_sid = Token::from_handle(token).user_sid()?;
        Ok(Self {
            user_sid,
            administrators_member,
        })
    }
}

fn has_administrators_group(token: &OwnedHandle) -> Result<bool, WinError> {
    let mut needed = 0u32;
    // SAFETY: size query with no buffer; failure with
    // ERROR_INSUFFICIENT_BUFFER is expected.
    let _ = unsafe { GetTokenInformation(token.raw(), TokenGroups, None, 0, &mut needed) };
    if needed == 0 {
        return Err(WinError::last("GetTokenInformation(TokenGroups)"));
    }
    // NOTE: u64 storage keeps the buffer 8-byte aligned for TOKEN_GROUPS.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` has at least `needed` bytes.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenGroups,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|e| WinError::new("GetTokenInformation(TokenGroups)", &e))?;
    // SAFETY: the kernel wrote a TOKEN_GROUPS header followed by
    // `GroupCount` entries into `buf`, which stays alive and unmodified
    // while the slice is used.
    let groups = unsafe {
        let g = &*buf.as_ptr().cast::<TOKEN_GROUPS>();
        std::slice::from_raw_parts(g.Groups.as_ptr(), g.GroupCount as usize)
    };
    Ok(groups.iter().any(|g| {
        // SAFETY: each SID points into `buf`, valid for the call.
        unsafe { IsWellKnownSid(g.Sid, WinBuiltinAdministratorsSid).as_bool() }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::FileTime;
    use strata_win::signature::SignatureStatus;

    #[derive(Debug)]
    struct AcceptAll;
    impl PeerVerifier for AcceptAll {
        fn verify(&self, _: &PeerIdentity) -> Result<(), TrustError> {
            Ok(())
        }
    }

    fn peer(pid: u32) -> PeerIdentity {
        PeerIdentity {
            pid,
            start_time: FileTime(1),
            image: std::env::current_exe().unwrap(),
            signature: SignatureStatus::Unsigned,
        }
    }

    #[test]
    fn launched_verifier_binds_the_pid() {
        let v = LaunchedClientVerifier::new(Arc::new(AcceptAll), 42);
        assert!(v.verify(&peer(42)).is_ok());
        assert!(matches!(
            v.verify(&peer(43)),
            Err(TrustError::Unauthorized { .. })
        ));
    }

    #[test]
    fn current_token_account() {
        let me = TokenAccount::of_process(std::process::id()).unwrap();
        assert_eq!(
            me.user_sid,
            strata_win::process::current_user().unwrap().sid
        );
        let elevation = strata_win::process::elevation_type().unwrap();
        if elevation != strata_win::process::ElevationType::Default {
            // A split (Limited/Full) token only exists for administrators.
            assert!(me.administrators_member);
        }
    }

    #[test]
    fn service_verifier_checks_user_and_policy() {
        let me = strata_win::process::current_user().unwrap().sid;
        let ok = ServiceClientVerifier::new(Arc::new(AcceptAll), me.clone());
        let account = TokenAccount::of_process(std::process::id()).unwrap();
        assert_eq!(
            ok.verify(&peer(std::process::id())).is_ok(),
            account.administrators_member
        );
        let other = ServiceClientVerifier::new(Arc::new(AcceptAll), "S-1-5-21-1-2-3-1001");
        assert!(matches!(
            other.verify(&peer(std::process::id())),
            Err(TrustError::Unauthorized { .. })
        ));
        let refuse = ServiceClientVerifier::new(
            Arc::new(LaunchedClientVerifier::new(Arc::new(AcceptAll), 1)),
            me,
        );
        assert!(refuse.verify(&peer(std::process::id())).is_err());
    }

    #[test]
    fn default_policy_accepts_this_unsigned_binary() {
        let exe = std::env::current_exe().unwrap();
        let policy = trust_policy(&exe).unwrap();
        let me = PeerIdentity::current().unwrap();
        if me.signature == SignatureStatus::Unsigned && cfg!(debug_assertions) {
            assert!(policy.verify(&me).is_ok());
        }
    }
}
