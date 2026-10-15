//! Pluggable digest credentials for stock Antminer CGI.
//!
//! Defaults are the well-known factory values documented by Bitmain
//! (`root` / `root`). Callers should inject real credentials from the
//! OS keychain; nothing here is ever written to logs or the repo.

use serde::{Deserialize, Serialize};

/// HTTP digest username/password for Antminer stock CGI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestCredentials {
    pub username: String,
    pub password: String,
}

impl DigestCredentials {
    /// Documented Antminer stock factory default. Prefer injecting
    /// real credentials; this is only a fallback when none are set.
    pub fn antminer_stock_default() -> Self {
        Self {
            username: "root".into(),
            password: "root".into(),
        }
    }
}

impl Default for DigestCredentials {
    fn default() -> Self {
        Self::antminer_stock_default()
    }
}
