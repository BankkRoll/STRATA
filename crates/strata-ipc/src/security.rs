//! Pipe security: the DACL, the pipe name, and peer verification (SPEC §4,
//! §15.7).
//!
//! The helper runs elevated and accepts commands that read raw volumes and
//! delete files, so the pipe is a trust boundary. Defenses, in order:
//!
//! 1. An unguessable per-session pipe name created with
//!    `FILE_FLAG_FIRST_PIPE_INSTANCE` (no squatting) and
//!    `PIPE_REJECT_REMOTE_CLIENTS` (local only).
//! 2. A DACL granting only the interactive user and SYSTEM, without the right
//!    to create further pipe instances.
//! 3. Peer verification after connect: the client's image path and
//!    Authenticode signer must match ([`TrustPolicy`]).
//! 4. A version handshake that also checks the claimed PID.
//! 5. Per-connection rate limiting.
//! 6. Request validation in the helper itself (never trust client paths).

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use strata_core::FileTime;
use strata_win::WinError;
use strata_win::path::eq_ignore_case;
use strata_win::process::ProcessInfo;
use strata_win::signature::{SignatureStatus, same_signer, verify_signature};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;

/// Pipe-name prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\strata-helper-";

/// Access mask granted to the interactive user on the pipe:
/// `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES |
/// FILE_WRITE_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE`.
///
/// SECURITY: deliberately excludes `FILE_APPEND_DATA`, which on a pipe is
/// `FILE_CREATE_PIPE_INSTANCE`: a same-user process must not be able to add
/// its own server instance and intercept the app's connection. Also excludes
/// `WRITE_DAC`/`WRITE_OWNER`.
pub const USER_PIPE_ACCESS: u32 = 0x0012_0183;

/// The access the client requests when opening the pipe (a subset of
/// [`USER_PIPE_ACCESS`]; `GENERIC_WRITE` would include
/// `FILE_CREATE_PIPE_INSTANCE` and be denied).
pub const CLIENT_PIPE_ACCESS: u32 = 0x0010_0183;

/// Builds the pipe's SDDL for `user_sid`.
///
/// - `D:P` protected DACL (no inherited ACEs).
/// - `(A;;0x120183;;;<user>)`: the interactive user, read/write only.
/// - `(A;;GA;;;SY)`: SYSTEM (service mode helper).
/// - `(A;;RC;;;OW)`: OWNER RIGHTS limited to read-control, removing the
///   owner's implicit `WRITE_DAC` so the owner cannot loosen the DACL.
/// - `S:(ML;;NW;;;ME)`: medium mandatory label, no-write-up.
///
/// SECURITY: objects created by a high-integrity process get a high
/// mandatory label by default, and no-write-up then blocks the medium-
/// integrity app from opening the pipe for writing even though the DACL
/// allows it. Lowering the label to medium lets the app (medium) connect
/// while low-integrity and AppContainer processes (sandboxed browser
/// renderers, etc.) still cannot write to the helper.
///
/// # Example
///
/// ```
/// let sddl = strata_ipc::security::pipe_sddl("S-1-5-21-1-2-3-1001").unwrap();
/// assert_eq!(
///     sddl,
///     "D:P(A;;0x120183;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)(A;;RC;;;OW)S:(ML;;NW;;;ME)"
/// );
/// ```
pub fn pipe_sddl(user_sid: &str) -> Result<String, WinError> {
    // SECURITY: the SID is interpolated into SDDL, so it must be a plain SID
    // string; anything else could inject extra ACEs.
    if !is_sid_string(user_sid) {
        return Err(WinError::from_win32("pipe_sddl (invalid SID)", 1337));
    }
    Ok(format!(
        "D:P(A;;{USER_PIPE_ACCESS:#x};;;{user_sid})(A;;GA;;;SY)(A;;RC;;;OW)S:(ML;;NW;;;ME)"
    ))
}

/// Whether `s` is a plain string SID (`S-1-` followed by decimal
/// sub-authorities), safe to interpolate into SDDL and pipe names.
///
/// # Example
///
/// ```
/// use strata_ipc::security::is_sid_string;
/// assert!(is_sid_string("S-1-5-21-1-2-3-1001"));
/// assert!(!is_sid_string("S-1-5-18)(A;;GA;;;WD"));
/// ```
#[must_use]
pub fn is_sid_string(s: &str) -> bool {
    s.len() < 200
        && s.starts_with("S-1-")
        && s[2..]
            .split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// An owned self-relative security descriptor built from SDDL.
#[derive(Debug)]
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable heap memory owned by this value.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; it is only read after construction.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Parses SDDL (`ConvertStringSecurityDescriptorToSecurityDescriptorW`).
    pub fn from_sddl(sddl: &str) -> Result<Self, WinError> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated; on success `sd` is LocalAlloc'd
        // and owned by the returned value.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                windows::core::PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| WinError::new("ConvertStringSecurityDescriptorToSecurityDescriptorW", &e))?;
        Ok(Self(sd))
    }

    /// The pipe descriptor for `user_sid` (see [`pipe_sddl`]).
    pub fn for_pipe(user_sid: &str) -> Result<Self, WinError> {
        Self::from_sddl(&pipe_sddl(user_sid)?)
    }

    /// Raw pointer, valid while `self` lives.
    #[must_use]
    pub fn as_ptr(&self) -> *mut c_void {
        self.0.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptor...W; freed once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// A fresh, unguessable pipe name for one helper session:
/// `\\.\pipe\strata-helper-<user SID>-<128 random bits as hex>`.
///
/// The app passes it to the helper on the command line.
pub fn session_pipe_name(user_sid: &str) -> Result<String, WinError> {
    if !is_sid_string(user_sid) {
        return Err(WinError::from_win32(
            "session_pipe_name (invalid SID)",
            1337,
        ));
    }
    let mut bytes = [0u8; 16];
    // SAFETY: `bytes` is writable; the system RNG needs no algorithm handle.
    let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.0 != 0 {
        return Err(WinError::new(
            "BCryptGenRandom",
            &windows::core::Error::from_hresult(status.to_hresult()),
        ));
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{PIPE_PREFIX}{user_sid}-{hex}"))
}

/// Whether `name` is a well-formed Strata pipe name (prefix, SID-ish and
/// hex characters only). The helper validates its command-line argument
/// with this before creating the pipe.
#[must_use]
pub fn is_valid_pipe_name(name: &str) -> bool {
    name.len() <= 256
        && name.strip_prefix(PIPE_PREFIX).is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// What is known about the process at the other end of the pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Process id.
    pub pid: u32,
    /// Process creation time ((pid, start) identifies the process).
    pub start_time: FileTime,
    /// Full image path.
    pub image: PathBuf,
    /// Authenticode verification of the image.
    pub signature: SignatureStatus,
}

impl PeerIdentity {
    /// Inspects a live process: image path and start time from one handle,
    /// then the image's signature.
    pub fn of_pid(pid: u32) -> Result<Self, WinError> {
        let info = ProcessInfo::of(pid)?;
        let signature = verify_signature(&info.image);
        Ok(Self {
            pid,
            start_time: info.start_time,
            image: info.image,
            signature,
        })
    }

    /// The current process.
    pub fn current() -> Result<Self, WinError> {
        Self::of_pid(std::process::id())
    }
}

/// Why a peer was not trusted. Logged by the verifying side; the remote side
/// only learns that it was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    /// The peer's image is not the expected binary.
    #[error("peer image {actual} is not the expected {expected}")]
    ImageMismatch {
        /// Expected path.
        expected: PathBuf,
        /// Actual path.
        actual: PathBuf,
    },
    /// Signatures are missing, untrusted, or from different publishers.
    #[error("peer signature does not match ours (peer: {peer}, self: {own})")]
    SignerMismatch {
        /// Summary of the peer's signature status.
        peer: String,
        /// Summary of our own signature status.
        own: String,
    },
    /// The peer process could not be inspected.
    #[error("could not inspect peer: {0}")]
    Inspect(WinError),
    /// The peer is the right binary but not the process or account this
    /// server serves (e.g. a different PID than the one that launched the
    /// helper, or a non-administrator in service mode).
    #[error("peer is not authorized: {reason}")]
    Unauthorized {
        /// Why, for the verifying side's log.
        reason: String,
    },
}

/// Decides whether a connected peer may talk to us.
///
/// Implemented by [`TrustPolicy`]; tests and the service-mode helper can
/// supply their own.
pub trait PeerVerifier: Send + Sync + std::fmt::Debug {
    /// Accepts or rejects `peer`.
    fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError>;
}

/// The standard verification policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustPolicy {
    /// Release policy: the peer image must be `expected_image`, and both the
    /// peer and this process must carry trusted signatures from the same
    /// publisher.
    Signed {
        /// The only binary allowed at the other end.
        expected_image: PathBuf,
        /// This process's own signature.
        own_signature: SignatureStatus,
    },
    /// Development builds only: like `Signed`, but when *both* binaries are
    /// unsigned, a matching image path is enough. Unavailable in release
    /// builds, so a shipped helper can never be configured to accept an
    /// unsigned client.
    #[cfg(debug_assertions)]
    DevUnsigned {
        /// The only binary allowed at the other end.
        expected_image: PathBuf,
        /// This process's own signature.
        own_signature: SignatureStatus,
    },
}

impl TrustPolicy {
    /// Release policy for `expected_image`, verifying our own signature now.
    pub fn signed(expected_image: impl Into<PathBuf>) -> Result<Self, WinError> {
        let exe = std::env::current_exe().map_err(|e| {
            WinError::from_win32("current_exe", e.raw_os_error().unwrap_or(0) as u32)
        })?;
        Ok(Self::Signed {
            expected_image: expected_image.into(),
            own_signature: verify_signature(&exe),
        })
    }

    /// Development policy for `expected_image` (debug builds only).
    #[cfg(debug_assertions)]
    pub fn dev_unsigned(expected_image: impl Into<PathBuf>) -> Result<Self, WinError> {
        let exe = std::env::current_exe().map_err(|e| {
            WinError::from_win32("current_exe", e.raw_os_error().unwrap_or(0) as u32)
        })?;
        Ok(Self::DevUnsigned {
            expected_image: expected_image.into(),
            own_signature: verify_signature(&exe),
        })
    }

    /// `name` next to the current executable: the app derives the helper's
    /// path (and vice versa) from its own install directory.
    pub fn sibling_of_current_exe(name: &str) -> Result<PathBuf, WinError> {
        let exe = std::env::current_exe().map_err(|e| {
            WinError::from_win32("current_exe", e.raw_os_error().unwrap_or(0) as u32)
        })?;
        Ok(exe
            .parent()
            .map_or_else(|| PathBuf::from(name), |d| d.join(name)))
    }

    fn parts(&self) -> (&Path, &SignatureStatus, bool) {
        match self {
            Self::Signed {
                expected_image,
                own_signature,
            } => (expected_image, own_signature, false),
            #[cfg(debug_assertions)]
            Self::DevUnsigned {
                expected_image,
                own_signature,
            } => (expected_image, own_signature, true),
        }
    }
}

impl PeerVerifier for TrustPolicy {
    fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError> {
        let (expected, own, allow_unsigned) = self.parts();
        // SECURITY: compare the path first. The image path comes from the
        // kernel (QueryFullProcessImageNameW), not from the peer.
        if !same_path(expected, &peer.image) {
            return Err(TrustError::ImageMismatch {
                expected: expected.to_path_buf(),
                actual: peer.image.clone(),
            });
        }
        if same_signer(own, &peer.signature) {
            return Ok(());
        }
        if allow_unsigned
            && *own == SignatureStatus::Unsigned
            && peer.signature == SignatureStatus::Unsigned
        {
            return Ok(());
        }
        Err(TrustError::SignerMismatch {
            peer: summarize(&peer.signature),
            own: summarize(own),
        })
    }
}

fn summarize(s: &SignatureStatus) -> String {
    match s {
        SignatureStatus::Trusted { signer, .. } => format!("trusted ({})", signer.subject),
        SignatureStatus::Unsigned => "unsigned".into(),
        SignatureStatus::Untrusted { hresult, .. } => format!("untrusted (0x{hresult:08X})"),
        SignatureStatus::Tampered { .. } => "tampered".into(),
        SignatureStatus::Error(e) => format!("error ({e})"),
    }
}

/// Path equality as Windows sees it: case-insensitive, after resolving both
/// to their final paths when possible (so `C:\PROGRA~1\x` matches
/// `C:\Program Files\x`).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| {
        strata_win::path::final_path_of(p)
            .map(|f| strata_win::path::strip_verbatim(&f))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    eq_ignore_case(canon(a).as_os_str(), canon(b).as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_win::signature::{SignatureKind, SignerInfo};

    fn signer(subject: &str) -> SignatureStatus {
        SignatureStatus::Trusted {
            kind: SignatureKind::Embedded,
            signer: SignerInfo {
                display_name: "Strata".into(),
                subject: subject.into(),
                issuer: "CN=CA".into(),
                thumbprint_sha1: String::new(),
                thumbprint_sha256: String::new(),
            },
        }
    }

    fn peer(image: &Path, signature: SignatureStatus) -> PeerIdentity {
        PeerIdentity {
            pid: 1,
            start_time: FileTime(1),
            image: image.to_path_buf(),
            signature,
        }
    }

    #[test]
    fn sddl_rejects_injection() {
        assert!(pipe_sddl("S-1-5-21-1-2-3-1001").is_ok());
        assert!(pipe_sddl("S-1-5-18)(A;;GA;;;WD").is_err());
        assert!(pipe_sddl("WD").is_err());
        assert!(pipe_sddl("S-1-").is_err());
        assert!(SecurityDescriptor::for_pipe("S-1-5-21-1-2-3-1001").is_ok());
    }

    #[test]
    fn pipe_names_are_unique_and_valid() {
        let a = session_pipe_name("S-1-5-21-1-2-3-1001").unwrap();
        let b = session_pipe_name("S-1-5-21-1-2-3-1001").unwrap();
        assert_ne!(a, b);
        assert!(is_valid_pipe_name(&a), "{a}");
        assert!(a.len() < 256);
        assert!(!is_valid_pipe_name(r"\\.\pipe\other"));
        assert!(!is_valid_pipe_name(r"\\.\pipe\strata-helper-..\x"));
        assert!(!is_valid_pipe_name(r"\\.\pipe\strata-helper-"));
        assert!(session_pipe_name("bad").is_err());
    }

    #[test]
    fn signed_policy() {
        let exe = std::env::current_exe().unwrap();
        let policy = TrustPolicy::Signed {
            expected_image: exe.clone(),
            own_signature: signer("CN=Strata"),
        };
        assert!(policy.verify(&peer(&exe, signer("CN=Strata"))).is_ok());
        let upper = PathBuf::from(exe.to_string_lossy().to_uppercase());
        assert!(policy.verify(&peer(&upper, signer("CN=Strata"))).is_ok());
        assert!(matches!(
            policy.verify(&peer(&exe, signer("CN=Evil"))),
            Err(TrustError::SignerMismatch { .. })
        ));
        assert!(matches!(
            policy.verify(&peer(&exe, SignatureStatus::Unsigned)),
            Err(TrustError::SignerMismatch { .. })
        ));
        assert!(matches!(
            policy.verify(&peer(Path::new(r"C:\evil.exe"), signer("CN=Strata"))),
            Err(TrustError::ImageMismatch { .. })
        ));
        let unsigned_policy = TrustPolicy::Signed {
            expected_image: exe.clone(),
            own_signature: SignatureStatus::Unsigned,
        };
        assert!(
            unsigned_policy
                .verify(&peer(&exe, SignatureStatus::Unsigned))
                .is_err(),
            "release policy never accepts unsigned pairs"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    fn dev_policy_needs_both_unsigned_and_same_path() {
        let exe = std::env::current_exe().unwrap();
        let policy = TrustPolicy::dev_unsigned(&exe).unwrap();
        let me = PeerIdentity::current().unwrap();
        assert_eq!(me.signature, SignatureStatus::Unsigned);
        assert!(policy.verify(&me).is_ok());
        let other = TrustPolicy::dev_unsigned(r"C:\Windows\System32\notepad.exe").unwrap();
        assert!(matches!(
            other.verify(&me),
            Err(TrustError::ImageMismatch { .. })
        ));
        let signed_peer = peer(&exe, signer("CN=Strata"));
        assert!(
            policy.verify(&signed_peer).is_err(),
            "unsigned self, signed peer"
        );
    }

    #[test]
    fn sibling_path() {
        let p = TrustPolicy::sibling_of_current_exe("strata-helper.exe").unwrap();
        assert_eq!(p.parent(), std::env::current_exe().unwrap().parent());
    }
}
