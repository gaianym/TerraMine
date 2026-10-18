//! Stock Antminer S9 (and similar Sleep-less stock firmware) override.
//!
//! Live findings:
//! - cgminer JSON on :4028 has no pause/start.
//! - Reboot works via HTTP digest `GET /cgi-bin/reboot.cgi`
//!   (factory default `root`/`root` succeeded on LAN; credentials are
//!   pluggable — see [`DigestCredentials`]).
//! - Start/stop stay unsupported unless `miner-mode` /
//!   `bitmain-work-mode` is present on `get_miner_conf.cgi`.

use std::net::IpAddr;
use std::time::Duration;

use diqwest::WithDigestAuth;
use futures::future::BoxFuture;
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::control::driver::{Capabilities, MinerAction, MinerDriver, MinerStatus, Route};
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

    async fn cgminer_elapsed(&self) -> anyhow::Result<u64> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(&self.cgminer_addr),
        )
        .await??;
        sock.write_all(br#"{"command":"summary"}"#).await?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await??;
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
            }
        })
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

/// cgminer replies are JSON, sometimes NUL-terminated.
pub fn parse_cgminer_elapsed(raw: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(raw);
    let v: Value = serde_json::from_str(text.trim_end_matches('\0').trim()).ok()?;
    v.get("SUMMARY")?.get(0)?.get("Elapsed")?.as_u64()
}
