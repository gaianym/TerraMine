//! Per-firmware miner credentials.
//!
//! The app uses [`KeyringStore`] (macOS Keychain, Windows Credential
//! Manager, Secret Service on Linux). Secrets are never logged, never
//! returned to the UI, and never stored in the repo or localStorage.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::overrides::DigestCredentials;

pub const KEYRING_SERVICE: &str = "TerraMine";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareKind {
    AntminerStock,
    Braiins,
    Luxos,
    Vnish,
    Whatsminer,
}

impl FirmwareKind {
    pub const ALL: [FirmwareKind; 5] = [
        FirmwareKind::AntminerStock,
        FirmwareKind::Braiins,
        FirmwareKind::Luxos,
        FirmwareKind::Vnish,
        FirmwareKind::Whatsminer,
    ];
    pub fn key(self) -> &'static str {
        match self {
            FirmwareKind::AntminerStock => "antminer-stock",
            FirmwareKind::Braiins => "braiins",
            FirmwareKind::Luxos => "luxos",
            FirmwareKind::Vnish => "vnish",
            FirmwareKind::Whatsminer => "whatsminer",
        }
    }
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, kind: FirmwareKind) -> anyhow::Result<Option<DigestCredentials>>;
    fn set(&self, kind: FirmwareKind, creds: &DigestCredentials) -> anyhow::Result<()>;
    fn delete(&self, kind: FirmwareKind) -> anyhow::Result<()>;
}

/// Serialized as one JSON blob per firmware: {"username","password"}.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(kind: FirmwareKind) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(KEYRING_SERVICE, kind.key())?)
    }
}

impl CredentialStore for KeyringStore {
    fn get(&self, kind: FirmwareKind) -> anyhow::Result<Option<DigestCredentials>> {
        match Self::entry(kind)?.get_password() {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, kind: FirmwareKind, creds: &DigestCredentials) -> anyhow::Result<()> {
        Self::entry(kind)?.set_password(&serde_json::to_string(creds)?)?;
        Ok(())
    }
    fn delete(&self, kind: FirmwareKind) -> anyhow::Result<()> {
        match Self::entry(kind)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<FirmwareKind, DigestCredentials>>);

impl CredentialStore for MemoryStore {
    fn get(&self, kind: FirmwareKind) -> anyhow::Result<Option<DigestCredentials>> {
        Ok(self.0.lock().unwrap().get(&kind).cloned())
    }
    fn set(&self, kind: FirmwareKind, creds: &DigestCredentials) -> anyhow::Result<()> {
        self.0.lock().unwrap().insert(kind, creds.clone());
        Ok(())
    }
    fn delete(&self, kind: FirmwareKind) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(&kind);
        Ok(())
    }
}

/// What the UI may see: which firmwares have a stored login, never the secret.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialSummary {
    pub firmware: FirmwareKind,
    pub username: Option<String>,
    pub has_password: bool,
}

pub fn summarize(store: &dyn CredentialStore) -> Vec<CredentialSummary> {
    FirmwareKind::ALL
        .iter()
        .map(|&k| {
            let c = store.get(k).ok().flatten();
            CredentialSummary {
                firmware: k,
                username: c.as_ref().map(|c| c.username.clone()),
                has_password: c.map(|c| !c.password.is_empty()).unwrap_or(false),
            }
        })
        .collect()
}
