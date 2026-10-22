//! Plan step I end to end: the capability matrix over every mock (HTTP /
//! RPC mocks through the real resolver and drivers, plus the per-firmware
//! asic-rs settings mocks) agrees with the gates it is built from and
//! writes nothing; the audit viewer reads what the engine writes, filters
//! it, and never shows a secret.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use super::admin::{self, AdminAction};
use super::audit_view::{self, AuditFilter, ExportFormat};
use super::cache::{DeviceCache, Resolved, Resolver};
use super::driver::{DriverFactory, MinerAction};
use super::hardening_commands::matrix_for;
use super::matrix::{self, settings_probes, CellStatus, MatrixRow};
use super::mock_miner::{MockFirmware, MockKind, MockMiner, MockMinerFactory};
use super::safety::audit::{AuditRecord, AuditSink, JsonlAudit};
use super::safety::divert::divert_groups;
use super::safety::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, Target};
use super::safety::mock_native::{MockNative, MockNativeDriver, NativeFirmware};
use super::safety::settings::Settings;
use super::safety::snapshot::SnapshotStore;
use super::safety::{Confirmation, DeviceClass, NullAudit};
use super::ssh::plan::{supports_model, SshAction};

const AXE210: [u8; 4] = [192, 0, 2, 1];
const AXE215: [u8; 4] = [192, 0, 2, 2];
const S9: [u8; 4] = [192, 0, 2, 3];
const S9_MODE: [u8; 4] = [192, 0, 2, 4];
const ANTMINER: [u8; 4] = [192, 0, 2, 5];
const BRAIINS: [u8; 4] = [192, 0, 2, 6];
const WM3: [u8; 4] = [192, 0, 2, 7];
const WM1: [u8; 4] = [192, 0, 2, 8];

fn engine() -> SafetyEngine {
    SafetyEngine::new(
        Arc::new(NullAudit),
        Arc::new(SnapshotStore::in_memory(4)),
        SafetyOptions::default(),
    )
}

fn cell<'a>(r: &'a MatrixRow, id: &str) -> &'a matrix::MatrixCell {
    r.cells
        .iter()
        .find(|c| c.action == id)
        .unwrap_or_else(|| panic!("{} has no {id} cell", r.ip))
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_over_every_http_mock_matches_the_gates_and_writes_nothing() {
    let kinds = [
        (AXE210, MockKind::AxeOs210),
        (AXE215, MockKind::AxeOs215),
        (S9, MockKind::S9Stock { miner_mode: false }),
        (S9_MODE, MockKind::S9Stock { miner_mode: true }),
        (ANTMINER, MockKind::AsicRs(MockFirmware::AntminerV2020)),
        (BRAIINS, MockKind::AsicRs(MockFirmware::Braiins2507)),
        (WM3, MockKind::AsicRs(MockFirmware::WhatsminerV3)),
        (WM1, MockKind::AsicRs(MockFirmware::WhatsminerV1)),
    ];
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let mut miners = HashMap::new();
    for (ip, k) in kinds {
        let m = MockMiner::spawn(k, Duration::from_millis(200))
            .await
            .unwrap();
        f.add(IpAddr::from(ip), &m);
        miners.insert(ip, m);
    }
    let factory: Arc<dyn DriverFactory> = Arc::new(f);
    let cache = Arc::new(DeviceCache::default());
    let resolver = Resolver::new(factory, cache.clone());
    let eng = engine();
    let n_cells = MinerAction::ALL.len()
        + settings_probes().len()
        + AdminAction::ALL.len()
        + SshAction::ALL.len()
        + 1;

    let mut rows = HashMap::new();
    for (ip, _) in kinds {
        let ip4 = IpAddr::from(ip);
        let row = matrix_for(&resolver, &eng, &cache, ip4).await;
        assert!(row.found, "{ip4}: {:?}", row.error);
        assert_eq!(row.cells.len(), n_cells);
        // Control cells = the driver's capability gate, exactly.
        let Resolved::Ready { driver, .. } = resolver.resolve(ip4, Duration::from_secs(2)).await
        else {
            panic!("resolve {ip4}")
        };
        let caps = driver.caps();
        for a in MinerAction::ALL {
            let c = cell(&row, a.name());
            let want = if !caps.supports(a) {
                CellStatus::Unsupported
            } else if caps.confirmed(a) {
                CellStatus::Confirmed
            } else {
                CellStatus::DryRunOnly
            };
            assert_eq!(c.status, want, "{ip4} {}", a.name());
            assert!(
                !c.paths_tried.is_empty(),
                "{ip4} {}: paths tried listed",
                a.name()
            );
        }
        // Settings cells = the engine's path selection, exactly.
        let t = Target::new(ip4, driver.clone());
        for (id, _, change) in settings_probes() {
            let (b, tried) = eng.select(&t, &change, true);
            let c = cell(&row, id);
            assert_eq!(c.path, b.as_ref().map(|b| b.id().to_string()), "{ip4} {id}");
            assert_eq!(c.paths_tried, tried, "{ip4} {id}");
            assert_eq!(c.status == CellStatus::Unsupported, b.is_none());
            assert_eq!(
                c.status == CellStatus::Confirmed,
                b.is_some_and(|b| b.confirmed_for(&change)),
                "{ip4} {id}"
            );
        }
        // Admin cells (step G) = the admin path gate, exactly.
        let backends = admin::backends_for(
            driver.as_ref(),
            cache.get(ip4).and_then(|c| c.miner),
            ip4,
            Duration::from_secs(2),
        );
        for a in AdminAction::ALL {
            let sup = admin::support(&backends, a);
            let c = cell(&row, a.name());
            assert_eq!(c.group, "admin");
            assert_eq!(c.path, sup.path, "{ip4} {}", a.name());
            assert_eq!(c.status == CellStatus::Unsupported, sup.path.is_none());
            assert_eq!(
                c.status == CellStatus::Confirmed,
                sup.path.is_some() && sup.confirmed,
                "{ip4} {}",
                a.name()
            );
            if !a.is_read() {
                assert_ne!(
                    c.status,
                    CellStatus::Confirmed,
                    "{ip4} {}: writes unconfirmed",
                    a.name()
                );
            }
            assert!(!c.paths_tried.is_empty(), "{ip4} {}", a.name());
        }
        // SSH cells (K2) = the shell table; never live until confirmed.
        for a in SshAction::ALL {
            let c = cell(&row, a.name());
            assert_eq!(c.group, "fallback");
            match supports_model(t.profile.class, &t.profile.model, a) {
                Err(_) => assert_eq!(c.status, CellStatus::Unsupported, "{ip4} {}", a.name()),
                Ok(sh) => assert_eq!(
                    c.status,
                    if sh.hw_confirmed {
                        CellStatus::Confirmed
                    } else {
                        CellStatus::DryRunOnly
                    },
                    "{ip4} {}",
                    a.name()
                ),
            }
            assert!(!c.paths_tried.is_empty(), "{ip4} {}", a.name());
        }
        // Pool divert = the divert job's own limit check + path selection.
        let change = Settings {
            pools: Some(divert_groups()),
            ..Default::default()
        };
        let want = if eng.limits.check_pools(&t.profile, &change).is_err() {
            None
        } else {
            eng.select(&t, &change, true).0
        };
        let c = cell(&row, "pool_divert");
        assert_eq!(
            c.path,
            want.as_ref().map(|b| b.id().to_string()),
            "{ip4} divert"
        );
        assert_eq!(
            c.status == CellStatus::Confirmed,
            want.is_some_and(|b| b.confirmed_for(&change)),
            "{ip4} divert"
        );
        rows.insert(ip, row);
    }

    // Step G / K2 spot checks.
    let s9 = &rows[&S9];
    assert_eq!(
        cell(s9, "ssh_grant_api_write").status,
        CellStatus::DryRunOnly
    );
    assert_eq!(cell(s9, "ssh_restart").status, CellStatus::DryRunOnly);
    assert!(cell(s9, "ssh_restart")
        .paths_tried
        .iter()
        .any(|p| p.starts_with("cgminer-rpc restart")));
    let axe = &rows[&AXE215];
    for id in ["ssh_restart", "ssh_read_logs", "ssh_grant_api_write"] {
        assert_eq!(cell(axe, id).status, CellStatus::Unsupported, "AxeOS {id}");
    }
    assert_eq!(
        cell(&rows[&BRAIINS], "ssh_restart").status,
        CellStatus::DryRunOnly
    );
    assert_eq!(
        cell(&rows[&BRAIINS], "ssh_grant_api_write").status,
        CellStatus::Unsupported
    );
    for (ip, r) in &rows {
        for id in ["change_password", "factory_reset"] {
            assert_ne!(cell(r, id).status, CellStatus::Confirmed, "{ip:?} {id}");
        }
    }

    // Spot checks against what each gate is known to say.
    let axe = &rows[&AXE215];
    assert_eq!(axe.class, Some(DeviceClass::AxeOs));
    assert_eq!(
        cell(axe, "frequency").status,
        CellStatus::Confirmed,
        "AxeOS 2.15 frequency is hardware-confirmed"
    );
    assert_eq!(cell(axe, "fan_manual").status, CellStatus::Confirmed);
    assert_eq!(
        cell(axe, "core_voltage").status,
        CellStatus::DryRunOnly,
        "core voltage is not"
    );
    assert_eq!(
        cell(&rows[&S9], "light_on").status,
        CellStatus::DryRunOnly,
        "S9 blink CGI is unconfirmed"
    );
    let wm1 = cell(&rows[&WM1], "light_on");
    assert_eq!(wm1.status, CellStatus::Unsupported);
    assert!(
        wm1.paths_tried
            .iter()
            .any(|p| p.contains("supports_set_fault_light")),
        "{:?}",
        wm1.paths_tried
    );
    // AxeOS paths only ever appear on AxeOS rows.
    for (ip, r) in &rows {
        if r.class != Some(DeviceClass::AxeOs) {
            assert!(
                r.cells
                    .iter()
                    .all(|c| c.path.as_deref().is_none_or(|p| !p.contains("axeos"))),
                "{ip:?}: {:?}",
                r.cells
            );
        }
    }
    // A matrix is a read.
    for (ip, m) in &miners {
        assert_eq!(m.writes(), 0, "{ip:?}");
        assert_eq!(m.reboots(), 0, "{ip:?}");
        assert!(
            m.requests()
                .iter()
                .all(|r| !r.starts_with("POST ") || r.contains("/asic-rs/data")),
            "{ip:?}: {:?}",
            m.requests()
        );
    }
    // Serialises with the documented status names.
    let j = serde_json::to_value(&rows[&AXE215]).unwrap();
    assert!(j["cells"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["status"] == "dry_run_only"));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_matrix_per_asic_rs_firmware_follows_supports_flags() {
    let eng = engine();
    for (i, fw) in NativeFirmware::ALL.into_iter().enumerate() {
        let m = MockNative::new(fw);
        let ip = IpAddr::from([198, 51, 100, i as u8 + 1]);
        let row = matrix::matrix_row(
            &eng,
            ip,
            Arc::new(MockNativeDriver::new(ip, m.clone())),
            None,
        );
        let flags = fw.flags();
        let st = |id| cell(&row, id).status;
        // A false supports_* flag can never show as supported.
        if !flags.pools {
            assert_eq!(st("pools"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.timezone {
            assert_eq!(st("timezone"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.tuning_percent {
            assert_eq!(st("tuning_percent"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.presets {
            assert_eq!(st("tuning_preset"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.scaling {
            assert_eq!(st("scaling"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.power_limit && !flags.tuning_config {
            assert_eq!(st("power_limit"), CellStatus::Unsupported, "{fw:?}");
        }
        if !flags.fan {
            assert_eq!(st("fan_manual"), CellStatus::Unsupported, "{fw:?}");
        }
        // AxeOS-only knobs never show on an asic-rs firmware mock.
        assert_eq!(st("frequency"), CellStatus::Unsupported, "{fw:?}");
        assert_eq!(st("core_voltage"), CellStatus::Unsupported, "{fw:?}");
        // asic-rs settings backends are not hardware-confirmed: dry run only.
        for c in row.cells.iter().filter(|c| c.group == "settings") {
            assert_ne!(c.status, CellStatus::Confirmed, "{fw:?} {}", c.action);
            if c.status == CellStatus::Unsupported {
                assert!(!c.paths_tried.is_empty(), "{fw:?} {}", c.action);
            }
        }
        if flags.power_limit {
            assert_eq!(st("power_limit"), CellStatus::DryRunOnly, "{fw:?}");
        }
        assert!(
            m.writes().is_empty(),
            "{fw:?}: matrix wrote {:?}",
            m.writes()
        );
    }
}

fn rec(ts: u64, ip: &str, action: &str, phase: &str, outcome: &str) -> AuditRecord {
    AuditRecord {
        ts_ms: ts,
        run_id: format!("run-{ts}"),
        ip: ip.into(),
        action: action.into(),
        phase: phase.into(),
        outcome: outcome.into(),
        ..Default::default()
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("terramine-{name}-{}", uuid::Uuid::new_v4()))
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_viewer_reads_engine_records_filters_and_never_shows_secrets() {
    let dir = tmp("audit");
    let path = dir.join("audit/actions.jsonl");
    let sink = Arc::new(JsonlAudit::new(path.clone()));

    // Real engine records: a dry run and a refused live pool change with a
    // pool password in the request.
    let eng = SafetyEngine::new(
        sink.clone(),
        Arc::new(SnapshotStore::in_memory(4)),
        SafetyOptions::default(),
    );
    let ip = IpAddr::from([198, 51, 100, 9]);
    let t = Target::new(
        ip,
        Arc::new(MockNativeDriver::new(
            ip,
            MockNative::new(NativeFirmware::Braiins2507),
        )),
    );
    let dry = SafeWriteRequest {
        change: Settings {
            power_limit_w: Some(2500),
            ..Default::default()
        },
        dry_run: true,
        confirm: Confirmation::default(),
    };
    assert_eq!(
        eng.apply_settings("run-a", &t, &dry).await.status.as_str(),
        "dry_run"
    );
    let pool: Settings = serde_json::from_value(serde_json::json!({"pool_op": {"op": "add", "pool": {"url": "stratum+tcp://new.example.com:3333", "user": "w", "password": "hunter2"}}})).unwrap();
    let live = SafeWriteRequest {
        change: pool.clone(),
        dry_run: false,
        confirm: Confirmation::default(),
    };
    // Unconfirmed backend: no live path.
    assert_eq!(
        eng.apply_settings("run-b", &t, &live).await.status.as_str(),
        "no_path"
    );
    let dry_pool = SafeWriteRequest {
        change: pool,
        dry_run: true,
        confirm: Confirmation::default(),
    };
    assert_eq!(
        eng.apply_settings("run-c", &t, &dry_pool)
            .await
            .status
            .as_str(),
        "dry_run"
    );

    // Hand-written lines a careless writer could produce, plus junk.
    let mut leaky = rec(
        9_000_000_000_000,
        "198.51.100.10",
        "change_password",
        "outcome",
        "failed",
    );
    leaky.requested = Some(
        serde_json::json!({"username": "root", "new_password": "hunter2", "nested": [{"token": "abc123"}]}),
    );
    leaky.pre_state = Some(serde_json::json!({"pools": [{"url": "x", "password": "hunter2"}]}));
    leaky.msg = Some("login failed: password=hunter2, secret: s3cr3t".into());
    sink.record(&leaky).unwrap();
    sink.record(&rec(1, "198.51.100.11", "=cmd()", "attempt", "pending"))
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"{not json\n"))
        .unwrap();

    let (all, bad) = audit_view::load(&path).unwrap();
    assert_eq!(bad, 1, "a broken line is counted, not hidden");
    assert!(all.len() >= 4);

    let page = audit_view::page(all.clone(), bad, &AuditFilter::default());
    assert_eq!(page.total, all.len());
    assert!(
        page.records.windows(2).all(|w| w[0].ts_ms >= w[1].ts_ms),
        "newest first"
    );
    assert_eq!(page.records[0].action, "change_password");
    assert!(
        page.actions.contains(&"apply_settings".to_string())
            && page.outcomes.contains(&"no_path".to_string())
    );
    let shown = serde_json::to_string(&page).unwrap();
    assert!(
        !shown.contains("hunter2") && !shown.contains("abc123") && !shown.contains("s3cr3t"),
        "{shown}"
    );
    assert!(shown.contains("password=<redacted>"), "keys stay readable");

    // Filters.
    let f = |j| {
        audit_view::page(
            all.clone(),
            0,
            &serde_json::from_value::<AuditFilter>(j).unwrap(),
        )
    };
    assert!(f(serde_json::json!({"ip": "198.51.100.9"}))
        .records
        .iter()
        .all(|r| r.ip == "198.51.100.9"));
    assert_eq!(f(serde_json::json!({"outcome": "no_path"})).total, 1);
    assert_eq!(
        f(serde_json::json!({"action": "apply_settings", "phase": "outcome", "dry_run": true}))
            .total,
        2
    );
    assert!(f(serde_json::json!({"text": "not yet confirmed"})).total >= 1);
    assert_eq!(
        f(serde_json::json!({"since_ms": 8_000_000_000_000u64})).total,
        1
    );
    assert_eq!(
        f(serde_json::json!({"limit": 1, "offset": 1}))
            .records
            .len(),
        1
    );

    // Export: both formats, redacted, CSV formula-guarded and quoted.
    let out = dir.join("exports");
    let (p, n) = audit_view::export(
        all.clone(),
        &AuditFilter::default(),
        ExportFormat::Csv,
        &out,
        0,
    )
    .unwrap();
    let csv = std::fs::read_to_string(&p).unwrap();
    assert_eq!(n, all.len());
    assert!(csv.starts_with(audit_view::CSV_HEADER));
    assert_eq!(csv.lines().count(), n + 1);
    assert!(!csv.contains("hunter2") && !csv.contains("s3cr3t"));
    assert!(csv.contains(",'=cmd(),"), "formula guard");
    let (p, _) = audit_view::export(
        all,
        &AuditFilter {
            ip: Some("198.51.100.10".into()),
            ..Default::default()
        },
        ExportFormat::Jsonl,
        &out,
        1,
    )
    .unwrap();
    let jl = std::fs::read_to_string(&p).unwrap();
    assert_eq!(jl.lines().count(), 1);
    assert!(!jl.contains("hunter2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn audit_helpers() {
    assert_eq!(audit_view::iso_utc(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        audit_view::iso_utc(1_792_500_000_123),
        "2026-10-20T12:40:00.123Z"
    );
    assert_eq!(
        audit_view::redact_text("no secrets here, passwords are long"),
        "no secrets here, passwords are long"
    );
    assert_eq!(
        audit_view::redact_text(r#"{"password":"x1"}"#),
        r#"{"password":"<redacted>"}"#
    );
    assert_eq!(
        audit_view::redact_text("token: abc def"),
        "token: <redacted> def"
    );
    assert!(
        audit_view::load(&tmp("missing")).unwrap().0.is_empty(),
        "no file = empty log"
    );
}
