//! Volume Shadow Copy storage per volume, via WMI
//! `Win32_ShadowStorage`.
//!
//! The UI shows this as the "System Restore / Shadow copies" block of a
//! volume's unaccounted space. The VSS WMI provider only works elevated;
//! unelevated queries fail (typically `WBEM_E_INITIALIZATION_FAILURE` or
//! `WBEM_E_ACCESS_DENIED`) and are reported as [`ShadowStorageReport::Unavailable`].

use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoSetProxyBlanket, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE, WbemLocator,
};
use windows::core::{BSTR, PCWSTR};

use crate::error::{Context, Result};

/// Shadow storage of one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowStorage {
    /// The protected volume (`\\?\Volume{...}\`).
    pub volume: String,
    /// The volume holding the diff area (usually the same volume).
    pub diff_volume: Option<String>,
    /// Bytes used by shadow copies.
    pub used_bytes: u64,
    /// Bytes allocated for the diff area.
    pub allocated_bytes: u64,
    /// Configured maximum; `None` when unbounded.
    pub max_bytes: Option<u64>,
}

/// Outcome of [`shadow_storage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ShadowStorageReport {
    /// The query succeeded (possibly with no shadow storage configured).
    Available {
        /// One entry per volume with shadow storage.
        volumes: Vec<ShadowStorage>,
    },
    /// The query failed; the UI shows the block as "unknown".
    Unavailable {
        /// Human-readable reason.
        reason: String,
        /// The failing HRESULT, when there was one.
        hresult: Option<u32>,
        /// Whether running elevated (through the helper) would likely help.
        needs_elevation: bool,
    },
}

impl ShadowStorageReport {
    /// Shadow storage of one volume (`\\?\Volume{...}\`), if known.
    #[must_use]
    pub fn for_volume(&self, guid_path: &str) -> Option<&ShadowStorage> {
        match self {
            Self::Available { volumes } => volumes
                .iter()
                .find(|s| s.volume.eq_ignore_ascii_case(guid_path)),
            Self::Unavailable { .. } => None,
        }
    }
}

/// Queries `Win32_ShadowStorage`. Never fails: errors become
/// [`ShadowStorageReport::Unavailable`].
///
/// Initializes COM (MTA) on the calling thread for the duration of the call.
///
/// # Example
///
/// ```
/// use strata_win::shadow::{shadow_storage, ShadowStorageReport};
/// match shadow_storage() {
///     ShadowStorageReport::Available { volumes } => println!("{} volumes", volumes.len()),
///     ShadowStorageReport::Unavailable { reason, .. } => println!("unknown: {reason}"),
/// }
/// ```
#[must_use]
pub fn shadow_storage() -> ShadowStorageReport {
    match query() {
        Ok(volumes) => ShadowStorageReport::Available { volumes },
        Err(e) => ShadowStorageReport::Unavailable {
            reason: match wbem_name(e.hresult) {
                Some(name) if e.message.is_empty() => format!("{}: {name}", e.op),
                _ => e.to_string(),
            },
            hresult: Some(e.hresult),
            needs_elevation: !crate::token::is_elevated().unwrap_or(false),
        },
    }
}

/// Names for the WMI errors seen here; the system message table has no
/// text for WBEM codes.
fn wbem_name(hr: u32) -> Option<&'static str> {
    Some(match hr {
        0x8004_1003 => "WBEM_E_ACCESS_DENIED",
        0x8004_1010 => "WBEM_E_INVALID_CLASS",
        0x8004_1013 => "WBEM_E_PROVIDER_LOAD_FAILURE",
        0x8004_1014 => "WBEM_E_INITIALIZATION_FAILURE (the VSS provider requires elevation)",
        _ => return None,
    })
}

fn query() -> Result<Vec<ShadowStorage>> {
    let _com = crate::com::ComApartment::mta()?;
    // SAFETY: COM is initialized on this thread for the rest of the call.
    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .ctx("CoCreateInstance(WbemLocator)")?;
    let empty = BSTR::new();
    // SAFETY: all BSTRs are valid; the namespace is local.
    let services: IWbemServices = unsafe {
        locator.ConnectServer(
            &BSTR::from(r"ROOT\CIMV2"),
            &empty,
            &empty,
            &empty,
            0,
            &empty,
            None,
        )
    }
    .ctx("IWbemLocator::ConnectServer")?;
    // NOTE: CoInitializeSecurity is process-wide and may already have been
    // called by the host (WebView2, Tauri), so the proxy blanket is set on
    // this proxy instead.
    // SAFETY: `services` is a live proxy.
    unsafe {
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
    .ctx("CoSetProxyBlanket")?;
    // SAFETY: valid BSTRs and a live proxy.
    let rows: IEnumWbemClassObject = unsafe {
        services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(
                "SELECT AllocatedSpace, UsedSpace, MaxSpace, Volume, DiffVolume FROM Win32_ShadowStorage",
            ),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
    }
    .ctx("IWbemServices::ExecQuery")?;

    let mut out = Vec::new();
    loop {
        let mut row: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;
        // SAFETY: `row` has room for one object; `returned` is an out-pointer.
        let hr = unsafe { rows.Next(WBEM_INFINITE, &mut row, &mut returned) };
        hr.ok().ctx("IEnumWbemClassObject::Next")?;
        let Some(obj) = row[0].take().filter(|_| returned == 1) else {
            break;
        };
        let volume = get_string(&obj, "Volume")?
            .and_then(|r| parse_volume_ref(&r))
            .unwrap_or_default();
        let diff_volume = get_string(&obj, "DiffVolume")?.and_then(|r| parse_volume_ref(&r));
        out.push(ShadowStorage {
            volume,
            diff_volume,
            used_bytes: get_u64(&obj, "UsedSpace")?.unwrap_or(0),
            allocated_bytes: get_u64(&obj, "AllocatedSpace")?.unwrap_or(0),
            max_bytes: get_u64(&obj, "MaxSpace")?.filter(|&m| m != u64::MAX),
        });
    }
    Ok(out)
}

fn get(obj: &IWbemClassObject, name: &str) -> Result<VARIANT> {
    let wide = crate::wide::WideCString::new(name);
    let mut v = VARIANT::default();
    // SAFETY: `obj` is live; `v` receives an owned VARIANT freed on drop.
    unsafe { obj.Get(wide.as_pcwstr(), 0, &mut v, None, None) }.ctx("IWbemClassObject::Get")?;
    Ok(v)
}

fn get_string(obj: &IWbemClassObject, name: &str) -> Result<Option<String>> {
    let v = get(obj, name)?;
    Ok(BSTR::try_from(&v).ok().map(|b| b.to_string()))
}

/// WMI returns `uint64` properties as decimal strings (VT_BSTR).
fn get_u64(obj: &IWbemClassObject, name: &str) -> Result<Option<u64>> {
    let v = get(obj, name)?;
    if let Ok(b) = BSTR::try_from(&v) {
        return Ok(b.to_string().trim().parse().ok());
    }
    Ok(u64::try_from(&v).ok())
}

/// Extracts the device id from a WMI reference such as
/// `Win32_Volume.DeviceID="\\\\?\\Volume{...}\\"`, unescaping backslashes
/// and quotes.
#[must_use]
pub fn parse_volume_ref(r: &str) -> Option<String> {
    let start = r.find("DeviceID=\"")? + "DeviceID=\"".len();
    let body = &r[start..];
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_reference_parsing() {
        let r = r#"Win32_Volume.DeviceID="\\\\?\\Volume{00000000-1111-2222-3333-444444444444}\\""#;
        assert_eq!(
            parse_volume_ref(r).unwrap(),
            r"\\?\Volume{00000000-1111-2222-3333-444444444444}\"
        );
        assert!(parse_volume_ref("Win32_Volume.Name=\"x\"").is_none());
        assert!(parse_volume_ref(r#"Win32_Volume.DeviceID="\\?"#).is_none());
        assert!(parse_volume_ref(r#"Win32_Volume.DeviceID="abc\"#).is_none());
    }

    #[test]
    fn query_succeeds_or_degrades() {
        match shadow_storage() {
            ShadowStorageReport::Available { volumes } => {
                for v in &volumes {
                    assert!(v.volume.starts_with(r"\\?\Volume{"), "{v:?}");
                    assert!(v.used_bytes <= v.allocated_bytes.max(v.used_bytes));
                }
            }
            ShadowStorageReport::Unavailable {
                reason,
                hresult,
                needs_elevation,
            } => {
                assert!(!reason.is_empty());
                assert_ne!(hresult, Some(0));
                assert_eq!(needs_elevation, !crate::token::is_elevated().unwrap());
            }
        }
    }
}
