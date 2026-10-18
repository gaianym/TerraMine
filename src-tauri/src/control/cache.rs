//! Device cache filled by discovery and reused by every action.
//!
//! Actions do not identify a miner again: they rebuild (or reuse) the
//! driver from the cached record and make one cheap identity read. Only a
//! missing entry, or a MAC/firmware mismatch, triggers identification —
//! and a mismatch refuses that miner for the current run.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::driver::{DriverFactory, Identity, MinerDriver, Observation, SharedMiner};
use super::safety::audit::now_ms;
use super::safety::DeviceProfile;

/// Which driver family handles the device (our overrides or asic-rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    AxeOs,
    S9Stock,
    AsicRs,
}

impl DriverKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::AxeOs => "axeos",
            Self::S9Stock => "s9-stock",
            Self::AsicRs => "asic-rs",
        }
    }

    /// Same routing rule as `OverlayFactory::connect`.
    pub fn for_device(make: &str, model: &str) -> Self {
        let make = make.to_ascii_lowercase();
        let model = model.to_ascii_lowercase();
        if make.contains("bitaxe") || make.contains("nerdaxe") {
            Self::AxeOs
        } else if make.contains("antminer") && model.contains("s9") {
            Self::S9Stock
        } else {
            Self::AsicRs
        }
    }
}

#[derive(Clone)]
pub struct CachedDevice {
    pub profile: DeviceProfile,
    pub identity: Identity,
    pub kind: DriverKind,
    pub seen_at_ms: u64,
    /// asic-rs miner object kept from discovery (asic-rs devices).
    pub miner: Option<SharedMiner>,
    /// Driver memoized after first use.
    pub driver: Option<Arc<dyn MinerDriver>>,
}

impl CachedDevice {
    pub fn firmware(&self) -> Option<&str> {
        self.identity.firmware.as_deref()
    }
}

#[derive(Default)]
pub struct DeviceCache {
    inner: Mutex<HashMap<IpAddr, CachedDevice>>,
}

impl DeviceCache {
    /// Process-wide cache shared by discovery and control.
    pub fn global() -> Arc<DeviceCache> {
        static G: OnceLock<Arc<DeviceCache>> = OnceLock::new();
        G.get_or_init(|| Arc::new(DeviceCache::default())).clone()
    }

    pub fn get(&self, ip: IpAddr) -> Option<CachedDevice> {
        self.inner.lock().unwrap().get(&ip).cloned()
    }

    pub fn put(&self, ip: IpAddr, d: CachedDevice) {
        self.inner.lock().unwrap().insert(ip, d);
    }

    pub fn invalidate(&self, ip: IpAddr) {
        self.inner.lock().unwrap().remove(&ip);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn update(&self, ip: IpAddr, f: impl FnOnce(&mut CachedDevice)) {
        if let Some(d) = self.inner.lock().unwrap().get_mut(&ip) {
            f(d);
        }
    }

    /// Up to `n` cached IPs not in `exclude`, most recently seen first
    /// (used as untouched controls during a batch watch).
    pub fn others(&self, exclude: &[IpAddr], n: usize) -> Vec<IpAddr> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<_> = g.iter().filter(|(ip, _)| !exclude.contains(ip)).map(|(ip, d)| (*ip, d.seen_at_ms)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(n).map(|(ip, _)| ip).collect()
    }

    /// Record a discovered miner (identity from the enrich data).
    pub fn record_discovery(&self, ip: IpAddr, miner: SharedMiner, mac: Option<String>, firmware_version: Option<String>) {
        let info = miner.get_device_info();
        let kind = DriverKind::for_device(&info.make, &info.model);
        let profile = DeviceProfile::classify(&info.make, &info.model, &info.firmware, None);
        self.put(
            ip,
            CachedDevice {
                profile,
                identity: Identity { mac, firmware: firmware_version },
                kind,
                seen_at_ms: now_ms(),
                miner: Some(miner),
                driver: None,
            },
        );
    }

    fn from_driver(d: Arc<dyn MinerDriver>, identity: Identity) -> CachedDevice {
        CachedDevice { profile: d.profile(), identity, kind: d.kind(), seen_at_ms: now_ms(), miner: None, driver: Some(d) }
    }
}

/// Result of resolving an IP for an action.
pub enum Resolved {
    Ready { driver: Arc<dyn MinerDriver>, obs: Option<Observation> },
    /// Identity at this IP changed; re-identified once, refuse this run.
    Changed(String),
    NotFound(Option<String>),
    Failed(String),
}

/// Cache-first driver resolution.
pub struct Resolver {
    pub factory: Arc<dyn DriverFactory>,
    pub cache: Arc<DeviceCache>,
}

impl Resolver {
    pub fn new(factory: Arc<dyn DriverFactory>, cache: Arc<DeviceCache>) -> Self {
        Self { factory, cache }
    }

    async fn identify(&self, ip: IpAddr, timeout: Duration) -> Result<Option<(Arc<dyn MinerDriver>, Option<Observation>)>, Resolved> {
        let d = match tokio::time::timeout(timeout, self.factory.connect(ip)).await {
            Ok(Ok(Some(d))) => Arc::<dyn MinerDriver>::from(d),
            Ok(Ok(None)) => return Ok(None),
            Ok(Err(e)) => return Err(Resolved::Failed(e.to_string())),
            Err(_) => return Err(Resolved::NotFound(Some("connect timeout".into()))),
        };
        let obs = tokio::time::timeout(timeout, d.observe()).await.ok().and_then(|r| r.ok()).flatten();
        let identity = obs.as_ref().map(|o| o.identity.clone()).unwrap_or_default();
        self.cache.put(ip, DeviceCache::from_driver(d.clone(), identity));
        Ok(Some((d, obs)))
    }

    pub async fn resolve(&self, ip: IpAddr, timeout: Duration) -> Resolved {
        let Some(cached) = self.cache.get(ip) else {
            return match self.identify(ip, timeout).await {
                Ok(Some((driver, obs))) => Resolved::Ready { driver, obs },
                Ok(None) => Resolved::NotFound(None),
                Err(r) => r,
            };
        };
        let driver = match &cached.driver {
            Some(d) => d.clone(),
            None => match tokio::time::timeout(timeout, self.factory.build(ip, &cached)).await {
                Ok(Ok(Some(d))) => {
                    let d = Arc::<dyn MinerDriver>::from(d);
                    self.cache.update(ip, |c| c.driver = Some(d.clone()));
                    d
                }
                Ok(Ok(None)) => return Resolved::NotFound(None),
                Ok(Err(e)) => return Resolved::Failed(e.to_string()),
                Err(_) => return Resolved::NotFound(Some("connect timeout".into())),
            },
        };
        let obs = match tokio::time::timeout(timeout, driver.observe()).await {
            Ok(Ok(o)) => o,
            Ok(Err(_)) => return Resolved::NotFound(None),
            Err(_) => return Resolved::NotFound(Some("connect timeout".into())),
        };
        if let Some(o) = &obs {
            if let Some(why) = cached.identity.mismatch(&o.identity) {
                self.cache.invalidate(ip);
                let now = match self.identify(ip, timeout).await {
                    Ok(Some((d, _))) => {
                        let p = d.profile();
                        format!("re-identified as {} {} ({})", p.make, p.model, p.firmware)
                    }
                    Ok(None) => "re-identify found no miner".into(),
                    Err(_) => "re-identify failed".into(),
                };
                return Resolved::Changed(format!("device at {ip} changed ({why}); {now}; not sent this run"));
            }
            self.cache.update(ip, |c| {
                c.seen_at_ms = now_ms();
                if c.identity.mac.is_none() {
                    c.identity.mac = o.identity.mac.clone();
                }
                if c.identity.firmware.is_none() {
                    c.identity.firmware = o.identity.firmware.clone();
                }
            });
        }
        Resolved::Ready { driver, obs }
    }
}
