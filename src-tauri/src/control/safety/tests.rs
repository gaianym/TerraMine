//! Unit tests for the safety layer: limits, policy, snapshot/audit,
//! backend selection, health judging, the engine, the device cache and
//! batch alert grouping.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use super::asic::{asic_accepts, settings_from_configs};
use super::audit::{AuditRecord, AuditSink, JsonlAudit, MemoryAudit};
use super::axeos::{axeos_patch_body, health_from_info, parse_axeos_settings, AxeOsSettings};
use super::batch::build_alert;
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome, WriteStatus};
use super::health::{judge_hashrate, judge_temp, watch, FailureCause, HealthPolicy};
use super::limits::LimitsTable;
use super::policy::{preflight, ActionKind, Confirmation, Refusal};
use super::profile::{classify_class, DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};
use super::snapshot::SnapshotStore;
use crate::control::cache::{CachedDevice, DeviceCache, DriverKind, Resolved, Resolver};
use crate::control::driver::{Capabilities, DriverFactory, Identity, MinerAction, MinerDriver, MinerStatus, Observation};
use crate::control::overrides::bitaxe::parse_axeos_identity;
use crate::control::overrides::s9::{parse_cgminer_ghs, parse_s9_identity};

fn bitaxe() -> DeviceProfile {
    DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS 2.15.3", Some("BM1370"))
}
fn s9() -> DeviceProfile {
    DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None)
}
fn freq(f: u32) -> Settings {
    Settings { frequency_mhz: Some(f), ..Default::default() }
}

// ---------------- profile ----------------

#[test]
fn classify_device_classes() {
    assert_eq!(bitaxe().class, DeviceClass::AxeOs);
    assert_eq!(s9().class, DeviceClass::AntminerStock);
    assert_eq!(classify_class("AntMiner", "BraiinsOS"), DeviceClass::BraiinsOs);
    assert_eq!(classify_class("AntMiner", "LuxOS"), DeviceClass::LuxOs);
    assert_eq!(classify_class("AntMiner", "VNish"), DeviceClass::Vnish);
    assert_eq!(classify_class("WhatsMiner", "Stock"), DeviceClass::Whatsminer);
    assert_eq!(classify_class("NerdAxe", ""), DeviceClass::AxeOs);
    assert_eq!(classify_class("", ""), DeviceClass::Unknown);
}

// ---------------- limits ----------------

#[test]
fn builtin_limits_load_and_are_sane() {
    let t = LimitsTable::builtin();
    assert!(!t.entries.is_empty());
    for e in &t.entries {
        assert!(e.max_temp_c > 0.0 && e.max_temp_c <= 100.0, "{}", e.model);
    }
}

#[test]
fn limits_allow_in_range_and_refuse_out_of_range() {
    let t = LimitsTable::builtin();
    assert!(t.check(&bitaxe(), &freq(575)).is_ok());
    let err = t.check(&bitaxe(), &freq(900)).unwrap_err();
    assert!(err[0].contains("frequency_mhz 900 outside"), "{err:?}");
    let v = Settings { core_voltage_mv: Some(1400), ..Default::default() };
    assert!(t.check(&bitaxe(), &v).is_err());
}

#[test]
fn limits_fail_closed_for_unknown_model_and_unwritable_field() {
    let t = LimitsTable::builtin();
    let unknown = DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS", Some("BM9999"));
    assert!(t.check(&unknown, &freq(500)).unwrap_err()[0].contains("no limits"));
    assert!(t.check(&DeviceProfile::unknown(), &freq(500)).is_err());
    // S9 has no core-voltage range: not writable.
    let v = Settings { core_voltage_mv: Some(900), ..Default::default() };
    assert!(t.check(&s9(), &v).unwrap_err()[0].contains("not writable"));
}

#[test]
fn limits_reject_inverted_ranges() {
    let bad = r#"{"version":1,"entries":[{"class":"axe_os","model":"X","frequency_mhz":[600,400],"max_temp_c":70}]}"#;
    assert!(LimitsTable::from_json(bad).unwrap_err().contains("inverted"));
}

// ---------------- policy ----------------

#[test]
fn phase1_actions_are_never_refused_and_keep_dry_run_default() {
    for k in [ActionKind::Start, ActionKind::Stop, ActionKind::Reboot] {
        assert!(!k.irreversible());
        assert!(!k.default_dry_run(), "phase-1 default must stay unchanged");
        for n in [1, 64] {
            assert!(preflight(k, n, false, &Confirmation::default()).is_ok());
        }
    }
}

#[test]
fn new_write_classes_default_to_dry_run() {
    for k in ActionKind::ALL {
        if !matches!(k, ActionKind::Start | ActionKind::Stop | ActionKind::Reboot) {
            assert!(k.default_dry_run(), "{k:?}");
        }
    }
    let r: SafeWriteRequest = serde_json::from_str(r#"{"change":{"frequency_mhz":500}}"#).unwrap();
    assert!(r.dry_run);
}

#[test]
fn irreversible_needs_typed_name_and_flag_and_single_device() {
    for k in [ActionKind::FactoryReset, ActionKind::FirmwareUpdate, ActionKind::ChangePassword] {
        assert!(k.irreversible());
        let typed = |s: &str, allow| Confirmation { typed: Some(s.into()), allow_irreversible: allow, ..Default::default() };
        assert_eq!(preflight(k, 1, false, &Confirmation::default()), Err(Refusal::IrreversibleNotAllowed));
        assert_eq!(preflight(k, 1, false, &typed(k.name(), false)), Err(Refusal::IrreversibleNotAllowed));
        assert!(matches!(preflight(k, 1, false, &typed("yes", true)), Err(Refusal::TypedConfirmationMismatch { .. })));
        let upper = k.name().to_uppercase();
        assert!(preflight(k, 1, false, &typed(&upper, true)).is_err(), "must match exactly");
        assert_eq!(preflight(k, 2, false, &typed(k.name(), true)), Err(Refusal::IrreversibleBatch(2)));
        assert!(preflight(k, 1, false, &typed(k.name(), true)).is_ok());
        // A dry run sends nothing, so it needs no confirmation.
        assert!(preflight(k, 5, true, &Confirmation::default()).is_ok());
    }
}

// ---------------- settings ----------------

#[test]
fn settings_restrict_mismatch_and_describe() {
    let cur = Settings { frequency_mhz: Some(525), core_voltage_mv: Some(1150), fan_pct: Some(60), ..Default::default() };
    let change = Settings { frequency_mhz: Some(575), ..Default::default() };
    assert_eq!(cur.restrict_to(&change), freq(525));
    assert_eq!(cur.mismatches(&freq(525)), Vec::<String>::new());
    assert_eq!(cur.mismatches(&change).len(), 1);
    assert_eq!(cur.describe_change(&change), "frequency_mhz 525 → 575");
    assert!(Settings::default().is_empty());
}


// ---------------- snapshot ----------------

#[test]
fn snapshot_store_keeps_latest_and_caps() {
    let st = SnapshotStore::in_memory(2);
    let a = st.take("192.0.2.1", &bitaxe(), freq(500)).unwrap();
    let b = st.take("192.0.2.1", &bitaxe(), freq(510)).unwrap();
    assert_eq!(st.latest_for("192.0.2.1").unwrap().id, b.id);
    st.take("192.0.2.2", &bitaxe(), freq(520)).unwrap();
    assert_eq!(st.len(), 2);
    assert!(st.get(&a.id).is_none(), "oldest evicted");
    assert_eq!(st.get(&b.id).unwrap().settings, freq(510));
}

#[test]
fn snapshot_and_audit_persist_as_jsonl() {
    let dir = std::env::temp_dir().join(format!("tm-safety-{}", uuid::Uuid::new_v4()));
    let st = SnapshotStore::with_file(10, dir.join("snapshots.jsonl"));
    let snap = st.take("192.0.2.1", &bitaxe(), freq(525)).unwrap();
    let a = JsonlAudit::new(dir.join("audit/actions.jsonl"));
    a.record(&AuditRecord { ip: "192.0.2.1".into(), outcome: "dry_run".into(), ..Default::default() }).unwrap();
    a.record(&AuditRecord { ip: "192.0.2.2".into(), outcome: "verified".into(), ..Default::default() }).unwrap();
    let snaps = std::fs::read_to_string(dir.join("snapshots.jsonl")).unwrap();
    let back: super::snapshot::Snapshot = serde_json::from_str(snaps.lines().next().unwrap()).unwrap();
    assert_eq!(back, snap);
    let lines: Vec<AuditRecord> = std::fs::read_to_string(a.path())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(), ["dry_run", "verified"]);
    let _ = std::fs::remove_dir_all(dir);
}


// ---------------- fake device ----------------

#[derive(Default)]
struct Dev {
    settings: Mutex<Settings>,
    /// Applied to health while settings differ from `good`.
    bad: Mutex<Option<HealthSample>>,
    good: Mutex<Settings>,
    ignore_writes: Mutex<bool>,
    fail_writes: Mutex<bool>,
    confirmed: Mutex<bool>,
    writes: AtomicUsize,
    /// Health/status polls (to prove nothing reconnects per poll).
    polls: AtomicUsize,
    observes: AtomicUsize,
    identity: Mutex<Identity>,
}

impl Dev {
    fn new() -> Arc<Self> {
        let s = Settings { frequency_mhz: Some(525), core_voltage_mv: Some(1150), ..Default::default() };
        Arc::new(Self {
            settings: Mutex::new(s.clone()),
            good: Mutex::new(s),
            confirmed: Mutex::new(true),
            identity: Mutex::new(Identity { mac: Some("02:00:00:00:00:09".into()), firmware: Some("v2.15.3".into()) }),
            ..Default::default()
        })
    }
    fn sample(&self) -> HealthSample {
        let ok = HealthSample { reachable: true, hashrate_ghs: Some(1000.0), temp_c: Some(55.0) };
        if *self.settings.lock().unwrap() != *self.good.lock().unwrap() {
            if let Some(b) = *self.bad.lock().unwrap() {
                return b;
            }
        }
        ok
    }
}

struct DevSettings(Arc<Dev>);

impl SettingsDriver for DevSettings {
    fn id(&self) -> &'static str {
        "fake-axeos"
    }
    fn hardware_confirmed(&self) -> bool {
        *self.0.confirmed.lock().unwrap()
    }
    fn accepts(&self, p: &DeviceProfile, _: &Settings) -> Result<(), String> {
        if p.class == DeviceClass::AxeOs { Ok(()) } else { Err(format!("not AxeOs: {:?}", p.class)) }
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move { Ok(self.0.settings.lock().unwrap().clone()) })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let s = s.clone();
        Box::pin(async move {
            self.0.writes.fetch_add(1, Ordering::SeqCst);
            if *self.0.fail_writes.lock().unwrap() {
                anyhow::bail!("device rejected write");
            }
            if !*self.0.ignore_writes.lock().unwrap() {
                let mut cur = self.0.settings.lock().unwrap();
                if let Some(f) = s.frequency_mhz {
                    cur.frequency_mhz = Some(f);
                }
                if let Some(v) = s.core_voltage_mv {
                    cur.core_voltage_mv = Some(v);
                }
            }
            Ok(())
        })
    }
}

/// A miner driver over the fake device.
struct DevMiner {
    ip: IpAddr,
    dev: Arc<Dev>,
    profile: DeviceProfile,
}

impl MinerDriver for DevMiner {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities::flags(true, true, true, crate::control::driver::Route::Mock)
    }
    fn execute(&self, _: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn profile(&self) -> DeviceProfile {
        self.profile.clone()
    }
    fn kind(&self) -> DriverKind {
        DriverKind::AxeOs
    }
    fn observe(&self) -> BoxFuture<'_, anyhow::Result<Option<Observation>>> {
        Box::pin(async move {
            self.dev.observes.fetch_add(1, Ordering::SeqCst);
            let identity = self.dev.identity.lock().unwrap().clone();
            Ok(Some(Observation { status: Some(MinerStatus { uptime_s: Some(100), paused: Some(false) }), identity }))
        })
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            self.dev.polls.fetch_add(1, Ordering::SeqCst);
            self.dev.sample()
        })
    }
    fn settings_backends(&self) -> Vec<Box<dyn SettingsDriver>> {
        vec![Box::new(DevSettings(self.dev.clone()))]
    }
}

fn dev_target(dev: Arc<Dev>) -> Target {
    let ip: IpAddr = "192.0.2.9".parse().unwrap();
    Target::new(ip, Arc::new(DevMiner { ip, dev, profile: bitaxe() }))
}

fn fast_policy() -> HealthPolicy {
    HealthPolicy {
        window: Duration::from_secs(60),
        interval: Duration::from_secs(5),
        settle: Duration::from_secs(20),
        min_hashrate_ratio: 0.8,
        max_temp_c: None,
        max_unreachable_polls: 2,
    }
}

fn opts(allow_unconfirmed: bool) -> SafetyOptions {
    SafetyOptions {
        op_timeout: Duration::from_secs(5),
        readback_timeout: Duration::from_secs(10),
        health: fast_policy(),
        rollback_timeout: Duration::from_secs(30),
        allow_unconfirmed_paths: allow_unconfirmed,
    }
}

fn engine(audit: Arc<dyn AuditSink>) -> SafetyEngine {
    SafetyEngine::new(audit, Arc::new(SnapshotStore::in_memory(16)), opts(false))
}

fn apply(change: Settings) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run: false, confirm: raise_ok() }
}

/// These tests exercise other gates; the frequency-raise hold has its own.
fn raise_ok() -> Confirmation {
    Confirmation { allow_frequency_raise: true, ..Default::default() }
}

// ---------------- health ----------------

#[test]
fn health_judges() {
    let p = fast_policy().with_temp_ceiling(70.0);
    let base = HealthSample { reachable: true, hashrate_ghs: Some(1000.0), temp_c: Some(50.0) };
    assert!(judge_temp(&p, &HealthSample { temp_c: Some(71.0), ..base }).is_some());
    assert!(judge_temp(&p, &base).is_none());
    assert!(judge_hashrate(&p, &base, &HealthSample { hashrate_ghs: Some(799.0), ..base }).is_some());
    assert!(judge_hashrate(&p, &base, &HealthSample { hashrate_ghs: Some(800.0), ..base }).is_none());
    // Paused / unknown baseline cannot be judged.
    let zero = HealthSample { hashrate_ghs: Some(0.0), ..base };
    assert!(judge_hashrate(&p, &zero, &HealthSample { hashrate_ghs: Some(0.0), ..base }).is_none());
    assert_eq!(fast_policy().with_temp_ceiling(80.0).with_temp_ceiling(70.0).max_temp_c, Some(70.0));
}

#[tokio::test(start_paused = true)]
async fn health_watch_verdicts_carry_cause() {
    let base = HealthSample { reachable: true, hashrate_ghs: Some(1000.0), temp_c: Some(55.0) };
    let p = fast_policy().with_temp_ceiling(70.0);
    let dev = Dev::new();
    let t = dev_target(dev.clone());
    assert!(watch(t.driver.as_ref(), &p, base).await.is_ok());

    *dev.settings.lock().unwrap() = freq(600);
    *dev.bad.lock().unwrap() = Some(HealthSample { hashrate_ghs: Some(10.0), ..base });
    let u = watch(t.driver.as_ref(), &p, base).await.unwrap_err();
    assert_eq!(u.cause, FailureCause::HashrateDrop);
    assert!(u.reason.contains("hashrate"));
    assert_eq!(u.sample.unwrap().hashrate_ghs, Some(10.0));

    *dev.bad.lock().unwrap() = Some(HealthSample { temp_c: Some(90.0), ..base });
    let t0 = tokio::time::Instant::now();
    let u = watch(t.driver.as_ref(), &p, base).await.unwrap_err();
    assert_eq!(u.cause, FailureCause::Overheat);
    assert!(u.reason.contains("temperature"));
    assert!(t0.elapsed() < p.settle, "overheat fails immediately");

    *dev.bad.lock().unwrap() = Some(HealthSample::unreachable());
    let u = watch(t.driver.as_ref(), &p, base).await.unwrap_err();
    assert_eq!(u.cause, FailureCause::Unreachable);
}

#[test]
fn health_and_identity_parsing_from_device_reads() {
    let info = serde_json::json!({"hashRate": 1180.5, "temp": 61.2, "macAddr": "02:00:00:00:00:09", "version": "v2.15.3"});
    let h = health_from_info(&info);
    assert_eq!((h.reachable, h.hashrate_ghs, h.temp_c), (true, Some(1180.5), Some(61.2)));
    let id = parse_axeos_identity(&info);
    assert_eq!(id.mac.as_deref(), Some("02:00:00:00:00:09"));
    assert_eq!(id.firmware.as_deref(), Some("v2.15.3"));
    let s9 = serde_json::json!({"macaddr": "02:00:00:00:00:0A", "system_filesystem_version": "Thu May 31 2018"});
    let id = parse_s9_identity(&s9);
    assert_eq!(id.mac.as_deref(), Some("02:00:00:00:00:0A"));
    assert_eq!(id.firmware.as_deref(), Some("Thu May 31 2018"));
    assert_eq!(parse_cgminer_ghs(br#"{"SUMMARY":[{"GHS 5s":"13500.5"}]}"#), Some(13500.5));
    assert_eq!(parse_cgminer_ghs(b"{\"SUMMARY\":[{\"GHS 5s\":12000}]}\0"), Some(12000.0));
    assert_eq!(parse_cgminer_ghs(b"garbage"), None);
}

// ---------------- identity / cache ----------------

#[test]
fn identity_mismatch_ignores_formatting_and_missing_fields() {
    let id = |m: Option<&str>, f: Option<&str>| Identity { mac: m.map(Into::into), firmware: f.map(Into::into) };
    let a = id(Some("02:00:00:00:00:09"), Some("v2.15.3"));
    assert_eq!(a.mismatch(&id(Some("02-00-00-00-00-09"), Some("2.15.3"))), None);
    assert_eq!(a.mismatch(&id(Some("02:00:00:00:00:09".to_uppercase().as_str()), Some(" V2.15.3 "))), None);
    assert_eq!(a.mismatch(&id(None, None)), None, "unknown on one side is not a change");
    let why = a.mismatch(&id(Some("02:00:00:00:00:0A"), Some("v2.16.0"))).unwrap();
    assert!(why.contains("MAC 02:00:00:00:00:09 → 02:00:00:00:00:0A"), "{why}");
    assert!(why.contains("firmware v2.15.3 → v2.16.0"), "{why}");
}

/// Factory that hands out `DevMiner`s and counts identifications.
struct DevFactory {
    dev: Arc<Dev>,
    connects: AtomicUsize,
    builds: AtomicUsize,
}

impl DriverFactory for DevFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(Some(Box::new(DevMiner { ip, dev: self.dev.clone(), profile: bitaxe() }) as Box<dyn MinerDriver>)) })
    }
    fn build<'a>(&'a self, ip: IpAddr, c: &'a CachedDevice) -> BoxFuture<'a, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let profile = c.profile.clone();
        Box::pin(async move { Ok(Some(Box::new(DevMiner { ip, dev: self.dev.clone(), profile }) as Box<dyn MinerDriver>)) })
    }
}

fn dev_resolver(dev: Arc<Dev>) -> (Arc<DevFactory>, Resolver) {
    let f = Arc::new(DevFactory { dev, connects: AtomicUsize::new(0), builds: AtomicUsize::new(0) });
    let r = Resolver::new(f.clone(), Arc::new(DeviceCache::default()));
    (f, r)
}

#[tokio::test]
async fn resolver_identifies_once_then_reuses_cached_driver() {
    let dev = Dev::new();
    let (f, r) = dev_resolver(dev.clone());
    let ip: IpAddr = "192.0.2.9".parse().unwrap();
    for _ in 0..5 {
        assert!(matches!(r.resolve(ip, Duration::from_secs(1)).await, Resolved::Ready { .. }));
    }
    assert_eq!(f.connects.load(Ordering::SeqCst), 1, "identified once, not per action");
    assert_eq!(f.builds.load(Ordering::SeqCst), 0, "memoized driver reused");
    assert_eq!(dev.observes.load(Ordering::SeqCst), 5, "one cheap identity check per action");
    let c = r.cache.get(ip).unwrap();
    assert_eq!(c.kind, DriverKind::AxeOs);
    assert_eq!(c.identity.mac.as_deref(), Some("02:00:00:00:00:09"));
}

#[tokio::test]
async fn resolver_uses_discovery_record_without_identifying() {
    let dev = Dev::new();
    let (f, r) = dev_resolver(dev.clone());
    let ip: IpAddr = "192.0.2.9".parse().unwrap();
    r.cache.put(
        ip,
        CachedDevice {
            profile: bitaxe(),
            // Discovery formats differ slightly (asic-rs MacAddr / no "v").
            identity: Identity { mac: Some("02-00-00-00-00-09".into()), firmware: Some("2.15.3".into()) },
            kind: DriverKind::AxeOs,
            seen_at_ms: 1,
            miner: None,
            driver: None,
        },
    );
    for _ in 0..3 {
        assert!(matches!(r.resolve(ip, Duration::from_secs(1)).await, Resolved::Ready { .. }));
    }
    assert_eq!(f.connects.load(Ordering::SeqCst), 0, "discovery identity reused");
    assert_eq!(f.builds.load(Ordering::SeqCst), 1, "driver built from the cache once");
    assert!(r.cache.get(ip).unwrap().seen_at_ms > 1);
}

#[tokio::test]
async fn resolver_refuses_and_reidentifies_once_on_identity_change() {
    let dev = Dev::new();
    let (f, r) = dev_resolver(dev.clone());
    let ip: IpAddr = "192.0.2.9".parse().unwrap();
    assert!(matches!(r.resolve(ip, Duration::from_secs(1)).await, Resolved::Ready { .. }));
    dev.identity.lock().unwrap().mac = Some("02:00:00:00:00:77".into());
    match r.resolve(ip, Duration::from_secs(1)).await {
        Resolved::Changed(m) => {
            assert!(m.contains("MAC 02:00:00:00:00:09 → 02:00:00:00:00:77"), "{m}");
            assert!(m.contains("re-identified as Bitaxe"), "{m}");
            assert!(m.contains("not sent this run"), "{m}");
        }
        other => panic!("expected Changed, got {}", resolved_name(&other)),
    }
    assert_eq!(f.connects.load(Ordering::SeqCst), 2, "re-identified exactly once");
    // The next run uses the fresh identity without identifying again.
    assert!(matches!(r.resolve(ip, Duration::from_secs(1)).await, Resolved::Ready { .. }));
    assert_eq!(f.connects.load(Ordering::SeqCst), 2);
}

fn resolved_name(r: &Resolved) -> &'static str {
    match r {
        Resolved::Ready { .. } => "Ready",
        Resolved::Changed(_) => "Changed",
        Resolved::NotFound(_) => "NotFound",
        Resolved::Failed(_) => "Failed",
    }
}

#[test]
fn cache_others_excludes_job_and_orders_by_recency() {
    let c = DeviceCache::default();
    let mk = |t| CachedDevice { profile: bitaxe(), identity: Identity::default(), kind: DriverKind::AxeOs, seen_at_ms: t, miner: None, driver: None };
    let ips: Vec<IpAddr> = (1..=5).map(|i| format!("192.0.2.{i}").parse().unwrap()).collect();
    for (i, ip) in ips.iter().enumerate() {
        c.put(*ip, mk(i as u64 + 1));
    }
    assert_eq!(c.others(&ips[3..], 2), vec![ips[2], ips[1]]);
    assert_eq!(c.others(&ips, 3), Vec::<IpAddr>::new());
    assert_eq!(DriverKind::for_device("Bitaxe", "Gamma"), DriverKind::AxeOs);
    assert_eq!(DriverKind::for_device("AntMiner", "Antminer S9"), DriverKind::S9Stock);
    assert_eq!(DriverKind::for_device("AntMiner", "Antminer S19"), DriverKind::AsicRs);
}

// ---------------- selection rules ----------------

#[test]
fn asic_rs_accepts_only_what_it_supports() {
    use super::asic::NativeSettingsFlags as F;
    let power = Settings { power_limit_w: Some(3000), ..Default::default() };
    let pct = Settings { tuning_pct: Some(80), ..Default::default() };
    let fan = Settings { fan_pct: Some(70), ..Default::default() };
    let all = F { pools: false, timezone: false, power_limit: true, tuning_percent: true, fan: true, auto_target: true, temperature_config: false };
    assert!(asic_accepts(&power, F { power_limit: true, ..F::default() }).is_ok());
    assert!(asic_accepts(&power, F { tuning_percent: true, fan: true, ..F::default() })
        .unwrap_err()
        .contains("supports_set_power_limit() = false"));
    assert!(asic_accepts(&pct, F { tuning_percent: true, ..F::default() }).is_ok());
    assert!(asic_accepts(&pct, F { power_limit: true, fan: true, ..F::default() })
        .unwrap_err()
        .contains("supports_set_tuning_percent() = false"));
    assert!(asic_accepts(&fan, all).is_ok());
    assert!(asic_accepts(&fan, F { fan: false, ..all }).unwrap_err().contains("fan config not supported"));
    assert!(asic_accepts(&freq(500), all).unwrap_err().contains("no manual frequency"));
    assert!(asic_accepts(&Settings { auto_fan: Some(true), ..Default::default() }, all)
        .unwrap_err()
        .contains("auto fan needs a target temperature"));
}

/// Permanent (bug fixed in PR C): stock Antminer reports
/// `supports_tuning_config() = true` but its `set_tuning_config` rejects a
/// power target, and `supports_set_power_limit() = false`. The old
/// selector accepted a power change there and only failed at write time,
/// after the snapshot. Power now needs `supports_set_power_limit()`.
#[test]
fn power_limit_needs_set_power_limit_not_tuning_config() {
    use super::asic::NativeSettingsFlags as F;
    let power = Settings { power_limit_w: Some(3000), ..Default::default() };
    // Antminer stock v2020 / v2023 in asic-rs 0.8.6: tuning_config yes, power limit no, fan yes.
    let antminer = F { fan: true, ..F::default() };
    assert!(asic_accepts(&power, antminer).is_err());
}

#[test]
fn axeos_override_is_bitaxe_only_and_unconfirmed() {
    let b = AxeOsSettings::new("http://192.0.2.1".into(), reqwest::Client::new());
    assert_eq!(b.id(), "axeos-http");
    assert!(!b.hardware_confirmed(), "nothing is hardware-confirmed yet");
    assert!(b.accepts(&bitaxe(), &freq(500)).is_ok());
    assert!(b.accepts(&s9(), &freq(500)).is_err());
    // Power is mapped to a frequency stand-in, so it cannot be combined with one.
    assert!(b.accepts(&bitaxe(), &Settings { power_limit_w: Some(10), ..Default::default() }).is_ok());
    assert!(b.accepts(&bitaxe(), &Settings { power_limit_w: Some(10), frequency_mhz: Some(500), ..Default::default() }).is_err());
}

#[test]
fn asic_rs_configs_map_to_settings() {
    use asic_rs_core::config::fan::FanConfig;
    let s = settings_from_configs(None, Some(&FanConfig::Manual { fan_speed: 70 }));
    assert_eq!((s.fan_pct, s.auto_fan), (Some(70), Some(false)));
    assert_eq!(settings_from_configs(None, None), Settings::default());
}

#[test]
fn axeos_body_round_trip() {
    let s = Settings { frequency_mhz: Some(575), core_voltage_mv: Some(1200), fan_pct: Some(70), temp_target_c: Some(58), auto_fan: Some(false), ..Default::default() };
    let body = axeos_patch_body(&s, "fanspeed");
    assert_eq!(body, serde_json::json!({"frequency":575,"coreVoltage":1200,"fanspeed":70,"temptarget":58,"autofanspeed":0}));
    assert_eq!(parse_axeos_settings(&body), s);
    let body = axeos_patch_body(&s, "manualFanSpeed");
    assert_eq!(body["manualFanSpeed"], 70);
    assert_eq!(parse_axeos_settings(&body), s);
    assert_eq!(axeos_patch_body(&freq(500), "fanspeed"), serde_json::json!({"frequency":500}));
}

/// A driver with no settings backends (e.g. stock S9).
struct NoBackends(IpAddr);
impl MinerDriver for NoBackends {
    fn ip(&self) -> IpAddr {
        self.0
    }
    fn caps(&self) -> Capabilities {
        Capabilities::default()
    }
    fn execute(&self, _: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn profile(&self) -> DeviceProfile {
        s9()
    }
    fn kind(&self) -> DriverKind {
        DriverKind::S9Stock
    }
}

#[tokio::test(start_paused = true)]
async fn selection_reports_reasons_and_refuses_without_a_path() {
    let e = engine(Arc::new(MemoryAudit::default()));
    let ip: IpAddr = "192.0.2.8".parse().unwrap();
    let o = e.apply_settings("r", &Target::new(ip, Arc::new(NoBackends(ip))), &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert_eq!(o.paths_tried, ["s9-stock: no settings write path for this device"]);

    let dev = Dev::new();
    *dev.confirmed.lock().unwrap() = false;
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::NoPath);
    assert_eq!(o.paths_tried, ["fake-axeos: skipped (not yet confirmed on real hardware)"]);
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
    let e2 = SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(4)), opts(true));
    let o = e2.apply_settings("r", &dev_target(dev.clone()), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Verified, "explicit opt-in uses it: {o:?}");
    assert_eq!(o.paths_tried, ["fake-axeos: selected"]);
}

// ---------------- engine ----------------

#[tokio::test(start_paused = true)]
async fn engine_verified_change_sticks() {
    let dev = Dev::new();
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(audit.clone());
    let o = e.apply_settings("r1", &dev_target(dev.clone()), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(dev.settings.lock().unwrap().frequency_mhz, Some(575));
    let snap = e.snapshots.get(o.snapshot_id.as_deref().unwrap()).unwrap();
    assert_eq!(snap.settings, freq(525), "snapshot holds only the changed field");
    assert_eq!((o.before.clone(), o.after.clone()), (Some(freq(525)), Some(freq(575))));
    let recs = audit.records();
    assert_eq!(recs.iter().map(|r| r.phase.as_str()).collect::<Vec<_>>(), ["attempt", "outcome"]);
    assert_eq!(recs[1].outcome, "verified");
    assert_eq!(recs[1].device_class, Some(DeviceClass::AxeOs));
    assert_eq!(dev.observes.load(Ordering::SeqCst), 0, "the engine never re-identifies");
}

#[tokio::test(start_paused = true)]
async fn engine_dry_run_and_refusals_touch_nothing() {
    let dev = Dev::new();
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(audit.clone());
    let t = dev_target(dev.clone());
    let dry = SafeWriteRequest { change: freq(575), dry_run: true, confirm: Confirmation::default() };
    let o = e.apply_settings("r", &t, &dry).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    assert_eq!(
        o.msg.as_deref(),
        Some("would set frequency_mhz 525 → 575 via fake-axeos; frequency raise 525 → 575 MHz is held: needs an explicit OK (allow_frequency_raise)")
    );
    for change in [freq(900), Settings::default(), Settings { core_voltage_mv: Some(2000), ..Default::default() }] {
        let o = e.apply_settings("r", &t, &apply(change)).await;
        assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    }
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
    assert!(audit.records().iter().all(|r| r.phase == "outcome"), "no attempt record when nothing is sent");
    assert_eq!(audit.records().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn engine_rolls_back_when_unhealthy_with_cause_and_values() {
    let dev = Dev::new();
    *dev.bad.lock().unwrap() = Some(HealthSample { reachable: true, hashrate_ghs: Some(0.0), temp_c: Some(55.0) });
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(audit.clone());
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("unhealthy after change: hashrate"));
    assert_eq!(o.cause, Some(FailureCause::HashrateDrop));
    assert_eq!((o.before.clone(), o.attempted.clone(), o.after.clone()), (Some(freq(525)), Some(freq(600)), Some(freq(525))));
    assert_eq!(o.sample.unwrap().hashrate_ghs, Some(0.0));
    assert!(o.window.is_some());
    assert_eq!(*dev.settings.lock().unwrap(), *dev.good.lock().unwrap(), "snapshot restored");
    assert_eq!(dev.writes.load(Ordering::SeqCst), 2);
    assert_eq!(audit.records().last().unwrap().outcome, "rolled_back");
    assert!(dev.polls.load(Ordering::SeqCst) > 3, "health polled on the same driver");
}

#[tokio::test(start_paused = true)]
async fn engine_rolls_back_on_read_back_mismatch_and_write_error() {
    let dev = Dev::new();
    *dev.ignore_writes.lock().unwrap() = true;
    let e = engine(Arc::new(MemoryAudit::default()));
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::ReadBackMismatch));
    assert!(o.msg.unwrap().contains("read-back failed: mismatch"));

    let dev = Dev::new();
    *dev.fail_writes.lock().unwrap() = true;
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(freq(575))).await;
    // Rollback writes fail too, so it must say so loudly.
    assert_eq!(o.status, WriteStatus::RollbackFailed, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::RollbackFailed));
    assert_eq!(o.trigger, Some(FailureCause::WriteFailed));
    assert!(o.msg.unwrap().contains("ROLLBACK FAILED"));
}

struct BrokenAudit;
impl AuditSink for BrokenAudit {
    fn record(&self, _: &AuditRecord) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
}

#[tokio::test(start_paused = true)]
async fn engine_refuses_write_without_audit_trail() {
    let dev = Dev::new();
    let e = engine(Arc::new(BrokenAudit));
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert!(o.msg.unwrap().contains("audit log unavailable"));
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn engine_refuses_when_snapshot_cannot_capture_field() {
    let dev = Dev::new();
    let e = engine(Arc::new(MemoryAudit::default()));
    // Device does not report fan speed, so a fan change cannot be undone.
    let o = e.apply_settings("r", &dev_target(dev.clone()), &apply(Settings { fan_pct: Some(80), ..Default::default() })).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(o.msg.unwrap().contains("cannot snapshot"));
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
}

// ---------------- batch alert ----------------

fn failed(ip: &str, status: WriteStatus, cause: FailureCause) -> WriteOutcome {
    let mut o = WriteOutcome::new(ip, status, Some(format!("{} happened", cause.label())));
    o.cause = Some(cause);
    o.before = Some(freq(525));
    o.attempted = Some(freq(600));
    o.after = Some(freq(525));
    o
}

#[test]
fn alert_groups_failures_by_cause_with_values() {
    let req = apply(freq(600));
    let ok = WriteOutcome::new("192.0.2.1", WriteStatus::Verified, None);
    assert!(build_alert("r", std::slice::from_ref(&ok), &req, &[]).is_none(), "no alert when nothing failed");
    let outs = vec![
        ok,
        failed("192.0.2.2", WriteStatus::RolledBack, FailureCause::Overheat),
        failed("192.0.2.3", WriteStatus::RolledBack, FailureCause::Overheat),
        failed("192.0.2.4", WriteStatus::RolledBack, FailureCause::ReadBackMismatch),
        failed("192.0.2.5", WriteStatus::RollbackFailed, FailureCause::RollbackFailed),
    ];
    let a = build_alert("r", &outs, &req, &[]).unwrap();
    assert_eq!(a.headline, "3 of 5 rolled back, 1 rollback failed");
    assert_eq!((a.total, a.rolled_back, a.rollback_failed, a.halted), (5, 3, 1, 0));
    let g: Vec<_> = a.groups.iter().map(|g| (g.cause, g.count)).collect();
    assert!(g.contains(&(FailureCause::Overheat, 2)) && g.contains(&(FailureCause::ReadBackMismatch, 1)));
    assert!(g.contains(&(FailureCause::RollbackFailed, 1)));
    let m = &a.groups.iter().find(|g| g.cause == FailureCause::Overheat).unwrap().miners[0];
    assert_eq!((m.before.clone(), m.attempted.clone(), m.after.clone()), (Some(freq(525)), Some(freq(600)), Some(freq(525))));
    assert_eq!(a.retry.ips.len(), 4, "retry covers failed miners only");
    assert!(a.retry.dry_run, "retry starts as a dry run");
    assert!(!a.likely_external && a.external_note.is_none());
}
