//! Stock Antminer S9 (and similar Sleep-less stock firmware) override.
//!
//! Live findings:
//! - cgminer JSON on :4028 has no pause/start.
//! - Reboot works via HTTP digest `GET /cgi-bin/reboot.cgi`
//!   (factory default `root`/`root` succeeded on LAN; credentials are
//!   pluggable — see [`DigestCredentials`]).
//! - Start/stop stay unsupported unless `miner-mode` /
//!   `bitmain-work-mode` is present on `get_miner_conf.cgi`.
//! - Fault light: the stock Antminer CGI documented by asic-rs (its
//!   `v2020` backend): `POST /cgi-bin/blink.cgi {"blink":"true"|"false"}`
//!   and `GET /cgi-bin/get_blink_status.cgi` → `{"blink": true|false}`.
//!   Not confirmed on the 2018 S9 build yet, so it is dry-run only.
//! - Fan / frequency / temperature: no safe partial write. The 2018 build's
//!   `set_miner_conf.cgi` takes form fields and rewrites the whole miner
//!   config (pools included); asic-rs only documents the JSON
//!   read-modify-write of later (2020+) stock firmware.

use std::net::IpAddr;
use std::time::Duration;

use diqwest::WithDigestAuth;
use futures::future::BoxFuture;
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::control::cache::DriverKind;
use crate::control::driver::{Capabilities, Identity, MinerAction, MinerDriver, MinerStatus, Observation, Route};
use crate::control::safety::settings::{HealthSample, NoSettingsPath, SettingsDriver};

/// Why the stock S9 has no settings write path (shown in "paths tried").
pub const S9_NO_SETTINGS: &str = "stock S9 firmware: set_miner_conf.cgi replaces the whole miner config (pools included) from form fields; no documented partial fan / frequency / temperature route";
use crate::control::safety::DeviceProfile;
use crate::control::overrides::auth::DigestCredentials;

#[derive(Debug, Clone)]
pub struct S9StockDriver {
    ip: IpAddr,
    pub(crate) base: String,
    client: Client,
    creds: DigestCredentials,
    has_miner_mode: bool,
    /// cgminer API address used for uptime (`summary` → `Elapsed`).
    pub(crate) cgminer_addr: String,
}

impl S9StockDriver {
    pub fn from_parts(
        ip: IpAddr,
        base: String,
        client: Client,
        creds: DigestCredentials,
        has_miner_mode: bool,
    ) -> Self {
        Self {
            ip,
            base,
            client,
            creds,
            has_miner_mode,
            cgminer_addr: format!("{ip}:4028"),
        }
    }

    pub fn with_cgminer_addr(mut self, addr: String) -> Self {
        self.cgminer_addr = addr;
        self
    }

    async fn cgminer_summary(&self) -> anyhow::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(&self.cgminer_addr),
        )
        .await??;
        sock.write_all(br#"{"command":"summary"}"#).await?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await??;
        Ok(buf)
    }

    async fn cgminer_elapsed(&self) -> anyhow::Result<u64> {
        let buf = self.cgminer_summary().await?;
        parse_cgminer_elapsed(&buf).ok_or_else(|| anyhow::anyhow!("no Elapsed in cgminer summary"))
    }

    pub async fn probe(
        ip: IpAddr,
        creds: DigestCredentials,
        timeout: Duration,
    ) -> anyhow::Result<Option<Self>> {
        let base = format!("http://{ip}");
        Self::probe_at(ip, base, creds, timeout).await
    }

    pub async fn probe_at(
        ip: IpAddr,
        base: String,
        creds: DigestCredentials,
        timeout: Duration,
    ) -> anyhow::Result<Option<Self>> {
        let client = Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()?;
        // Prefer get_miner_conf to learn about miner-mode; fall back to a
        // cheap digest challenge against reboot.cgi (without rebooting —
        // we only check that digest auth is accepted via a 401→retry
        // that we abort by using get_system_info.cgi first).
        let conf_url = format!("{base}/cgi-bin/get_miner_conf.cgi");
        let conf = match client
            .get(&conf_url)
            .send_digest_auth((creds.username.as_str(), creds.password.as_str()))
            .await
        {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            Ok(r) if r.status() == StatusCode::UNAUTHORIZED => return Ok(None),
            _ => None,
        };

        let has_miner_mode = conf
            .as_ref()
            .map(|c| c.get("miner-mode").is_some() || c.get("bitmain-work-mode").is_some())
            .unwrap_or(false);

        // Confirm this looks like stock Antminer CGI: either we got conf,
        // or reboot.cgi answers with digest (401 without creds is enough
        // of a fingerprint when conf is missing).
        if conf.is_none() {
            let reboot = format!("{base}/cgi-bin/reboot.cgi");
            match client.get(&reboot).send().await {
                Ok(r) if r.status() == StatusCode::UNAUTHORIZED => {
                    // digest-protected → stock CGI present
                }
                Ok(r) if r.status().is_success() => {
                    // unusual (no auth) but treat as present
                }
                _ => return Ok(None),
            }
        }

        Ok(Some(Self::from_parts(ip, base, client, creds, has_miner_mode)))
    }

    pub fn caps_for(has_miner_mode: bool) -> Capabilities {
        Capabilities::flags(has_miner_mode, has_miner_mode, true, Route::S9Cgi)
            .with_unconfirmed(MinerAction::LightOn, Route::S9Cgi)
            .with_unconfirmed(MinerAction::LightOff, Route::S9Cgi)
    }

    async fn digest_get(&self, path: &str) -> anyhow::Result<(StatusCode, Option<Value>)> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .client
            .get(url)
            .send_digest_auth((self.creds.username.as_str(), self.creds.password.as_str()))
            .await?;
        let status = resp.status();
        // Reboot CGI often returns empty body / connection drop.
        let body = resp.json::<Value>().await.ok();
        Ok((status, body))
    }

    async fn digest_post_json(&self, path: &str, body: Value) -> anyhow::Result<bool> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .client
            .post(url)
            .json(&body)
            .send_digest_auth((self.creds.username.as_str(), self.creds.password.as_str()))
            .await?;
        Ok(resp.status().is_success())
    }
}

impl MinerDriver for S9StockDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }

    fn profile(&self) -> DeviceProfile {
        DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::S9Stock
    }

    /// No settings writes: see [`S9_NO_SETTINGS`].
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(NoSettingsPath { id: "s9-cgi", reason: S9_NO_SETTINGS })]
    }

    /// `get_system_info.cgi` → MAC + firmware build (one digest read).
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        Box::pin(async move {
            let (st, body) = self.digest_get("/cgi-bin/get_system_info.cgi").await?;
            anyhow::ensure!(st.is_success(), "get_system_info.cgi: {st}");
            let body = body.unwrap_or_default();
            Ok(Some(Observation { status: None, identity: parse_s9_identity(&body) }))
        })
    }

    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            match self.cgminer_summary().await {
                Ok(raw) => HealthSample { reachable: true, hashrate_ghs: parse_cgminer_ghs(&raw), temp_c: None },
                Err(_) => HealthSample::unreachable(),
            }
        })
    }

    fn caps(&self) -> Capabilities {
        Self::caps_for(self.has_miner_mode)
    }

    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Reboot => {
                    let (status, _) = self.digest_get("/cgi-bin/reboot.cgi").await?;
                    if status == StatusCode::UNAUTHORIZED {
                        anyhow::bail!("digest auth rejected for reboot.cgi");
                    }
                    // Success, or connection dropped mid-reboot.
                    Ok(status.is_success()
                        || status.is_server_error()
                        || status == StatusCode::REQUEST_TIMEOUT)
                }
                MinerAction::Stop if self.has_miner_mode => {
                    self.digest_post_json(
                        "/cgi-bin/set_miner_conf.cgi",
                        serde_json::json!({"miner-mode": "1"}), // Sleep
                    )
                    .await
                }
                MinerAction::Start if self.has_miner_mode => {
                    self.digest_post_json(
                        "/cgi-bin/set_miner_conf.cgi",
                        serde_json::json!({"miner-mode": "0"}), // Normal
                    )
                    .await
                }
                MinerAction::Start | MinerAction::Stop => Ok(false),
                MinerAction::LightOn | MinerAction::LightOff => {
                    let on = action == MinerAction::LightOn;
                    self.digest_post_json("/cgi-bin/blink.cgi", serde_json::json!({ "blink": on.to_string() })).await
                }
            }
        })
    }

    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        Box::pin(async move {
            let (st, body) = self.digest_get("/cgi-bin/get_blink_status.cgi").await?;
            anyhow::ensure!(st.is_success(), "get_blink_status.cgi: {st}");
            Ok(body.as_ref().and_then(parse_blink_status))
        })
    }

    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        vec![match action {
            MinerAction::LightOn | MinerAction::LightOff => {
                "s9-cgi: POST /cgi-bin/blink.cgi (not yet confirmed on stock S9 firmware)".to_string()
            }
            MinerAction::Start | MinerAction::Stop if !self.has_miner_mode => {
                "s9-cgi: no miner-mode on this firmware (no sleep API)".to_string()
            }
            a => format!("s9-cgi: {} available", a.name()),
        }]
    }

    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async move {
            let uptime_s = Some(self.cgminer_elapsed().await?);
            let paused = if self.has_miner_mode {
                let (_, conf) = self.digest_get("/cgi-bin/get_miner_conf.cgi").await?;
                conf.and_then(|c| c.get("miner-mode").and_then(|m| m.as_str().map(|m| m == "1")))
            } else {
                None
            };
            Ok(MinerStatus { uptime_s, paused })
        })
    }
}

/// `{"blink": true}` (bool or "true"/"false" string).
pub fn parse_blink_status(body: &Value) -> Option<bool> {
    let b = body.get("blink")?;
    b.as_bool().or_else(|| match b.as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    })
}

/// MAC and firmware build from stock `get_system_info.cgi`.
pub fn parse_s9_identity(body: &Value) -> Identity {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Identity { mac: s("macaddr"), firmware: s("system_filesystem_version") }
}

/// `GHS 5s` from a cgminer summary (string or number).
pub fn parse_cgminer_ghs(raw: &[u8]) -> Option<f64> {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let v: Value = serde_json::from_slice(&raw[..end]).ok()?;
    let g = v.get("SUMMARY")?.get(0)?.get("GHS 5s")?;
    g.as_f64().or_else(|| g.as_str()?.trim().parse().ok())
}

/// cgminer replies are JSON, sometimes NUL-terminated.
pub fn parse_cgminer_elapsed(raw: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(raw);
    let v: Value = serde_json::from_str(text.trim_end_matches('\0').trim()).ok()?;
    v.get("SUMMARY")?.get(0)?.get("Elapsed")?.as_u64()
}
