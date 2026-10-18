//! Miner driver abstraction for start / stop / reboot.
//!
//! Assumptions:
//! - "stop" = pause/sleep (miner stays reachable), "start" = resume.
//! - Default path is asic-rs via [`AsicRsDriver`]. Live overrides for
//!   Bitaxe/AxeOS and stock Antminer S9 live in [`super::overrides`].
//! - A reboot often drops the connection before replying, so a transport
//!   error or timeout on reboot is reported as "probably rebooting".

use std::net::IpAddr;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::cache::{CachedDevice, DriverKind};
use super::overrides::{BitaxeDriver, DigestCredentials, S9StockDriver};
use super::safety::settings::{HealthSample, SettingsDriver};
use super::safety::DeviceProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MinerAction {
    Start,
    Stop,
    Reboot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub start: bool,
    pub stop: bool,
    pub reboot: bool,
}

impl Capabilities {
    pub fn supports(&self, action: MinerAction) -> bool {
        match action {
            MinerAction::Start => self.start,
            MinerAction::Stop => self.stop,
            MinerAction::Reboot => self.reboot,
        }
    }
}

pub trait MinerDriver: Send + Sync {
    #[allow(dead_code)]
    fn ip(&self) -> IpAddr;
    fn caps(&self) -> Capabilities;
    /// Ok(true) = accepted, Ok(false) = firmware refused / no-op.
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>>;
    /// Current state used for post-action verification.
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async { anyhow::bail!("status not supported") })
    }
    /// Identity for the safety layer (limits, path selection, audit).
    fn profile(&self) -> DeviceProfile {
        DeviceProfile::unknown()
    }
    /// Which driver family built this (cached so actions can rebuild it
    /// without identifying the device again).
    fn kind(&self) -> DriverKind {
        DriverKind::AsicRs
    }
    /// One cheap read: current status plus MAC/firmware, used to check the
    /// cached identity is still right. `Ok(None)` = no identity read
    /// available for this driver (nothing is sent).
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        Box::pin(async { Ok(None) })
    }
    /// Health sample for the post-change watch. Never errors.
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            match self.status().await {
                Ok(_) => HealthSample { reachable: true, hashrate_ghs: None, temp_c: None },
                Err(_) => HealthSample::unreachable(),
            }
        })
    }
    /// Settings write backends for this device, in preference order.
    /// Empty = this device has no settings write path.
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        Vec::new()
    }
}

/// MAC + firmware as reported by the device right now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub mac: Option<String>,
    pub firmware: Option<String>,
}

impl Identity {
    /// `Some(reason)` when both sides report a field and the values differ.
    pub fn mismatch(&self, now: &Identity) -> Option<String> {
        // Formatting differences (case, MAC separators, a leading "v" on
        // versions, spacing) are not a different device.
        let mac = |s: &str| s.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase();
        let norm = |s: &str| {
            let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
            s.strip_prefix('v').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).map(str::to_string).unwrap_or(s)
        };
        let mut why = Vec::new();
        if let (Some(a), Some(b)) = (&self.mac, &now.mac) {
            if mac(a) != mac(b) {
                why.push(format!("MAC {a} → {b}"));
            }
        }
        if let (Some(a), Some(b)) = (&self.firmware, &now.firmware) {
            if norm(a) != norm(b) {
                why.push(format!("firmware {a} → {b}"));
            }
        }
        (!why.is_empty()).then(|| why.join(", "))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// Status from the same read when available (reboot baseline).
    pub status: Option<MinerStatus>,
    pub identity: Identity,
}

/// Snapshot used to verify an action took effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinerStatus {
    /// Seconds since boot (or since the mining process started).
    pub uptime_s: Option<u64>,
    /// Some(true) = paused/sleeping, Some(false) = hashing.
    pub paused: Option<bool>,
}

/// Resolves an IP to a driver (None = no miner found).
pub trait DriverFactory: Send + Sync {
    /// Full identification (the expensive path).
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>>;
    /// Build a driver from a cached discovery record without identifying.
    /// Default falls back to [`Self::connect`].
    fn build<'a>(&'a self, ip: IpAddr, cached: &'a CachedDevice) -> BoxFuture<'a, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        let _ = cached;
        self.connect(ip)
    }
}

// ---------------- asic-rs backed driver ----------------

pub type SharedMiner = Arc<dyn asic_rs_core::traits::miner::Miner>;

pub struct AsicRsDriver {
    ip: IpAddr,
    miner: SharedMiner,
}

impl AsicRsDriver {
    pub fn new(ip: IpAddr, miner: Box<dyn asic_rs_core::traits::miner::Miner>) -> Self {
        Self { ip, miner: Arc::from(miner) }
    }
    pub fn from_shared(ip: IpAddr, miner: SharedMiner) -> Self {
        Self { ip, miner }
    }
}

impl MinerDriver for AsicRsDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities {
            start: self.miner.supports_resume(),
            stop: self.miner.supports_pause(),
            reboot: self.miner.supports_restart(),
        }
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Start => self.miner.resume(None).await,
                MinerAction::Stop => self.miner.pause(None).await,
                MinerAction::Reboot => self.miner.restart().await,
            }
        })
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async move {
            let d = self.miner.get_data().await;
            Ok(MinerStatus { uptime_s: d.uptime.map(|u| u.as_secs()), paused: Some(!d.is_mining) })
        })
    }
    fn profile(&self) -> DeviceProfile {
        let i = self.miner.get_device_info();
        DeviceProfile::classify(&i.make, &i.model, &i.firmware, None)
    }
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        Box::pin(async move {
            let mac = self.miner.get_mac().await.map(|m| m.to_string());
            let firmware = self.miner.get_firmware_version().await;
            if mac.is_none() && firmware.is_none() {
                anyhow::bail!("no identity reported");
            }
            Ok(Some(Observation { status: None, identity: Identity { mac, firmware } }))
        })
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move { super::safety::asic::health_from_miner_data(&self.miner.get_data().await) })
    }
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(super::safety::asic::AsicRsSettings::new(self.miner.clone()))]
    }
}

pub struct AsicRsFactory {
    pub(crate) factory: asic_rs::MinerFactory,
}

impl AsicRsFactory {
    pub fn new(timeout: Duration) -> Self {
        Self {
            factory: asic_rs::MinerFactory::new()
                .with_connectivity_timeout(timeout)
                .with_identification_timeout(timeout),
        }
    }
}

impl DriverFactory for AsicRsFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            let m = self.factory.get_miner(ip).await?;
            Ok(m.map(|m| Box::new(AsicRsDriver::new(ip, m)) as Box<dyn MinerDriver>))
        })
    }
}

// ---------------- overlay factory (asic-rs + live overrides) ----------------

/// Tries Bitaxe HTTP and stock-S9 CGI overrides first, then asic-rs.
pub struct OverlayFactory {
    timeout: Duration,
    s9_creds: DigestCredentials,
    asic: AsicRsFactory,
}

impl OverlayFactory {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            s9_creds: DigestCredentials::antminer_stock_default(),
            asic: AsicRsFactory::new(timeout),
        }
    }

    /// Loads S9 digest credentials from the credential store (OS keychain
    /// in the app), falling back to the documented factory default.
    pub fn with_store(timeout: Duration, store: &dyn super::creds::CredentialStore) -> Self {
        let creds = store
            .get(super::creds::FirmwareKind::AntminerStock)
            .ok()
            .flatten()
            .unwrap_or_else(DigestCredentials::antminer_stock_default);
        Self::new(timeout).with_s9_credentials(creds)
    }

    pub fn s9_credentials(&self) -> &DigestCredentials {
        &self.s9_creds
    }

    pub fn with_s9_credentials(mut self, creds: DigestCredentials) -> Self {
        self.s9_creds = creds;
        self
    }
}

impl DriverFactory for OverlayFactory {
    /// Rebuild from the discovery cache: no identification requests. The
    /// stock-S9 override needs one config read for its miner-mode flag;
    /// the resulting driver is memoized by the cache.
    fn build<'a>(&'a self, ip: IpAddr, c: &'a CachedDevice) -> BoxFuture<'a, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            Ok(match c.kind {
                DriverKind::AxeOs => {
                    let client = reqwest::Client::builder().timeout(self.timeout).no_proxy().build()?;
                    let version = c.firmware().and_then(super::overrides::bitaxe::parse_loose_version);
                    Some(Box::new(
                        BitaxeDriver::from_parts(ip, format!("http://{ip}"), client, version, false)
                            .with_asic_model(c.profile.chip.clone()),
                    ) as Box<dyn MinerDriver>)
                }
                DriverKind::S9Stock => S9StockDriver::probe(ip, self.s9_creds.clone(), self.timeout)
                    .await?
                    .map(|d| Box::new(d) as Box<dyn MinerDriver>),
                DriverKind::AsicRs => match &c.miner {
                    Some(m) => Some(Box::new(AsicRsDriver::from_shared(ip, m.clone())) as Box<dyn MinerDriver>),
                    None => return self.connect(ip).await,
                },
            })
        })
    }

    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            if let Some(d) = BitaxeDriver::probe(ip, self.timeout).await? {
                return Ok(Some(Box::new(d) as Box<dyn MinerDriver>));
            }
            if let Some(d) =
                S9StockDriver::probe(ip, self.s9_creds.clone(), self.timeout).await?
            {
                return Ok(Some(Box::new(d) as Box<dyn MinerDriver>));
            }
            // asic-rs may still identify a Bitaxe with wrong caps — re-check.
            if let Some(m) = self.asic.factory.get_miner(ip).await? {
                let info = m.get_device_info();
                let make = info.make.to_ascii_lowercase();
                let model = info.model.to_ascii_lowercase();
                if make.contains("bitaxe") || make.contains("nerdaxe") {
                    if let Some(d) = BitaxeDriver::probe(ip, self.timeout).await? {
                        return Ok(Some(Box::new(d) as Box<dyn MinerDriver>));
                    }
                }
                if make.contains("antminer") && model.contains("s9") {
                    if let Some(d) =
                        S9StockDriver::probe(ip, self.s9_creds.clone(), self.timeout).await?
                    {
                        return Ok(Some(Box::new(d) as Box<dyn MinerDriver>));
                    }
                }
                return Ok(Some(Box::new(AsicRsDriver::new(ip, m)) as Box<dyn MinerDriver>));
            }
            Ok(None)
        })
    }
}

// ---------------- mock driver (tests / dry runs) ----------------

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockBehavior {
    Ok,
    Refuse,
    Error,
    Hang,
}

#[cfg(test)]
#[derive(Default)]
pub struct MockLog {
    pub calls: Mutex<Vec<(IpAddr, MinerAction, tokio::time::Instant)>>,
    pub in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
}

#[cfg(test)]
pub struct MockDriver {
    pub ip: IpAddr,
    pub caps: Capabilities,
    pub behavior: MockBehavior,
    pub delay: Duration,
    pub log: std::sync::Arc<MockLog>,
    pub status: Option<MinerStatus>,
}

#[cfg(test)]
impl MinerDriver for MockDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        self.caps
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            let log = &self.log;
            log.calls.lock().unwrap().push((self.ip, action, tokio::time::Instant::now()));
            let now = log.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            log.max_in_flight.fetch_max(now, Ordering::SeqCst);
            if self.behavior == MockBehavior::Hang {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            } else {
                tokio::time::sleep(self.delay).await;
            }
            log.in_flight.fetch_sub(1, Ordering::SeqCst);
            match self.behavior {
                MockBehavior::Ok | MockBehavior::Hang => Ok(true),
                MockBehavior::Refuse => Ok(false),
                MockBehavior::Error => anyhow::bail!("mock transport error"),
            }
        })
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        let st = self.status;
        Box::pin(async move { st.ok_or_else(|| anyhow::anyhow!("no status")) })
    }
}
