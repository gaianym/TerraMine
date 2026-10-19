//! Pools (add / replace / set) and timezone (plan PR F) through the full
//! safety pipeline: batch runner → engine (path, limits, pool operation
//! resolved per miner, dry run, explicit pool OK, audit, snapshot, write,
//! read-back + pool alive check, health watch, rollback) → the asic-rs
//! backend on per-firmware mocks, and the AxeOS override on the 2.15 HTTP
//! mock (primary + fallback slots, restart to apply). Passwords are
//! redacted everywhere they are serialised.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::audit::MemoryAudit;
use super::batch::run_settings_job;
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome, WriteStatus};
use super::health::{FailureCause, HealthPolicy};
use super::mock_native::{NativeFirmware, MOCK_POOL, MOCK_WORKER};
use super::policy::Confirmation;
use super::pools::{apply_op, normalize_url, PoolGroupSpec, PoolOp, PoolSpec, Secret, REDACTED};
use super::power_e2e_tests::{batch, ip_of, rig, Rig, AXE};
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::cache::{DeviceCache, Resolved, Resolver};
use crate::control::mock_miner::{MockKind, MockMiner, MockMinerFactory};

const NEW: &str = "stratum+tcp://new.example.com:3333";

fn spec(url: &str, user: &str, pw: Option<&str>) -> PoolSpec {
    PoolSpec { url: url.into(), user: user.into(), password: pw.map_or(Secret::Unknown, |p| Secret::Value(p.into())) }
}
fn op(o: PoolOp) -> Settings {
    Settings { pool_op: Some(o), ..Default::default() }
}
fn replace0(pw: Option<&str>) -> Settings {
    op(PoolOp::Replace { index: 0, pool: spec(NEW, "acct.w2", pw) })
}
fn tz(z: &str) -> Settings {
    Settings { timezone: Some(z.into()), ..Default::default() }
}
fn group(p: Vec<PoolSpec>) -> Vec<PoolGroupSpec> {
    vec![PoolGroupSpec { name: "default".into(), quota: 1, pools: p }]
}

async fn run_c(r: &Rig, ips: Vec<IpAddr>, change: Settings, dry_run: bool, allow_pools: bool) -> HashMap<IpAddr, WriteOutcome> {
    let req = SafeWriteRequest { change, dry_run, confirm: Confirmation { allow_pool_change: allow_pools, ..Default::default() } };
    let (out, _) =
        run_settings_job("pools-run", ips, req, r.resolver.clone(), r.engine.clone(), batch(None), CancellationToken::new(), Arc::new(|_| {})).await;
    out.into_iter().map(|o| (o.ip.parse().unwrap(), o)).collect()
}

const POOL_CAPABLE: [NativeFirmware; 8] = [
    NativeFirmware::Braiins2507,
    NativeFirmware::Vnish13,
    NativeFirmware::WhatsminerV3,
    NativeFirmware::ProtoV1,
    NativeFirmware::Sealminer2025,
    NativeFirmware::AuradineV1,
    NativeFirmware::AntminerStock,
    NativeFirmware::EpicV1,
];

/// Dry run by default; each class shows the resolved list or "not
/// supported" with paths tried; nothing is written anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn pool_dry_run_by_default_per_class() {
    let req: crate::control::SettingsJobRequest = serde_json::from_value(serde_json::json!({
        "ips": ["192.0.2.100"],
        "change": {"pool_op": {"op": "add", "pool": {"url": NEW, "user": "acct.w2", "password": "hunter2"}}}
    }))
    .unwrap();
    assert!(req.dry_run, "pools are a new write class: dry run by default");
    assert!(!req.confirm.allow_pool_change);

    let r = rig(false, true).await;
    let mut ips: Vec<IpAddr> = POOL_CAPABLE.map(ip_of).to_vec();
    ips.push(ip_of(NativeFirmware::AvalonA));
    ips.push(IpAddr::from(AXE));
    let o = run_c(&r, ips, req.change.clone(), true, false).await;
    for fw in POOL_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::DryRun, "{fw:?}: {x:?}");
        let msg = x.msg.as_deref().unwrap();
        assert!(msg.contains(&format!("pools [{MOCK_POOL} {MOCK_WORKER}] → [{MOCK_POOL} {MOCK_WORKER}, {NEW} acct.w2] via asic-rs")), "{fw:?}: {msg}");
    }
    let a = &o[&ip_of(NativeFirmware::AvalonA)];
    assert_eq!(a.status, WriteStatus::NoPath, "{a:?}");
    assert!(a.paths_tried[0].contains("supports_pools_config() = false"), "{a:?}");
    // AxeOS has exactly two slots: an add on a full miner is refused.
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::Refused, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("3 pools requested; this firmware takes at most 2"), "{x:?}");
    for m in r.miners.values() {
        assert!(m.writes().is_empty(), "{:?}: dry run wrote", m.fw);
    }
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")));
    let json = serde_json::to_string(&o.values().collect::<Vec<_>>()).unwrap();
    assert!(!json.contains("hunter2"), "password leaked into outcomes");
}

/// A live pool change needs allow_pool_change, even on a confirmed path.
#[tokio::test(start_paused = true)]
async fn live_pool_change_needs_explicit_ok() {
    let r = rig(true, false).await;
    let b = NativeFirmware::Braiins2507;
    let o = run_c(&r, vec![ip_of(b)], replace0(None), false, false).await;
    let x = &o[&ip_of(b)];
    assert_eq!(x.status, WriteStatus::Refused, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("allow_pool_change"), "{x:?}");
    assert!(r.m(b).writes().is_empty());
}

/// Replace, verified: one write, read back by URL + worker, pool alive,
/// snapshot = previous list; the password reaches the miner but never the
/// audit log or the outcome.
#[tokio::test(start_paused = true)]
async fn replace_pool_verified_and_password_redacted() {
    let r = rig(true, false).await;
    let b = NativeFirmware::Braiins2507;
    let o = run_c(&r, vec![ip_of(b)], replace0(Some("hunter2")), false, true).await;
    let x = &o[&ip_of(b)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(b).writes(), [format!("set_pools_config [{NEW} acct.w2]")]);
    assert_eq!(r.m(b).pool_passwords(), ["hunter2"]);
    let before = x.before.as_ref().unwrap().pools.as_ref().unwrap();
    assert_eq!(super::pools::flatten(before), [(MOCK_POOL.to_string(), MOCK_WORKER.to_string())]);
    let audit = serde_json::to_string(&r.audit.records()).unwrap();
    assert!(!audit.contains("hunter2"), "password in audit log");
    assert!(audit.contains(REDACTED));
    let out = serde_json::to_string(x).unwrap();
    assert!(!out.contains("hunter2"), "password in outcome");
    assert!(!format!("{x:?}").contains("hunter2"), "password in Debug");
}

/// A pool that never comes alive is rolled back to the previous list.
#[tokio::test(start_paused = true)]
async fn dead_pool_rolls_back() {
    let r = rig(true, false).await;
    let v = NativeFirmware::Vnish13;
    let dead = op(PoolOp::Replace { index: 0, pool: spec("stratum+tcp://dead.example.com:3333", "acct.w2", None) });
    let o = run_c(&r, vec![ip_of(v)], dead, false, true).await;
    let x = &o[&ip_of(v)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert_eq!(x.cause, Some(FailureCause::ReadBackMismatch));
    assert!(x.msg.as_deref().unwrap().contains("none of the new pools is alive"), "{x:?}");
    assert_eq!(r.m(v).pool_list(), [(MOCK_POOL.to_string(), MOCK_WORKER.to_string())]);
    assert_eq!(r.m(v).writes().len(), 2, "change + rollback");
}

/// Ignored write → read-back mismatch → rollback.
#[tokio::test(start_paused = true)]
async fn ignored_pool_write_rolls_back() {
    let r = rig(true, false).await;
    let w = NativeFirmware::WhatsminerV3;
    r.m(w).set_ignore_writes(true);
    let o = run_c(&r, vec![ip_of(w)], replace0(None), false, true).await;
    let x = &o[&ip_of(w)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("pools: got"), "{x:?}");
}

/// Request checks before anything is sent.
#[tokio::test(start_paused = true)]
async fn pool_request_checks() {
    let r = rig(true, false).await;
    let a = ip_of(NativeFirmware::AntminerStock);
    let cases: Vec<(Settings, &str)> = vec![
        (op(PoolOp::Set { groups: group(vec![]) }), "at least one pool"),
        (
            op(PoolOp::Set { groups: group((0..4).map(|i| spec(&format!("stratum+tcp://p{i}.example.com:3333"), "w", None)).collect()) }),
            "takes at most 3",
        ),
        (op(PoolOp::Replace { index: 5, pool: spec(NEW, "w", None) }), "no pool at position 6"),
        (op(PoolOp::Add { pool: spec(MOCK_POOL, MOCK_WORKER, None) }), "already configured"),
        (op(PoolOp::Add { pool: spec("new.example.com", "w", None) }), "needs a host and a port"),
        (op(PoolOp::Add { pool: spec(NEW, " ", None) }), "worker / user is empty"),
        (Settings { fan_pct: Some(50), ..replace0(None) }, "send a pool change on its own"),
    ];
    for (change, want) in cases {
        let o = run_c(&r, vec![a], change.clone(), false, true).await;
        let x = &o[&a];
        assert!(matches!(x.status, WriteStatus::Refused | WriteStatus::NoPath), "{change:?}: {x:?}");
        assert!(x.msg.as_deref().unwrap_or("").contains(want) || x.paths_tried.iter().any(|p| p.contains(want)), "{want}: {x:?}");
    }
    // A password that was redacted on the way out must be re-entered.
    let back: Settings =
        serde_json::from_value(serde_json::json!({"pool_op": {"op": "add", "pool": {"url": NEW, "user": "w", "password": REDACTED}}})).unwrap();
    let o = run_c(&r, vec![a], back, false, true).await;
    assert!(o[&a].msg.as_deref().unwrap().contains("password was redacted; re-enter it"), "{:?}", o[&a]);
    assert!(r.m(NativeFirmware::AntminerStock).writes().is_empty());
}

/// Timezone: asic-rs where supported, must be listed by the miner.
#[tokio::test(start_paused = true)]
async fn timezone_paths_and_checks() {
    let r = rig(true, false).await;
    let b = NativeFirmware::Braiins2507;
    let o = run_c(&r, vec![ip_of(b)], tz("America/Chicago"), false, false).await;
    let x = &o[&ip_of(b)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(b).writes(), ["set_timezone_config America/Chicago"]);
    assert_eq!(x.before, Some(tz("UTC")));

    let o = run_c(&r, vec![ip_of(b)], tz("Asia/Tokyo"), false, false).await;
    assert!(o[&ip_of(b)].msg.as_deref().unwrap().contains("not one this miner accepts"));
    let o = run_c(&r, vec![ip_of(b)], tz("Mars/Base"), true, false).await;
    assert!(o[&ip_of(b)].paths_tried[0].contains("is not an IANA timezone"), "{:?}", o[&ip_of(b)]);

}

/// Timezone where there is no route: Whatsminer (asic-rs says no) and the
/// Bitaxe (AxeOS has no timezone). Real time (multi_thread), not a paused
/// clock: the Bitaxe is the real-socket AxeOS mock, and a paused clock
/// auto-advances past the resolve timeout while its I/O is in flight
/// (macOS CI flake). `paused_clock_tests_use_no_socket_mocks` guards this.
#[tokio::test(flavor = "multi_thread")]
async fn timezone_not_supported_with_paths_tried() {
    let r = rig(true, true).await;
    let o = run_c(&r, vec![ip_of(NativeFirmware::WhatsminerV3), IpAddr::from(AXE)], tz("UTC"), true, false).await;
    let w = &o[&ip_of(NativeFirmware::WhatsminerV3)];
    assert_eq!(w.status, WriteStatus::NoPath);
    assert!(w.paths_tried[0].contains("supports_timezone_config() = false"), "{w:?}");
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::NoPath);
    assert!(x.paths_tried.iter().any(|p| p.contains("AxeOS has no timezone setting")), "{x:?}");
}

// ---------- AxeOS (Bitaxe) live on the 2.15 HTTP mock ----------

fn fast(allow_unconfirmed: bool) -> SafetyOptions {
    SafetyOptions {
        op_timeout: Duration::from_secs(2),
        readback_timeout: Duration::from_secs(3),
        health: HealthPolicy {
            window: Duration::from_millis(500),
            interval: Duration::from_millis(50),
            settle: Duration::from_millis(150),
            min_hashrate_ratio: 0.8,
            max_temp_c: None,
            max_unreachable_polls: 3,
        },
        rollback_timeout: Duration::from_secs(4),
        allow_unconfirmed_paths: allow_unconfirmed,
    }
}

async fn axe_target() -> (MockMiner, Target, SafetyEngine) {
    let axe = MockMiner::spawn(MockKind::AxeOs215, Duration::from_millis(300)).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let ip = IpAddr::from([192, 0, 2, 61]);
    f.add(ip, &axe);
    let resolver = Resolver::new(Arc::new(f), Arc::new(DeviceCache::default()));
    let t = match resolver.resolve(ip, Duration::from_secs(2)).await {
        Resolved::Ready { driver, .. } => Target::new(ip, driver),
        _ => panic!("mock did not resolve"),
    };
    let engine = SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(8)), fast(true));
    (axe, t, engine)
}

fn live(change: Settings) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run: false, confirm: Confirmation { allow_pool_change: true, ..Default::default() } }
}

/// Replace the primary: PATCH with host / port / user (+ password only
/// when given), restart to apply, read back, verified.
#[tokio::test(flavor = "multi_thread")]
async fn axeos_replace_primary_verified() {
    let (axe, t, engine) = axe_target().await;
    let o = engine.apply_settings("axe-pools", &t, &live(replace0(Some("hunter2")))).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(o.paths_tried[0], "axeos-http: selected");
    let st = axe.stratum();
    assert_eq!((st[0].0.as_str(), st[0].1, st[0].2.as_str(), st[0].3.as_str()), ("new.example.com", 3333, "acct.w2", "hunter2"));
    assert_eq!(st[1].0, "backup.example.com", "fallback untouched");
    let patch = axe.patches().join("\n");
    assert!(patch.contains(r#""stratumURL":"new.example.com""#) && patch.contains(r#""stratumPort":3333"#), "{patch}");
    assert!(axe.requests().iter().any(|q| q == "POST /api/system/restart"), "restart to apply");
    assert!(o.msg.as_deref().unwrap().contains("rollback cannot restore the previous one"), "{o:?}");
    assert!(!serde_json::to_string(&o).unwrap().contains("hunter2"));
}

/// New primary unreachable → AxeOS runs on its fallback → rolled back to
/// the previous primary (no password sent on rollback).
#[tokio::test(flavor = "multi_thread")]
async fn axeos_dead_primary_rolls_back() {
    let (axe, t, engine) = axe_target().await;
    let dead = op(PoolOp::Replace { index: 0, pool: spec("stratum+tcp://dead.example.com:3333", "acct.w2", None) });
    let o = engine.apply_settings("axe-pools", &t, &live(dead)).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("fallback pool"), "{o:?}");
    assert_eq!(axe.stratum()[0].0, "pool.example.com");
    assert!(axe.patches().iter().all(|p| !p.contains("Password")), "no password sent: {:?}", axe.patches());
}

/// AxeOS constraints: two slots exactly, stratum+tcp only; the native
/// asic-rs Bitaxe backend never takes a pool write.
#[tokio::test(flavor = "multi_thread")]
async fn axeos_pool_constraints() {
    let (axe, t, engine) = axe_target().await;
    let one = op(PoolOp::Set { groups: group(vec![spec(NEW, "w", None)]) });
    let o = engine.apply_settings("axe-pools", &t, &live(one)).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("primary and a fallback pool"), "{o:?}");
    let tls = op(PoolOp::Replace { index: 1, pool: spec("stratum+ssl://new.example.com:4333", "w", None) });
    let o = engine.apply_settings("axe-pools", &t, &live(tls)).await;
    assert!(o.msg.as_deref().unwrap().contains("stratum+tcp only"), "{o:?}");
    assert!(axe.patches().is_empty());
}

/// Stock S9: no pool path (whole-config rewrite only), reported with the reason.
#[tokio::test(flavor = "multi_thread")]
async fn s9_pools_not_supported_with_reason() {
    let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, Duration::from_millis(300)).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let ip = IpAddr::from([192, 0, 2, 62]);
    f.add(ip, &s9);
    let resolver = Resolver::new(Arc::new(f), Arc::new(DeviceCache::default()));
    let Resolved::Ready { driver, .. } = resolver.resolve(ip, Duration::from_secs(2)).await else { panic!() };
    let engine = SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(8)), fast(true));
    let o = engine.apply_settings("s9-pools", &Target::new(ip, driver), &live(replace0(None))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(o.paths_tried[0].contains("replaces the whole miner config"), "{o:?}");
}

#[test]
fn secret_serialises_redacted_and_round_trips_unknown() {
    let p = spec(NEW, "w", Some("hunter2"));
    let j = serde_json::to_value(&p).unwrap();
    assert_eq!(j["password"], REDACTED);
    assert!(!format!("{p:?}").contains("hunter2"));
    let u = spec(NEW, "w", None);
    assert!(serde_json::to_value(&u).unwrap().get("password").is_none());
    let back: PoolSpec = serde_json::from_value(j).unwrap();
    assert_eq!(back.password, Secret::Redacted);
}

#[test]
fn pool_ops_resolve_against_current_list() {
    let cur = group(vec![spec("pool.example.com:3333", "a", None), spec("stratum+tcp://b.example.com:3333", "b", None)]);
    let out = apply_op(&cur, &PoolOp::Replace { index: 1, pool: spec(NEW, "c", None) }, 3).unwrap();
    assert_eq!(super::pools::flatten(&out)[1], (NEW.to_string(), "c".to_string()));
    assert_eq!(super::pools::flatten(&out)[0].0, "stratum+tcp://pool.example.com:3333", "scheme normalised");
    let added = apply_op(&cur, &PoolOp::Add { pool: spec(NEW, "c", None) }, 3).unwrap();
    assert_eq!(super::pools::flatten(&added).len(), 3);
    assert!(apply_op(&added, &PoolOp::Add { pool: spec("stratum+tcp://d.example.com:1", "d", None) }, 3).is_err());
    assert_eq!(normalize_url("Pool.Example.com:3333"), "stratum+tcp://pool.example.com:3333");
}

/// The snapshot file never holds a pool password; the in-memory snapshot
/// keeps it so a rollback can restore it.
#[test]
fn snapshot_file_redacts_pool_passwords() {
    let dir = std::env::temp_dir().join(format!("tm-snap-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("snapshots.jsonl");
    let store = SnapshotStore::with_file(4, path.clone());
    let p = super::profile::DeviceProfile::classify("AntMiner", "Antminer S19", "BraiinsOS 25.07", None);
    let s = Settings { pools: Some(group(vec![spec(NEW, "w", Some("hunter2"))])), ..Default::default() };
    let snap = store.take("192.0.2.9", &p, s).unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(!file.contains("hunter2"), "{file}");
    assert!(file.contains(REDACTED));
    let mem = store.get(&snap.id).unwrap();
    assert_eq!(mem.settings.pools.unwrap()[0].pools[0].password, Secret::Value("hunter2".into()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}
