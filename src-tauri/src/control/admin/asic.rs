//! asic-rs (0.8.6) admin path: `get_data().hashboards`, `ReadLogs`,
//! `ChangePassword`, `FactoryReset`, gated by `supports_read_logs()`,
//! `supports_change_password()` and `supports_factory_reset()`.
//!
//! The cached asic-rs miner (shared, default login) serves reads when no
//! login is stored. Anything that needs a stored login, and every
//! password change (asic-rs takes `&mut self` there), runs on a miner
//! rebuilt from the same asic-rs firmware entry the cached miner came
//! from (`FirmwareEntry::build_miner(ip, Some(login))`): two model /
//! version reads, no discovery scan.
//!
//! [`NativeAdmin`] is the seam the mock miners drive (asic-rs hardcodes
//! real ports), so tests run this file's gating and mapping unchanged.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use asic_rs_core::data::board::BoardData;
use asic_rs_core::traits::miner::{Miner, MinerAuth};
use futures::future::BoxFuture;

use super::{AdminAction, AdminBackend, Hashboard};
use crate::control::driver::SharedMiner;
use crate::control::overrides::DigestCredentials;

/// asic-rs `supports_*()` values for the admin traits on one backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeAdminFlags {
    pub read_logs: bool,
    pub change_password: bool,
    pub factory_reset: bool,
}

/// The asic-rs calls this path makes (real miner or a mock stand-in).
pub trait NativeAdmin: Send + Sync {
    fn flags(&self) -> NativeAdminFlags;
    /// "make model (firmware)" for messages.
    fn label(&self) -> String;
    /// asic-rs firmware family name (e.g. "AntMiner Stock", "Braiins").
    fn firmware(&self) -> String;
    fn boards(&self) -> BoxFuture<'_, anyhow::Result<Vec<BoardData>>>;
    fn read_logs<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
    ) -> BoxFuture<'a, anyhow::Result<String>>;
    fn change_password<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
        new_password: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<bool>>;
    fn factory_reset<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
    ) -> BoxFuture<'a, anyhow::Result<bool>>;
}

/// Factory username per asic-rs firmware family (each backend's
/// `default_auth()` in asic-rs-firmwares-* 0.8.6), for the families whose
/// backends implement `change_password`. Used only when no login is
/// stored for the device.
pub fn default_username(firmware: &str) -> Option<&'static str> {
    match firmware {
        "AntMiner Stock" | "Braiins" | "VolcMiner Stock" => Some("root"),
        // Password-only logins.
        "VNish" | "UMC OS" | "Proto Stock" => Some(""),
        _ => None,
    }
}

pub fn auth(c: &DigestCredentials) -> MinerAuth {
    MinerAuth::new(c.username.clone(), c.password.clone())
}

pub struct AsicRsAdmin {
    native: Arc<dyn NativeAdmin>,
}

impl AsicRsAdmin {
    pub fn new(native: Arc<dyn NativeAdmin>) -> Self {
        Self { native }
    }
    /// The real asic-rs miner (cached from discovery).
    pub fn live(ip: IpAddr, miner: SharedMiner, timeout: Duration) -> Self {
        Self::new(Arc::new(LiveNative { ip, miner, timeout }))
    }
    fn flag(&self, action: AdminAction) -> (&'static str, bool) {
        let f = self.native.flags();
        match action {
            AdminAction::Hashboards => ("get_data", true),
            AdminAction::ReadLogs => ("supports_read_logs", f.read_logs),
            AdminAction::ChangePassword => ("supports_change_password", f.change_password),
            AdminAction::FactoryReset => ("supports_factory_reset", f.factory_reset),
        }
    }
}

impl AdminBackend for AsicRsAdmin {
    fn id(&self) -> &'static str {
        "asic-rs"
    }
    fn supports(&self, action: AdminAction) -> Result<(), String> {
        match self.flag(action) {
            (_, true) => Ok(()),
            (flag, false) => Err(format!("{flag}() = false on {}", self.native.label())),
        }
    }
    /// Never run against real hardware (password / reset are destructive).
    fn confirmed(&self, _action: AdminAction) -> bool {
        false
    }
    fn hashboards(&self) -> BoxFuture<'_, anyhow::Result<Vec<Hashboard>>> {
        Box::pin(async move {
            Ok(self
                .native
                .boards()
                .await?
                .iter()
                .map(Hashboard::from_board)
                .collect())
        })
    }
    fn read_logs<'a>(
        &'a self,
        login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            anyhow::ensure!(
                self.native.flags().read_logs,
                "Reading logs is not supported on this platform"
            );
            let a = login.map(auth);
            self.native.read_logs(a.as_ref()).await
        })
    }
    fn change_password<'a>(
        &'a self,
        login: Option<&'a DigestCredentials>,
        new_password: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            anyhow::ensure!(
                self.native.flags().change_password,
                "Setting password is not supported on this platform"
            );
            let a = login.map(auth);
            self.native.change_password(a.as_ref(), new_password).await
        })
    }
    fn factory_reset<'a>(
        &'a self,
        login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            anyhow::ensure!(
                self.native.flags().factory_reset,
                "Factory resetting is not supported on this platform"
            );
            let a = login.map(auth);
            self.native.factory_reset(a.as_ref()).await
        })
    }
    /// An authenticated asic-rs read with the new login (`read_logs`,
    /// where the backend has it).
    fn check_login<'a>(
        &'a self,
        login: &'a DigestCredentials,
    ) -> BoxFuture<'a, anyhow::Result<Option<bool>>> {
        Box::pin(async move {
            if !self.native.flags().read_logs {
                return Ok(None);
            }
            let a = auth(login);
            Ok(Some(self.native.read_logs(Some(&a)).await.is_ok()))
        })
    }
    fn default_username(&self) -> Option<String> {
        default_username(&self.native.firmware()).map(str::to_string)
    }
}

/// The real asic-rs miner.
pub struct LiveNative {
    ip: IpAddr,
    miner: SharedMiner,
    timeout: Duration,
}

impl LiveNative {
    /// An owned miner from the cached miner's own asic-rs firmware entry,
    /// with `login` applied (`None` = that backend's default).
    async fn rebuild(&self, login: Option<&MinerAuth>) -> anyhow::Result<Box<dyn Miner>> {
        let fw = self.miner.get_device_info().firmware;
        let entry = asic_rs::factory::default_firmware_registry()
            .into_iter()
            .find(|e| e.to_string() == fw)
            .ok_or_else(|| anyhow::anyhow!("no asic-rs firmware entry named {fw:?}"))?;
        let m = tokio::time::timeout(self.timeout, entry.build_miner(self.ip, login))
            .await
            .map_err(|_| anyhow::anyhow!("timeout rebuilding the asic-rs miner"))?
            .map_err(|e| anyhow::anyhow!("rebuilding the asic-rs miner failed: {e}"))?;
        Ok(m)
    }
}

impl NativeAdmin for LiveNative {
    fn flags(&self) -> NativeAdminFlags {
        NativeAdminFlags {
            read_logs: self.miner.supports_read_logs(),
            change_password: self.miner.supports_change_password(),
            factory_reset: self.miner.supports_factory_reset(),
        }
    }
    fn label(&self) -> String {
        let i = self.miner.get_device_info();
        format!("{} {} ({})", i.make, i.model, i.firmware)
    }
    fn firmware(&self) -> String {
        self.miner.get_device_info().firmware
    }
    fn boards(&self) -> BoxFuture<'_, anyhow::Result<Vec<BoardData>>> {
        Box::pin(async move { Ok(self.miner.get_data().await.hashboards) })
    }
    fn read_logs<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            match login {
                None => self.miner.read_logs().await,
                Some(_) => self.rebuild(login).await?.read_logs().await,
            }
        })
    }
    fn change_password<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
        new_password: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let mut m = self.rebuild(login).await?;
            m.change_password(new_password).await
        })
    }
    fn factory_reset<'a>(
        &'a self,
        login: Option<&'a MinerAuth>,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            match login {
                None => self.miner.factory_reset().await,
                Some(_) => self.rebuild(login).await?.factory_reset().await,
            }
        })
    }
}
