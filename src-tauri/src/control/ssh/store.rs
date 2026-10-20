//! SSH logins and pinned host keys, in the OS keychain.
//!
//! Logins are stored per firmware shell (`ssh-login:<class>`), optionally
//! overridden per device (`ssh-login:dev:<key>`). Host keys are pinned per
//! device (`ssh-hostkey:<key>`) as an OpenSSH SHA256 fingerprint, after the
//! operator accepts the fingerprint in a visible prompt (trust on first
//! use). Secrets never leave this module except into the SSH transport.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::control::creds::KEYRING_SERVICE;
use crate::control::safety::DeviceClass;

/// How to authenticate. A private key is OpenSSH/PEM text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum SshSecret {
    Password(String),
    PrivateKey(String),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshLogin {
    pub username: String,
    pub secret: SshSecret,
}

impl fmt::Debug for SshLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.secret {
            SshSecret::Password(_) => "password",
            SshSecret::PrivateKey(_) => "private_key",
        };
        write!(
            f,
            "SshLogin {{ username: {:?}, secret: <{kind} redacted> }}",
            self.username
        )
    }
}

impl fmt::Debug for SshSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Stable per-device key: MAC when known (survives DHCP), else IP.
pub fn device_key(ip: &str, mac: Option<&str>) -> String {
    match mac.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("mac:{}", m.to_ascii_lowercase()),
        None => format!("ip:{ip}"),
    }
}

fn class_key(class: DeviceClass) -> String {
    format!("{class:?}").to_ascii_lowercase()
}

pub trait SecretKv: Send + Sync {
    fn get_raw(&self, name: &str) -> anyhow::Result<Option<String>>;
    fn set_raw(&self, name: &str, value: &str) -> anyhow::Result<()>;
    fn delete_raw(&self, name: &str) -> anyhow::Result<()>;
}

impl<'a> dyn SecretKv + 'a {
    /// Device override first, then the firmware-wide login.
    pub fn login(&self, class: DeviceClass, dev: &str) -> anyhow::Result<Option<SshLogin>> {
        for name in [
            format!("ssh-login:dev:{dev}"),
            format!("ssh-login:{}", class_key(class)),
        ] {
            if let Some(s) = self.get_raw(&name)? {
                return Ok(Some(serde_json::from_str(&s)?));
            }
        }
        Ok(None)
    }
    pub fn set_login(
        &self,
        class: DeviceClass,
        dev: Option<&str>,
        login: &SshLogin,
    ) -> anyhow::Result<()> {
        let name = match dev {
            Some(d) => format!("ssh-login:dev:{d}"),
            None => format!("ssh-login:{}", class_key(class)),
        };
        self.set_raw(&name, &serde_json::to_string(login)?)
    }
    pub fn delete_login(&self, class: DeviceClass, dev: Option<&str>) -> anyhow::Result<()> {
        match dev {
            Some(d) => self.delete_raw(&format!("ssh-login:dev:{d}")),
            None => self.delete_raw(&format!("ssh-login:{}", class_key(class))),
        }
    }
    pub fn pinned(&self, dev: &str) -> anyhow::Result<Option<String>> {
        self.get_raw(&format!("ssh-hostkey:{dev}"))
    }
    pub fn pin(&self, dev: &str, fingerprint: &str) -> anyhow::Result<()> {
        if !fingerprint.starts_with("SHA256:") {
            anyhow::bail!("not a SHA256 host-key fingerprint");
        }
        self.set_raw(&format!("ssh-hostkey:{dev}"), fingerprint)
    }
    pub fn unpin(&self, dev: &str) -> anyhow::Result<()> {
        self.delete_raw(&format!("ssh-hostkey:{dev}"))
    }
}

/// OS keychain, service "TerraMine".
pub struct KeyringKv;

impl SecretKv for KeyringKv {
    fn get_raw(&self, name: &str) -> anyhow::Result<Option<String>> {
        match keyring::Entry::new(KEYRING_SERVICE, name)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set_raw(&self, name: &str, value: &str) -> anyhow::Result<()> {
        keyring::Entry::new(KEYRING_SERVICE, name)?.set_password(value)?;
        Ok(())
    }
    fn delete_raw(&self, name: &str) -> anyhow::Result<()> {
        match keyring::Entry::new(KEYRING_SERVICE, name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryKv(Mutex<HashMap<String, String>>);

impl SecretKv for MemoryKv {
    fn get_raw(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(name).cloned())
    }
    fn set_raw(&self, name: &str, value: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().insert(name.into(), value.into());
        Ok(())
    }
    fn delete_raw(&self, name: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(name);
        Ok(())
    }
}

/// What the UI may see about SSH access to one device.
#[derive(Debug, Clone, Serialize)]
pub struct SshAccessSummary {
    pub username: Option<String>,
    pub auth: Option<&'static str>,
    pub pinned_fingerprint: Option<String>,
}

pub fn summarize(store: &dyn SecretKv, class: DeviceClass, dev: &str) -> SshAccessSummary {
    let login = store.login(class, dev).ok().flatten();
    SshAccessSummary {
        username: login.as_ref().map(|l| l.username.clone()),
        auth: login.map(|l| match l.secret {
            SshSecret::Password(_) => "password",
            SshSecret::PrivateKey(_) => "private_key",
        }),
        pinned_fingerprint: store.pinned(dev).ok().flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_override_beats_class_login_and_debug_redacts() {
        let s = MemoryKv::default();
        let s: &dyn SecretKv = &s;
        let class_login = SshLogin {
            username: "root".into(),
            secret: SshSecret::Password("class-pw".into()),
        };
        let dev_login = SshLogin {
            username: "root".into(),
            secret: SshSecret::PrivateKey("-----BEGIN KEY-----".into()),
        };
        s.set_login(DeviceClass::AntminerStock, None, &class_login)
            .unwrap();
        let dev = device_key("192.0.2.5", Some("02:00:00:00:00:01"));
        assert_eq!(dev, "mac:02:00:00:00:00:01");
        assert_eq!(
            s.login(DeviceClass::AntminerStock, &dev).unwrap(),
            Some(class_login)
        );
        s.set_login(DeviceClass::AntminerStock, Some(&dev), &dev_login)
            .unwrap();
        assert_eq!(
            s.login(DeviceClass::AntminerStock, &dev).unwrap(),
            Some(dev_login.clone())
        );
        let dbg = format!("{dev_login:?}");
        assert!(!dbg.contains("BEGIN") && dbg.contains("redacted"), "{dbg}");
        let sum = summarize(s, DeviceClass::AntminerStock, &dev);
        assert_eq!(sum.auth, Some("private_key"));
        assert!(!serde_json::to_string(&sum).unwrap().contains("BEGIN"));
    }

    #[test]
    fn pins_require_sha256_and_unpin() {
        let s = MemoryKv::default();
        let s: &dyn SecretKv = &s;
        assert!(s.pin("ip:192.0.2.5", "md5:aa").is_err());
        s.pin("ip:192.0.2.5", "SHA256:abc").unwrap();
        assert_eq!(
            s.pinned("ip:192.0.2.5").unwrap().as_deref(),
            Some("SHA256:abc")
        );
        s.unpin("ip:192.0.2.5").unwrap();
        assert_eq!(s.pinned("ip:192.0.2.5").unwrap(), None);
    }
}
