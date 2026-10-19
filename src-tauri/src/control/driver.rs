//! Miner driver abstraction for start / stop / reboot / fault light.
//!
//! Assumptions:
//! - "stop" = pause/sleep (miner stays reachable), "start" = resume.
//! - Default path is asic-rs via [`AsicRsDriver`]. Live overrides for
//!   Bitaxe/AxeOS and stock Antminer S9 live in [`super::overrides`].
//! - Capabilities are a per-action map of routes. asic-rs (native) is
//!   preferred; an override is used only for actions asic-rs does not
//!   support on that firmware (e.g. Bitaxe reboot on asic-rs 0.8.6).
//! - A reboot often drops the connection before replying, so a transport
//!   error or timeout on reboot is reported as "probably rebooting".

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

use super::cache::{CachedDevice, DriverKind};
use super::overrides::{BitaxeDriver, DigestCredentials, S9StockDriver};
use super::safety::settings::{HealthSample, SettingsDriver};
use super::safety::DeviceProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MinerAction {
    Start,
    Stop,
    Reboot,
    /// Fault / locate light on (blink).
    LightOn,
    /// Fault / locate light off.
    LightOff,
}

impl MinerAction {
    pub const ALL: [MinerAction; 5] =
        [MinerAction::Start, MinerAction::Stop, MinerAction::Reboot, MinerAction::LightOn, MinerAction::LightOff];

    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reboot => "reboot",
            Self::LightOn => "light_on",
            Self::LightOff => "light_off",
        }
    }

    /// `Some(wanted light state)` for the fault-light actions.
    pub fn light_target(self) -> Option<bool> {
        match self {
            Self::LightOn => Some(true),
            Self::LightOff => Some(false),
            _ => None,
        }
    }
}

/// asic-rs `supports_*()` flags that decide native routes. Kept as plain
/// data so the routing rule is one function, shared with the test doubles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeFlags {
    pub resume: bool,
    pub pause: bool,
    pub restart: bool,
    pub fault_light: bool,
}

impl NativeFlags {
    pub fn of(m: &dyn asic_rs_core::traits::miner::Miner) -> Self {
        Self {
            resume: m.supports_resume(),
            pause: m.supports_pause(),
            restart: m.supports_restart(),
            fault_light: m.supports_set_fault_light(),
        }
    }

    pub fn caps(self) -> Capabilities {
        let mut c = Capabilities::flags(self.resume, self.pause, self.restart, Route::AsicRs);
        if self.fault_light {
            c = c.with(MinerAction::LightOn, Route::AsicRs).with(MinerAction::LightOff, Route::AsicRs);
        }
        c
    }

    /// The asic-rs flag behind `action`, for "paths tried" messages.
    pub fn explain(self, action: MinerAction) -> String {
        let (flag, on) = match action {
            MinerAction::Start => ("supports_resume", self.resume),
            MinerAction::Stop => ("supports_pause", self.pause),
            MinerAction::Reboot => ("supports_restart", self.restart),
            MinerAction::LightOn | MinerAction::LightOff => ("supports_set_fault_light", self.fault_light),
        };
        format!("asic-rs: {flag}() = {on}")
    }
}

/// How an action reaches the miner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    /// asic-rs native support (`supports_*()` is true).
    AsicRs,
    /// AxeOS HTTP override (Bitaxe / NerdAxe).
    Axeos,
    /// Stock Antminer S9 CGI override.
    S9Cgi,
    /// Test double.
    Mock,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::AsicRs => "asic-rs",
            Self::Axeos => "axeos",
            Self::S9Cgi => "s9-cgi",
            Self::Mock => "mock",
        }
    }
}

/// Per-action capability map: an action is supported iff it has a route.
///
/// Serialized for the UI as `{start, stop, reboot, light_on, light_off}`
/// booleans plus `routes: {action: route}` and `unconfirmed: [action]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    routes: BTreeMap<MinerAction, Route>,
    /// Routes not yet confirmed on real hardware: dry run only.
    unconfirmed: BTreeSet<MinerAction>,
}

impl Capabilities {
    pub fn none() -> Self {
        Self::default()
    }

    /// Same route for every supported action.
    pub fn flags(start: bool, stop: bool, reboot: bool, route: Route) -> Self {
        let mut c = Self::none();
        for (a, on) in [(MinerAction::Start, start), (MinerAction::Stop, stop), (MinerAction::Reboot, reboot)] {
            if on {
                c.routes.insert(a, route);
            }
        }
        c
    }

    pub fn with(mut self, action: MinerAction, route: Route) -> Self {
        self.routes.insert(action, route);
        self.unconfirmed.remove(&action);
        self
    }

    /// Like [`Self::with`], but the route is not hardware-confirmed yet, so
    /// the job runner only dry-runs it.
    pub fn with_unconfirmed(mut self, action: MinerAction, route: Route) -> Self {
        self.routes.insert(action, route);
        self.unconfirmed.insert(action);
        self
    }

    pub fn confirmed(&self, action: MinerAction) -> bool {
        self.supports(action) && !self.unconfirmed.contains(&action)
    }

    pub fn supports(&self, action: MinerAction) -> bool {
        self.routes.contains_key(&action)
    }

    pub fn route(&self, action: MinerAction) -> Option<Route> {
        self.routes.get(&action).copied()
    }

    pub fn routes(&self) -> &BTreeMap<MinerAction, Route> {
        &self.routes
    }

    /// (start, stop, reboot) support flags.
    pub fn as_flags(&self) -> (bool, bool, bool) {
        (self.supports(MinerAction::Start), self.supports(MinerAction::Stop), self.supports(MinerAction::Reboot))
    }

    /// Per action: `native` when it supports it, else `fallback`.
    pub fn prefer(native: &Self, fallback: &Self) -> Self {
        let mut c = fallback.clone();
        for (a, r) in &native.routes {
            c.routes.insert(*a, *r);
            if native.unconfirmed.contains(a) {
                c.unconfirmed.insert(*a);
            } else {
                c.unconfirmed.remove(a);
            }
        }
        c
    }
}

impl Serialize for Capabilities {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut m = ser.serialize_map(Some(MinerAction::ALL.len() + 2))?;
        for a in MinerAction::ALL {
            m.serialize_entry(a.name(), &self.supports(a))?;
        }
        m.serialize_entry("routes", &self.routes)?;
        m.serialize_entry("unconfirmed", &self.unconfirmed)?;
        m.end()
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
    /// Fault-light read-back: `Ok(None)` = the firmware does not report it.
    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        Box::pin(async { Ok(None) })
    }
    /// Every path considered for `action` and why it was or was not
    /// usable, for "not supported, paths tried" messages.
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        vec![format!("{}: no route for {}", self.kind().label(), action.name())]
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
        NativeFlags::of(self.miner.as_ref()).caps()
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Start => self.miner.resume(None).await,
                MinerAction::Stop => self.miner.pause(None).await,
                MinerAction::Reboot => self.miner.restart().await,
                MinerAction::LightOn => self.miner.set_fault_light(true).await,
                MinerAction::LightOff => self.miner.set_fault_light(false).await,
            }
        })
    }
    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        Box::pin(async move { Ok(self.miner.get_light_flashing().await) })
    }
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let i = self.miner.get_device_info();
        vec![format!("{} on {} {} ({})", NativeFlags::of(self.miner.as_ref()).explain(action), i.make, i.model, i.firmware)]
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
        vec![Box::new(super::safety::asic::AsicRsSettings::new(self.miner.clone()).with_profile(&self.profile()))]
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

// ---------------- composite: native first, override for the gaps ----------------

/// asic-rs for every action it supports; the override for the rest.
pub struct CompositeDriver {
    ip: IpAddr,
    native: Box<dyn MinerDriver>,
    fallback: Box<dyn MinerDriver>,
}

impl CompositeDriver {
    pub fn new(ip: IpAddr, native: Box<dyn MinerDriver>, fallback: Box<dyn MinerDriver>) -> Self {
        Self { ip, native, fallback }
    }
}

impl MinerDriver for CompositeDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities::prefer(&self.native.caps(), &self.fallback.caps())
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        if self.native.caps().supports(action) {
            self.native.execute(action)
        } else {
            self.fallback.execute(action)
        }
    }
    /// Read the light back on the same path that set it.
    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        if self.native.caps().supports(MinerAction::LightOn) {
            self.native.light_state()
        } else {
            self.fallback.light_state()
        }
    }
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let mut v = self.native.paths_tried(action);
        v.extend(self.fallback.paths_tried(action));
        v
    }
    /// The override's status read is the one verification was built on.
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async move {
            match self.fallback.status().await {
                Ok(s) => Ok(s),
                Err(_) => self.native.status().await,
            }
        })
    }
    // Identity, health and cache kind come from the override (one cheap
    // read with MAC, version, temp and hashrate), so the cache rebuilds
    // this same composite.
    fn profile(&self) -> DeviceProfile {
        self.fallback.profile()
    }
    fn kind(&self) -> DriverKind {
        self.fallback.kind()
    }
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        self.fallback.observe()
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        self.fallback.health()
    }
    /// asic-rs backends first, then the override's.
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        let mut v = self.native.settings_backends();
        v.extend(self.fallback.settings_backends());
        v
    }
}

/// asic-rs 0.8.6 native Bitaxe miner for AxeOS >= 2.14 (`v2_14_0`
/// backend: native pause/resume, no restart). Two GETs, no full scan.
async fn native_bitaxe(ip: IpAddr, timeout: Duration) -> Option<Box<dyn MinerDriver>> {
    use asic_rs::core::traits::entry::FirmwareEntry;
    let fw = asic_rs::bitaxe::firmware::BitaxeFirmware {};
    let m = tokio::time::timeout(timeout, fw.build_miner(ip, None)).await.ok()?.ok()?;
    Some(Box::new(AsicRsDriver::new(ip, m)))
}

/// Wrap a Bitaxe override with the asic-rs native driver when the
/// firmware has one; otherwise the override alone.
pub async fn bitaxe_with_native(ip: IpAddr, d: BitaxeDriver, timeout: Duration) -> Box<dyn MinerDriver> {
    if d.native_backend_available() {
        if let Some(native) = native_bitaxe(ip, timeout).await {
            return Box::new(CompositeDriver::new(ip, native, Box::new(d)));
        }
    }
    Box::new(d)
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
                    let d = BitaxeDriver::from_parts(ip, format!("http://{ip}"), client, version, false)
                        .with_asic_model(c.profile.chip.clone());
                    // Native first: reuse discovery's asic-rs miner when it has one.
                    match &c.miner {
                        Some(m) if d.native_backend_available() => {
                            let native = Box::new(AsicRsDriver::from_shared(ip, m.clone()));
                            Some(Box::new(CompositeDriver::new(ip, native, Box::new(d))) as Box<dyn MinerDriver>)
                        }
                        _ => Some(bitaxe_with_native(ip, d, self.timeout).await),
                    }
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
                return Ok(Some(bitaxe_with_native(ip, d, self.timeout).await));
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
                        // Reuse the miner asic-rs already identified.
                        let native = Box::new(AsicRsDriver::new(ip, m));
                        return Ok(Some(Box::new(CompositeDriver::new(ip, native, Box::new(d))) as Box<dyn MinerDriver>));
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
        self.caps.clone()
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
