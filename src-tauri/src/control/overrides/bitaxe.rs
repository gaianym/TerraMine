//! Bitaxe / AxeOS HTTP override.
//!
//! asic-rs (0.8.6, `v2_14_0` backend) reports `supports_restart = false`
//! for Bitaxe, but live hardware accepts `POST /api/system/restart` (200),
//! so reboot stays on this override. Pause/resume (`POST /api/system/pause`
//! and `/resume`, AxeOS >= [`PAUSE_RESUME_MIN`] = 2.14) are sent natively
//! through asic-rs when it has a backend for the firmware (fleet devices
//! are on 2.15.3); this override covers them only as a fallback. Devices
//! below 2.14 advertise reboot only.
//!
//! Fault light: asic-rs reports `supports_set_fault_light = false` for
//! every Bitaxe backend, and AxeOS has no LED endpoint. The documented
//! stand-in is `POST /api/system/identify` (ESP-Miner >= 2.12, see its
//! openapi.yaml): the screen says "Hi!" for 30 s. It is a toggle with no
//! readable state, so TerraMine only offers it as "light on" (never as
//! "off": a second call would toggle, and it expires by itself), cannot
//! read it back. It is hardware-confirmed on AxeOS 2.15.x (one Bitaxe
//! Gamma on v2.15.3, Oct 17 2026: HTTP 2xx, still hashing, no pause, no
//! reboot; see `tests/hw_bitaxe_identify.rs`) and stays dry-run only on
//! every other version until checked there.

use std::net::IpAddr;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use semver::Version;
use serde_json::Value;

use crate::control::cache::DriverKind;
use crate::control::driver::{Capabilities, Identity, MinerAction, MinerDriver, MinerStatus, Observation, Route};
use crate::control::safety::axeos::{health_from_info, AxeOsSettings};
use crate::control::safety::settings::{HealthSample, SettingsDriver};
use crate::control::safety::DeviceProfile;

/// Minimum AxeOS version that exposes pause/resume endpoints.
pub const PAUSE_RESUME_MIN: Version = Version::new(2, 14, 0);
/// First ESP-Miner release with `POST /api/system/identify`.
pub const IDENTIFY_MIN: Version = Version::new(2, 12, 0);
/// AxeOS `major.minor` lines where identify is confirmed on real hardware.
pub const IDENTIFY_HW_CONFIRMED: [(u64, u64); 1] = [(2, 15)];

#[derive(Debug, Clone)]
pub struct BitaxeDriver {
    ip: IpAddr,
    /// Overridable for tests (e.g. `http://127.0.0.1:PORT`).
    pub(crate) base: String,
    client: Client,
    version: Option<Version>,
    /// True when a probe of `/api/system/pause` returned success even if
    /// the version string was missing or below the floor.
    pause_probe_ok: bool,
    /// `ASICModel` from system info (limits are keyed on the chip).
    asic_model: Option<String>,
}

impl BitaxeDriver {
    pub fn from_parts(
        ip: IpAddr,
        base: String,
        client: Client,
        version: Option<Version>,
        pause_probe_ok: bool,
    ) -> Self {
        Self {
            ip,
            base,
            client,
            version,
            pause_probe_ok,
            asic_model: None,
        }
    }

    pub fn with_asic_model(mut self, chip: Option<String>) -> Self {
        self.asic_model = chip;
        self
    }

    /// Probe `GET {base}/api/system/info` and optionally pause support.
    pub async fn probe(ip: IpAddr, timeout: Duration) -> anyhow::Result<Option<Self>> {
        let base = format!("http://{ip}");
        Self::probe_at(ip, base, timeout).await
    }

    pub async fn probe_at(ip: IpAddr, base: String, timeout: Duration) -> anyhow::Result<Option<Self>> {
        let client = Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()?;
        let info_url = format!("{base}/api/system/info");
        let resp = match client.get(&info_url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return Ok(None),
        };
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if !looks_like_axeos(&body) {
            return Ok(None);
        }
        let version = parse_axeos_version(&body);
        // Soft probe: `miningPaused` in /info is only present once pause
        // support shipped; avoids POSTing pause against a live miner.
        let pause_probe_ok = body.get("miningPaused").is_some();
        let chip = body.get("ASICModel").and_then(|v| v.as_str()).map(str::to_string);
        Ok(Some(Self::from_parts(ip, base, client, version, pause_probe_ok).with_asic_model(chip)))
    }

    fn pause_resume_supported(&self) -> bool {
        self.pause_probe_ok
            || self
                .version
                .as_ref()
                .map(|v| *v >= PAUSE_RESUME_MIN)
                .unwrap_or(false)
    }

    async fn info(&self) -> anyhow::Result<Value> {
        let url = format!("{}/api/system/info", self.base);
        Ok(self.client.get(url).send().await?.error_for_status()?.json().await?)
    }

    async fn post(&self, path: &str) -> anyhow::Result<bool> {
        let url = format!("{}{path}", self.base);
        let resp = self.client.post(url).send().await?;
        let status = resp.status();
        // Reboot often closes the socket mid-response; treat that upstream.
        Ok(status.is_success() || status.as_u16() == 204)
    }
}

impl BitaxeDriver {
    /// asic-rs 0.8.6 has a native backend (`v2_14_0`) for this firmware:
    /// it parses `version` strictly, so require a clean >= 2.14 version.
    pub fn native_backend_available(&self) -> bool {
        self.version.as_ref().is_some_and(|v| *v >= ASIC_RS_NATIVE_MIN)
    }

    fn identify_supported(&self) -> bool {
        identify_supported(self.version.as_ref())
    }
}

/// First AxeOS version asic-rs 0.8.6 drives natively (pause/resume).
pub const ASIC_RS_NATIVE_MIN: Version = Version::new(2, 14, 0);

impl MinerDriver for BitaxeDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }

    fn caps(&self) -> Capabilities {
        axeos_caps(self.version.as_ref(), self.pause_probe_ok)
    }

    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Reboot => self.post("/api/system/restart").await,
                MinerAction::Stop if self.pause_resume_supported() => {
                    self.post("/api/system/pause").await
                }
                MinerAction::Start if self.pause_resume_supported() => {
                    self.post("/api/system/resume").await
                }
                MinerAction::Start | MinerAction::Stop => Ok(false),
                MinerAction::LightOn if self.identify_supported() => self.post("/api/system/identify").await,
                MinerAction::LightOn | MinerAction::LightOff => Ok(false),
            }
        })
    }

    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let msg = match action {
            MinerAction::LightOn if self.identify_supported() => format!(
                "axeos-http: POST /api/system/identify (30 s on-screen \"Hi!\", no read-back; {})",
                if identify_hw_confirmed(self.version.as_ref()) {
                    "hardware-confirmed on this AxeOS line"
                } else {
                    "not yet confirmed on hardware"
                }
            ),
            MinerAction::LightOn => format!(
                "axeos-http: no identify endpoint before AxeOS {IDENTIFY_MIN} (device reports {})",
                self.version.as_ref().map_or("no version".to_string(), |v| v.to_string())
            ),
            MinerAction::LightOff => {
                "axeos-http: identify expires after 30 s and has no off command (a second call toggles)".to_string()
            }
            MinerAction::Start | MinerAction::Stop if !self.pause_resume_supported() => {
                format!("axeos-http: pause/resume needs AxeOS >= {PAUSE_RESUME_MIN}")
            }
            _ => format!("axeos-http: {} available", action.name()),
        };
        vec![msg]
    }

    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async move { Ok(parse_axeos_status(&self.info().await?)) })
    }

    fn kind(&self) -> DriverKind {
        DriverKind::AxeOs
    }

    /// Status and identity come from the same `/api/system/info` read.
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        Box::pin(async move {
            let body = self.info().await?;
            Ok(Some(Observation { status: Some(parse_axeos_status(&body)), identity: parse_axeos_identity(&body) }))
        })
    }

    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            match self.info().await {
                Ok(v) => health_from_info(&v),
                Err(_) => HealthSample::unreachable(),
            }
        })
    }

    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(AxeOsSettings::new(self.base.clone(), self.client.clone()).with_version(self.version.clone()))]
    }

    fn profile(&self) -> DeviceProfile {
        let fw = match &self.version {
            Some(v) => format!("AxeOS {v}"),
            None => "AxeOS".to_string(),
        };
        DeviceProfile::classify("Bitaxe", "Bitaxe", &fw, self.asic_model.as_deref())
    }
}

/// Uptime and pause state from AxeOS `/api/system/info`.
pub fn parse_axeos_status(body: &Value) -> MinerStatus {
    MinerStatus {
        uptime_s: body.get("uptimeSeconds").and_then(|v| v.as_u64()),
        paused: body.get("miningPaused").and_then(|v| v.as_bool()),
    }
}

/// MAC (`macAddr`) and firmware version from AxeOS system info.
pub fn parse_axeos_identity(body: &Value) -> Identity {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Identity { mac: s("macAddr"), firmware: s("version").or_else(|| s("axeOSVersion")) }
}

pub fn looks_like_axeos(body: &Value) -> bool {
    body.get("ASICModel").is_some()
        || body.get("axeOSVersion").is_some()
        || (body.get("version").is_some() && body.get("boardVersion").is_some())
}

/// Parse a `version` (preferred) or `axeOSVersion` string from system info.
pub fn parse_axeos_version(body: &Value) -> Option<Version> {
    for key in ["version", "axeOSVersion"] {
        if let Some(s) = body.get(key).and_then(|v| v.as_str()) {
            if let Some(v) = parse_loose_version(s) {
                return Some(v);
            }
        }
    }
    None
}

pub fn parse_loose_version(s: &str) -> Option<Version> {
    let trimmed = s.trim().trim_start_matches('v');
    // Take major.minor.patch prefix; ignore trailing build metadata junk.
    let mut parts = trimmed.split(|c: char| !c.is_ascii_digit() && c != '.');
    let candidate = parts.next().unwrap_or("");
    Version::parse(candidate)
        .ok()
        .or_else(|| {
            // "2.10" -> 2.10.0
            let bits: Vec<_> = candidate.split('.').collect();
            match bits.as_slice() {
                [a, b] => Version::parse(&format!("{a}.{b}.0")).ok(),
                [a] => Version::parse(&format!("{a}.0.0")).ok(),
                _ => None,
            }
        })
}

/// Capability matrix used by the driver and by unit tests.
pub fn axeos_caps(version: Option<&Version>, pause_probe_ok: bool) -> Capabilities {
    let pause = pause_probe_ok || version.map(|v| *v >= PAUSE_RESUME_MIN).unwrap_or(false);
    // Reboot: live POST /api/system/restart returns 200 even on 2.10.
    let c = Capabilities::flags(pause, pause, true, Route::Axeos);
    if identify_hw_confirmed(version) {
        c.with(MinerAction::LightOn, Route::Axeos)
    } else if identify_supported(version) {
        c.with_unconfirmed(MinerAction::LightOn, Route::Axeos)
    } else {
        c
    }
}

/// Identify has been sent live and checked on this firmware line.
pub fn identify_hw_confirmed(version: Option<&Version>) -> bool {
    identify_supported(version) && version.is_some_and(|v| IDENTIFY_HW_CONFIRMED.contains(&(v.major, v.minor)))
}

pub fn identify_supported(version: Option<&Version>) -> bool {
    version.is_some_and(|v| *v >= IDENTIFY_MIN)
}

/// Convenience: build caps for a version string.
pub fn caps_for_version_str(s: &str) -> Capabilities {
    axeos_caps(parse_loose_version(s).as_ref(), false)
}
