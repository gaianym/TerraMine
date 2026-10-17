//! Unit tests for each safety piece (no network).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use super::audit::{AuditRecord, AuditSink, JsonlAudit, MemoryAudit};
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, WriteStatus};
use super::health::{judge_hashrate, judge_temp, watch, HealthPolicy};
use super::limits::LimitsTable;
use super::paths::{axeos_patch_body, parse_axeos_settings, Attempt, FallbackChain, Open, Target, WritePath};
use super::policy::{preflight, ActionKind, Confirmation, Refusal};
use super::profile::{classify_class, DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};
use super::snapshot::SnapshotStore;

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
    assert_eq!(classify_class("AntMiner", "Antminer S19", "BraiinsOS"), DeviceClass::BraiinsOs);
    assert_eq!(classify_class("AntMiner", "Antminer S19", "LuxOS"), DeviceClass::LuxOs);
    assert_eq!(classify_class("AntMiner", "Antminer S19", "VNish"), DeviceClass::Vnish);
    assert_eq!(classify_class("WhatsMiner", "M30S", "Stock"), DeviceClass::Whatsminer);
    assert_eq!(classify_class("NerdAxe", "NerdQAxe++", ""), DeviceClass::AxeOs);
    assert_eq!(classify_class("", "", ""), DeviceClass::Unknown);
    assert!(DeviceClass::AntminerStock.is_cgminer_family());
    assert!(!DeviceClass::AxeOs.is_cgminer_family());
    assert!(!DeviceClass::Unknown.is_cgminer_family());
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
        let typed = |s: &str, allow| Confirmation { typed: Some(s.into()), allow_irreversible: allow };
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

#[test]
fn axeos_body_round_trip() {
    let s = Settings { frequency_mhz: Some(575), core_voltage_mv: Some(1200), fan_pct: Some(70), auto_fan: Some(false), power_limit_w: None };
    let body = axeos_patch_body(&s);
    assert_eq!(body, serde_json::json!({"frequency":575,"coreVoltage":1200,"fanspeed":70,"autofanspeed":0}));
    assert_eq!(parse_axeos_settings(&body), s);
    assert_eq!(axeos_patch_body(&freq(500)), serde_json::json!({"frequency":500}));
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
    writes: AtomicUsize,
}

impl Dev {
    fn new() -> Arc<Self> {
        let s = Settings { frequency_mhz: Some(525), core_voltage_mv: Some(1150), ..Default::default() };
        Arc::new(Self { settings: Mutex::new(s.clone()), good: Mutex::new(s), ..Default::default() })
    }
}

struct DevDriver(Arc<Dev>);

impl SettingsDriver for DevDriver {
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
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            let ok = HealthSample { reachable: true, hashrate_ghs: Some(1000.0), temp_c: Some(55.0) };
            if *self.0.settings.lock().unwrap() != *self.0.good.lock().unwrap() {
                if let Some(b) = *self.0.bad.lock().unwrap() {
                    return b;
                }
            }
            ok
        })
    }
}

struct FakePath {
    id: &'static str,
    class: DeviceClass,
    dev: Arc<Dev>,
    confirmed: bool,
    opens: AtomicUsize,
}

impl WritePath for FakePath {
    fn id(&self) -> &'static str {
        self.id
    }
    fn hardware_confirmed(&self) -> bool {
        self.confirmed
    }
    fn accepts(&self, p: &DeviceProfile, _: &Settings) -> Result<(), String> {
        if p.class == self.class { Ok(()) } else { Err(format!("not {:?}", self.class)) }
    }
    fn open<'a>(&'a self, _: &'a Target, _: Duration) -> BoxFuture<'a, anyhow::Result<Open>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(Open::Opened(Box::new(DevDriver(self.dev.clone())))) })
    }
}

fn fake_path(id: &'static str, class: DeviceClass, dev: Arc<Dev>) -> Arc<FakePath> {
    Arc::new(FakePath { id, class, dev, confirmed: true, opens: AtomicUsize::new(0) })
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

fn engine(dev: Arc<Dev>, audit: Arc<dyn AuditSink>) -> SafetyEngine {
    let opts = SafetyOptions {
        op_timeout: Duration::from_secs(5),
        readback_timeout: Duration::from_secs(10),
        health: fast_policy(),
        rollback_timeout: Duration::from_secs(30),
        allow_unconfirmed_paths: false,
    };
    let mut e = SafetyEngine::new(audit, Arc::new(SnapshotStore::in_memory(16)), opts);
    e.chain = FallbackChain { paths: vec![fake_path("fake-axeos", DeviceClass::AxeOs, dev)] };
    e
}

fn target() -> Target {
    Target::for_ip("192.0.2.9".parse().unwrap(), bitaxe())
}

fn apply(change: Settings) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run: false, confirm: Confirmation::default() }
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
async fn health_watch_verdicts() {
    let base = HealthSample { reachable: true, hashrate_ghs: Some(1000.0), temp_c: Some(55.0) };
    let p = fast_policy().with_temp_ceiling(70.0);
    let dev = Dev::new();
    assert!(watch(&DevDriver(dev.clone()), &p, base).await.is_ok());

    *dev.settings.lock().unwrap() = freq(600);
    *dev.bad.lock().unwrap() = Some(HealthSample { hashrate_ghs: Some(10.0), ..base });
    assert!(watch(&DevDriver(dev.clone()), &p, base).await.unwrap_err().contains("hashrate"));

    *dev.bad.lock().unwrap() = Some(HealthSample { temp_c: Some(90.0), ..base });
    let t0 = tokio::time::Instant::now();
    assert!(watch(&DevDriver(dev.clone()), &p, base).await.unwrap_err().contains("temperature"));
    assert!(t0.elapsed() < p.settle, "overheat fails immediately");

    *dev.bad.lock().unwrap() = Some(HealthSample::unreachable());
    assert!(watch(&DevDriver(dev.clone()), &p, base).await.unwrap_err().contains("unreachable"));
}

// ---------------- fallback chain ----------------

#[tokio::test]
async fn chain_skips_with_reason_and_reports_paths_tried() {
    let dev = Dev::new();
    let axe = fake_path("fake-axeos", DeviceClass::AxeOs, dev.clone());
    let cg = fake_path("fake-cgminer", DeviceClass::AntminerStock, dev.clone());
    let chain = FallbackChain { paths: vec![axe.clone(), cg.clone()] };

    let (got, tried) = chain.open(&target(), &freq(500), false, Duration::from_secs(1)).await;
    assert_eq!(got.map(|g| g.0), Some("fake-axeos"));
    assert_eq!(tried, vec![Attempt::Opened { path: "fake-axeos" }]);
    assert_eq!(cg.opens.load(Ordering::SeqCst), 0, "cgminer path never opened for AxeOS");

    let t = Target::for_ip("192.0.2.9".parse().unwrap(), DeviceProfile::unknown());
    let (got, tried) = chain.open(&t, &freq(500), false, Duration::from_secs(1)).await;
    assert!(got.is_none());
    assert_eq!(tried.len(), 2);
    assert!(tried.iter().all(|a| matches!(a, Attempt::Skipped { .. })));
    assert_eq!(tried[0].to_string(), "fake-axeos: skipped (not AxeOs)");
    assert_eq!(axe.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unconfirmed_paths_need_explicit_opt_in() {
    let dev = Dev::new();
    let p = Arc::new(FakePath { id: "new", class: DeviceClass::AxeOs, dev, confirmed: false, opens: AtomicUsize::new(0) });
    let chain = FallbackChain { paths: vec![p.clone()] };
    let (got, tried) = chain.open(&target(), &freq(500), false, Duration::from_secs(1)).await;
    assert!(got.is_none());
    assert!(tried[0].to_string().contains("not yet confirmed on real hardware"));
    assert_eq!(p.opens.load(Ordering::SeqCst), 0);
    let (got, _) = chain.open(&target(), &freq(500), true, Duration::from_secs(1)).await;
    assert!(got.is_some());
}

#[test]
fn builtin_paths_gate_by_class_before_any_io() {
    let chain = FallbackChain::builtin();
    let ids: Vec<_> = chain.paths.iter().map(|p| p.id()).collect();
    assert_eq!(ids, ["axeos-http", "cgminer-rpc"]);
    let axe = &chain.paths[0];
    let cg = &chain.paths[1];
    assert!(axe.accepts(&bitaxe(), &freq(500)).is_ok());
    assert!(axe.accepts(&s9(), &freq(500)).is_err());
    assert!(cg.accepts(&bitaxe(), &freq(500)).is_err());
    assert!(cg.accepts(&s9(), &freq(500)).is_err(), "no settings writes over plain cgminer");
    assert!(chain.paths.iter().all(|p| !p.hardware_confirmed()), "nothing is hardware-confirmed yet");
}

// ---------------- engine ----------------

#[tokio::test(start_paused = true)]
async fn engine_verified_change_sticks() {
    let dev = Dev::new();
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(dev.clone(), audit.clone());
    let o = e.apply_settings("r1", &target(), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(dev.settings.lock().unwrap().frequency_mhz, Some(575));
    let snap = e.snapshots.get(o.snapshot_id.as_deref().unwrap()).unwrap();
    assert_eq!(snap.settings, freq(525), "snapshot holds only the changed field");
    let recs = audit.records();
    assert_eq!(recs.iter().map(|r| r.phase.as_str()).collect::<Vec<_>>(), ["attempt", "outcome"]);
    assert_eq!(recs[1].outcome, "verified");
    assert_eq!(recs[1].device_class, Some(DeviceClass::AxeOs));
}

#[tokio::test(start_paused = true)]
async fn engine_dry_run_and_refusals_touch_nothing() {
    let dev = Dev::new();
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(dev.clone(), audit.clone());
    let dry = SafeWriteRequest { change: freq(575), dry_run: true, confirm: Confirmation::default() };
    let o = e.apply_settings("r", &target(), &dry).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    assert_eq!(o.msg.as_deref(), Some("would set frequency_mhz 525 → 575"));
    for change in [freq(900), Settings::default(), Settings { core_voltage_mv: Some(2000), ..Default::default() }] {
        let o = e.apply_settings("r", &target(), &apply(change)).await;
        assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    }
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
    assert!(audit.records().iter().all(|r| r.phase == "outcome"), "no attempt record when nothing is sent");
    assert_eq!(audit.records().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn engine_rolls_back_when_unhealthy() {
    let dev = Dev::new();
    *dev.bad.lock().unwrap() = Some(HealthSample { reachable: true, hashrate_ghs: Some(0.0), temp_c: Some(55.0) });
    let audit = Arc::new(MemoryAudit::default());
    let e = engine(dev.clone(), audit.clone());
    let o = e.apply_settings("r", &target(), &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("unhealthy after change: hashrate"));
    assert_eq!(*dev.settings.lock().unwrap(), *dev.good.lock().unwrap(), "snapshot restored");
    assert_eq!(dev.writes.load(Ordering::SeqCst), 2);
    assert_eq!(audit.records().last().unwrap().outcome, "rolled_back");
}

#[tokio::test(start_paused = true)]
async fn engine_rolls_back_on_read_back_mismatch_and_write_error() {
    let dev = Dev::new();
    *dev.ignore_writes.lock().unwrap() = true;
    let e = engine(dev.clone(), Arc::new(MemoryAudit::default()));
    let o = e.apply_settings("r", &target(), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.unwrap().contains("read-back failed: mismatch"));

    let dev = Dev::new();
    *dev.fail_writes.lock().unwrap() = true;
    let e = engine(dev.clone(), Arc::new(MemoryAudit::default()));
    let o = e.apply_settings("r", &target(), &apply(freq(575))).await;
    // Rollback writes fail too, so it must say so loudly.
    assert_eq!(o.status, WriteStatus::RollbackFailed, "{o:?}");
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
    let e = engine(dev.clone(), Arc::new(BrokenAudit));
    let o = e.apply_settings("r", &target(), &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert!(o.msg.unwrap().contains("audit log unavailable"));
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn engine_refuses_when_snapshot_cannot_capture_field() {
    let dev = Dev::new();
    let e = engine(dev.clone(), Arc::new(MemoryAudit::default()));
    // Device does not report fan speed, so a fan change cannot be undone.
    let o = e.apply_settings("r", &target(), &apply(Settings { fan_pct: Some(80), ..Default::default() })).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(o.msg.unwrap().contains("cannot snapshot"));
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
}
