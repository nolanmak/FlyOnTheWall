//! Transactional updates for the daemon’s receipt maps stored in settings.

use std::collections::HashMap;

use fotw_store::{Db, Result, StoreError};
use serde::{Serialize, de::DeserializeOwned};

/// Apply a delta to the current map and write it back if the callback returns true.
/// The callback runs under the SQLite writer lock and only changes the map.
pub(crate) fn update<T: Serialize + DeserializeOwned>(
    db: &mut Db,
    key: &str,
    apply: impl FnOnce(&mut HashMap<String, T>) -> bool,
) -> Result<()> {
    db.update_setting(key, |current| {
        let mut entries = current
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default();
        #[cfg(test)]
        tests::after_read();
        if !apply(&mut entries) {
            return Ok(None);
        }
        serde_json::to_string(&entries)
            .map(Some)
            .map_err(|error| StoreError::InvalidArgument(error.to_string()))
    })
}

/// Synthetic fixtures and scheduling hooks for the receipt writers.
#[cfg(test)]
pub(crate) mod tests;
