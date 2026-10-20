//! Miner credentials: one login per firmware kind, plus per-device
//! overrides (a password changed on one miner applies to that miner only).
//!
//! Lookup order for a device: its own entry (keyed by MAC, else IP), then
//! its firmware kind's entry, then the backend's factory default.
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
    /// Per-device entry (`key` from [`device_key`]). Default: none stored.
    fn get_device(&self, key: &str) -> anyhow::Result<Option<DigestCredentials>> {
        let _ = key;
        Ok(None)
    }
    /// Default: this store cannot hold per-device entries.
    fn set_device(&self, key: &str, creds: &DigestCredentials) -> anyhow::Result<()> {
        let _ = (key, creds);
        anyhow::bail!("this credential store has no per-device entries")
    }
    fn delete_device(&self, key: &str) -> anyhow::Result<()> {
        let _ = key;
        Ok(())
    }
}

/// Keychain key for one device: `device:<mac>` (lowercase hex, no
/// separators) when the MAC is known, else `device:<ip>`.
pub fn device_key(ip: std::net::IpAddr, mac: Option<&str>) -> String {
    let mac: String = mac
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if mac.len() == 12 {
        format!("device:{mac}")
    } else {
        format!("device:{ip}")
    }
}

/// Key holding a new password while a change is in flight: written before
/// the miner is touched, promoted to the device key once the change is
/// accepted, removed when it is not.
pub fn pending_key(device_key: &str) -> String {
    format!("{device_key}:pending")
}

/// Firmware kind whose shared login applies to a device class.
pub fn kind_for_class(class: super::safety::DeviceClass) -> Option<FirmwareKind> {
    use super::safety::DeviceClass as C;
    match class {
        C::AntminerStock => Some(FirmwareKind::AntminerStock),
        C::BraiinsOs => Some(FirmwareKind::Braiins),
        C::LuxOs => Some(FirmwareKind::Luxos),
        C::Vnish => Some(FirmwareKind::Vnish),
        C::Whatsminer => Some(FirmwareKind::Whatsminer),
        _ => None,
    }
}

/// Current login for a device: its own entry, else its firmware kind's.
/// `None` = use the backend default.
pub fn resolve(
    store: &dyn CredentialStore,
    device_key: &str,
    class: super::safety::DeviceClass,
) -> Option<DigestCredentials> {
    if let Ok(Some(c)) = store.get_device(device_key) {
        return Some(c);
    }
    kind_for_class(class).and_then(|k| store.get(k).ok().flatten())
}

/// Serialized as one JSON blob per firmware: {"username","password"}.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(kind: FirmwareKind) -> anyhow::Result<keyring::Entry> {
        Self::named(kind.key())
    }
    fn named(key: &str) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(KEYRING_SERVICE, key)?)
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
    fn get_device(&self, key: &str) -> anyhow::Result<Option<DigestCredentials>> {
        match Self::named(key)?.get_password() {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set_device(&self, key: &str, creds: &DigestCredentials) -> anyhow::Result<()> {
        Self::named(key)?.set_password(&serde_json::to_string(creds)?)?;
        Ok(())
    }
    fn delete_device(&self, key: &str) -> anyhow::Result<()> {
        match Self::named(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore(
    Mutex<HashMap<FirmwareKind, DigestCredentials>>,
    Mutex<HashMap<String, DigestCredentials>>,
);

impl MemoryStore {
    /// Per-device entries (tests).
    pub fn devices(&self) -> HashMap<String, DigestCredentials> {
        self.1.lock().unwrap().clone()
    }
}

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
    fn get_device(&self, key: &str) -> anyhow::Result<Option<DigestCredentials>> {
        Ok(self.1.lock().unwrap().get(key).cloned())
    }
    fn set_device(&self, key: &str, creds: &DigestCredentials) -> anyhow::Result<()> {
        self.1
            .lock()
            .unwrap()
            .insert(key.to_string(), creds.clone());
        Ok(())
    }
    fn delete_device(&self, key: &str) -> anyhow::Result<()> {
        self.1.lock().unwrap().remove(key);
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
