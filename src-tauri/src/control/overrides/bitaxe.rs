//! Bitaxe / AxeOS HTTP override.
//!
//! asic-rs (0.8.6, `v2_14_0` backend) reports `supports_restart = false`
//! for Bitaxe, but live hardware accepts `POST /api/system/restart` (200),
//! so reboot stays on this override. Pause/resume (`POST /api/system/pause`
//! and `/resume`, AxeOS >= [`PAUSE_RESUME_MIN`] = 2.14) are sent natively
//! through asic-rs when it has a backend for the firmware (fleet devices
//! are on 2.15.3); this override covers them only as a fallback. Devices
//! below 2.14 advertise reboot only.

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
            }
        })
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
        vec![Box::new(AxeOsSettings::new(self.base.clone(), self.client.clone()))]
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
    Capabilities::flags(pause, pause, true, Route::Axeos)
}

/// Convenience: build caps for a version string.
pub fn caps_for_version_str(s: &str) -> Capabilities {
    axeos_caps(parse_loose_version(s).as_ref(), false)
}
