//! cgminer / bmminer JSON-RPC (TCP :4028) for stock Antminer firmware,
//! and the S9 fallback driver built on it when the CGI is unusable.
//!
//! Safety rules, enforced here rather than by callers:
//! - **Class gate.** A client is only built for [`DeviceClass::AntminerStock`]
//!   (bmminer). It is never built for AxeOS, so no Bitaxe ever receives a
//!   :4028 request, and never for third-party firmware (which has its own
//!   asic-rs backend).
//! - **Command allowlist.** Reads (`summary`, `stats`, `pools`, `devs`,
//!   `version`, `privileged`) plus exactly one write, `restart` (restarts
//!   the bmminer process, not the controller). Everything else is refused
//!   locally before a socket is opened: in particular `save` (rewrites the
//!   whole miner config file), `addpool` / `removepool` / `switchpool` /
//!   `enablepool` / `disablepool` (pool changes belong to the pools PR and
//!   its safety checks), `ascset` / `pgaset` (no documented S9 options),
//!   and `quit`.
//! - **Capability check first.** `restart` is privileged; the route is only
//!   offered when `privileged` answers success. A denial is reported in
//!   "paths tried".
//! - **Not hardware-confirmed.** The `restart` route is dry-run only until
//!   it is checked on a real S9 (`Capabilities::with_unconfirmed`).

use std::net::IpAddr;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::control::cache::DriverKind;
use crate::control::driver::{Capabilities, MinerAction, MinerDriver, MinerStatus, Route};
use crate::control::safety::settings::{HealthSample, NoSettingsPath, SettingsDriver};
use crate::control::safety::{DeviceClass, DeviceProfile};

/// Read commands the client may send.
pub const READ_COMMANDS: [&str; 6] = ["summary", "stats", "pools", "devs", "version", "privileged"];
/// The only write command the client may send.
pub const WRITE_COMMANDS: [&str; 1] = ["restart"];

/// Why the S9's bmminer RPC is not a settings path (shown in "paths tried").
pub const RPC_NO_SETTINGS: &str = "cgminer-rpc: bmminer has no fan / frequency / voltage / temperature set command (only `save`, which rewrites the whole config, and is never sent)";
/// Why bmminer RPC cannot start or stop hashing (live S9 finding).
pub const RPC_NO_PAUSE: &str = "cgminer-rpc: bmminer has no pause / resume command (pool diversion is not a documented safe route; not attempted)";
/// The fault light has no RPC equivalent on bmminer.
pub const RPC_NO_LIGHT: &str = "cgminer-rpc: bmminer has no locate-light command";

/// Is `command` allowed at all (read or write)?
pub fn allowed(command: &str) -> bool {
    READ_COMMANDS.contains(&command) || WRITE_COMMANDS.contains(&command)
}

/// cgminer / bmminer JSON-RPC client. Build with [`CgminerRpc::for_class`].
#[derive(Debug, Clone)]
pub struct CgminerRpc {
    addr: String,
    timeout: Duration,
}

impl CgminerRpc {
    /// Only stock Antminer (bmminer) gets an RPC client.
    pub fn for_class(class: DeviceClass, addr: String, timeout: Duration) -> Result<Self, String> {
        match class {
            DeviceClass::AntminerStock => Ok(Self { addr, timeout }),
            c => Err(format!("cgminer-rpc: not used for {c:?} (stock Antminer bmminer only)")),
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// One command, one connection (cgminer closes after each reply).
    /// Commands outside the allowlist are refused without connecting.
    pub async fn call(&self, command: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(allowed(command), "cgminer-rpc: command `{command}` is not on the allowlist; not sent");
        let mut sock = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(&self.addr)).await??;
        let req = serde_json::json!({ "command": command }).to_string();
        sock.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        tokio::time::timeout(self.timeout, sock.read_to_end(&mut buf)).await??;
        parse_reply(&buf)
    }

    /// `privileged`: Ok(()) when write commands are allowed for us.
    pub async fn privileged(&self) -> Result<(), String> {
        match self.call("privileged").await {
            Ok(v) => match status_of(&v) {
                Some(("S", _)) | Some(("I", _)) => Ok(()),
                Some((_, msg)) => Err(format!("cgminer-rpc: privileged access denied ({msg})")),
                None => Err("cgminer-rpc: privileged reply had no STATUS".into()),
            },
            Err(e) => Err(format!("cgminer-rpc: unreachable ({e})")),
        }
    }

    /// `restart` (bmminer process). Ok(true) = accepted.
    pub async fn restart(&self) -> anyhow::Result<bool> {
        match self.call("restart").await {
            Ok(v) => Ok(!matches!(status_of(&v), Some(("E", _)) | Some(("F", _)))),
            // cgminer may close the socket while restarting.
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => Ok(true),
            Err(e) => Err(e),
        }
    }

    pub async fn health(&self) -> HealthSample {
        let Ok(summary) = self.call("summary").await else { return HealthSample::unreachable() };
        let temp_c = self.call("stats").await.ok().as_ref().and_then(max_chip_temp);
        HealthSample { reachable: true, hashrate_ghs: summary_ghs(&summary), temp_c }
    }

    pub async fn elapsed(&self) -> anyhow::Result<u64> {
        let v = self.call("summary").await?;
        summary_elapsed(&v).ok_or_else(|| anyhow::anyhow!("no Elapsed in cgminer summary"))
    }
}

/// cgminer replies are JSON, often NUL-terminated; bmminer's `stats` emits
/// `}{` between objects (invalid JSON), which is repaired here.
pub fn parse_reply(raw: &[u8]) -> anyhow::Result<Value> {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let text = String::from_utf8_lossy(&raw[..end]);
    let text = text.trim();
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(_) => Ok(serde_json::from_str(&text.replace("}{", "},{"))?),
    }
}

/// `version` reply from stock Antminer bmminer / cgminer: a `BMMiner`
/// key, or a `Type` naming an Antminer. Anything else (Braiins, Avalon,
/// Whatsminer cgminer APIs) is not this driver's.
pub fn is_stock_antminer_version(v: &Value) -> bool {
    let Some(x) = v.get("VERSION").and_then(|x| x.get(0)) else { return false };
    x.get("BMMiner").is_some() || x.get("Type").and_then(|t| t.as_str()).is_some_and(|t| t.to_ascii_lowercase().contains("antminer"))
}

/// Why the CGI was not usable, for "paths tried".
pub const CGI_UNUSABLE: &str = "digest login rejected or CGI unavailable (set the S9 web password in TerraMine)";

/// Stock Antminer S9: the CGI driver when its digest login works, else
/// the bmminer RPC fallback. `None` when neither answers.
pub async fn stock_s9_driver(
    ip: IpAddr,
    base: String,
    creds: crate::control::overrides::DigestCredentials,
    cgminer_addr: String,
    timeout: Duration,
) -> anyhow::Result<Option<Box<dyn MinerDriver>>> {
    if let Some(d) = super::S9StockDriver::probe_at(ip, base, creds, timeout).await? {
        return Ok(Some(Box::new(d.with_cgminer_addr(cgminer_addr))));
    }
    Ok(S9RpcDriver::probe(ip, cgminer_addr, timeout, CGI_UNUSABLE.into()).await?.map(|d| Box::new(d) as Box<dyn MinerDriver>))
}

/// (STATUS code, Msg) of a reply.
pub fn status_of(v: &Value) -> Option<(&str, &str)> {
    let s = v.get("STATUS")?.get(0)?;
    Some((s.get("STATUS")?.as_str()?, s.get("Msg").and_then(|m| m.as_str()).unwrap_or("")))
}

/// `GHS 5s` (string or number).
pub fn summary_ghs(v: &Value) -> Option<f64> {
    let g = v.get("SUMMARY")?.get(0)?.get("GHS 5s")?;
    g.as_f64().or_else(|| g.as_str()?.trim().parse().ok())
}

pub fn summary_elapsed(v: &Value) -> Option<u64> {
    v.get("SUMMARY")?.get(0)?.get("Elapsed")?.as_u64()
}

/// Hottest chip temperature from bmminer `stats` (`temp2_N`; board
/// sensors `tempN` as a fallback). Zero readings (empty slots) are ignored.
pub fn max_chip_temp(v: &Value) -> Option<f64> {
    let stats = v.get("STATS")?.as_array()?;
    let pick = |prefix: &str| {
        stats
            .iter()
            .filter_map(|o| o.as_object())
            .flat_map(|o| o.iter())
            .filter(|(k, _)| {
                k.strip_prefix(prefix).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit() || c == '_'))
            })
            .filter_map(|(_, v)| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
            .filter(|t| *t > 0.0)
            .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
    };
    pick("temp2_").or_else(|| pick("temp"))
}

// ---------------- S9 fallback driver (CGI unusable) ----------------

/// Stock S9 reached over bmminer RPC only, used when the digest CGI
/// rejected TerraMine's credentials. Offers `reboot` as a bmminer
/// restart (mining process only), dry-run only until hardware-confirmed,
/// and nothing else.
#[derive(Debug, Clone)]
pub struct S9RpcDriver {
    ip: IpAddr,
    rpc: CgminerRpc,
    /// `None` = privileged: restart route offered. `Some(reason)` = not.
    denied: Option<String>,
    cgi_reason: String,
}

impl S9RpcDriver {
    /// Capability check first: one `privileged` read. `Ok(None)` when no
    /// RPC answers (not a stock Antminer here).
    pub async fn probe(ip: IpAddr, addr: String, timeout: Duration, cgi_reason: String) -> anyhow::Result<Option<Self>> {
        let rpc = CgminerRpc::for_class(DeviceClass::AntminerStock, addr, timeout).map_err(anyhow::Error::msg)?;
        // Must look like bmminer: version answers.
        let Ok(v) = rpc.call("version").await else { return Ok(None) };
        if !is_stock_antminer_version(&v) {
            return Ok(None);
        }
        let denied = rpc.privileged().await.err();
        Ok(Some(Self { ip, rpc, denied, cgi_reason }))
    }
}

impl MinerDriver for S9RpcDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        match self.denied {
            None => Capabilities::none().with_unconfirmed(MinerAction::Reboot, Route::CgminerRpc),
            Some(_) => Capabilities::none(),
        }
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Reboot if self.denied.is_none() => self.rpc.restart().await,
                _ => Ok(false),
            }
        })
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        Box::pin(async move { Ok(MinerStatus { uptime_s: Some(self.rpc.elapsed().await?), paused: None }) })
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(self.rpc.health())
    }
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let mut v = vec![format!("s9-cgi: {}", self.cgi_reason)];
        v.push(match action {
            MinerAction::Reboot => match &self.denied {
                None => "cgminer-rpc: `restart` (restarts bmminer, not the controller; not yet confirmed on real hardware)".into(),
                Some(r) => r.clone(),
            },
            MinerAction::Start | MinerAction::Stop => RPC_NO_PAUSE.into(),
            MinerAction::LightOn | MinerAction::LightOff => RPC_NO_LIGHT.into(),
        });
        v
    }
    fn profile(&self) -> DeviceProfile {
        DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None)
    }
    fn kind(&self) -> DriverKind {
        DriverKind::S9Stock
    }
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(NoSettingsPath { id: "cgminer-rpc", reason: RPC_NO_SETTINGS })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stock_antminer_gets_a_client() {
        for c in [
            DeviceClass::AxeOs,
            DeviceClass::BraiinsOs,
            DeviceClass::LuxOs,
            DeviceClass::Vnish,
            DeviceClass::Whatsminer,
            DeviceClass::Avalon,
            DeviceClass::Epic,
            DeviceClass::Unknown,
        ] {
            let e = CgminerRpc::for_class(c, "127.0.0.1:1".into(), Duration::from_secs(1)).unwrap_err();
            assert!(e.contains("stock Antminer bmminer only"), "{c:?}: {e}");
        }
        assert!(CgminerRpc::for_class(DeviceClass::AntminerStock, "127.0.0.1:1".into(), Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn allowlist_has_no_config_or_pool_writes() {
        for c in ["save", "addpool", "removepool", "switchpool", "enablepool", "disablepool", "poolpriority", "ascset", "pgaset", "quit", "setconfig", "zero"] {
            assert!(!allowed(c), "{c} must never be sent");
        }
        assert_eq!(WRITE_COMMANDS, ["restart"]);
    }

    /// A refused command never opens a socket: the listener sees no
    /// connection.
    #[tokio::test]
    async fn refused_command_sends_nothing() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = CgminerRpc::for_class(DeviceClass::AntminerStock, l.local_addr().unwrap().to_string(), Duration::from_secs(1)).unwrap();
        let e = rpc.call("save").await.unwrap_err().to_string();
        assert!(e.contains("not on the allowlist"), "{e}");
        let accepted = tokio::time::timeout(Duration::from_millis(100), l.accept()).await;
        assert!(accepted.is_err(), "no connection may be made for a refused command");
    }

    #[test]
    fn version_fingerprint_is_stock_antminer_only() {
        let bm = serde_json::json!({"STATUS":[{"STATUS":"S"}],"VERSION":[{"BMMiner":"2.0.0","API":"3.1","Miner":"16.8.1.3","Type":"Antminer S9"}]});
        assert!(is_stock_antminer_version(&bm));
        for other in [
            serde_json::json!({"STATUS":[{"STATUS":"S"}],"VERSION":[{"CGMiner":"4.11.1","API":"3.7","PROD":"AvalonMiner 1246"}]}),
            serde_json::json!({"STATUS":[{"STATUS":"S"}],"VERSION":[{"BOSminer":"0.2.0","API":"3.7"}]}),
            serde_json::json!({"STATUS":[{"STATUS":"S"}]}),
        ] {
            assert!(!is_stock_antminer_version(&other), "{other}");
        }
    }

    #[test]
    fn parses_nul_terminated_and_bmminer_stats() {
        let v = parse_reply(b"{\"STATUS\":[{\"STATUS\":\"S\",\"Msg\":\"ok\"}],\"SUMMARY\":[{\"Elapsed\":42,\"GHS 5s\":\"13500.5\"}]}\0").unwrap();
        assert_eq!(summary_elapsed(&v), Some(42));
        assert_eq!(summary_ghs(&v), Some(13500.5));
        assert_eq!(status_of(&v), Some(("S", "ok")));
        // bmminer: `}{` between STATS objects, string temps, empty slots = 0.
        let raw = br#"{"STATUS":[{"STATUS":"S"}],"STATS":[{"BMMiner":"2.0.0"}{"temp6":61,"temp7":0,"temp2_6":"74","temp2_7":78,"temp2_8":0,"temp_max":80}],"id":1}"#;
        let v = parse_reply(raw).unwrap();
        assert_eq!(max_chip_temp(&v), Some(78.0));
        let board_only = parse_reply(br#"{"STATS":[{},{"temp6":61,"temp7":66,"temp_max":90}]}"#).unwrap();
        assert_eq!(max_chip_temp(&board_only), Some(66.0), "temp_max is not a sensor");
    }
}
