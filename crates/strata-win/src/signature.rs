//! Authenticode verification (SPEC §4, §15.7).
//!
//! The helper and the app verify each other before trusting the pipe: both
//! binaries must carry a valid signature from the same publisher.
//!
//! [`verify_signature`] checks an embedded signature first and falls back to
//! the system catalogs (most Windows binaries are catalog-signed, with no
//! embedded signature). The signer certificate is read from the WinVerifyTrust
//! provider state, which works for both kinds, instead of parsing the PKCS#7
//! blob with `CryptQueryObject` (embedded-only).

use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{GENERIC_READ, HANDLE, HWND};
use windows::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2,
    CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext,
    CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
};
use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_NAME_ISSUER_FLAG, CERT_NAME_RDN_TYPE, CERT_NAME_SIMPLE_DISPLAY_TYPE,
    CERT_SHA1_HASH_PROP_ID, CERT_SHA256_HASH_PROP_ID, CERT_X500_NAME_STR,
    CertGetCertificateContextProperty, CertGetNameStringW,
};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE,
    WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
    WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows::core::{GUID, PCWSTR};

use crate::error::{Context, Result, WinError};
use crate::handle::OwnedHandle;
use crate::wide::{WideCString, from_wide_nul};

const TRUST_E_NOSIGNATURE: u32 = 0x800B_0100;
const TRUST_E_SUBJECT_FORM_UNKNOWN: u32 = 0x800B_0003;
const TRUST_E_PROVIDER_UNKNOWN: u32 = 0x800B_0001;
const TRUST_E_BAD_DIGEST: u32 = 0x8009_6010;
const CERT_E_REVOKED: u32 = 0x800B_010C;

/// Where the signature lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureKind {
    /// Embedded in the file (Authenticode PKCS#7 in the PE security dir).
    Embedded,
    /// The file's hash is listed in a signed system catalog (`.cat`).
    Catalog,
}

/// The signing certificate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SignerInfo {
    /// Display name, e.g. `Microsoft Windows`.
    pub display_name: String,
    /// Full X.500 subject (`CN=..., O=..., C=...`).
    pub subject: String,
    /// Full X.500 issuer.
    pub issuer: String,
    /// SHA-1 thumbprint, uppercase hex (what certmgr shows).
    pub thumbprint_sha1: String,
    /// SHA-256 of the certificate, uppercase hex.
    pub thumbprint_sha256: String,
}

/// Result of [`verify_signature`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum SignatureStatus {
    /// Valid signature chaining to a trusted root.
    Trusted {
        /// Embedded or catalog.
        kind: SignatureKind,
        /// The leaf signing certificate.
        signer: SignerInfo,
    },
    /// No embedded signature and not in any catalog.
    Unsigned,
    /// Signed, but the chain is not trusted (untrusted root, expired,
    /// explicitly distrusted, revoked, wrong usage, ...).
    Untrusted {
        /// Embedded or catalog.
        kind: SignatureKind,
        /// The signer, when the chain could be read.
        signer: Option<SignerInfo>,
        /// The `WinVerifyTrust` HRESULT.
        hresult: u32,
        /// Whether the failure was revocation.
        revoked: bool,
    },
    /// The file was modified after signing (`TRUST_E_BAD_DIGEST`).
    Tampered {
        /// Embedded or catalog.
        kind: SignatureKind,
    },
    /// Verification could not run (file missing, unreadable, ...).
    Error(WinError),
}

impl SignatureStatus {
    /// The signer, if the signature is trusted.
    #[must_use]
    pub fn trusted_signer(&self) -> Option<&SignerInfo> {
        match self {
            Self::Trusted { signer, .. } => Some(signer),
            _ => None,
        }
    }

    /// Whether the signature is valid and trusted.
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted { .. })
    }
}

/// Revocation checking policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Revocation {
    /// No revocation check. The default: the pipe handshake must not block on
    /// the network, and a missing CRL would otherwise fail verification.
    #[default]
    None,
    /// Check the whole chain using only locally cached CRLs/OCSP responses.
    CacheOnly,
    /// Check the whole chain, fetching CRLs/OCSP online (may block).
    Online,
}

/// Options for [`verify_signature_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerifyOptions {
    /// Revocation policy.
    pub revocation: Revocation,
}

/// Verifies `path` with default options (no revocation check).
///
/// # Example
///
/// ```
/// use strata_win::signature::{verify_signature, SignatureStatus};
/// let notepad = std::path::Path::new(r"C:\Windows\System32\notepad.exe");
/// if let SignatureStatus::Trusted { signer, .. } = verify_signature(notepad) {
///     println!("signed by {}", signer.display_name);
/// }
/// ```
#[must_use]
pub fn verify_signature(path: &Path) -> SignatureStatus {
    verify_signature_with(path, VerifyOptions::default())
}

/// Verifies `path`: embedded signature first, then system catalogs.
#[must_use]
pub fn verify_signature_with(path: &Path, opts: VerifyOptions) -> SignatureStatus {
    let file = match open_for_hash(path) {
        Ok(f) => f,
        Err(e) => return SignatureStatus::Error(e),
    };
    // NOTE: since Windows 8, WTD_CHOICE_FILE also consults catalogs on its
    // own, so its result cannot tell the two kinds apart. The PE certificate
    // table decides which path to take.
    if has_embedded_signature(path) {
        let embedded = verify_embedded(path, file.raw(), opts);
        match embedded.hresult {
            TRUST_E_NOSIGNATURE | TRUST_E_SUBJECT_FORM_UNKNOWN | TRUST_E_PROVIDER_UNKNOWN => {}
            _ => return embedded.into_status(SignatureKind::Embedded),
        }
    }
    match verify_catalog(path, file.raw(), opts) {
        Ok(Some(outcome)) => outcome.into_status(SignatureKind::Catalog),
        Ok(None) => SignatureStatus::Unsigned,
        Err(e) => SignatureStatus::Error(e),
    }
}

/// Whether two verification results are trusted signatures from the same
/// publisher: same leaf subject and same issuer.
///
/// The leaf thumbprint is deliberately not compared: Azure Trusted Signing
/// issues short-lived certificates (days), so two binaries from one release
/// pipeline can carry different leaf certificates with the same identity.
///
/// # Example
///
/// ```
/// use strata_win::signature::{same_signer, verify_signature};
/// let a = verify_signature(r"C:\Windows\System32\notepad.exe".as_ref());
/// let b = verify_signature(r"C:\Windows\System32\kernel32.dll".as_ref());
/// assert_eq!(same_signer(&a, &b), a.is_trusted() && b.is_trusted());
/// ```
#[must_use]
pub fn same_signer(a: &SignatureStatus, b: &SignatureStatus) -> bool {
    match (a.trusted_signer(), b.trusted_signer()) {
        (Some(x), Some(y)) => x.subject == y.subject && x.issuer == y.issuer,
        _ => false,
    }
}

/// [`same_signer`] on two files.
#[must_use]
pub fn same_signer_files(a: &Path, b: &Path) -> bool {
    same_signer(&verify_signature(a), &verify_signature(b))
}

fn has_embedded_signature(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::with_capacity(4096);
    if f.take(4096).read_to_end(&mut head).is_err() {
        return false;
    }
    pe_cert_table(&head).is_some_and(|(_, size)| size > 0)
}

/// Locates the PE certificate table (data directory 4) in the file header:
/// `(file offset, size)`. `None` for non-PE input or a header that does not
/// fit in `head`. Never panics on malformed input.
fn pe_cert_table(head: &[u8]) -> Option<(u32, u32)> {
    let u16_at = |o: usize| head.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| {
        head.get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if head.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3C)? as usize;
    if head.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let opt = pe.checked_add(24)?;
    let (dirs, count_at) = match u16_at(opt)? {
        0x10B => (opt + 96, opt + 92),
        0x20B => (opt + 112, opt + 108),
        _ => return None,
    };
    const SECURITY: usize = 4;
    if (u32_at(count_at)? as usize) <= SECURITY {
        return None;
    }
    let entry = dirs.checked_add(SECURITY * 8)?;
    Some((u32_at(entry)?, u32_at(entry + 4)?))
}

fn open_for_hash(path: &Path) -> Result<OwnedHandle> {
    let wide = WideCString::new(path);
    // SAFETY: `wide` is NUL-terminated; the handle is owned below.
    let h = unsafe {
        CreateFileW(
            wide.as_pcwstr(),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_SEQUENTIAL_SCAN,
            None,
        )
    }
    .ctx("CreateFileW")?;
    // SAFETY: fresh handle from CreateFileW.
    unsafe { OwnedHandle::from_raw(h) }.ok_or_else(|| WinError::from_win32("CreateFileW", 6))
}

struct Outcome {
    hresult: u32,
    signer: Option<SignerInfo>,
}

impl Outcome {
    fn into_status(self, kind: SignatureKind) -> SignatureStatus {
        match (self.hresult, self.signer) {
            (0, Some(signer)) => SignatureStatus::Trusted { kind, signer },
            (0, None) => SignatureStatus::Error(WinError::from_win32("signer certificate", 13)),
            (TRUST_E_BAD_DIGEST, _) => SignatureStatus::Tampered { kind },
            (hr, signer) => SignatureStatus::Untrusted {
                kind,
                signer,
                hresult: hr,
                revoked: hr == CERT_E_REVOKED,
            },
        }
    }
}

fn base_data(opts: VerifyOptions) -> WINTRUST_DATA {
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    match opts.revocation {
        Revocation::None => {
            data.fdwRevocationChecks = WTD_REVOKE_NONE;
            data.dwProvFlags = WTD_REVOCATION_CHECK_NONE;
        }
        Revocation::CacheOnly => {
            data.fdwRevocationChecks = WTD_REVOKE_WHOLECHAIN;
            data.dwProvFlags = WTD_CACHE_ONLY_URL_RETRIEVAL;
        }
        Revocation::Online => data.fdwRevocationChecks = WTD_REVOKE_WHOLECHAIN,
    }
    data
}

/// Runs WinVerifyTrust with `data`, extracts the signer, and always closes
/// the provider state.
fn run_verify(data: &mut WINTRUST_DATA) -> Outcome {
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // NOTE: INVALID_HANDLE_VALUE as the window means "no interactive UI".
    let no_ui = HWND(-1isize as *mut c_void);
    // SAFETY: `data` is fully initialized and every pointer it carries
    // (file/catalog info and their strings) outlives this function.
    let hr = unsafe { WinVerifyTrust(no_ui, &mut action, (data as *mut WINTRUST_DATA).cast()) };
    // SAFETY: the state handle is valid until WTD_STATEACTION_CLOSE below.
    let signer = unsafe { signer_from_state(data.hWVTStateData) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: closes the provider state allocated by the verify call.
    unsafe {
        WinVerifyTrust(no_ui, &mut action, (data as *mut WINTRUST_DATA).cast());
    }
    Outcome {
        hresult: hr as u32,
        signer,
    }
}

fn verify_embedded(path: &Path, file: HANDLE, opts: VerifyOptions) -> Outcome {
    let wide = WideCString::new(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_pcwstr(),
        hFile: file,
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = base_data(opts);
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous = WINTRUST_DATA_0 { pFile: &mut info };
    run_verify(&mut data)
}

struct CatAdmin(isize);

impl Drop for CatAdmin {
    fn drop(&mut self) {
        // SAFETY: the context came from CryptCATAdminAcquireContext2.
        unsafe {
            let _ = CryptCATAdminReleaseContext(self.0, 0);
        }
    }
}

struct CatInfo<'a>(&'a CatAdmin, isize);

impl Drop for CatInfo<'_> {
    fn drop(&mut self) {
        // SAFETY: the catalog context came from EnumCatalogFromHash on the
        // same admin context, which outlives this guard.
        unsafe {
            let _ = CryptCATAdminReleaseCatalogContext(self.0.0, self.1, 0);
        }
    }
}

/// Looks the file's hash up in the system catalogs and verifies the
/// matching catalog. `Ok(None)` when no catalog lists the file.
// NOTE: catalogs are hashed with SHA-256 since Windows 8; older catalogs use
// SHA-1, so both are tried.
fn verify_catalog(path: &Path, file: HANDLE, opts: VerifyOptions) -> Result<Option<Outcome>> {
    for alg in ["SHA256", "SHA1"] {
        let alg_w = WideCString::new(alg);
        let mut admin = 0isize;
        // SAFETY: `admin` is an out-pointer; the default subsystem is used.
        unsafe { CryptCATAdminAcquireContext2(&mut admin, None, alg_w.as_pcwstr(), None, None) }
            .ctx("CryptCATAdminAcquireContext2")?;
        let admin = CatAdmin(admin);
        let mut len = 0u32;
        // SAFETY: size query; `len` receives the hash length.
        let _ =
            unsafe { CryptCATAdminCalcHashFromFileHandle2(admin.0, file, &mut len, None, None) };
        if len == 0 || len > 64 {
            continue;
        }
        let mut hash = vec![0u8; len as usize];
        // SAFETY: `hash` has `len` writable bytes.
        unsafe {
            CryptCATAdminCalcHashFromFileHandle2(
                admin.0,
                file,
                &mut len,
                Some(hash.as_mut_ptr()),
                None,
            )
        }
        .ctx("CryptCATAdminCalcHashFromFileHandle2")?;
        hash.truncate(len as usize);
        // SAFETY: `hash` is valid; no previous catalog context is passed.
        let cat = unsafe { CryptCATAdminEnumCatalogFromHash(admin.0, &hash, None, None) };
        if cat == 0 {
            continue;
        }
        let cat = CatInfo(&admin, cat);
        let mut ci = CATALOG_INFO {
            cbStruct: std::mem::size_of::<CATALOG_INFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `cat.1` is a live catalog context; `ci` is writable.
        unsafe { CryptCATCatalogInfoFromContext(cat.1, &mut ci, 0) }
            .ctx("CryptCATCatalogInfoFromContext")?;

        let member_tag = WideCString::new(hex_upper(&hash));
        let member_path = WideCString::new(path);
        let catalog_path: Vec<u16> = {
            let mut v: Vec<u16> = ci.wszCatalogFile.to_vec();
            if !v.contains(&0) {
                v.push(0);
            }
            v
        };
        let mut info = WINTRUST_CATALOG_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_CATALOG_INFO>() as u32,
            pcwszCatalogFilePath: PCWSTR(catalog_path.as_ptr()),
            pcwszMemberTag: member_tag.as_pcwstr(),
            pcwszMemberFilePath: member_path.as_pcwstr(),
            hMemberFile: file,
            pbCalculatedFileHash: hash.as_mut_ptr(),
            cbCalculatedFileHash: len,
            hCatAdmin: admin.0,
            ..Default::default()
        };
        let mut data = base_data(opts);
        data.dwUnionChoice = WTD_CHOICE_CATALOG;
        data.Anonymous = WINTRUST_DATA_0 {
            pCatalog: &mut info,
        };
        let outcome = run_verify(&mut data);
        drop(cat);
        return Ok(Some(outcome));
    }
    Ok(None)
}

/// Reads the leaf signer certificate from WinVerifyTrust provider state.
///
/// # Safety
///
/// `state` must be null or a live `hWVTStateData` from a VERIFY call that has
/// not been closed yet.
unsafe fn signer_from_state(state: HANDLE) -> Option<SignerInfo> {
    if state.0.is_null() {
        return None;
    }
    // SAFETY: `state` is live per the caller's contract.
    let prov = unsafe { WTHelperProvDataFromStateData(state) };
    if prov.is_null() {
        return None;
    }
    // SAFETY: `prov` is valid while the state is open.
    let sgnr = unsafe { WTHelperGetProvSignerFromChain(prov, 0, false, 0) };
    if sgnr.is_null() {
        return None;
    }
    // SAFETY: `sgnr` points into provider data that lives until close.
    let sgnr = unsafe { &*sgnr };
    if sgnr.csCertChain == 0 || sgnr.pasCertChain.is_null() {
        return None;
    }
    // SAFETY: the chain has at least one element; element 0 is the leaf.
    let leaf = unsafe { &*sgnr.pasCertChain };
    if leaf.pCert.is_null() {
        return None;
    }
    // SAFETY: `leaf.pCert` is a valid certificate context until close.
    unsafe { signer_info(leaf.pCert) }
}

/// # Safety
///
/// `cert` must be a valid certificate context for the duration of the call.
unsafe fn signer_info(cert: *const CERT_CONTEXT) -> Option<SignerInfo> {
    // SAFETY: forwarded caller contract.
    unsafe {
        Some(SignerInfo {
            display_name: cert_name(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0)?,
            subject: cert_name(cert, CERT_NAME_RDN_TYPE, 0)?,
            issuer: cert_name(cert, CERT_NAME_RDN_TYPE, CERT_NAME_ISSUER_FLAG)?,
            thumbprint_sha1: hex_upper(&cert_prop(cert, CERT_SHA1_HASH_PROP_ID)?),
            thumbprint_sha256: hex_upper(&cert_prop(cert, CERT_SHA256_HASH_PROP_ID)?),
        })
    }
}

/// # Safety
///
/// `cert` must be a valid certificate context.
unsafe fn cert_name(cert: *const CERT_CONTEXT, ty: u32, flags: u32) -> Option<String> {
    let x500 = CERT_X500_NAME_STR.0;
    let para: Option<*const c_void> =
        (ty == CERT_NAME_RDN_TYPE).then_some((&raw const x500).cast());
    // SAFETY: size query; `para` points to a live u32 when present.
    let n = unsafe { CertGetNameStringW(cert, ty, flags, para, None) };
    if n <= 1 {
        return None;
    }
    let mut buf = vec![0u16; n as usize];
    // SAFETY: `buf` holds `n` units.
    unsafe { CertGetNameStringW(cert, ty, flags, para, Some(&mut buf)) };
    Some(from_wide_nul(&buf).to_string_lossy().into_owned())
}

/// # Safety
///
/// `cert` must be a valid certificate context.
unsafe fn cert_prop(cert: *const CERT_CONTEXT, id: u32) -> Option<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: size query.
    unsafe { CertGetCertificateContextProperty(cert, id, None, &mut len) }.ok()?;
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` has `len` writable bytes.
    unsafe { CertGetCertificateContextProperty(cert, id, Some(buf.as_mut_ptr().cast()), &mut len) }
        .ok()?;
    buf.truncate(len as usize);
    Some(buf)
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn system32(name: &str) -> PathBuf {
        PathBuf::from(std::env::var_os("windir").unwrap())
            .join("System32")
            .join(name)
    }

    #[test]
    fn catalog_signed_system_binaries_are_trusted() {
        // NOTE: notepad.exe and cmd.exe have no PE certificate table and are
        // signed only through catalogs; kernel32.dll carries both, and the
        // embedded signature is verified first.
        for (name, expected) in [
            ("notepad.exe", SignatureKind::Catalog),
            ("cmd.exe", SignatureKind::Catalog),
            ("kernel32.dll", SignatureKind::Embedded),
        ] {
            match verify_signature(&system32(name)) {
                SignatureStatus::Trusted { kind, signer } => {
                    assert_eq!(kind, expected, "{name}");
                    assert!(signer.subject.contains("Microsoft"), "{signer:?}");
                    assert_eq!(signer.thumbprint_sha1.len(), 40);
                    assert_eq!(signer.thumbprint_sha256.len(), 64);
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        let a = verify_signature(&system32("kernel32.dll"));
        let b = verify_signature(&system32("notepad.exe"));
        assert!(same_signer(&a, &b));
    }

    #[test]
    fn embedded_signature_and_tampering() {
        let src = system32("MpSigStub.exe");
        let status = verify_signature(&src);
        let SignatureStatus::Trusted { kind, .. } = status else {
            // NOTE: MpSigStub ships with Defender; skip where it is absent.
            return;
        };
        assert_eq!(kind, SignatureKind::Embedded);

        let dir = std::env::temp_dir().join(format!("strata-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("tampered.exe");
        let mut bytes = std::fs::read(&src).unwrap();
        // Flip a byte inside the code section, well before the signature
        // (which sits at the end of the file).
        let at = bytes.len() / 3;
        bytes[at] ^= 0xFF;
        std::fs::write(&copy, &bytes).unwrap();
        let tampered = verify_signature(&copy);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(
                tampered,
                SignatureStatus::Tampered {
                    kind: SignatureKind::Embedded
                }
            ),
            "{tampered:?}"
        );
    }

    #[test]
    fn unsigned_files() {
        let dir = std::env::temp_dir().join(format!("strata-unsigned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("plain.txt");
        std::fs::write(&txt, b"not signed").unwrap();
        assert_eq!(verify_signature(&txt), SignatureStatus::Unsigned);
        let _ = std::fs::remove_dir_all(&dir);

        // The test binary itself is an unsigned PE.
        let me = std::env::current_exe().unwrap();
        let s = verify_signature(&me);
        assert_eq!(s, SignatureStatus::Unsigned);
        assert!(!same_signer(&s, &s), "unsigned never matches");

        assert!(matches!(
            verify_signature(Path::new(r"C:\definitely\missing.exe")),
            SignatureStatus::Error(_)
        ));
    }

    #[test]
    fn same_signer_requires_trust_and_identity() {
        let signer = |subject: &str| SignerInfo {
            display_name: "x".into(),
            subject: subject.into(),
            issuer: "CN=CA".into(),
            thumbprint_sha1: "A".into(),
            thumbprint_sha256: "B".into(),
        };
        let t = |s: &str| SignatureStatus::Trusted {
            kind: SignatureKind::Embedded,
            signer: signer(s),
        };
        assert!(same_signer(&t("CN=Strata"), &t("CN=Strata")));
        assert!(!same_signer(&t("CN=Strata"), &t("CN=Evil")));
        let untrusted = SignatureStatus::Untrusted {
            kind: SignatureKind::Embedded,
            signer: Some(signer("CN=Strata")),
            hresult: 0x800B_0109,
            revoked: false,
        };
        assert!(!same_signer(&t("CN=Strata"), &untrusted));
        assert_eq!(hex_upper(&[0x0A, 0xFF]), "0AFF");
    }

    #[test]
    fn pe_header_parsing_is_bounds_checked() {
        let mut pe = vec![0u8; 512];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3C] = 0x80;
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        let opt = 0x80 + 24;
        pe[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 112 + 32;
        pe[sec..sec + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[sec + 4..sec + 8].copy_from_slice(&0x200u32.to_le_bytes());
        assert_eq!(pe_cert_table(&pe), Some((0x1000, 0x200)));
        for cut in 0..pe.len() {
            let _ = pe_cert_table(&pe[..cut]);
        }
        let mut huge = pe.clone();
        huge[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(pe_cert_table(&huge), None);
        assert_eq!(pe_cert_table(b"not a pe"), None);

        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(pe_cert_table(&me[..4096]).map(|t| t.1), Some(0));
    }
}
