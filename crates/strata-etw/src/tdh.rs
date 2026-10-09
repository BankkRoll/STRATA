//! Manifest layouts from the Trace Data Helper (TDH) API.
//!
//! `TdhGetManifestEventInformation` returns the property list of an event
//! version from the provider manifest registered on this machine. It needs no
//! session and no elevation, so the built-in layout tables are checked
//! against the installed manifest in ordinary tests, and the monitor loads
//! layouts for event versions newer than this build at session start.
//!
//! This module only reads metadata; per-event decoding never calls TDH (see
//! [`crate::decode`] for why).

use windows::core::GUID;

use crate::layout::{Layout, LayoutTable, Provider};
use crate::{KERNEL_FILE_PROVIDER, KERNEL_PROCESS_PROVIDER};

/// Reads the top-level fields of one event version from the installed
/// manifest. `Ok(None)` when the manifest does not define that version.
///
/// # Errors
///
/// The TDH status code for any other failure.
pub fn manifest_layout(provider: &GUID, id: u16, version: u8) -> Result<Option<Layout>, u32> {
    crate::ffi::manifest_layout(provider, id, version)
}

/// The provider GUID for a [`Provider`].
#[must_use]
pub const fn provider_guid(p: Provider) -> GUID {
    match p {
        Provider::KernelFile => KERNEL_FILE_PROVIDER,
        Provider::KernelProcess => KERNEL_PROCESS_PROVIDER,
    }
}

/// Loads every version (0..=`max_version`) of the given events from the
/// installed manifests. Versions the manifest lacks are skipped; TDH errors
/// are collected so the caller can fall back to built-in layouts.
#[must_use]
pub fn load_layouts(events: &[(Provider, u16)], max_version: u8) -> (LayoutTable, Vec<String>) {
    let mut table = LayoutTable::default();
    let mut errors = Vec::new();
    for &(p, id) in events {
        let guid = provider_guid(p);
        for v in 0..=max_version {
            match manifest_layout(&guid, id, v) {
                Ok(Some(l)) if !l.fields.is_empty() => table.insert(p, id, v, l),
                Ok(_) => {}
                Err(code) => errors.push(format!("{p:?} event {id} v{v}: TDH status {code}")),
            }
        }
    }
    (table, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{DECODED_EVENTS, MAX_PROBED_VERSION, builtin_layouts};

    /// The built-in tables must match the manifests installed on this
    /// machine field for field (names, order and wire types). Runs
    /// unelevated: TDH reads registered manifests without a session.
    #[test]
    fn builtin_layouts_match_installed_manifests() {
        let (installed, errors) = load_layouts(DECODED_EVENTS, MAX_PROBED_VERSION);
        assert!(errors.is_empty(), "{errors:?}");
        let builtin = builtin_layouts();
        let mut compared = 0;
        for (p, id, v) in builtin.keys() {
            let Some(m) = installed.get(p, id, v) else {
                // NOTE: older Windows builds may lack the newest versions.
                continue;
            };
            assert_eq!(builtin.get(p, id, v), Some(m), "{p:?} event {id} v{v}");
            compared += 1;
        }
        assert_eq!(
            compared,
            builtin.len(),
            "manifest lacks versions this build knows"
        );
        // Any newer version on this machine must still carry the fields we read.
        for (p, id, v) in installed.keys() {
            let names: Vec<&str> = installed
                .get(p, id, v)
                .unwrap()
                .fields
                .iter()
                .map(|f| f.name.as_ref())
                .collect();
            let need: &[&str] = match (p, id) {
                (Provider::KernelFile, 16) => &["FileObject", "FileKey", "IOSize", "IOFlags"],
                (Provider::KernelFile, 12 | 30) => &["FileObject", "CreateOptions", "FileName"],
                (Provider::KernelProcess, 1) => &["ProcessID", "CreateTime", "ImageName"],
                _ => &[],
            };
            for n in need {
                assert!(names.contains(n), "{p:?} {id} v{v} lacks {n}");
            }
        }
    }

    #[test]
    fn unknown_event_is_not_found() {
        assert_eq!(manifest_layout(&KERNEL_FILE_PROVIDER, 9999, 0), Ok(None));
    }
}
