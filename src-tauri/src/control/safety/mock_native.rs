//! In-process stand-ins for asic-rs firmware backends, for settings tests.
//!
//! asic-rs hardcodes real ports, so it cannot reach a local mock. Each
//! [`NativeFirmware`] carries the `supports_set_power_limit()` /
//! `supports_set_tuning_percent()` values of that asic-rs 0.8.6 backend and
//! is driven through the real [`AsicRsSettings`] backend (same accept /
//! read / write code as the app) via the [`NativeTuning`] seam. Every call
//! is logged so tests can prove what was (not) sent.
//!
//! A mock can be made unhealthy below a power level (hashrate drops to 0)
//! or told to ignore writes (read-back mismatch).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use asic_rs_core::config::fan::FanConfig;
use futures::future::BoxFuture;

use super::asic::{AsicRsSettings, NativeSettingsFlags, NativeTuning};
use super::profile::DeviceProfile;
use super::settings::{HealthSample, SettingsDriver};
use crate::control::cache::{CachedDevice, DriverKind};
use crate::control::driver::{Capabilities, CompositeDriver, DriverFactory, MinerAction, MinerDriver, MinerStatus};

/// asic-rs 0.8.6 backends by settings flags (read from each backend's
/// `supports_set_power_limit()` / `supports_set_tuning_percent()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeFirmware {
    /// `braiins/v25_07`: power limit yes, tuning percent no.
    Braiins2507,
    /// `vnish/v1_3_0`: power limit yes, tuning percent yes.
    Vnish13,
    /// `whatsminer/v3`: power limit yes.
    WhatsminerV3,
    /// `avalonminer/avalon_a`: power limit yes.
    AvalonA,
    /// `proto/v1`: power limit yes.
    ProtoV1,
    /// `sealminer/v2025`: power limit yes.
    Sealminer2025,
    /// `auradine/v1`: power limit yes.
    AuradineV1,
    /// `antminer/v2020` (stock): power limit no (tuning_config is modes only).
    AntminerStock,
    /// `epic/v1`: power limit no.
    EpicV1,
    /// `bitaxe/v2_14_0`: no settings setters at all.
    BitaxeV214,
}

impl NativeFirmware {
    pub const ALL: [NativeFirmware; 10] = [
        Self::Braiins2507,
        Self::Vnish13,
        Self::WhatsminerV3,
        Self::AvalonA,
        Self::ProtoV1,
        Self::Sealminer2025,
        Self::AuradineV1,
        Self::AntminerStock,
        Self::EpicV1,
        Self::BitaxeV214,
    ];

    pub fn flags(self) -> NativeSettingsFlags {
        let power_limit = !matches!(self, Self::AntminerStock | Self::EpicV1 | Self::BitaxeV214);
        NativeSettingsFlags { power_limit, tuning_percent: self == Self::Vnish13, fan: false }
    }

    /// (make, model, firmware) as asic-rs reports them.
    pub fn identity(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Braiins2507 => ("AntMiner", "Antminer S19", "BraiinsOS 25.07"),
            Self::Vnish13 => ("AntMiner", "Antminer S19", "VNish 1.3"),
            Self::WhatsminerV3 => ("WhatsMiner", "M50", "Whatsminer v3"),
            Self::AvalonA => ("AvalonMiner", "A1566", "Avalon A"),
            Self::ProtoV1 => ("Proto", "Rig", "Proto v1"),
            Self::Sealminer2025 => ("SealMiner", "A2", "SealMiner v2025"),
            Self::AuradineV1 => ("Auradine", "AT1500", "Auradine v1"),
            Self::AntminerStock => ("AntMiner", "Antminer S19", "Stock v2020"),
            Self::EpicV1 => ("AntMiner", "Antminer S19", "ePIC UMC"),
            Self::BitaxeV214 => ("Bitaxe", "Gamma", "AxeOS 2.15.3"),
        }
    }
}

#[derive(Debug)]
pub struct NativeState {
    pub power_w: u32,
    pub tuning_pct: u32,
    /// Every setter call, e.g. "set_power_limit 2500".
    pub calls: Vec<String>,
    /// Hashrate drops to 0 when the power limit is below this.
    pub unhealthy_below_w: Option<u32>,
    pub ignore_writes: bool,
}

/// One mocked asic-rs miner.
pub struct MockNative {
    pub fw: NativeFirmware,
    pub state: Arc<Mutex<NativeState>>,
}

impl MockNative {
    pub fn new(fw: NativeFirmware) -> Arc<Self> {
        Arc::new(Self {
            fw,
            state: Arc::new(Mutex::new(NativeState {
                power_w: 3000,
                tuning_pct: 100,
                calls: Vec::new(),
                unhealthy_below_w: None,
                ignore_writes: false,
            })),
        })
    }
    pub fn power(&self) -> u32 {
        self.state.lock().unwrap().power_w
    }
    pub fn tuning(&self) -> u32 {
        self.state.lock().unwrap().tuning_pct
    }
    /// Setter calls (writes) received.
    pub fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
    pub fn set_unhealthy_below(&self, w: Option<u32>) {
        self.state.lock().unwrap().unhealthy_below_w = w;
    }
    pub fn set_ignore_writes(&self, v: bool) {
        self.state.lock().unwrap().ignore_writes = v;
    }
    fn health(&self) -> HealthSample {
        let s = self.state.lock().unwrap();
        let dead = s.unhealthy_below_w.is_some_and(|w| s.power_w < w);
        let hr = if dead { 0.0 } else { 100_000.0 * f64::from(s.tuning_pct) / 100.0 };
        HealthSample { reachable: true, hashrate_ghs: Some(hr), temp_c: Some(65.0) }
    }
}

/// [`NativeTuning`] over the mock. Setters refuse exactly like asic-rs's
/// trait defaults when the backend does not support them.
struct Seam(Arc<MockNative>);

impl NativeTuning for Seam {
    fn flags(&self) -> NativeSettingsFlags {
        self.0.fw.flags()
    }
    fn power_target(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move { self.flags().power_limit.then(|| self.0.power()) })
    }
    fn set_power_limit(&self, watts: u32) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            anyhow::ensure!(self.flags().power_limit, "Setting power limit is not supported on this platform");
            let mut s = self.0.state.lock().unwrap();
            s.calls.push(format!("set_power_limit {watts}"));
            if !s.ignore_writes {
                s.power_w = watts;
            }
            Ok(true)
        })
    }
    fn tuning_percent(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move { self.flags().tuning_percent.then(|| self.0.tuning()) })
    }
    fn set_tuning_percent(&self, pct: u8) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            anyhow::ensure!(self.flags().tuning_percent, "Setting throttle is not supported on this platform");
            let mut s = self.0.state.lock().unwrap();
            s.calls.push(format!("set_tuning_percent {pct}"));
            if !s.ignore_writes {
                s.tuning_pct = u32::from(pct);
            }
            Ok(true)
        })
    }
    fn fan(&self) -> BoxFuture<'_, Option<FanConfig>> {
        Box::pin(async { None })
    }
    fn set_fan(&self, _f: FanConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("fan config not modelled in this mock") })
    }
}

/// Driver for a [`MockNative`]: asic-rs settings backend only.
pub struct MockNativeDriver {
    ip: IpAddr,
    m: Arc<MockNative>,
}

impl MockNativeDriver {
    pub fn new(ip: IpAddr, m: Arc<MockNative>) -> Self {
        Self { ip, m }
    }
}

impl MinerDriver for MockNativeDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities::none()
    }
    fn execute(&self, _a: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("actions are not modelled in the settings mock") })
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async { Ok(MinerStatus { uptime_s: Some(3600), paused: Some(false) }) })
    }
    fn profile(&self) -> DeviceProfile {
        let (make, model, fw) = self.m.fw.identity();
        DeviceProfile::classify(make, model, fw, None)
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        let h = self.m.health();
        Box::pin(async move { h })
    }
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(AsicRsSettings::from_native(Arc::new(Seam(self.m.clone()))))]
    }
}

/// Factory over [`MockNative`] miners. IPs it does not own go to `other`;
/// AxeOS drivers from there are composed with a native Bitaxe stand-in
/// (asic-rs first, AxeOS override second), as the app does.
pub struct MockNativeFactory {
    pub miners: HashMap<IpAddr, Arc<MockNative>>,
    pub other: Option<Arc<dyn DriverFactory>>,
}

impl MockNativeFactory {
    fn wrap(&self, ip: IpAddr, d: Box<dyn MinerDriver>) -> Box<dyn MinerDriver> {
        if d.kind() == DriverKind::AxeOs {
            let native = MockNativeDriver::new(ip, MockNative::new(NativeFirmware::BitaxeV214));
            Box::new(CompositeDriver::new(ip, Box::new(native), d))
        } else {
            d
        }
    }
}

impl DriverFactory for MockNativeFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            if let Some(m) = self.miners.get(&ip) {
                return Ok(Some(Box::new(MockNativeDriver::new(ip, m.clone())) as Box<dyn MinerDriver>));
            }
            match &self.other {
                Some(o) => Ok(o.connect(ip).await?.map(|d| self.wrap(ip, d))),
                None => Ok(None),
            }
        })
    }
    fn build<'a>(&'a self, ip: IpAddr, _c: &'a CachedDevice) -> BoxFuture<'a, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        self.connect(ip)
    }
}
