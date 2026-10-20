//! Pool divert (the Stop / Start fallback for miners with no native sleep)
//! end to end through the engine's path selection on the asic-rs mocks:
//! dry run by default, explicit pool OK, native route preferred, original
//! pools saved in the (memory) keychain, divert -> restore round trip,
//! read-back with rollback, and audit.

use std::net::IpAddr;
use std::sync::Arc;

use super::divert::{
    divert_groups, is_diverted, run_divert, saved, DivertOp, DivertRequest, DIVERT_URL,
};
use super::engine::{Target, WriteStatus};
use super::mock_native::{MockNativeDriver, NativeFirmware, MOCK_POOL, MOCK_WORKER};
use super::policy::Confirmation;
use super::pools::normalize_url;
use super::power_e2e_tests::{ip_of, rig};
use crate::control::driver::{Capabilities, MinerAction, MinerDriver, Route};
use crate::control::ssh::store::{MemoryKv, SecretKv};

fn target(r: &super::power_e2e_tests::Rig, fw: NativeFirmware) -> Target {
    let ip = ip_of(fw);
    Target::new(ip, Arc::new(MockNativeDriver::new(ip, r.m(fw).clone())))
}

fn req(op: DivertOp, dry_run: bool, ok: bool) -> DivertRequest {
    DivertRequest {
        op,
        dry_run,
        confirm: Confirmation {
            allow_pool_change: ok,
            ..Default::default()
        },
    }
}

/// A driver that does have native sleep / wake.
struct WithSleep(MockNativeDriver, IpAddr);
impl MinerDriver for WithSleep {
    fn ip(&self) -> IpAddr {
        self.1
    }
    fn caps(&self) -> Capabilities {
        Capabilities::none()
            .with(MinerAction::Stop, Route::AsicRs)
            .with(MinerAction::Start, Route::AsicRs)
    }
    fn execute(&self, a: MinerAction) -> futures::future::BoxFuture<'_, anyhow::Result<bool>> {
        self.0.execute(a)
    }
    fn profile(&self) -> super::profile::DeviceProfile {
        self.0.profile()
    }
    fn settings_backends(&self) -> Vec<Box<dyn super::settings::SettingsDriver>> {
        self.0.settings_backends()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn divert_round_trip_restores_the_original_pools() {
    let r = rig(true, false).await;
    let kv = MemoryKv::default();
    let fw = NativeFirmware::Braiins2507;
    let t = target(&r, fw);
    let orig = r.m(fw).pool_list();
    let orig_pw = r.m(fw).pool_passwords();

    // Default request: dry run, nothing written or saved.
    let d: DivertRequest = serde_json::from_value(serde_json::json!({"op": "divert"})).unwrap();
    assert!(d.dry_run);
    let o = run_divert(&r.engine, &kv, "t", &t, &d).await;
    assert_eq!(o.status, WriteStatus::DryRun, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains(DIVERT_URL), "{o:?}");
    assert!(
        o.paths_tried[0].starts_with("native stop: not supported"),
        "{:?}",
        o.paths_tried
    );
    assert!(r.m(fw).writes().is_empty());
    assert!(saved(&kv, &t.ip.to_string()).unwrap().is_none());

    // Live without the pool OK: refused.
    let o = run_divert(
        &r.engine,
        &kv,
        "t",
        &t,
        &req(DivertOp::Divert, false, false),
    )
    .await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert!(o.msg.unwrap().contains("allow_pool_change"));

    // Divert.
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, false, true)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert!(o.snapshot_id.is_some());
    let now = r.m(fw).pool_list();
    assert_eq!(now.len(), 1);
    assert_eq!(normalize_url(&now[0].0), normalize_url(DIVERT_URL));
    let kept = saved(&kv, &t.ip.to_string())
        .unwrap()
        .expect("original saved in the keychain");
    assert_eq!(kept[0].pools[0].user, MOCK_WORKER);
    // Serialised outcome never carries pool passwords.
    assert!(!serde_json::to_string(&o).unwrap().contains("\"x\""));

    // A second divert is refused (it would save the dead pool as original).
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, false, true)).await;
    assert_eq!(o.status, WriteStatus::Refused);

    // Restore: dry run first, then live.
    let o = run_divert(
        &r.engine,
        &kv,
        "t",
        &t,
        &req(DivertOp::Restore, true, false),
    )
    .await;
    assert_eq!(o.status, WriteStatus::DryRun, "{o:?}");
    assert!(o.msg.unwrap().contains(MOCK_WORKER));
    let o = run_divert(
        &r.engine,
        &kv,
        "t",
        &t,
        &req(DivertOp::Restore, false, true),
    )
    .await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.m(fw).pool_list(), orig, "round trip");
    assert_eq!(r.m(fw).pool_passwords(), orig_pw, "pool passwords survive the round trip");
    assert!(
        saved(&kv, &t.ip.to_string()).unwrap().is_none(),
        "saved copy cleared"
    );
    let o = run_divert(
        &r.engine,
        &kv,
        "t",
        &t,
        &req(DivertOp::Restore, false, true),
    )
    .await;
    assert_eq!(o.status, WriteStatus::Refused, "nothing left to restore");

    let recs = r.audit.records();
    assert!(recs
        .iter()
        .any(|x| x.action == "pool_divert" && x.outcome == "verified"));
    assert!(recs
        .iter()
        .any(|x| x.action == "pool_restore" && x.outcome == "verified"));
    assert!(!format!("{recs:?}").contains("password: Value"));
}

#[tokio::test(flavor = "multi_thread")]
async fn divert_prefers_native_sleep_and_unconfirmed_paths_are_dry_run_only() {
    let r = rig(false, false).await;
    let kv = MemoryKv::default();
    let fw = NativeFirmware::Vnish13;
    let ip = ip_of(fw);
    let t = Target::new(
        ip,
        Arc::new(WithSleep(MockNativeDriver::new(ip, r.m(fw).clone()), ip)),
    );
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, true, true)).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert!(
        o.paths_tried[0].contains("native stop: supported"),
        "{:?}",
        o.paths_tried
    );

    // App defaults (no backend hardware-confirmed): dry run names the path
    // and says live is refused; live is refused without writing.
    let t = target(&r, fw);
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, true, false)).await;
    assert_eq!(o.status, WriteStatus::DryRun, "{o:?}");
    assert!(o.msg.unwrap().contains("not yet confirmed"));
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, false, true)).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(o
        .paths_tried
        .iter()
        .any(|p| p.contains("not yet confirmed on real hardware")));
    assert!(r.m(fw).writes().is_empty());
    assert!(saved(&kv, &ip.to_string()).unwrap().is_none());

    // No pool path at all (Avalon has no pool limits).
    let t = target(&r, NativeFirmware::AvalonA);
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, true, false)).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn divert_rolls_back_when_the_write_does_not_read_back() {
    let r = rig(true, false).await;
    let kv = MemoryKv::default();
    let fw = NativeFirmware::WhatsminerV3;
    let t = target(&r, fw);
    let orig = r.m(fw).pool_list();
    r.m(fw).set_ignore_writes(true);
    let o = run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, false, true)).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(r.m(fw).pool_list(), orig);
    assert!(
        saved(&kv, &t.ip.to_string()).unwrap().is_none(),
        "no stale saved copy after a rollback"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_refuses_when_pools_changed_since_the_divert() {
    let r = rig(true, false).await;
    let kv = MemoryKv::default();
    let fw = NativeFirmware::Braiins2507;
    let t = target(&r, fw);
    assert_eq!(
        run_divert(&r.engine, &kv, "t", &t, &req(DivertOp::Divert, false, true))
            .await
            .status,
        WriteStatus::Verified
    );
    // Someone put the miner back on a real pool by hand.
    let set = super::settings::Settings {
        pools: Some(vec![super::pools::PoolGroupSpec {
            name: "default".into(),
            quota: 1,
            pools: vec![super::pools::PoolSpec {
                url: MOCK_POOL.into(),
                user: "other.worker".into(),
                password: Default::default(),
            }],
        }]),
        ..Default::default()
    };
    let drv = t.driver.settings_backends().remove(0);
    drv.write(&set).await.unwrap();
    let o = run_divert(
        &r.engine,
        &kv,
        "t",
        &t,
        &req(DivertOp::Restore, false, true),
    )
    .await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(o.msg.unwrap().contains("no longer on the divert pool"));
    assert!(
        saved(&kv, &t.ip.to_string()).unwrap().is_some(),
        "saved copy kept"
    );
    assert!(is_diverted(&divert_groups()));
    let _ = kv.delete_raw("unused");
}
