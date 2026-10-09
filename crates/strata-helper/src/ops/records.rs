//! `ReadRecords`: re-read MFT records after USN changes (SPEC §10.2).

use strata_core::{FileRef, ScanRecord};
use strata_ipc::protocol::Response;
use strata_ntfs::NtfsError;

use super::RequestCtx;
use crate::error::HelperError;
use crate::source::MftVolume;

/// Most references accepted in one request (the app re-measures once per
/// 250 ms tick; a burst larger than this is split by the app).
pub const MAX_RECORDS_PER_REQUEST: usize = 65_536;

/// What one reference resolved to.
enum Lookup {
    Found(ScanRecord),
    Missing,
    /// The record number lies past the MFT this volume handle knows about;
    /// the MFT may have grown since it was opened.
    Beyond,
}

fn lookup(volume: &MftVolume, r: FileRef) -> Result<Lookup, NtfsError> {
    match volume.read_record(r.record()) {
        // NOTE: a different sequence number means the record was reused by
        // another file; the reference the client holds is gone.
        Ok(Some(rec)) if rec.id == r => Ok(Lookup::Found(rec)),
        Ok(_) | Err(NtfsError::Record { .. }) => Ok(Lookup::Missing),
        Err(NtfsError::OutOfRange(_)) => Ok(Lookup::Beyond),
        Err(e) => Err(e),
    }
}

/// Reads `refs` from the cached volume and answers with `Records`.
///
/// # Errors
///
/// Too many references, the volume cannot be opened, or a read fails
/// (the cached handle is dropped so the next request reopens it).
pub fn read_records(
    ctx: &RequestCtx<'_>,
    volume: &str,
    refs: &[FileRef],
) -> Result<(), HelperError> {
    if refs.len() > MAX_RECORDS_PER_REQUEST {
        return Err(HelperError::bad_request(format!(
            "at most {MAX_RECORDS_PER_REQUEST} records per request"
        )));
    }
    let mut vol = ctx.shared.volumes.cached(volume)?;
    let mut reopened = false;
    let mut records = Vec::with_capacity(refs.len());
    let mut missing = Vec::new();
    for &r in refs {
        ctx.check_cancel()?;
        let mut found = lookup(&vol, r);
        if matches!(found, Ok(Lookup::Beyond)) && !reopened {
            reopened = true;
            vol = ctx.shared.volumes.open_fresh(volume)?;
            found = lookup(&vol, r);
        }
        match found {
            Ok(Lookup::Found(rec)) => records.push(rec),
            Ok(Lookup::Missing | Lookup::Beyond) => missing.push(r),
            Err(e) => {
                ctx.shared.volumes.invalidate(volume);
                return Err(e.into());
            }
        }
    }
    ctx.send(Response::Records { records, missing })
}
