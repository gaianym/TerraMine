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
//!
//! AxeOS mocks also hold settings (frequency, core voltage, fan) behind
//! `PATCH /api/system` and report `temp`. A [`Fault`] simulates a miner
//! that turns unhealthy whenever its frequency/voltage differ from the
//! known-good values, which is what the rollback tests exercise. Unlike
//! real AxeOS, the mock applies settings immediately (no restart).
//! Every request is logged so tests can prove which endpoints were hit.

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

use std::sync::atomic::{AtomicUsize, Ordering};

use super::cache::{CachedDevice, DriverKind};
use super::driver::{DriverFactory, Identity, MinerDriver};
use super::overrides::{BitaxeDriver, DigestCredentials, S9StockDriver};
use super::safety::audit::now_ms;
use super::safety::DeviceProfile;

/// How a mock misbehaves once its frequency/voltage leave the known-good values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Keeps answering but hashes at 0 GH/s.
    HashrateDrop,
    /// Keeps hashing but reports 95 °C.
    Overheat,
    /// Crashes: offline for the reboot downtime, then back at 0 GH/s.
    Unreachable,
}

pub const MOCK_FREQ_MHZ: u32 = 525;
pub const MOCK_CORE_MV: u32 = 1150;

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
    frequency: u32,
    core_voltage: u32,
    fanspeed: u32,
    autofan: bool,
    /// Known-good (frequency, core voltage); differing values trigger `fault`.
    healthy: (u32, u32),
    fault: Option<Fault>,
    /// Accept PATCH with 200 but do not apply it (read-back mismatch).
    ignore_writes: bool,
    requests: Vec<String>,
    mac: String,
    /// Simulated network/pool outage: reachable but hashing at 0.
    outage: bool,
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
    fn degraded(&self) -> Option<Fault> {
        self.fault.filter(|_| (self.frequency, self.core_voltage) != self.healthy)
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
    /// Make the miner unhealthy whenever frequency/voltage leave the
    /// current (known-good) values.
    pub fn set_fault(&self, f: Option<Fault>) {
        self.state.lock().unwrap().fault = f;
    }
    pub fn set_outage(&self, v: bool) {
        self.state.lock().unwrap().outage = v;
    }
    pub fn mac(&self) -> String {
        self.state.lock().unwrap().mac.clone()
    }
    /// Simulate a different device answering at this address.
    pub fn set_mac(&self, mac: &str) {
        self.state.lock().unwrap().mac = mac.to_string();
    }
    pub fn set_ignore_writes(&self, v: bool) {
        self.state.lock().unwrap().ignore_writes = v;
    }
    /// (frequency MHz, core voltage mV).
    pub fn settings(&self) -> (u32, u32) {
        let s = self.state.lock().unwrap();
        (s.frequency, s.core_voltage)
    }
    /// Every request seen, e.g. "GET /api/system/info", "cgminer", "unparsed".
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
    /// Count of setting writes (PATCH) received.
    pub fn writes(&self) -> usize {
        self.requests().iter().filter(|r| r.starts_with("PATCH ")).count()
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
            frequency: MOCK_FREQ_MHZ,
            core_voltage: MOCK_CORE_MV,
            fanspeed: 60,
            autofan: true,
            healthy: (MOCK_FREQ_MHZ, MOCK_CORE_MV),
            fault: None,
            ignore_writes: false,
            requests: Vec::new(),
            mac: String::new(),
            outage: false,
        }));
        let stop = CancellationToken::new();
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let http = l.local_addr()?;
        state.lock().unwrap().mac = format!("02:00:00:00:{:02X}:{:02X}", http.port() >> 8, http.port() & 0xff);
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
                s.requests.push("cgminer".into());
                s.settle();
                if !s.online() {
                    return; // drop the socket while "rebooting"
                }
                let ghs = if s.outage || s.paused { "0" } else { "13500" };
                format!(r#"{{"STATUS":[{{"STATUS":"S"}}],"SUMMARY":[{{"Elapsed":{},"GHS 5s":"{ghs}"}}],"id":1}}"#, s.uptime())
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
            let Some(req) = read_request(&mut sock).await else {
                st.lock().unwrap().requests.push("unparsed".into());
                return;
            };
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
    let m = req.method.as_str();
    let p = req.path.split('?').next().unwrap_or("");
    s.requests.push(format!("{m} {p}"));
    s.settle();
    if !s.online() {
        return http("503 Service Unavailable", "", "{}");
    }
    match s.kind {
        MockKind::AxeOs210 | MockKind::AxeOs215 => {
            let new = s.kind == MockKind::AxeOs215;
            match (m, p) {
                ("GET", "/api/system/info") => {
                    let ver = if new { "v2.15.3" } else { "v2.10.1" };
                    let paused = if new { format!(r#","miningPaused":{}"#, s.paused) } else { String::new() };
                    let fault = s.degraded();
                    let dead = matches!(fault, Some(Fault::HashrateDrop | Fault::Unreachable));
                    let hr = if s.paused || dead || s.outage { 0.0 } else { 1180.5 };
                    let temp = if fault == Some(Fault::Overheat) { 95.0 } else { 52.5 };
                    let body = format!(
                        r#"{{"ASICModel":"BM1370","boardVersion":"601","version":"{ver}","axeOSVersion":"{ver}","macAddr":"{}","uptimeSeconds":{},"hashRate":{hr},"temp":{temp},"frequency":{},"coreVoltage":{},"fanspeed":{},"autofanspeed":{}{paused}}}"#,
                        s.mac,
                        s.uptime(),
                        s.frequency,
                        s.core_voltage,
                        s.fanspeed,
                        u8::from(s.autofan)
                    );
                    http("200 OK", "", &body)
                }
                ("PATCH", "/api/system") => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&req.body) else {
                        return http("400 Bad Request", "", "{}");
                    };
                    if !s.ignore_writes {
                        let num = |k: &str| v.get(k).and_then(|x| x.as_u64()).map(|x| x as u32);
                        if let Some(f) = num("frequency") {
                            s.frequency = f;
                        }
                        if let Some(c) = num("coreVoltage") {
                            s.core_voltage = c;
                        }
                        if let Some(f) = num("fanspeed") {
                            s.fanspeed = f;
                        }
                        if let Some(a) = num("autofanspeed") {
                            s.autofan = a != 0;
                        }
                        if s.degraded() == Some(Fault::Unreachable) {
                            s.offline_until = Some(Instant::now() + s.downtime);
                        }
                    }
                    http("200 OK", "", "{}")
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
                "/cgi-bin/get_system_info.cgi" => {
                    let body = format!(r#"{{"macaddr":"{}","system_filesystem_version":"Thu May 31 2018","minertype":"Antminer S9"}}"#, s.mac);
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

/// Factory that maps fake IPs to mock miners on 127.0.0.1 ports. Counts
/// full identifications (`connects`) and cache rebuilds (`builds`).
pub struct MockMinerFactory {
    pub timeout: Duration,
    pub miners: HashMap<IpAddr, (MockKind, String, Option<SocketAddr>, DigestCredentials)>,
    macs: HashMap<IpAddr, String>,
    connects: AtomicUsize,
    builds: AtomicUsize,
}

impl MockMinerFactory {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout, miners: HashMap::new(), macs: HashMap::new(), connects: AtomicUsize::new(0), builds: AtomicUsize::new(0) }
    }
    pub fn add(&mut self, ip: IpAddr, m: &MockMiner) {
        self.miners.insert(ip, (m.kind, m.base(), m.cgminer, m.creds.clone()));
        self.macs.insert(ip, m.mac());
    }
    /// Full identifications performed.
    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
    /// Drivers rebuilt from a cached record (no identification).
    pub fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
    /// What discovery would cache for this mock (no driver yet).
    pub fn discovered(&self, ip: IpAddr) -> Option<CachedDevice> {
        let (kind, ..) = self.miners.get(&ip)?;
        let (profile, kind, fw) = match kind {
            MockKind::AxeOs210 => (DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS 2.10.1", Some("BM1370")), DriverKind::AxeOs, "v2.10.1"),
            MockKind::AxeOs215 => (DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS 2.15.3", Some("BM1370")), DriverKind::AxeOs, "v2.15.3"),
            MockKind::S9Stock { .. } => (DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None), DriverKind::S9Stock, "Thu May 31 2018"),
        };
        Some(CachedDevice {
            profile,
            identity: Identity { mac: self.macs.get(&ip).cloned(), firmware: Some(fw.into()) },
            kind,
            seen_at_ms: now_ms(),
            miner: None,
            driver: None,
        })
    }
}

impl DriverFactory for MockMinerFactory {
    fn build<'a>(&'a self, ip: IpAddr, _c: &'a CachedDevice) -> BoxFuture<'a, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            self.builds.fetch_add(1, Ordering::SeqCst);
            let Some((kind, base, cg, creds)) = self.miners.get(&ip).cloned() else { return Ok(None) };
            let client = reqwest::Client::builder().timeout(self.timeout).no_proxy().build()?;
            Ok(Some(match kind {
                MockKind::AxeOs210 | MockKind::AxeOs215 => {
                    let v = if kind == MockKind::AxeOs215 { semver::Version::new(2, 15, 3) } else { semver::Version::new(2, 10, 1) };
                    Box::new(BitaxeDriver::from_parts(ip, base, client, Some(v), false).with_asic_model(Some("BM1370".into())))
                        as Box<dyn MinerDriver>
                }
                MockKind::S9Stock { miner_mode } => {
                    let d = S9StockDriver::from_parts(ip, base, client, creds, miner_mode);
                    Box::new(match cg {
                        Some(a) => d.with_cgminer_addr(a.to_string()),
                        None => d,
                    }) as Box<dyn MinerDriver>
                }
            }))
        })
    }

    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let Some((kind, base, cg, creds)) = self.miners.get(&ip).cloned() else { return Ok(None) };
            Ok(match kind {
                MockKind::AxeOs210 | MockKind::AxeOs215 => BitaxeDriver::probe_at(ip, base, self.timeout)
                    .await?
                    .map(|d| Box::new(d) as Box<dyn MinerDriver>),
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
