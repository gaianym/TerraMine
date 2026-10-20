//! Mock-miner end-to-end tests of all four admin actions through the real
//! runner: resolver + cache, path selection, preflight, keychain flow,
//! audit. Real sockets, so `multi_thread` (see test_hygiene.rs).

use std::collections::HashMap;

use super::*;
use crate::control::cache::DeviceCache;
use crate::control::creds::MemoryStore;
use crate::control::mock_miner::{MockFirmware, MockKind, MockMiner, MockMinerFactory, MOCK_LOGIN};
use crate::control::safety::audit::MemoryAudit;

const ANT: [u8; 4] = [192, 0, 2, 10];
const BOS: [u8; 4] = [192, 0, 2, 11];
const WM: [u8; 4] = [192, 0, 2, 12];
const S9: [u8; 4] = [192, 0, 2, 13];
const AXE: [u8; 4] = [192, 0, 2, 14];
const AXE_OLD: [u8; 4] = [192, 0, 2, 15];
const NEW_PW: &str = "N3w-Pass-For-Mock";

struct Rig {
    miners: HashMap<IpAddr, MockMiner>,
    factory: Arc<MockMinerFactory>,
    resolver: Arc<Resolver>,
    store: Arc<MemoryStore>,
    audit: Arc<MemoryAudit>,
}

impl Rig {
    async fn new() -> Self {
        let kinds = [
            (ANT, MockKind::AsicRs(MockFirmware::AntminerV2020)),
            (BOS, MockKind::AsicRs(MockFirmware::Braiins2507)),
            (WM, MockKind::AsicRs(MockFirmware::WhatsminerV3)),
            (S9, MockKind::S9Stock { miner_mode: false }),
            (AXE, MockKind::AxeOs215),
            (AXE_OLD, MockKind::AxeOs210),
        ];
        let mut f = MockMinerFactory::new(Duration::from_secs(3));
        let mut miners = HashMap::new();
        for (ip, k) in kinds {
            let m = MockMiner::spawn(k, Duration::from_millis(300))
                .await
                .unwrap();
            f.add(IpAddr::from(ip), &m);
            miners.insert(IpAddr::from(ip), m);
        }
        let factory = Arc::new(f);
        let resolver = Arc::new(Resolver::new(
            factory.clone(),
            Arc::new(DeviceCache::default()),
        ));
        Self {
            miners,
            factory,
            resolver,
            store: Arc::new(MemoryStore::default()),
            audit: Arc::new(MemoryAudit::default()),
        }
    }
    fn m(&self, ip: [u8; 4]) -> &MockMiner {
        &self.miners[&IpAddr::from(ip)]
    }
    async fn run(
        &self,
        ips: &[[u8; 4]],
        req: AdminRequest,
        allow_unconfirmed: bool,
    ) -> HashMap<IpAddr, AdminOutcome> {
        self.run_with(ips, req, allow_unconfirmed, self.store.clone())
            .await
    }
    async fn run_with(
        &self,
        ips: &[[u8; 4]],
        mut req: AdminRequest,
        allow_unconfirmed: bool,
        store: Arc<dyn CredentialStore>,
    ) -> HashMap<IpAddr, AdminOutcome> {
        let ips: Vec<IpAddr> = ips.iter().map(|i| IpAddr::from(*i)).collect();
        req.ips = ips.iter().map(|i| i.to_string()).collect();
        req.stagger_ms = 0;
        req.timeout_sec = 5;
        req.verify_timeout_sec = 10;
        let opts = AdminOptions {
            allow_unconfirmed,
            verify_interval: Duration::from_millis(100),
            log_window: Duration::from_secs(1),
        };
        let out = run_admin_job(
            "admin-test",
            ips,
            req,
            self.resolver.clone(),
            store,
            self.audit.clone(),
            opts,
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
        out.into_iter()
            .map(|o| (o.ip.parse().unwrap(), o))
            .collect()
    }
    fn device_key(&self, ip: [u8; 4]) -> String {
        creds::device_key(IpAddr::from(ip), Some(&self.m(ip).mac()))
    }
}

fn get(out: &HashMap<IpAddr, AdminOutcome>, ip: [u8; 4]) -> &AdminOutcome {
    &out[&IpAddr::from(ip)]
}

const ALL: [[u8; 4]; 6] = [ANT, BOS, WM, S9, AXE, AXE_OLD];

fn live(action: AdminAction, pw: Option<&str>) -> AdminRequest {
    let mut r = AdminRequest::new(vec![], action);
    r.dry_run = Some(false);
    r.confirm = Confirmation {
        typed: Some(action.kind().name().into()),
        allow_irreversible: true,
        ..Default::default()
    };
    if let Some(p) = pw {
        r.new_password = Secret::Value(p.into());
    }
    r
}

/// Fleetwide hashboards: asic-rs `get_data` where there is one, bmminer
/// `stats` chains on the stock S9; identified once, then served from cache.
#[tokio::test(flavor = "multi_thread")]
async fn hashboards_fleetwide_per_firmware() {
    let r = Rig::new().await;
    let out = r
        .run(
            &ALL,
            AdminRequest::new(vec![], AdminAction::Hashboards),
            false,
        )
        .await;

    let ant = get(&out, ANT);
    assert_eq!(
        (ant.status, ant.route.as_deref()),
        (ActionStatus::Ok, Some("asic-rs")),
        "{ant:?}"
    );
    let b = ant.hashboards.as_ref().unwrap();
    assert_eq!(b.len(), 3);
    assert_eq!(b[1].problem().as_deref(), Some("70/76 chips"));
    assert!(ant.msg.as_deref().unwrap().contains("1 degraded"));
    assert_eq!(get(&out, BOS).status, ActionStatus::Ok);
    assert_eq!(
        get(&out, WM).status,
        ActionStatus::Ok,
        "hashboards come from get_data on every asic-rs backend"
    );

    let s9 = get(&out, S9);
    assert_eq!(
        (s9.status, s9.route.as_deref()),
        (ActionStatus::Ok, Some("cgminer-rpc")),
        "{s9:?}"
    );
    let b = s9.hashboards.as_ref().unwrap();
    assert_eq!(
        b.iter().map(|b| b.working_chips).collect::<Vec<_>>(),
        vec![Some(63), Some(29), Some(63)]
    );
    assert_eq!(b[1].problem().as_deref(), Some("29/63 chips"));
    assert!(r.m(S9).requests().contains(&"cgminer stats".to_string()));

    let axe = get(&out, AXE);
    assert_eq!(
        (axe.status, axe.route.as_deref()),
        (ActionStatus::Ok, Some("asic-rs")),
        "{axe:?}"
    );
    assert_eq!(axe.hashboards.as_ref().unwrap().len(), 1);
    // AxeOS 2.10 has no asic-rs driver in the mock (the app reuses the miner
    // discovery cached); the AxeOS path says where hashboards come from.
    let old = get(&out, AXE_OLD);
    assert_eq!(old.status, ActionStatus::Unsupported, "{old:?}");
    assert!(
        old.paths_tried
            .iter()
            .any(|p| p.contains("asic-rs get_data")),
        "{old:?}"
    );

    assert_eq!(
        r.factory.connects(),
        ALL.len(),
        "one identification per device"
    );
    let again = r
        .run(
            &ALL,
            AdminRequest::new(vec![], AdminAction::Hashboards),
            false,
        )
        .await;
    assert_eq!(get(&again, ANT).status, ActionStatus::Ok);
    assert_eq!(
        r.factory.connects(),
        ALL.len(),
        "second run is served from the cache"
    );
    // Reads never touch settings, pools, or power.
    for ip in ALL {
        assert_eq!(r.m(ip).writes(), 0);
        assert_eq!(r.m(ip).reboots(), 0);
    }
}

/// Fleetwide logs: asic-rs `read_logs` where supported, the S9 kernel log
/// CGI, the AxeOS `/api/ws` stream; unsupported rows list paths tried.
#[tokio::test(flavor = "multi_thread")]
async fn logs_fleetwide_per_firmware() {
    let r = Rig::new().await;
    let out = r
        .run(
            &ALL,
            AdminRequest::new(vec![], AdminAction::ReadLogs),
            false,
        )
        .await;

    let ant = get(&out, ANT);
    assert_eq!(
        (ant.status, ant.route.as_deref()),
        (ActionStatus::Ok, Some("asic-rs")),
        "{ant:?}"
    );
    assert!(ant.logs.as_deref().unwrap().contains("cgminer started"));
    assert_eq!(get(&out, BOS).status, ActionStatus::Ok);

    let wm = get(&out, WM);
    assert_eq!(wm.status, ActionStatus::Unsupported, "{wm:?}");
    assert!(
        wm.msg
            .as_deref()
            .unwrap()
            .contains("supports_read_logs() = false"),
        "{wm:?}"
    );

    let s9 = get(&out, S9);
    assert_eq!(
        (s9.status, s9.route.as_deref()),
        (ActionStatus::Ok, Some("s9-cgi")),
        "{s9:?}"
    );
    assert!(s9.logs.as_deref().unwrap().contains("Booting Linux"));

    for ip in [AXE, AXE_OLD] {
        let a = get(&out, ip);
        assert_eq!(
            (a.status, a.route.as_deref()),
            (ActionStatus::Ok, Some("axeos")),
            "{a:?}"
        );
        let logs = a.logs.as_deref().unwrap();
        assert!(
            logs.contains("ASIC 1 detected") && logs.contains("fan ramp"),
            "{logs}"
        );
        assert!(logs.contains("asic_result"), "16-bit length frame decoded");
        assert!(!logs.contains('\x1b'), "ANSI colours stripped");
        assert_eq!(logs.lines().count(), 3);
        assert!(r.m(ip).requests().contains(&"GET /api/ws".to_string()));
    }
    assert!(
        get(&out, AXE).paths_tried[0].contains("asic-rs: skipped (supports_read_logs() = false"),
        "{:?}",
        get(&out, AXE).paths_tried
    );

    // Log text is shown, never audited.
    let audit = serde_json::to_string(&r.audit.records()).unwrap();
    assert!(
        !audit.contains("cgminer started")
            && !audit.contains("Booting Linux")
            && !audit.contains("ASIC 1 detected")
    );
    assert!(r
        .audit
        .records()
        .iter()
        .all(|a| !a.dry_run && !a.irreversible && a.action == "read_logs"));
}

/// Password change and factory reset: dry run by default (fleetwide is
/// fine), per-firmware gating with paths tried, nothing sent.
#[tokio::test(flavor = "multi_thread")]
async fn destructive_actions_default_to_dry_run_with_per_firmware_gating() {
    let r = Rig::new().await;
    let mut req = AdminRequest::new(vec![], AdminAction::ChangePassword);
    req.new_password = Secret::Value(NEW_PW.into());
    let pw = r.run(&ALL, req, false).await;
    let reset = r
        .run(
            &ALL,
            AdminRequest::new(vec![], AdminAction::FactoryReset),
            false,
        )
        .await;

    let ant = get(&pw, ANT);
    assert_eq!(ant.status, ActionStatus::DryRun, "{ant:?}");
    assert!(ant
        .msg
        .as_deref()
        .unwrap()
        .contains("would change the login password via asic-rs"));
    assert!(ant
        .msg
        .as_deref()
        .unwrap()
        .contains("not yet confirmed on real hardware"));
    assert_eq!(get(&pw, BOS).status, ActionStatus::DryRun);
    assert_eq!(get(&reset, ANT).status, ActionStatus::DryRun);
    let bos = get(&reset, BOS);
    assert_eq!(
        bos.status,
        ActionStatus::Unsupported,
        "braiins v25_07 has no factory reset: {bos:?}"
    );
    assert!(bos
        .msg
        .as_deref()
        .unwrap()
        .contains("supports_factory_reset() = false"));
    for out in [&pw, &reset] {
        assert_eq!(get(out, WM).status, ActionStatus::Unsupported);
        let s9 = get(out, S9);
        assert_eq!(s9.status, ActionStatus::Unsupported, "{s9:?}");
        assert!(s9
            .paths_tried
            .iter()
            .any(|p| p.contains("no S9-specific override")));
        for ip in [AXE, AXE_OLD] {
            assert_eq!(get(out, ip).status, ActionStatus::Unsupported);
        }
    }
    assert!(get(&pw, AXE)
        .paths_tried
        .iter()
        .any(|p| p.contains("AxeOS has no login")));
    assert!(get(&reset, AXE)
        .paths_tried
        .iter()
        .any(|p| p.contains("no factory-reset endpoint")));
    for ip in ALL {
        assert_eq!(
            (
                r.m(ip).password_changes(),
                r.m(ip).factory_resets(),
                r.m(ip).reboots()
            ),
            (0, 0, 0)
        );
    }
    assert!(r.store.devices().is_empty(), "dry run stores nothing");
    let audit = serde_json::to_string(&r.audit.records()).unwrap();
    assert!(!audit.contains(NEW_PW));
    assert!(r.audit.records().iter().all(|a| a.dry_run));
}

/// Live sends need the typed action name, the irreversible checkbox and
/// exactly one device; in the app (no allow_unconfirmed) they are refused
/// even then, because no password / reset path is hardware-confirmed.
#[tokio::test(flavor = "multi_thread")]
async fn live_destructive_actions_are_gated() {
    let r = Rig::new().await;
    let mut no_type = live(AdminAction::FactoryReset, None);
    no_type.confirm.typed = Some("factory reset".into());
    let o = r.run(&[ANT], no_type, true).await;
    assert_eq!(get(&o, ANT).status, ActionStatus::Refused);
    let mut no_box = live(AdminAction::FactoryReset, None);
    no_box.confirm.allow_irreversible = false;
    assert_eq!(
        get(&r.run(&[ANT], no_box, true).await, ANT).status,
        ActionStatus::Refused
    );
    let o = r
        .run(
            &[ANT, BOS],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            true,
        )
        .await;
    assert!(
        o.values().all(|o| o.status == ActionStatus::Refused),
        "irreversible batch refused: {o:?}"
    );
    let o = r
        .run(&[ANT], live(AdminAction::ChangePassword, None), true)
        .await;
    assert!(get(&o, ANT)
        .msg
        .as_deref()
        .unwrap()
        .contains("no new password"));
    // Everything right, but the path is not hardware-confirmed.
    let o = r
        .run(
            &[ANT],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            false,
        )
        .await;
    let a = get(&o, ANT);
    assert_eq!(a.status, ActionStatus::Refused, "{a:?}");
    assert!(a
        .msg
        .as_deref()
        .unwrap()
        .contains("dry run only until a hardware check"));
    let o = r
        .run(&[ANT], live(AdminAction::FactoryReset, None), false)
        .await;
    assert_eq!(get(&o, ANT).status, ActionStatus::Refused);
    assert_eq!(
        (r.m(ANT).password_changes(), r.m(ANT).factory_resets()),
        (0, 0)
    );
    assert_eq!(r.m(ANT).login_password(), MOCK_LOGIN.1);
    assert!(r.store.devices().is_empty());
}

/// Full live flow on the mock (allow_unconfirmed): password change →
/// pending keychain entry promoted to the device login → verified with the
/// new login → logs read with it → factory reset → back to the factory
/// login, stored device login removed. The password never appears in the
/// audit trail or in any outcome.
#[tokio::test(flavor = "multi_thread")]
async fn password_change_then_factory_reset_on_mock() {
    let r = Rig::new().await;
    let key = r.device_key(ANT);
    let o = r
        .run(
            &[ANT],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            true,
        )
        .await;
    let a = get(&o, ANT);
    assert_eq!(a.status, ActionStatus::Verified, "{a:?}");
    assert_eq!(r.m(ANT).login_password(), NEW_PW);
    let devices = r.store.devices();
    assert_eq!(
        devices
            .get(&key)
            .map(|c| (c.username.as_str(), c.password.as_str())),
        Some(("root", NEW_PW))
    );
    assert!(
        !devices.contains_key(&creds::pending_key(&key)),
        "pending entry promoted"
    );
    assert!(!serde_json::to_string(&o).unwrap().contains(NEW_PW));

    // The factory login no longer works; the stored device login does.
    let logs = r
        .run(
            &[ANT],
            AdminRequest::new(vec![], AdminAction::ReadLogs),
            false,
        )
        .await;
    assert_eq!(
        get(&logs, ANT).status,
        ActionStatus::Ok,
        "{:?}",
        get(&logs, ANT)
    );

    let o = r
        .run(&[ANT], live(AdminAction::FactoryReset, None), true)
        .await;
    let a = get(&o, ANT);
    assert_eq!(a.status, ActionStatus::Verified, "{a:?}");
    assert!(a
        .msg
        .as_deref()
        .unwrap()
        .contains("stored login was removed"));
    assert_eq!((r.m(ANT).factory_resets(), r.m(ANT).reboots()), (1, 1));
    assert_eq!(r.m(ANT).login_password(), MOCK_LOGIN.1);
    assert!(r.store.devices().is_empty());
    let logs = r
        .run(
            &[ANT],
            AdminRequest::new(vec![], AdminAction::ReadLogs),
            false,
        )
        .await;
    assert_eq!(
        get(&logs, ANT).status,
        ActionStatus::Ok,
        "factory login works again"
    );

    let recs = r.audit.records();
    let audit = serde_json::to_string(&recs).unwrap();
    assert!(!audit.contains(NEW_PW), "password never audited");
    assert!(audit.contains("<redacted>"));
    let phases: Vec<_> = recs
        .iter()
        .filter(|a| a.irreversible)
        .map(|a| (a.action.as_str(), a.phase.as_str(), a.outcome.as_str()))
        .collect();
    assert_eq!(
        phases,
        vec![
            ("change_password", "attempt", "pending"),
            ("change_password", "outcome", "verified"),
            ("factory_reset", "attempt", "pending"),
            ("factory_reset", "outcome", "verified")
        ]
    );
    // Braiins 25.07: password yes, reset not offered.
    let o = r
        .run(
            &[BOS],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            true,
        )
        .await;
    assert_eq!(get(&o, BOS).status, ActionStatus::Verified);
    let o = r
        .run(&[BOS], live(AdminAction::FactoryReset, None), true)
        .await;
    assert_eq!(get(&o, BOS).status, ActionStatus::Unsupported);
    assert_eq!(r.m(BOS).factory_resets(), 0);
}

/// No keychain = no password change (the new password would be lost).
#[tokio::test(flavor = "multi_thread")]
async fn password_change_refused_without_keychain() {
    struct FirmwareOnly;
    impl CredentialStore for FirmwareOnly {
        fn get(&self, _: creds::FirmwareKind) -> anyhow::Result<Option<DigestCredentials>> {
            Ok(None)
        }
        fn set(&self, _: creds::FirmwareKind, _: &DigestCredentials) -> anyhow::Result<()> {
            Ok(())
        }
        fn delete(&self, _: creds::FirmwareKind) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let r = Rig::new().await;
    let o = r
        .run_with(
            &[ANT],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            true,
            Arc::new(FirmwareOnly),
        )
        .await;
    let a = get(&o, ANT);
    assert_eq!(a.status, ActionStatus::Refused, "{a:?}");
    assert!(a
        .msg
        .as_deref()
        .unwrap()
        .contains("OS keychain unavailable"));
    assert_eq!(r.m(ANT).password_changes(), 0);
}

/// The miner rejects the current login: failed, outcome reported, the new
/// password kept as pending (never lost), the device login untouched.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_login_keeps_pending_and_old_login() {
    let r = Rig::new().await;
    let key = r.device_key(ANT);
    let wrong = DigestCredentials {
        username: "root".into(),
        password: "not-it".into(),
    };
    r.store.set_device(&key, &wrong).unwrap();
    let o = r
        .run(
            &[ANT],
            live(AdminAction::ChangePassword, Some(NEW_PW)),
            true,
        )
        .await;
    let a = get(&o, ANT);
    assert_eq!(a.status, ActionStatus::Failed, "{a:?}");
    assert!(a
        .msg
        .as_deref()
        .unwrap()
        .contains("kept in the keychain as pending"));
    let d = r.store.devices();
    assert_eq!(d.get(&key).unwrap().password, "not-it");
    assert_eq!(d.get(&creds::pending_key(&key)).unwrap().password, NEW_PW);
    assert_eq!(r.m(ANT).login_password(), MOCK_LOGIN.1);
    // Reads with the wrong stored login fail and say so (no silent default).
    let logs = r
        .run(
            &[ANT],
            AdminRequest::new(vec![], AdminAction::ReadLogs),
            false,
        )
        .await;
    assert_eq!(get(&logs, ANT).status, ActionStatus::Failed);
    assert!(get(&logs, ANT).msg.as_deref().unwrap().contains("401"));
}

/// The dialog's support matrix comes from the same gating.
#[tokio::test(flavor = "multi_thread")]
async fn support_rows_match_the_runner() {
    let r = Rig::new().await;
    for ip in ALL {
        let ip = IpAddr::from(ip);
        let Resolved::Ready { driver, .. } = r.resolver.resolve(ip, Duration::from_secs(3)).await
        else {
            panic!("resolve {ip}")
        };
        let mac = r.resolver.cache.get(ip).and_then(|d| d.identity.mac);
        let row = support_row(ip, Some((driver, None, mac)), r.store.as_ref()).await;
        assert!(row.found);
        let path = |a: AdminAction| row.actions[&a].path.clone();
        match ip.to_string().as_str() {
            "192.0.2.10" => {
                assert_eq!(path(AdminAction::FactoryReset).as_deref(), Some("asic-rs"));
                assert!(!row.actions[&AdminAction::FactoryReset].confirmed);
            }
            "192.0.2.11" => assert_eq!(path(AdminAction::FactoryReset), None),
            "192.0.2.13" => {
                assert_eq!(
                    path(AdminAction::Hashboards).as_deref(),
                    Some("cgminer-rpc")
                );
                assert_eq!(path(AdminAction::ReadLogs).as_deref(), Some("s9-cgi"));
            }
            "192.0.2.14" => assert_eq!(path(AdminAction::ReadLogs).as_deref(), Some("axeos")),
            _ => {}
        }
        assert!(!row.device_login);
    }
    let missing = support_row(IpAddr::from([192, 0, 2, 99]), None, r.store.as_ref()).await;
    assert!(!missing.found);
}
