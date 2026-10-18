//! Stateful mock miners for end-to-end tests and manual UI testing.
//!
//! Variants:
//! - AxeOS 2.10: `/api/system/info` + `POST /api/system/restart` only.
//! - AxeOS 2.15: adds `POST /api/system/pause` / `resume` and `miningPaused`.
//! - Stock S9: digest-protected CGI (`get_miner_conf`, `reboot`,
//!   `set_miner_conf`) plus a cgminer JSON API reporting `Elapsed`.
//!
//! A reboot takes the miner "offline" (HTTP 503 / closed cgminer socket)
//! for `reboot_downtime`, then resets uptime to zero.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::driver::{Capabilities, CompositeDriver, DriverFactory, MinerAction, MinerDriver, Route};
use super::overrides::{BitaxeDriver, DigestCredentials, S9StockDriver};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockKind {
    AxeOs210,
    AxeOs215,
    S9Stock { miner_mode: bool },
}

struct State {
    kind: MockKind,
    boot_at: Instant,
    /// Uptime the miner had at `boot_at` (lets a fresh mock start "old").
    uptime_offset: u64,
    offline_until: Option<Instant>,
    paused: bool,
    reboots: u32,
    downtime: Duration,
}

impl State {
    fn online(&self) -> bool {
        self.offline_until.map(|t| Instant::now() >= t).unwrap_or(true)
    }
    fn uptime(&self) -> u64 {
        self.uptime_offset + self.boot_at.elapsed().as_secs()
    }
    fn settle(&mut self) {
        if let Some(t) = self.offline_until {
            if Instant::now() >= t {
                self.offline_until = None;
                self.boot_at = t;
                self.uptime_offset = 0;
                self.paused = false; // stock sleep does not survive reboot
            }
        }
    }
    fn reboot(&mut self) {
        self.reboots += 1;
        self.offline_until = Some(Instant::now() + self.downtime);
    }
}

pub struct MockMiner {
    pub kind: MockKind,
    pub http: SocketAddr,
    pub cgminer: Option<SocketAddr>,
    pub creds: DigestCredentials,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
}

impl Drop for MockMiner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl MockMiner {
    pub fn base(&self) -> String {
        format!("http://{}", self.http)
    }
    pub fn reboots(&self) -> u32 {
        self.state.lock().unwrap().reboots
    }
    pub fn paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }

    pub async fn spawn(kind: MockKind, downtime: Duration) -> std::io::Result<Self> {
        let creds = DigestCredentials { username: "root".into(), password: "mock-pass".into() };
        let state = Arc::new(Mutex::new(State {
            kind,
            boot_at: Instant::now(),
            uptime_offset: 3600,
            offline_until: None,
            paused: false,
            reboots: 0,
            downtime,
        }));
        let stop = CancellationToken::new();
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let http = l.local_addr()?;
        tokio::spawn(serve_http(l, state.clone(), creds.clone(), stop.clone()));
        let cgminer = if matches!(kind, MockKind::S9Stock { .. }) {
            let l = TcpListener::bind("127.0.0.1:0").await?;
            let a = l.local_addr()?;
            tokio::spawn(serve_cgminer(l, state.clone(), stop.clone()));
            Some(a)
        } else {
            None
        };
        Ok(Self { kind, http, cgminer, creds, state, stop })
    }
}

async fn serve_cgminer(l: TcpListener, st: Arc<Mutex<State>>, stop: CancellationToken) {
    loop {
        let (mut sock, _) = tokio::select! {
            _ = stop.cancelled() => return,
            r = l.accept() => match r { Ok(x) => x, Err(_) => return },
        };
        let st = st.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let reply = {
                let mut s = st.lock().unwrap();
                s.settle();
                if !s.online() {
                    return; // drop the socket while "rebooting"
                }
                format!(r#"{{"STATUS":[{{"STATUS":"S"}}],"SUMMARY":[{{"Elapsed":{},"GHS 5s":"13500"}}],"id":1}}"#, s.uptime())
            };
            let mut out = reply.into_bytes();
            out.push(0);
            let _ = sock.write_all(&out).await;
        });
    }
}

async fn serve_http(l: TcpListener, st: Arc<Mutex<State>>, creds: DigestCredentials, stop: CancellationToken) {
    loop {
        let (mut sock, _) = tokio::select! {
            _ = stop.cancelled() => return,
            r = l.accept() => match r { Ok(x) => x, Err(_) => return },
        };
        let st = st.clone();
        let creds = creds.clone();
        tokio::spawn(async move {
            let Some(req) = read_request(&mut sock).await else { return };
            let resp = handle(&req, &st, &creds);
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

struct Req {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let len = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = sock.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let mut lines = head.lines();
            let first: Vec<_> = lines.next()?.split_whitespace().collect();
            let auth = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l[14..].trim().to_string());
            let body = String::from_utf8_lossy(&buf[end + 4..]).to_string();
            return Some(Req { method: first.first()?.to_string(), path: first.get(1)?.to_string(), auth, body });
        }
    }
    None
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

const REALM: &str = "antMiner Configuration";
const NONCE: &str = "6f1c3c1d2a";

fn http(status: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

fn md5_hex(s: &str) -> String {
    hex::encode(Md5::digest(s.as_bytes()))
}

fn param(h: &str, key: &str) -> Option<String> {
    let i = h.find(&format!("{key}="))?;
    let rest = &h[i + key.len() + 1..];
    if let Some(r) = rest.strip_prefix('"') {
        Some(r[..r.find('"')?].to_string())
    } else {
        Some(rest.split([',', ' ']).next()?.to_string())
    }
}

fn digest_ok(req: &Req, c: &DigestCredentials) -> bool {
    let Some(h) = &req.auth else { return false };
    if !h.to_ascii_lowercase().starts_with("digest") {
        return false;
    }
    if param(h, "username").as_deref() != Some(c.username.as_str()) {
        return false;
    }
    let uri = param(h, "uri").unwrap_or_default();
    let ha1 = md5_hex(&format!("{}:{REALM}:{}", c.username, c.password));
    let ha2 = md5_hex(&format!("{}:{uri}", req.method));
    let expect = match param(h, "qop") {
        Some(q) => md5_hex(&format!(
            "{ha1}:{NONCE}:{}:{}:{q}:{ha2}",
            param(h, "nc").unwrap_or_default(),
            param(h, "cnonce").unwrap_or_default()
        )),
        None => md5_hex(&format!("{ha1}:{NONCE}:{ha2}")),
    };
    param(h, "response").as_deref() == Some(expect.as_str())
}

fn handle(req: &Req, st: &Arc<Mutex<State>>, creds: &DigestCredentials) -> String {
    let mut s = st.lock().unwrap();
    s.settle();
    if !s.online() {
        return http("503 Service Unavailable", "", "{}");
    }
    let m = req.method.as_str();
    let p = req.path.split('?').next().unwrap_or("");
    match s.kind {
        MockKind::AxeOs210 | MockKind::AxeOs215 => {
            let new = s.kind == MockKind::AxeOs215;
            match (m, p) {
                ("GET", "/api/system/info") => {
                    let ver = if new { "v2.15.3" } else { "v2.10.1" };
                    let paused = if new { format!(r#","miningPaused":{}"#, s.paused) } else { String::new() };
                    let hr = if s.paused { 0.0 } else { 1180.5 };
                    let body = format!(
                        r#"{{"ASICModel":"BM1370","boardVersion":"601","version":"{ver}","axeOSVersion":"{ver}","uptimeSeconds":{},"hashRate":{hr}{paused}}}"#,
                        s.uptime()
                    );
                    http("200 OK", "", &body)
                }
                ("POST", "/api/system/restart") => {
                    s.reboot();
                    http("200 OK", "", r#"{"message":"System will restart shortly."}"#)
                }
                ("POST", "/api/system/pause") if new => {
                    s.paused = true;
                    http("200 OK", "", r#"{"message":"Mining paused"}"#)
                }
                ("POST", "/api/system/resume") if new => {
                    s.paused = false;
                    http("200 OK", "", r#"{"message":"Mining resumed"}"#)
                }
                _ => http("404 Not Found", "", "{}"),
            }
        }
        MockKind::S9Stock { miner_mode } => {
            if !digest_ok(req, creds) {
                let h = format!("WWW-Authenticate: Digest realm=\"{REALM}\", nonce=\"{NONCE}\", qop=\"auth\"\r\n");
                return http("401 Unauthorized", &h, "{}");
            }
            match p {
                "/cgi-bin/get_miner_conf.cgi" => {
                    let body = if miner_mode {
                        format!(r#"{{"pools":[],"miner-mode":"{}"}}"#, if s.paused { 1 } else { 0 })
                    } else {
                        r#"{"pools":[],"bitmain-fan-ctrl":false}"#.to_string()
                    };
                    http("200 OK", "", &body)
                }
                "/cgi-bin/reboot.cgi" => {
                    s.reboot();
                    http("200 OK", "", "{}")
                }
                "/cgi-bin/set_miner_conf.cgi" if miner_mode && m == "POST" => {
                    // {"miner-mode":"1"} = sleep, "0" = normal.
                    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap_or_default();
                    match v.get("miner-mode").and_then(|m| m.as_str()) {
                        Some("1") => s.paused = true,
                        Some("0") => s.paused = false,
                        _ => return http("400 Bad Request", "", "{}"),
                    }
                    http("200 OK", "", r#"{"stats":"success"}"#)
                }
                _ => http("404 Not Found", "", "{}"),
            }
        }
    }
}

/// Stand-in for the asic-rs 0.8.6 `v2_14_0` Bitaxe backend on a mock.
/// asic-rs hardcodes port 80, so it cannot reach a mock on a random local
/// port; this mirrors that backend exactly: `supports_pause/resume() =
/// true`, `supports_restart() = false`, and the same `POST
/// /api/system/pause|resume` calls (asic-rs-firmwares-bitaxe 0.8.6,
/// backends/v2_14_0/{mod.rs,web.rs}).
pub struct MockNativeBitaxe {
    ip: IpAddr,
    base: String,
    client: reqwest::Client,
}

impl MinerDriver for MockNativeBitaxe {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities::flags(true, true, false, Route::AsicRs)
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            let path = match action {
                MinerAction::Stop => "system/pause",
                MinerAction::Start => "system/resume",
                MinerAction::Reboot => anyhow::bail!("restart not supported by asic-rs v2_14_0"),
            };
            self.client.post(format!("{}/api/{path}", self.base)).send().await?.error_for_status()?;
            Ok(true)
        })
    }
}

/// Factory that maps fake IPs to mock miners on 127.0.0.1 ports.
pub struct MockMinerFactory {
    pub timeout: Duration,
    pub miners: HashMap<IpAddr, (MockKind, String, Option<SocketAddr>, DigestCredentials)>,
}

impl MockMinerFactory {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout, miners: HashMap::new() }
    }
    pub fn add(&mut self, ip: IpAddr, m: &MockMiner) {
        self.miners.insert(ip, (m.kind, m.base(), m.cgminer, m.creds.clone()));
    }
}

impl DriverFactory for MockMinerFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            let Some((kind, base, cg, creds)) = self.miners.get(&ip).cloned() else { return Ok(None) };
            Ok(match kind {
                MockKind::AxeOs210 | MockKind::AxeOs215 => match BitaxeDriver::probe_at(ip, base.clone(), self.timeout).await? {
                    // Same composition as the app: native first, AxeOS override for the gaps.
                    Some(d) if d.native_backend_available() => {
                        let client = reqwest::Client::builder().timeout(self.timeout).no_proxy().build()?;
                        let native = Box::new(MockNativeBitaxe { ip, base, client });
                        Some(Box::new(CompositeDriver::new(ip, native, Box::new(d))) as Box<dyn MinerDriver>)
                    }
                    Some(d) => Some(Box::new(d) as Box<dyn MinerDriver>),
                    None => None,
                },
                MockKind::S9Stock { .. } => S9StockDriver::probe_at(ip, base, creds, self.timeout)
                    .await?
                    .map(|d| {
                        let d = match cg {
                            Some(a) => d.with_cgminer_addr(a.to_string()),
                            None => d,
                        };
                        Box::new(d) as Box<dyn MinerDriver>
                    }),
            })
        })
    }
}
