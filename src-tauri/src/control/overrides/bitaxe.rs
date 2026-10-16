//! Bitaxe / AxeOS HTTP override.
//!
//! asic-rs 0.4.2 reports `supports_restart = false` for Bitaxe backends,
//! but live hardware accepts `POST /api/system/restart` (200). Pause and
//! resume arrived later as `POST /api/system/pause` and `/api/system/resume`
//! and need AxeOS >= [`PAUSE_RESUME_MIN`] (2.14). Fleet devices still on
//! 2.10 therefore advertise reboot only.

use std::net::IpAddr;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use semver::Version;
use serde_json::Value;

use crate::control::driver::{Capabilities, MinerAction, MinerDriver, MinerStatus};

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
        }
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
        Ok(Some(Self::from_parts(ip, base, client, version, pause_probe_ok)))
    }

    fn pause_resume_supported(&self) -> bool {
        self.pause_probe_ok
            || self
                .version
                .as_ref()
                .map(|v| *v >= PAUSE_RESUME_MIN)
                .unwrap_or(false)
    }

    async fn post(&self, path: &str) -> anyhow::Result<bool> {
        let url = format!("{}{path}", self.base);
        let resp = self.client.post(url).send().await?;
        let status = resp.status();
        // Reboot often closes the socket mid-response; treat that upstream.
        Ok(status.is_success() || status.as_u16() == 204)
    }
}

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
        Box::pin(async move {
            let url = format!("{}/api/system/info", self.base);
            let body: Value = self.client.get(url).send().await?.error_for_status()?.json().await?;
            Ok(parse_axeos_status(&body))
        })
    }
}

/// Uptime and pause state from AxeOS `/api/system/info`.
pub fn parse_axeos_status(body: &Value) -> MinerStatus {
    MinerStatus {
        uptime_s: body.get("uptimeSeconds").and_then(|v| v.as_u64()),
        paused: body.get("miningPaused").and_then(|v| v.as_bool()),
    }
}

fn looks_like_axeos(body: &Value) -> bool {
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

fn parse_loose_version(s: &str) -> Option<Version> {
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
    Capabilities {
        start: pause,
        stop: pause,
        reboot: true, // live: POST /api/system/restart returns 200 even on 2.10
    }
}

/// Convenience: build caps for a version string.
pub fn caps_for_version_str(s: &str) -> Capabilities {
    axeos_caps(parse_loose_version(s).as_ref(), false)
}
