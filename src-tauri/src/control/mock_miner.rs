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
//! AxeOS mocks also hold settings (frequency, core voltage, fan, target
//! temperature) behind `PATCH /api/system` and report `temp` / `power`.
//! Frequency and core voltage are restart-to-apply: PATCH only stores
//! them (`frequency` / `coreVoltage` report the stored value at once) and
//! the running values (`actualFrequency`, health, faults) change only
//! after `POST /api/system/restart`. Fan settings apply immediately. On
//! 2.15 the manual fan speed is `manualFanSpeed` and `fanspeed` is the
//! measured fan % (as in AxeOS ≥ 2.11); 2.10 uses `fanspeed` for both.
//! A [`Fault`] simulates a miner that turns unhealthy whenever its running
//! frequency/voltage differ from the known-good values, which is what the
//! rollback tests exercise.
//! Every request is logged so tests can prove which endpoints were hit.
//!
//! Fault light: the S9 mock serves the stock `blink.cgi` /
//! `get_blink_status.cgi` pair; AxeOS 2.15 serves `POST
//! /api/system/identify` (a 30 s toggle with no readable state, like
//! ESP-Miner). [`MockKind::AsicRs`] stands in for asic-rs firmware
//! backends TerraMine has no hardware for (Antminer stock, Braiins,
//! Whatsminer): asic-rs hardcodes real ports, so the stand-in driver
//! carries that backend's `supports_*()` flags and talks to this server on
//! `/asic-rs/...` paths instead, which keeps every call countable.

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
use super::driver::{Capabilities, CompositeDriver, DriverFactory, Identity, MinerAction, MinerDriver, Route};
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
/// Power draw at the default frequency / voltage.
pub const MOCK_POWER_W: f64 = 17.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockKind {
    AxeOs210,
    AxeOs215,
    S9Stock { miner_mode: bool },
    /// An asic-rs firmware backend (see [`MockFirmware`]).
    AsicRs(MockFirmware),
}

/// asic-rs 0.8.6 backends mirrored by flag (values read from each
/// backend's `supports_*()` in asic-rs-firmwares-* 0.8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFirmware {
    /// Antminer stock, `antminer/v2020` (S19 class): light yes.
    AntminerV2020,
    /// Braiins OS `braiins/v25_07`: light yes.
    Braiins2507,
    /// Whatsminer `whatsminer/v3`: light yes.
    WhatsminerV3,
    /// Whatsminer `whatsminer/v1`: no light (all setters false).
    WhatsminerV1,
}

impl MockFirmware {
    pub fn flags(self) -> super::driver::NativeFlags {
        use super::driver::NativeFlags;
        match self {
            Self::AntminerV2020 => NativeFlags { resume: true, pause: true, restart: true, fault_light: true },
            Self::Braiins2507 => NativeFlags { resume: true, pause: true, restart: true, fault_light: true },
            Self::WhatsminerV3 => NativeFlags { resume: true, pause: true, restart: true, fault_light: true },
            Self::WhatsminerV1 => NativeFlags::default(),
        }
    }
    /// (make, model, firmware) as asic-rs would report them.
    pub fn identity(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::AntminerV2020 => ("AntMiner", "Antminer S19", "Stock v2020"),
            Self::Braiins2507 => ("AntMiner", "Antminer S19", "BraiinsOS 25.07"),
            Self::WhatsminerV3 => ("WhatsMiner", "M50", "Whatsminer v3"),
            Self::WhatsminerV1 => ("WhatsMiner", "M20", "Whatsminer v1"),
        }
    }
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
    /// Running values (applied at boot).
    frequency: u32,
    core_voltage: u32,
    /// Stored (NVS) values, applied by the next restart.
    nvs_frequency: u32,
    nvs_core_voltage: u32,
    /// Manual fan speed setting.
    fanspeed: u32,
    autofan: bool,
    temptarget: u32,
    /// Running config that already crashed once (Fault::Unreachable).
    crashed_on: Option<(u32, u32)>,
    /// Answer `POST /api/system/restart` with 200 but do not restart.
    ignore_restart: bool,
    /// Running frequency never exceeds this (firmware throttling).
    freq_cap: Option<u32>,
    /// Known-good (frequency, core voltage); differing values trigger `fault`.
    healthy: (u32, u32),
    fault: Option<Fault>,
    /// Accept PATCH with 200 but do not apply it (read-back mismatch).
    ignore_writes: bool,
    requests: Vec<String>,
    mac: String,
    /// Simulated network/pool outage: reachable but hashing at 0.
    outage: bool,
    /// Fault light (S9 blink / asic-rs stand-ins).
    light: bool,
    /// AxeOS identify ("Hi!") active until.
    identify_until: Option<Instant>,
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
                self.frequency = self.freq_cap.map_or(self.nvs_frequency, |c| self.nvs_frequency.min(c));
                self.core_voltage = self.nvs_core_voltage;
                let running = (self.frequency, self.core_voltage);
                if self.degraded() == Some(Fault::Unreachable) && self.crashed_on != Some(running) {
                    // Crashes once more right after booting the bad config.
                    self.crashed_on = Some(running);
                    self.offline_until = Some(Instant::now() + self.downtime);
                }
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
    /// Set stored and running frequency (e.g. a unit held low after throttling).
    pub fn set_frequency(&self, f: u32) {
        let mut s = self.state.lock().unwrap();
        s.frequency = f;
        s.nvs_frequency = f;
        s.healthy.0 = f;
    }
    pub fn set_ignore_restart(&self, v: bool) {
        self.state.lock().unwrap().ignore_restart = v;
    }
    pub fn set_freq_cap(&self, cap: Option<u32>) {
        self.state.lock().unwrap().freq_cap = cap;
    }
    /// Stored (frequency MHz, core voltage mV), applied by the next restart.
    pub fn stored(&self) -> (u32, u32) {
        let s = self.state.lock().unwrap();
        (s.nvs_frequency, s.nvs_core_voltage)
    }
    /// (auto fan, manual fan %, target °C).
    pub fn fan(&self) -> (bool, u32, u32) {
        let s = self.state.lock().unwrap();
        (s.autofan, s.fanspeed, s.temptarget)
    }
    /// Running (frequency MHz, core voltage mV).
    pub fn settings(&self) -> (u32, u32) {
        let s = self.state.lock().unwrap();
        (s.frequency, s.core_voltage)
    }
    /// Every request seen, e.g. "GET /api/system/info", "cgminer", "unparsed".
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
    /// Fault light as the mock sees it.
    pub fn light(&self) -> bool {
        self.state.lock().unwrap().light
    }
    /// AxeOS identify currently showing "Hi!".
    pub fn identifying(&self) -> bool {
        self.state.lock().unwrap().identify_until.is_some_and(|t| Instant::now() < t)
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
            nvs_frequency: MOCK_FREQ_MHZ,
            nvs_core_voltage: MOCK_CORE_MV,
            fanspeed: 60,
            autofan: true,
            temptarget: 60,
            crashed_on: None,
            ignore_restart: false,
            freq_cap: None,
            healthy: (MOCK_FREQ_MHZ, MOCK_CORE_MV),
            fault: None,
            ignore_writes: false,
            requests: Vec::new(),
            mac: String::new(),
            outage: false,
            light: false,
            identify_until: None,
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
                    let volts = f64::from(s.core_voltage) / f64::from(MOCK_CORE_MV);
                    let power = if hr > 0.0 { MOCK_POWER_W * f64::from(s.frequency) / f64::from(MOCK_FREQ_MHZ) * volts * volts } else { 3.0 };
                    let fan = if new {
                        let measured = if s.autofan { 47 } else { s.fanspeed };
                        format!(r#""fanspeed":{measured},"manualFanSpeed":{},"actualFrequency":{}"#, s.fanspeed, s.frequency)
                    } else {
                        format!(r#""fanspeed":{}"#, s.fanspeed)
                    };
                    let body = format!(
                        r#"{{"ASICModel":"BM1370","boardVersion":"601","version":"{ver}","axeOSVersion":"{ver}","macAddr":"{}","uptimeSeconds":{},"hashRate":{hr},"temp":{temp},"power":{power:.2},"frequency":{},"coreVoltage":{},{fan},"autofanspeed":{},"temptarget":{}{paused}}}"#,
                        s.mac,
                        s.uptime(),
                        s.nvs_frequency,
                        s.nvs_core_voltage,
                        u8::from(s.autofan),
                        s.temptarget
                    );
                    http("200 OK", "", &body)
                }
                ("PATCH", "/api/system") => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&req.body) else {
                        return http("400 Bad Request", "", "{}");
                    };
                    if !s.ignore_writes {
                        let num = |k: &str| v.get(k).and_then(|x| x.as_u64()).map(|x| x as u32);
                        // Stored only; applied by the next restart.
                        if let Some(f) = num("frequency") {
                            s.nvs_frequency = f;
                        }
                        if let Some(c) = num("coreVoltage") {
                            s.nvs_core_voltage = c;
                        }
                        // Unknown keys are ignored, as on AxeOS.
                        if let Some(f) = num(if new { "manualFanSpeed" } else { "fanspeed" }) {
                            s.fanspeed = f;
                        }
                        if let Some(a) = num("autofanspeed") {
                            s.autofan = a != 0;
                        }
                        if let Some(t) = num("temptarget") {
                            s.temptarget = t;
                        }
                    }
                    http("200 OK", "", "{}")
                }
                ("POST", "/api/system/restart") => {
                    if !s.ignore_restart {
                        s.reboot();
                    }
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
                // ESP-Miner >= 2.12: toggles a 30 s on-screen "Hi!".
                ("POST", "/api/system/identify") if new => {
                    let on = s.identify_until.is_some_and(|t| Instant::now() < t);
                    s.identify_until = (!on).then(|| Instant::now() + Duration::from_secs(30));
                    http("200 OK", "", r#"{"message":"The device says \"Hi!\" for 30 seconds."}"#)
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
                "/cgi-bin/blink.cgi" if m == "POST" => {
                    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap_or_default();
                    match v.get("blink").and_then(|b| b.as_str()) {
                        Some("true") => s.light = true,
                        Some("false") => s.light = false,
                        _ => return http("400 Bad Request", "", "{}"),
                    }
                    http("200 OK", "", r#"{"code":"B000"}"#)
                }
                "/cgi-bin/get_blink_status.cgi" => http("200 OK", "", &format!(r#"{{"blink":{}}}"#, s.light)),
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
        MockKind::AsicRs(fw) => match (m, p) {
            ("GET", "/asic-rs/data") => {
                let hr = if s.paused || s.outage { 0.0 } else { 100_000.0 };
                http("200 OK", "", &format!(r#"{{"light":{},"paused":{},"uptime":{},"hashrate_ghs":{hr}}}"#, s.light, s.paused, s.uptime()))
            }
            ("POST", "/asic-rs/fault_light") if fw.flags().fault_light => {
                let v: serde_json::Value = serde_json::from_str(&req.body).unwrap_or_default();
                let Some(on) = v.get("on").and_then(|b| b.as_bool()) else { return http("400 Bad Request", "", "{}") };
                s.light = on;
                http("200 OK", "", "{}")
            }
            ("POST", "/asic-rs/pause") if fw.flags().pause => {
                s.paused = true;
                http("200 OK", "", "{}")
            }
            ("POST", "/asic-rs/resume") if fw.flags().resume => {
                s.paused = false;
                http("200 OK", "", "{}")
            }
            ("POST", "/asic-rs/restart") if fw.flags().restart => {
                s.reboot();
                http("200 OK", "", "{}")
            }
            _ => http("404 Not Found", "", "{}"),
        },
    }
}

/// Stand-in for an asic-rs firmware backend (see [`MockFirmware`]): same
/// routing rule as [`super::driver::AsicRsDriver`] (`NativeFlags::caps`),
/// same verbs, sent to the mock's `/asic-rs/...` paths.
pub struct MockAsicRs {
    ip: IpAddr,
    fw: MockFirmware,
    base: String,
    client: reqwest::Client,
}

impl MockAsicRs {
    async fn post(&self, path: &str, body: serde_json::Value) -> anyhow::Result<bool> {
        self.client.post(format!("{}/asic-rs/{path}", self.base)).json(&body).send().await?.error_for_status()?;
        Ok(true)
    }
    async fn data(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.client.get(format!("{}/asic-rs/data", self.base)).send().await?.error_for_status()?.json().await?)
    }
}

impl MinerDriver for MockAsicRs {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        self.fw.flags().caps()
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            // asic-rs's own default for an unsupported setter is an error.
            anyhow::ensure!(self.caps().supports(action), "{} is not supported on this platform", action.name());
            match action {
                MinerAction::Start => self.post("resume", serde_json::json!({})).await,
                MinerAction::Stop => self.post("pause", serde_json::json!({})).await,
                MinerAction::Reboot => self.post("restart", serde_json::json!({})).await,
                MinerAction::LightOn => self.post("fault_light", serde_json::json!({"on": true})).await,
                MinerAction::LightOff => self.post("fault_light", serde_json::json!({"on": false})).await,
            }
        })
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<super::driver::MinerStatus>> {
        Box::pin(async move {
            let d = self.data().await?;
            Ok(super::driver::MinerStatus { uptime_s: d["uptime"].as_u64(), paused: d["paused"].as_bool() })
        })
    }
    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        Box::pin(async move { Ok(self.data().await?["light"].as_bool()) })
    }
    fn profile(&self) -> DeviceProfile {
        let (make, model, fw) = self.fw.identity();
        DeviceProfile::classify(make, model, fw, None)
    }
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let (make, model, fw) = self.fw.identity();
        vec![format!("{} on {make} {model} ({fw})", self.fw.flags().explain(action))]
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
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        let f = super::driver::NativeFlags { resume: true, pause: true, restart: false, fault_light: false };
        vec![format!("{} on Bitaxe (AxeOS 2.15.3)", f.explain(action))]
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            let path = match action {
                MinerAction::Stop => "system/pause",
                MinerAction::Start => "system/resume",
                MinerAction::Reboot => anyhow::bail!("restart not supported by asic-rs v2_14_0"),
                MinerAction::LightOn | MinerAction::LightOff => anyhow::bail!("fault light not supported by asic-rs v2_14_0"),
            };
            self.client.post(format!("{}/api/{path}", self.base)).send().await?.error_for_status()?;
            Ok(true)
        })
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
            MockKind::AsicRs(f) => {
                let (make, model, fw) = f.identity();
                (DeviceProfile::classify(make, model, fw, None), DriverKind::AsicRs, fw)
            }
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
                    let d = BitaxeDriver::from_parts(ip, base.clone(), client.clone(), Some(v), false)
                        .with_asic_model(Some("BM1370".into()));
                    if d.native_backend_available() {
                        let native = Box::new(MockNativeBitaxe { ip, base, client });
                        Box::new(CompositeDriver::new(ip, native, Box::new(d))) as Box<dyn MinerDriver>
                    } else {
                        Box::new(d) as Box<dyn MinerDriver>
                    }
                }
                MockKind::S9Stock { miner_mode } => {
                    let d = S9StockDriver::from_parts(ip, base, client, creds, miner_mode);
                    Box::new(match cg {
                        Some(a) => d.with_cgminer_addr(a.to_string()),
                        None => d,
                    }) as Box<dyn MinerDriver>
                }
                MockKind::AsicRs(fw) => Box::new(MockAsicRs { ip, fw, base, client }) as Box<dyn MinerDriver>,
            }))
        })
    }

    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            self.connects.fetch_add(1, Ordering::SeqCst);
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
                MockKind::AsicRs(fw) => {
                    let client = reqwest::Client::builder().timeout(self.timeout).no_proxy().build()?;
                    Some(Box::new(MockAsicRs { ip, fw, base, client }) as Box<dyn MinerDriver>)
                }
            })
        })
    }
}
