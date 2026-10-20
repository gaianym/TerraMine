//! Admin actions (plan PR G): hashboard detail, read logs, change
//! password, factory reset — fleetwide, on the asic-rs 0.8.6 traits.
//!
//! - **asic-rs first.** Hashboards come from `get_data().hashboards`,
//!   logs from `ReadLogs`, password from `ChangePassword`, reset from
//!   `FactoryReset`, each gated by its `supports_*()` flag ([`asic`]).
//!   TerraMine adds a path only where asic-rs has none for that firmware:
//!   AxeOS logs over its `/api/ws` log socket ([`axeos`]) and, for the
//!   stock S9, its kernel-log CGI and bmminer `stats` chains ([`s9`]).
//! - **Identity from the cache.** Every action resolves through the
//!   discovery cache ([`Resolver`]); the asic-rs miner discovery kept is
//!   reused. A password change needs an owned asic-rs miner (`&mut self`),
//!   so it is rebuilt from the cached firmware family's asic-rs entry
//!   (`FirmwareEntry::build_miner`) with the stored login — no discovery.
//! - **Capability-gated.** Each path declares what it can do for this
//!   make / model / firmware; a device with no path ends `unsupported`
//!   with every path tried and why.
//! - **Reads** change nothing: no dry run, no confirmation, fleetwide. A
//!   read path that fails falls through to the next one.
//! - **Writes** (password, factory reset) are irreversible: dry run by
//!   default, and a live send needs the typed action name plus
//!   `allow_irreversible`, one device at a time ([`preflight`]). No path
//!   is hardware-confirmed, so a live send is refused outside mock tests.
//!   A write never falls through to another path.
//! - **Secrets.** The new password arrives as a [`Secret`] (redacted in
//!   Debug / Serialize), is written to the OS keychain under this device
//!   *before* the miner is touched (as a pending entry), promoted once the
//!   miner accepts it, and never appears in events, audit or logs.
//! - **Audit.** One record per attempt and per outcome; log contents are
//!   never audited (only their size).

pub mod asic;
pub mod axeos;
#[cfg(test)]
mod e2e_tests;
pub mod s9;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use asic_rs_core::data::board::BoardData;
use asic_rs_core::data::hashrate::HashRateUnit;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::cache::{DeviceCache, Resolved, Resolver};
use super::creds::{self, CredentialStore};
use super::driver::MinerDriver;
use super::job::ActionStatus;
use super::overrides::DigestCredentials;
use super::safety::audit::{now_ms, AuditRecord, AuditSink};
use super::safety::pools::Secret;
use super::safety::{preflight, ActionKind, Confirmation, DeviceClass, DeviceProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAction {
    Hashboards,
    ReadLogs,
    ChangePassword,
    FactoryReset,
}

impl AdminAction {
    pub const ALL: [AdminAction; 4] = [
        Self::Hashboards,
        Self::ReadLogs,
        Self::ChangePassword,
        Self::FactoryReset,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Hashboards => "hashboards",
            Self::ReadLogs => "read_logs",
            Self::ChangePassword => "change_password",
            Self::FactoryReset => "factory_reset",
        }
    }

    pub fn kind(self) -> ActionKind {
        match self {
            Self::Hashboards => ActionKind::ReadHashboards,
            Self::ReadLogs => ActionKind::ReadLogs,
            Self::ChangePassword => ActionKind::ChangePassword,
            Self::FactoryReset => ActionKind::FactoryReset,
        }
    }

    pub fn is_read(self) -> bool {
        self.kind().is_read()
    }
}

/// One hashboard as shown in the UI (from asic-rs `BoardData`, or the
/// stock-S9 bmminer chains).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hashboard {
    /// 0-based, as asic-rs numbers boards.
    pub position: u8,
    pub hashrate_ghs: Option<f64>,
    pub expected_hashrate_ghs: Option<f64>,
    pub board_temp_c: Option<f64>,
    pub inlet_chip_temp_c: Option<f64>,
    pub outlet_chip_temp_c: Option<f64>,
    pub working_chips: Option<u16>,
    pub expected_chips: Option<u16>,
    pub active: Option<bool>,
    pub serial: Option<String>,
    pub voltage_v: Option<f64>,
    pub frequency_mhz: Option<f64>,
    /// Per-chip rows the firmware reported (most report none).
    pub chips_reported: usize,
}

impl Hashboard {
    pub fn from_board(b: &BoardData) -> Self {
        let ghs = |h: &Option<asic_rs_core::data::hashrate::HashRate>| {
            h.clone().map(|h| h.as_unit(HashRateUnit::GigaHash).value)
        };
        Self {
            position: b.position,
            hashrate_ghs: ghs(&b.hashrate),
            expected_hashrate_ghs: ghs(&b.expected_hashrate),
            board_temp_c: b.board_temperature.map(|t| t.as_celsius()),
            inlet_chip_temp_c: b.inlet_chip_temperature.map(|t| t.as_celsius()),
            outlet_chip_temp_c: b.outlet_chip_temperature.map(|t| t.as_celsius()),
            working_chips: b.working_chips,
            expected_chips: b.expected_chips,
            active: b.active,
            serial: b.serial_number.clone(),
            voltage_v: b.voltage.map(|v| v.as_volts()),
            frequency_mhz: b.frequency.map(|f| f.as_megahertz()),
            chips_reported: b.chips.len(),
        }
    }

    /// Reported anything measured (not just the expected shape).
    pub fn has_data(&self) -> bool {
        self.hashrate_ghs.is_some()
            || self.working_chips.is_some()
            || self.board_temp_c.is_some()
            || self.inlet_chip_temp_c.is_some()
            || self.outlet_chip_temp_c.is_some()
    }

    /// Why this board looks unhealthy, if it does.
    pub fn problem(&self) -> Option<String> {
        let mut why = Vec::new();
        if let (Some(w), Some(e)) = (self.working_chips, self.expected_chips) {
            if w < e {
                why.push(format!("{w}/{e} chips"));
            }
        }
        if self.active == Some(false) {
            why.push("inactive".into());
        } else if self.hashrate_ghs.is_some_and(|h| h <= 0.0) {
            why.push("0 hashrate".into());
        }
        (!why.is_empty()).then(|| why.join(", "))
    }
}

/// "3 boards: #0 63/63 chips 4.6 TH/s 72°C; …" plus a degraded count.
pub fn summarize_boards(boards: &[Hashboard]) -> String {
    let rate = |g: f64| {
        if g >= 1000.0 {
            format!("{:.2} TH/s", g / 1000.0)
        } else {
            format!("{g:.0} GH/s")
        }
    };
    let parts: Vec<String> = boards
        .iter()
        .map(|b| {
            let mut s = format!("#{}", b.position);
            match (b.working_chips, b.expected_chips) {
                (Some(w), Some(e)) => s.push_str(&format!(" {w}/{e} chips")),
                (Some(w), None) => s.push_str(&format!(" {w} chips")),
                _ => {}
            }
            if let Some(h) = b.hashrate_ghs {
                s.push_str(&format!(" {}", rate(h)));
            }
            if let Some(t) = b
                .outlet_chip_temp_c
                .or(b.inlet_chip_temp_c)
                .or(b.board_temp_c)
            {
                s.push_str(&format!(" {t:.0}°C"));
            }
            if b.problem().is_some() {
                s.push_str(" (degraded)");
            }
            s
        })
        .collect();
    let bad = boards.iter().filter(|b| b.problem().is_some()).count();
    let head = format!(
        "{} board{}",
        boards.len(),
        if boards.len() == 1 { "" } else { "s" }
    );
    let tail = if bad > 0 {
        format!("; {bad} degraded")
    } else {
        String::new()
    };
    format!("{head}: {}{tail}", parts.join("; "))
}

/// A path that can carry admin actions for one device.
pub trait AdminBackend: Send + Sync {
    /// Path id in "paths tried" and events (`asic-rs`, `axeos`, `s9-cgi`, …).
    fn id(&self) -> &'static str;
    /// Capability check, no I/O. `Err` = why this path cannot do `action`
    /// on this make / model / firmware.
    fn supports(&self, action: AdminAction) -> Result<(), String>;
    /// Confirmed on real hardware for `action`. Live writes need it (reads
    /// change nothing, so they do not).
    fn confirmed(&self, _action: AdminAction) -> bool {
        false
    }
    fn hashboards(&self) -> BoxFuture<'_, anyhow::Result<Vec<Hashboard>>> {
        Box::pin(async { anyhow::bail!("hashboards not supported on this path") })
    }
    /// `login` = the stored login for this device (None = path default).
    fn read_logs<'a>(
        &'a self,
        _login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async { anyhow::bail!("logs not supported on this path") })
    }
    /// Ok(true) = the miner accepted the new password.
    fn change_password<'a>(
        &'a self,
        _login: Option<&'a DigestCredentials>,
        _new_password: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("password change not supported on this path") })
    }
    /// Ok(true) = the miner accepted the reset.
    fn factory_reset<'a>(
        &'a self,
        _login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("factory reset not supported on this path") })
    }
    /// Read-back after a password change: an authenticated read with
    /// `login`. `Ok(None)` = this path has no such read.
    fn check_login<'a>(
        &'a self,
        _login: &'a DigestCredentials,
    ) -> BoxFuture<'a, anyhow::Result<Option<bool>>> {
        Box::pin(async { Ok(None) })
    }
    /// Username to store with a new password when no login is stored yet
    /// (the backend's factory username). `None` = unknown: a live change
    /// is refused until a login is set.
    fn default_username(&self) -> Option<String> {
        None
    }
}

/// Every path for a device, asic-rs first: the driver's native one, else
/// one built from the asic-rs miner discovery cached; then the driver's
/// overrides.
pub fn backends_for(
    driver: &dyn MinerDriver,
    cached_miner: Option<super::driver::SharedMiner>,
    ip: IpAddr,
    timeout: Duration,
) -> Vec<Box<dyn AdminBackend>> {
    let mut v: Vec<Box<dyn AdminBackend>> = Vec::new();
    match driver.native_admin(timeout) {
        Some(n) => v.push(n),
        None => {
            if let Some(m) = cached_miner {
                v.push(Box::new(asic::AsicRsAdmin::live(ip, m, timeout)));
            }
        }
    }
    v.extend(driver.admin_backends());
    v
}

/// Path support for one action (dialog display).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathSupport {
    /// First path that can do the action (`None` = not supported).
    pub path: Option<String>,
    /// False = dry run only until hardware-confirmed (writes).
    pub confirmed: bool,
    pub tried: Vec<String>,
}

pub fn support(backends: &[Box<dyn AdminBackend>], action: AdminAction) -> PathSupport {
    let mut tried = Vec::new();
    if backends.is_empty() {
        tried.push("no admin path for this device".to_string());
    }
    for b in backends {
        match b.supports(action) {
            Err(why) => tried.push(format!("{}: skipped ({why})", b.id())),
            Ok(()) => {
                let confirmed = action.is_read() || b.confirmed(action);
                tried.push(format!(
                    "{}: {}",
                    b.id(),
                    if confirmed {
                        "selected"
                    } else {
                        "selected (not yet confirmed on real hardware)"
                    }
                ));
                return PathSupport {
                    path: Some(b.id().into()),
                    confirmed,
                    tried,
                };
            }
        }
    }
    PathSupport {
        path: None,
        confirmed: false,
        tried,
    }
}

/// A resolved device: driver, the asic-rs miner cached at discovery (if
/// any), and its MAC.
pub type ResolvedDevice = (
    Arc<dyn MinerDriver>,
    Option<super::driver::SharedMiner>,
    Option<String>,
);

#[derive(Debug, Clone, Serialize)]
pub struct AdminSupportRow {
    pub ip: String,
    pub found: bool,
    pub class: Option<DeviceClass>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub actions: BTreeMap<AdminAction, PathSupport>,
    /// A per-device login is stored (never the secret itself).
    pub device_login: bool,
}

pub async fn support_row(
    ip: IpAddr,
    resolved: Option<ResolvedDevice>,
    store: &dyn CredentialStore,
) -> AdminSupportRow {
    let Some((d, miner, mac)) = resolved else {
        return AdminSupportRow {
            ip: ip.to_string(),
            found: false,
            class: None,
            model: None,
            firmware: None,
            actions: BTreeMap::new(),
            device_login: false,
        };
    };
    let p = d.profile();
    let b = backends_for(d.as_ref(), miner, ip, Duration::from_secs(10));
    let actions = AdminAction::ALL
        .into_iter()
        .map(|a| (a, support(&b, a)))
        .collect();
    let key = creds::device_key(ip, mac.as_deref());
    AdminSupportRow {
        ip: ip.to_string(),
        found: true,
        class: Some(p.class),
        model: Some(p.model),
        firmware: Some(p.firmware),
        actions,
        device_login: store.get_device(&key).ok().flatten().is_some(),
    }
}

fn default_true() -> bool {
    true
}
fn default_concurrency() -> usize {
    4
}
fn default_stagger() -> u64 {
    500
}
fn default_timeout() -> u64 {
    15
}
fn default_verify_timeout() -> u64 {
    180
}
/// Logs kept per miner (the tail): 256 KiB.
pub const DEFAULT_MAX_LOG_BYTES: usize = 256 * 1024;
fn default_max_log() -> usize {
    DEFAULT_MAX_LOG_BYTES
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminRequest {
    pub ips: Vec<String>,
    pub action: AdminAction,
    /// Absent = the action default (on for password / reset, off for reads).
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub confirm: Confirmation,
    /// change_password only. Redacted in Debug; never serialised back.
    #[serde(default)]
    pub new_password: Secret,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_stagger")]
    pub stagger_ms: u64,
    #[serde(default = "default_timeout")]
    pub timeout_sec: u64,
    #[serde(default = "default_true")]
    pub verify: bool,
    #[serde(default = "default_verify_timeout")]
    pub verify_timeout_sec: u64,
    #[serde(default = "default_max_log")]
    pub max_log_bytes: usize,
}

impl AdminRequest {
    pub fn new(ips: Vec<String>, action: AdminAction) -> Self {
        Self {
            ips,
            action,
            dry_run: None,
            confirm: Confirmation::default(),
            new_password: Secret::Unknown,
            concurrency: default_concurrency(),
            stagger_ms: default_stagger(),
            timeout_sec: default_timeout(),
            verify: true,
            verify_timeout_sec: default_verify_timeout(),
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
        }
    }
    pub fn dry_run(&self) -> bool {
        self.dry_run
            .unwrap_or_else(|| self.action.kind().default_dry_run())
    }
}

/// Runner knobs the app never changes (tests shorten them).
#[derive(Debug, Clone)]
pub struct AdminOptions {
    /// Send on paths not yet confirmed on real hardware (mock tests only).
    pub allow_unconfirmed: bool,
    pub verify_interval: Duration,
    /// How long the AxeOS log socket is read.
    pub log_window: Duration,
}

impl Default for AdminOptions {
    fn default() -> Self {
        Self {
            allow_unconfirmed: false,
            verify_interval: Duration::from_secs(5),
            log_window: Duration::from_secs(4),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminOutcome {
    pub ip: String,
    pub action: AdminAction,
    pub status: ActionStatus,
    pub msg: Option<String>,
    /// Path that carried the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths_tried: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashboards: Option<Vec<Hashboard>>,
    /// Log text (the tail, ANSI colours stripped). UI only: never audited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
    #[serde(default)]
    pub logs_truncated: bool,
}

impl AdminOutcome {
    pub fn new(
        ip: impl Into<String>,
        action: AdminAction,
        status: ActionStatus,
        msg: Option<String>,
    ) -> Self {
        Self {
            ip: ip.into(),
            action,
            status,
            msg,
            route: None,
            paths_tried: Vec::new(),
            hashboards: None,
            logs: None,
            logs_truncated: false,
        }
    }
}

pub type AdminSink = Arc<dyn Fn(AdminOutcome) + Send + Sync>;

/// Remove ANSI colour / control sequences (ESP-Miner colours every line).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Keep the last `max` bytes (on a char boundary, from a line start when
/// one is near). Returns (text, truncated).
pub fn tail(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    if let Some(nl) = s[start..].find('\n').filter(|n| *n < 512) {
        start += nl + 1;
    }
    (s[start..].to_string(), true)
}

/// A new password the miner can take: 1..=64 printable characters.
pub fn check_new_password(p: &Secret) -> Result<&str, String> {
    match p {
        Secret::Value(v) if v.is_empty() => Err("new password is empty".into()),
        Secret::Value(v) if v.chars().count() > 64 => {
            Err("new password is longer than 64 characters".into())
        }
        Secret::Value(v) if v.chars().any(|c| c.is_control()) => {
            Err("new password contains control characters".into())
        }
        Secret::Value(v) => Ok(v),
        Secret::Redacted => Err("new password came back redacted; type it again".into()),
        Secret::Unknown => Err("no new password given".into()),
    }
}

struct Ctx<'a> {
    run_id: &'a str,
    req: &'a AdminRequest,
    dry_run: bool,
    opts: &'a AdminOptions,
    audit: &'a dyn AuditSink,
    store: &'a dyn CredentialStore,
    cache: &'a DeviceCache,
}

fn audit(
    c: &Ctx<'_>,
    o: &AdminOutcome,
    profile: Option<&DeviceProfile>,
    phase: &str,
) -> std::io::Result<()> {
    let kind = c.req.action.kind();
    let requested = match c.req.action {
        // Only that a password was given; never its value.
        AdminAction::ChangePassword => {
            Some(serde_json::json!({ "new_password": c.req.new_password }))
        }
        _ => None,
    };
    c.audit.record(&AuditRecord {
        ts_ms: now_ms(),
        run_id: c.run_id.to_string(),
        ip: o.ip.clone(),
        action: kind.name().into(),
        phase: phase.into(),
        dry_run: c.dry_run,
        irreversible: kind.irreversible(),
        device_class: profile.map(|p| p.class),
        model: profile.map(|p| p.model.clone()),
        firmware: profile.map(|p| p.firmware.clone()),
        requested,
        snapshot_id: None,
        pre_state: None,
        paths_tried: o.paths_tried.clone(),
        outcome: if phase == "attempt" {
            "pending".into()
        } else {
            serde_json::to_value(o.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        },
        msg: o.msg.clone(),
    })
}

/// Run `req.action` against every IP. Returns the final outcome per IP, in
/// input order. Every final outcome is audited and sent to `sink`.
#[allow(clippy::too_many_arguments)]
pub async fn run_admin_job(
    run_id: &str,
    ips: Vec<IpAddr>,
    req: AdminRequest,
    resolver: Arc<Resolver>,
    store: Arc<dyn CredentialStore>,
    audit_sink: Arc<dyn AuditSink>,
    opts: AdminOptions,
    cancel: CancellationToken,
    sink: AdminSink,
) -> Vec<AdminOutcome> {
    let action = req.action;
    let dry_run = req.dry_run();
    for ip in &ips {
        sink(AdminOutcome::new(
            ip.to_string(),
            action,
            ActionStatus::Queued,
            None,
        ));
    }
    let c = Ctx {
        run_id,
        req: &req,
        dry_run,
        opts: &opts,
        audit: audit_sink.as_ref(),
        store: store.as_ref(),
        cache: resolver.cache.as_ref(),
    };
    let refuse_all = |msg: String| -> Vec<AdminOutcome> {
        ips.iter()
            .map(|ip| {
                let o = AdminOutcome::new(
                    ip.to_string(),
                    action,
                    ActionStatus::Refused,
                    Some(msg.clone()),
                );
                if let Err(e) = audit(&c, &o, None, "outcome") {
                    eprintln!("audit: failed to record {} for {ip}: {e}", action.name());
                }
                sink(o.clone());
                o
            })
            .collect()
    };
    if let Err(r) = preflight(action.kind(), ips.len(), dry_run, &req.confirm) {
        return refuse_all(r.to_string());
    }
    if action == AdminAction::ChangePassword && !dry_run {
        if let Err(e) = check_new_password(&req.new_password) {
            return refuse_all(e);
        }
    }
    let timeout = Duration::from_secs(req.timeout_sec.max(1));
    let stagger = Duration::from_millis(req.stagger_ms);
    let start = tokio::time::Instant::now();
    let c = &c;
    let resolver = &resolver;
    let mut results = stream::iter(ips.iter().copied().enumerate())
        .map(|(i, ip)| {
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                let slot = start + stagger * (i as u32);
                let mut profile = None;
                let mut o = tokio::select! {
                    _ = cancel.cancelled() => AdminOutcome::new(ip.to_string(), action, ActionStatus::Cancelled, None),
                    o = async {
                        tokio::time::sleep_until(slot).await;
                        one(c, ip, resolver, timeout, &sink, &mut profile).await
                    } => o,
                };
                if let Err(e) = audit(c, &o, profile.as_ref(), "outcome") {
                    eprintln!("audit: failed to record {} for {ip}: {e}", action.name());
                    o.msg = Some(format!("{} (audit record failed: {e})", o.msg.unwrap_or_default()));
                }
                sink(o.clone());
                (i, o)
            }
        })
        .buffer_unordered(req.concurrency.clamp(1, 64))
        .collect::<Vec<_>>()
        .await;
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, o)| o).collect()
}

async fn one(
    c: &Ctx<'_>,
    ip: IpAddr,
    resolver: &Resolver,
    timeout: Duration,
    sink: &AdminSink,
    profile_out: &mut Option<DeviceProfile>,
) -> AdminOutcome {
    let action = c.req.action;
    let out = |status, msg: Option<String>| AdminOutcome::new(ip.to_string(), action, status, msg);
    let (driver, obs) = match resolver.resolve(ip, timeout).await {
        Resolved::Ready { driver, obs } => (driver, obs),
        Resolved::Changed(m) => return out(ActionStatus::Refused, Some(m)),
        Resolved::NotFound(m) => return out(ActionStatus::NotFound, m),
        Resolved::Failed(e) => return out(ActionStatus::Failed, Some(e)),
    };
    let profile = driver.profile();
    *profile_out = Some(profile.clone());
    let cached = c.cache.get(ip);
    let mac = obs
        .as_ref()
        .and_then(|o| o.identity.mac.clone())
        .or_else(|| cached.as_ref().and_then(|d| d.identity.mac.clone()));
    let backends = backends_for(driver.as_ref(), cached.and_then(|d| d.miner), ip, timeout);
    let sup = support(&backends, action);
    let mut o = out(ActionStatus::Unsupported, None);
    o.paths_tried = sup.tried.clone();
    let Some(path) = sup.path.clone() else {
        o.msg = Some(format!(
            "not supported on {:?} {}; paths tried: {}",
            profile.class,
            profile.model,
            sup.tried.join("; ")
        ));
        return o;
    };
    o.route = Some(path.clone());
    let key = creds::device_key(ip, mac.as_deref());
    if action.is_read() {
        let login = creds::resolve(c.store, &key, profile.class);
        return read(c, &backends, login.as_ref(), o).await;
    }
    let Some(b) = backends.iter().find(|b| b.id() == path) else {
        unreachable!("selected path is in the list")
    };
    if c.dry_run {
        let what = match action {
            AdminAction::ChangePassword => {
                let given = if check_new_password(&c.req.new_password).is_ok() { "the new password" } else { "a new password (none given yet)" };
                format!("would change the login password via {path}; {given} would be stored in the OS keychain for this device first")
            }
            _ => format!("would factory-reset via {path} (settings, pools and login return to factory defaults)"),
        };
        let note = if sup.confirmed {
            ""
        } else {
            "; path not yet confirmed on real hardware, a live send is refused"
        };
        o.status = ActionStatus::DryRun;
        o.msg = Some(format!("{what}{note}"));
        return o;
    }
    if !sup.confirmed && !c.opts.allow_unconfirmed {
        o.status = ActionStatus::Refused;
        o.msg = Some(format!("{} via {path} is not yet confirmed on real hardware; dry run only until a hardware check", action.name()));
        return o;
    }
    let login = creds::resolve(c.store, &key, profile.class);
    o.status = ActionStatus::Sent;
    // The attempt must be on record before the device is touched.
    if let Err(e) = audit(c, &o, Some(&profile), "attempt") {
        o.status = ActionStatus::Refused;
        o.msg = Some(format!("audit log unavailable, refusing: {e}"));
        return o;
    }
    match action {
        AdminAction::ChangePassword => change_password(c, b.as_ref(), &key, login, o, sink).await,
        AdminAction::FactoryReset => {
            factory_reset(c, b.as_ref(), driver.as_ref(), &key, login, o, sink).await
        }
        _ => unreachable!("reads handled above"),
    }
}

async fn read(
    c: &Ctx<'_>,
    backends: &[Box<dyn AdminBackend>],
    login: Option<&DigestCredentials>,
    mut o: AdminOutcome,
) -> AdminOutcome {
    let action = c.req.action;
    let to = Duration::from_secs(c.req.timeout_sec.max(1)) + c.opts.log_window;
    let mut tried = Vec::new();
    for b in backends {
        if let Err(why) = b.supports(action) {
            tried.push(format!("{}: skipped ({why})", b.id()));
            continue;
        }
        let res = match action {
            AdminAction::Hashboards => match tokio::time::timeout(to, b.hashboards()).await {
                Ok(Ok(boards)) if boards.iter().any(Hashboard::has_data) => {
                    o.msg = Some(summarize_boards(&boards));
                    o.hashboards = Some(boards);
                    Ok(())
                }
                Ok(Ok(boards)) => Err(format!(
                    "returned {} board(s) with no chip, hashrate or temperature data",
                    boards.len()
                )),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            },
            _ => match tokio::time::timeout(to, b.read_logs(login)).await {
                Ok(Ok(text)) => {
                    let text = strip_ansi(&text);
                    let bytes = text.len();
                    let lines = text.lines().count();
                    let (t, cut) = tail(text, c.req.max_log_bytes.max(1024));
                    o.msg = Some(if cut {
                        format!(
                            "{lines} lines, {bytes} bytes (showing the last {} bytes)",
                            t.len()
                        )
                    } else {
                        format!("{lines} lines, {bytes} bytes")
                    });
                    o.logs = Some(t);
                    o.logs_truncated = cut;
                    Ok(())
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            },
        };
        match res {
            Ok(()) => {
                tried.push(format!("{}: ok", b.id()));
                o.status = ActionStatus::Ok;
                o.route = Some(b.id().into());
                o.paths_tried = tried;
                return o;
            }
            Err(e) => tried.push(format!("{}: failed ({e})", b.id())),
        }
    }
    o.status = ActionStatus::Failed;
    o.route = None;
    o.msg = Some(format!("every path failed: {}", tried.join("; ")));
    o.paths_tried = tried;
    o
}

async fn change_password(
    c: &Ctx<'_>,
    b: &dyn AdminBackend,
    key: &str,
    login: Option<DigestCredentials>,
    mut o: AdminOutcome,
    sink: &AdminSink,
) -> AdminOutcome {
    let new = match check_new_password(&c.req.new_password) {
        Ok(p) => p.to_string(),
        Err(e) => {
            o.status = ActionStatus::Refused;
            o.msg = Some(e);
            return o;
        }
    };
    let Some(username) = login
        .as_ref()
        .map(|l| l.username.clone())
        .or_else(|| b.default_username())
    else {
        o.status = ActionStatus::Refused;
        o.msg = Some("no stored login for this device and no known factory username; set its credentials first".into());
        return o;
    };
    let next = DigestCredentials {
        username,
        password: new.clone(),
    };
    // Keychain first: if the miner takes the password and the app then
    // dies, the new password is not lost.
    let pending = creds::pending_key(key);
    if let Err(e) = c.store.set_device(&pending, &next) {
        o.status = ActionStatus::Refused;
        o.msg = Some(format!(
            "OS keychain unavailable, refusing (the new password would be lost): {e}"
        ));
        return o;
    }
    sink(o.clone());
    let to = Duration::from_secs(c.req.timeout_sec.max(1));
    match tokio::time::timeout(to, b.change_password(login.as_ref(), &new)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            let _ = c.store.delete_device(&pending);
            o.status = ActionStatus::Refused;
            o.msg =
                Some("the miner refused the password change; the old login still applies".into());
            return o;
        }
        Ok(Err(e)) => {
            o.status = ActionStatus::Failed;
            o.msg = Some(format!("password change failed ({e}); outcome unknown, the new password is kept in the keychain as pending for this device"));
            return o;
        }
        Err(_) => {
            o.status = ActionStatus::Failed;
            o.msg = Some("password change timed out; outcome unknown, the new password is kept in the keychain as pending for this device".into());
            return o;
        }
    }
    if let Err(e) = c.store.set_device(key, &next) {
        o.status = ActionStatus::Failed;
        o.msg = Some(format!("the miner took the new password but saving it as this device's login failed ({e}); it is still in the keychain as pending"));
        return o;
    }
    let _ = c.store.delete_device(&pending);
    o.status = ActionStatus::Ok;
    o.msg = Some("password changed; new login stored in the OS keychain for this device".into());
    if !c.req.verify {
        return o;
    }
    o.status = ActionStatus::Verifying;
    sink(o.clone());
    match tokio::time::timeout(to, b.check_login(&next)).await {
        Ok(Ok(Some(true))) => {
            o.status = ActionStatus::Verified;
            o.msg = Some("password changed and the new login works; stored in the OS keychain for this device".into());
        }
        Ok(Ok(Some(false))) => {
            o.status = ActionStatus::Unverified;
            o.msg = Some("the miner accepted the change but a login with the new password was rejected; the new password is stored for this device".into());
        }
        Ok(Ok(None)) => {
            o.status = ActionStatus::Unverified;
            o.msg = Some("password changed (miner accepted it); this firmware has no authenticated read to confirm the new login".into());
        }
        Ok(Err(e)) => {
            o.status = ActionStatus::Unverified;
            o.msg = Some(format!(
                "password changed (miner accepted it); confirming the new login failed: {e}"
            ));
        }
        Err(_) => {
            o.status = ActionStatus::Unverified;
            o.msg = Some(
                "password changed (miner accepted it); confirming the new login timed out".into(),
            );
        }
    }
    o
}

async fn factory_reset(
    c: &Ctx<'_>,
    b: &dyn AdminBackend,
    driver: &dyn MinerDriver,
    key: &str,
    login: Option<DigestCredentials>,
    mut o: AdminOutcome,
    sink: &AdminSink,
) -> AdminOutcome {
    sink(o.clone());
    let to = Duration::from_secs(c.req.timeout_sec.max(1));
    match tokio::time::timeout(to, b.factory_reset(login.as_ref())).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            o.status = ActionStatus::Refused;
            o.msg = Some("the miner refused the factory reset".into());
            return o;
        }
        Ok(Err(e)) => {
            o.status = ActionStatus::Failed;
            o.msg = Some(format!("factory reset failed: {e}"));
            return o;
        }
        Err(_) => {
            o.status = ActionStatus::Failed;
            o.msg = Some("factory reset timed out; outcome unknown".into());
            return o;
        }
    }
    // Its identity / settings may change: identify it afresh next time.
    let ip: IpAddr = o.ip.parse().expect("own ip");
    c.cache.invalidate(ip);
    // The login is the factory one again: a stored per-device login would
    // now be wrong (firmware-wide credentials, if any, still apply).
    let forgot =
        matches!(c.store.get_device(key), Ok(Some(_))) && c.store.delete_device(key).is_ok();
    let after = if forgot {
        "settings are factory defaults now (pools, tuning, login); this device's stored login was removed from the keychain"
    } else {
        "settings are factory defaults now (pools, tuning; the login may be the factory one again)"
    };
    o.status = ActionStatus::Ok;
    o.msg = Some(format!("factory reset accepted; {after}"));
    if !c.req.verify {
        return o;
    }
    o.status = ActionStatus::Verifying;
    sink(o.clone());
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(c.req.verify_timeout_sec.clamp(10, 900));
    loop {
        tokio::time::sleep(c.opts.verify_interval).await;
        let h = driver.health().await;
        if h.reachable {
            o.status = ActionStatus::Verified;
            o.msg = Some(format!(
                "factory reset accepted and the miner answers again; {after}"
            ));
            return o;
        }
        if tokio::time::Instant::now() >= deadline {
            o.status = ActionStatus::Unverified;
            o.msg = Some(format!("factory reset accepted but the miner did not answer again before the timeout; {after}"));
            return o;
        }
    }
}
