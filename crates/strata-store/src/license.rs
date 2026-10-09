//! Storage for the activated license.
//!
//! Only persistence lives here: the payload and signature are stored exactly
//! as received and verified elsewhere on every launch. Keeping the license in
//! `state.db` means a history reset never deactivates the app.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Store;
use crate::clock::Timestamp;
use crate::error::Result;

/// The stored license.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseRecord {
    /// Signed license payload bytes.
    pub payload: Vec<u8>,
    /// Ed25519 signature over `payload`.
    pub signature: Vec<u8>,
    /// When it was activated on this machine.
    pub activated_at: Timestamp,
}

const SQL_SAVE_LICENSE: &str = "
INSERT INTO license (id, payload, signature, activated_at)
VALUES (1, ?1, ?2, ?3)
ON CONFLICT (id) DO UPDATE SET
    payload = excluded.payload,
    signature = excluded.signature,
    activated_at = excluded.activated_at";

const SQL_LOAD_LICENSE: &str = "
SELECT payload, signature, activated_at FROM license WHERE id = 1";

const SQL_CLEAR_LICENSE: &str = "
DELETE FROM license";

impl Store {
    /// Stores (or replaces) the activated license, stamped with the current
    /// time.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::Store;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// store.save_license(b"payload", &[0u8; 64]).unwrap();
    /// assert_eq!(store.load_license().unwrap().unwrap().payload, b"payload");
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn save_license(&self, payload: &[u8], signature: &[u8]) -> Result<LicenseRecord> {
        let now = self.now();
        self.state().write(|tx| {
            tx.execute(SQL_SAVE_LICENSE, params![payload, signature, now.0])?;
            Ok(())
        })?;
        Ok(LicenseRecord {
            payload: payload.to_vec(),
            signature: signature.to_vec(),
            activated_at: now,
        })
    }

    /// The stored license, if any.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn load_license(&self) -> Result<Option<LicenseRecord>> {
        self.state().read(|c| {
            Ok(c.query_row(SQL_LOAD_LICENSE, [], |r| {
                Ok(LicenseRecord {
                    payload: r.get(0)?,
                    signature: r.get(1)?,
                    activated_at: Timestamp(r.get(2)?),
                })
            })
            .optional()?)
        })
    }

    /// Removes the stored license (deactivation).
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn clear_license(&self) -> Result<()> {
        self.state().write(|tx| {
            tx.execute(SQL_CLEAR_LICENSE, [])?;
            Ok(())
        })
    }
}
