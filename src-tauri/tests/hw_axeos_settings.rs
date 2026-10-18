//! Hardware checks (ignored; run by hand against ONE Bitaxe on AxeOS 2.15.x):
//!   TM_HW_BITAXE_IP=<ip> cargo test --test hw_axeos_settings -- --ignored --nocapture --test-threads=1
//!
//! Both go through TerraMine's real safety pipeline (SafetyEngine: path
//! selection, limits, snapshot, write, read-back, health watch, automatic
//! rollback), with not-yet-confirmed routes allowed for this test only.
//!
//! - `hw_axeos_fan_manual_then_auto`: manual fan at or above the current
//!   measured fan % (never below 30%), read back, ~60 s health watch, then
//!   auto fan with the original target and manual % restored.
//! - `hw_axeos_frequency_down_and_back`: frequency 25 MHz below baseline,
//!   read back (stored value and `actualFrequency`), health watch, then back
//!   to exactly the baseline value (the only use of allow_frequency_raise).
//!   Core voltage is never part of a change. With
//!   `TM_HW_AXEOS_PROBE_NO_RESTART=1` the AxeOS backend is wrapped so it
//!   skips the restart-to-apply, to prove whether the firmware applies the
//!   frequency live (actualFrequency must confirm or the engine rolls back).
//!
//! If anything ends neither verified nor rolled back, or the end state is
//! not the baseline, the test PATCHes the baseline values directly.
//!
//! First run: one Bitaxe Gamma (AxeOS v2.15.3, BM1370), Oct 17 2026, on
//! merged main (where the AxeOS backend still restarted to apply, hence
//! the probe for the frequency run). Path axeos-http both times (asic-rs
//! has no fan config or frequency target for Bitaxe).
//! - Fan: manual 50% (measured 49.8%) verified and healthy over 60 s
//!   (59.25-59.6 °C), then auto fan / target 60 °C / manual 49% restored,
//!   verified.
//! - Frequency (probe, no restart): 525 -> 500 MHz, stored and
//!   `actualFrequency` 500 without any restart (uptime kept counting),
//!   healthy over 180 s (1026 GH/s vs 1160 baseline, 60.0 °C); back to
//!   525, `actualFrequency` 525, healthy (1149 GH/s). Core voltage 1150
//!   mV throughout.
//! - End state equal to baseline; no rollback needed.
//!
//! Since then AxeOS 2.15.x sends no restart for frequency-only changes,
//! so the probe flag is only needed against older builds.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use serde_json::{json, Value};
use terra_mine_lib::control::cache::DriverKind;
use terra_mine_lib::control::driver::{
    Capabilities, DriverFactory, MinerAction, MinerDriver, MinerStatus, Observation, OverlayFactory,
};
use terra_mine_lib::control::safety::health::{FailureCause, HealthPolicy};
use terra_mine_lib::control::safety::settings::{HealthSample, Settings, SettingsDriver};
use terra_mine_lib::control::safety::snapshot::SnapshotStore;
use terra_mine_lib::control::safety::{
    Confirmation, DeviceProfile, MemoryAudit, SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome,
    WriteStatus,
};

#[derive(Debug, Clone, PartialEq)]
struct Base {
    frequency: u64,
    core_voltage: u64,
    autofan: u64,
    manual_fan: u64,
    temptarget: u64,
}

fn ip() -> IpAddr {
    std::env::var("TM_HW_BITAXE_IP").expect("TM_HW_BITAXE_IP").parse().unwrap()
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().unwrap()
}

async fn info(c: &reqwest::Client, ip: IpAddr) -> anyhow::Result<Value> {
    Ok(c.get(format!("http://{ip}/api/system/info")).send().await?.error_for_status()?.json().await?)
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_f64).map(|f| f.round() as u64).unwrap_or_else(|| panic!("{k} missing"))
}

fn base_of(v: &Value) -> Base {
    Base {
        frequency: n(v, "frequency"),
        core_voltage: n(v, "coreVoltage"),
        autofan: n(v, "autofanspeed"),
        manual_fan: n(v, "manualFanSpeed"),
        temptarget: n(v, "temptarget"),
    }
}

fn log(t0: Instant, what: &str, v: &Value) {
    let g = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    println!(
        "[t+{:>4}s] {what}: version={} frequency={} actualFrequency={} coreVoltage={} coreVoltageActual={} autofanspeed={} manualFanSpeed={} fanspeed={} fanrpm={} temptarget={} hashRate={} temp={} vrTemp={} power={} uptimeSeconds={} miningPaused={}",
        t0.elapsed().as_secs(),
        g("version"), g("frequency"), g("actualFrequency"), g("coreVoltage"), g("coreVoltageActual"),
        g("autofanspeed"), g("manualFanSpeed"), g("fanspeed"), g("fanrpm"), g("temptarget"),
        g("hashRate"), g("temp"), g("vrTemp"), g("power"), g("uptimeSeconds"), g("miningPaused")
    );
}

/// Last resort: write the baseline settings straight to AxeOS.
async fn emergency_restore(c: &reqwest::Client, ip: IpAddr, b: &Base) {
    let body = json!({
        "frequency": b.frequency, "coreVoltage": b.core_voltage, "autofanspeed": b.autofan,
        "manualFanSpeed": b.manual_fan, "temptarget": b.temptarget
    });
    println!("EMERGENCY RESTORE: PATCH /api/system {body}");
    let r = c.patch(format!("http://{ip}/api/system")).json(&body).send().await;
    println!("  -> {:?}", r.map(|r| r.status()));
}

/// The AxeOS backend without its restart-to-apply (probe only).
struct NoRestart(Box<dyn SettingsDriver>);

impl SettingsDriver for NoRestart {
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn hardware_confirmed(&self) -> bool {
        self.0.hardware_confirmed()
    }
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        self.0.accepts(p, change)
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        self.0.read()
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        self.0.write(s)
    }
    fn translate<'a>(&'a self, change: &'a Settings) -> BoxFuture<'a, Result<(Settings, Option<String>), String>> {
        self.0.translate(change)
    }
    fn restart_to_apply(&self, _change: &Settings) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }
    fn uptime_s(&self) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        self.0.uptime_s()
    }
    fn check_applied<'a>(&'a self, want: &'a Settings) -> BoxFuture<'a, Result<(), String>> {
        self.0.check_applied(want)
    }
}

/// The real driver, with the AxeOS settings backend wrapped in [`NoRestart`].
struct ProbeDriver(Box<dyn MinerDriver>);

impl MinerDriver for ProbeDriver {
    fn ip(&self) -> IpAddr {
        self.0.ip()
    }
    fn caps(&self) -> Capabilities {
        self.0.caps()
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        self.0.execute(action)
    }
    fn status(&self) -> BoxFuture<'_, anyhow::Result<MinerStatus>> {
        self.0.status()
    }
    fn light_state(&self) -> BoxFuture<'_, anyhow::Result<Option<bool>>> {
        self.0.light_state()
    }
    fn paths_tried(&self, action: MinerAction) -> Vec<String> {
        self.0.paths_tried(action)
    }
    fn profile(&self) -> DeviceProfile {
        self.0.profile()
    }
    fn kind(&self) -> DriverKind {
        self.0.kind()
    }
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        self.0.observe()
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        self.0.health()
    }
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        self.0
            .settings_backends()
            .into_iter()
            .map(|b| if b.id() == "axeos-http" { Box::new(NoRestart(b)) as Box<dyn SettingsDriver> } else { b })
            .collect()
    }
}

async fn target(probe_no_restart: bool) -> Target {
    let ip = ip();
    let f = OverlayFactory::new(Duration::from_secs(15));
    let d = f.connect(ip).await.expect("connect").expect("miner found");
    let d: Arc<dyn MinerDriver> = if probe_no_restart { Arc::new(ProbeDriver(d)) } else { Arc::from(d) };
    let t = Target::new(ip, d);
    println!("profile: {:?}", t.profile);
    t
}

fn engine(window: Duration, settle: Duration) -> SafetyEngine {
    let opts = SafetyOptions {
        readback_timeout: Duration::from_secs(90),
        health: HealthPolicy { window, settle, interval: Duration::from_secs(5), ..HealthPolicy::default() },
        // Routes not yet hardware-confirmed: enabled for this test only.
        allow_unconfirmed_paths: true,
        ..SafetyOptions::default()
    };
    SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(16)), opts)
}

async fn apply(e: &SafetyEngine, t: &Target, change: Settings, confirm: Confirmation, t0: Instant, label: &str) -> WriteOutcome {
    let dry = e.apply_settings("hw-dry", t, &SafeWriteRequest { change: change.clone(), dry_run: true, confirm: confirm.clone() }).await;
    println!("[t+{:>4}s] {label} dry run: {:?} {:?} paths={:?}", t0.elapsed().as_secs(), dry.status, dry.msg, dry.paths_tried);
    assert_eq!(dry.status, WriteStatus::DryRun, "{dry:?}");
    let o = e.apply_settings("hw", t, &SafeWriteRequest { change, dry_run: false, confirm }).await;
    println!(
        "[t+{:>4}s] {label}: {:?} msg={:?} paths={:?} before={:?} attempted={:?} after={:?} cause={:?}",
        t0.elapsed().as_secs(), o.status, o.msg, o.paths_tried, o.before, o.attempted, o.after, o.cause
    );
    assert!(o.paths_tried.iter().any(|p| p == "axeos-http: selected"), "{:?}", o.paths_tried);
    o
}

/// Final state must equal the baseline settings; otherwise restore directly.
async fn finish(c: &reqwest::Client, base: &Base, t0: Instant, mut problems: Vec<String>) {
    let ip = ip();
    let end = info(c, ip).await.expect("final info");
    log(t0, "final", &end);
    if base_of(&end) != *base {
        problems.push(format!("end state {:?} != baseline {:?}", base_of(&end), base));
        emergency_restore(c, ip, base).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let again = info(c, ip).await.expect("info after restore");
        log(t0, "after emergency restore", &again);
    }
    assert!(problems.is_empty(), "{problems:?}");
}

fn ok_or_rolled_back(o: &WriteOutcome, label: &str, problems: &mut Vec<String>) -> bool {
    match o.status {
        WriteStatus::Verified => true,
        WriteStatus::RolledBack => {
            problems.push(format!("{label}: rolled back ({:?})", o.msg));
            false
        }
        s => {
            problems.push(format!("{label}: {s:?} ({:?})", o.msg));
            false
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real hardware: set TM_HW_BITAXE_IP"]
async fn hw_axeos_fan_manual_then_auto() {
    let (ip, c, t0) = (ip(), http(), Instant::now());
    let v = info(&c, ip).await.expect("baseline");
    log(t0, "baseline", &v);
    let base = base_of(&v);
    assert_eq!(v["miningPaused"], json!(false), "must start hashing");
    let measured = v["fanspeed"].as_f64().expect("fanspeed");
    // At or above the current fan %, never below 30%: round up to a 5% step.
    let manual = (((measured.ceil() as u32).max(base.manual_fan as u32).max(30)).div_ceil(5) * 5).min(100);
    assert!(f64::from(manual) >= measured && manual >= 30);
    println!("manual fan target {manual}% (measured {measured:.1}%)");

    let t = target(false).await;
    let e = engine(Duration::from_secs(60), Duration::from_secs(20));
    let mut problems = Vec::new();
    let set = Settings { auto_fan: Some(false), fan_pct: Some(manual), ..Default::default() };
    let o = apply(&e, &t, set, Confirmation::default(), t0, "fan manual").await;
    if ok_or_rolled_back(&o, "fan manual", &mut problems) {
        log(t0, "after manual", &info(&c, ip).await.unwrap());
        let restore = Settings {
            auto_fan: Some(base.autofan != 0),
            fan_pct: Some(base.manual_fan as u32),
            temp_target_c: Some(base.temptarget as u32),
            ..Default::default()
        };
        let o = apply(&e, &t, restore, Confirmation::default(), t0, "fan restore").await;
        ok_or_rolled_back(&o, "fan restore", &mut problems);
    }
    finish(&c, &base, t0, problems).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real hardware: set TM_HW_BITAXE_IP"]
async fn hw_axeos_frequency_down_and_back() {
    let (ip, c, t0) = (ip(), http(), Instant::now());
    let probe = std::env::var("TM_HW_AXEOS_PROBE_NO_RESTART").is_ok_and(|v| v == "1");
    let v = info(&c, ip).await.expect("baseline");
    log(t0, "baseline", &v);
    let base = base_of(&v);
    assert_eq!(v["miningPaused"], json!(false), "must start hashing");
    let up0 = n(&v, "uptimeSeconds");
    let low = base.frequency as u32 - 25;

    let t = target(probe).await;
    let e = engine(Duration::from_secs(180), Duration::from_secs(90));
    let mut problems = Vec::new();
    let s0 = Instant::now();
    let mut o = apply(&e, &t, Settings { frequency_mhz: Some(low), ..Default::default() }, Confirmation::default(), t0, "frequency down").await;
    let mut t = t;
    if probe && o.status == WriteStatus::RolledBack && o.cause == Some(FailureCause::ReadBackMismatch) {
        // Not applied live: the restart is needed. Rolled back already; go
        // again through the stock backend (PATCH + restart).
        println!("[t+{:>4}s] live apply NOT confirmed: restart is required; retrying with restart", t0.elapsed().as_secs());
        t = target(false).await;
        o = apply(&e, &t, Settings { frequency_mhz: Some(low), ..Default::default() }, Confirmation::default(), t0, "frequency down (restart)").await;
    }
    let after = info(&c, ip).await.unwrap();
    log(t0, "after down", &after);
    let up1 = n(&after, "uptimeSeconds");
    let restarted = up1 < up0 + s0.elapsed().as_secs().saturating_sub(5);
    println!("restart during apply: {restarted} (uptime {up0}s -> {up1}s over {}s; probe_no_restart={probe})", s0.elapsed().as_secs());
    if ok_or_rolled_back(&o, "frequency down", &mut problems) {
        assert_eq!(n(&after, "frequency"), u64::from(low));
        println!("actualFrequency after down: {}", after["actualFrequency"]);
        // Back to exactly the baseline value: the one place a raise is allowed.
        let back = Confirmation { allow_frequency_raise: true, ..Default::default() };
        let o = apply(&e, &t, Settings { frequency_mhz: Some(base.frequency as u32), ..Default::default() }, back, t0, "frequency restore").await;
        ok_or_rolled_back(&o, "frequency restore", &mut problems);
    }
    let end = info(&c, ip).await.unwrap();
    assert_eq!(n(&end, "coreVoltage"), base.core_voltage, "core voltage unchanged");
    finish(&c, &base, t0, problems).await;
}
