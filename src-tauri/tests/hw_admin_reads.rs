//! Hardware check (ignored; READ-ONLY): hashboards and logs through
//! TerraMine's real admin path, the way the app runs them: asic-rs
//! discovery first (its miner is cached), then the admin runner with the
//! cache-first resolver and `OverlayFactory`.
//!   TM_HW_ADMIN_IPS=<ip>[,<ip>…] cargo test --test hw_admin_reads -- --ignored --nocapture
//! Only `hashboards` and `read_logs` are run; there is no flag to run
//! anything else. Log text is never printed (it can carry pool
//! accounts and host names): only line / byte counts and the path used.
//!
//! First run, Oct 20 2026: five Bitaxe Gamma units (AxeOS 2.15.x).
//! asic-rs identified all five as "Bitaxe Stock" with
//! `supports_read_logs/change_password/factory_reset() = false`.
//! Hashboards: ok on all five via asic-rs `get_data` (1 board, 1/1 chips,
//! 1.03-1.13 TH/s, 52-60 °C). Logs: asic-rs skipped (flag false), ok on
//! all five via the AxeOS `/api/ws` stream (3-16 lines in the 4 s window).
//! The stock S9 was offline, so its paths are mock-tested only.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use terra_mine_lib::control::admin::{run_admin_job, AdminAction, AdminOptions, AdminRequest};
use terra_mine_lib::control::cache::{DeviceCache, Resolver};
use terra_mine_lib::control::creds::MemoryStore;
use terra_mine_lib::control::driver::{DriverFactory, OverlayFactory};
use terra_mine_lib::control::safety::NullAudit;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real hardware: set TM_HW_ADMIN_IPS"]
async fn hw_admin_reads_are_read_only() {
    let ips: Vec<IpAddr> = std::env::var("TM_HW_ADMIN_IPS")
        .expect("TM_HW_ADMIN_IPS")
        .split(',')
        .map(|s| s.trim().parse().expect("ip"))
        .collect();
    let cache = Arc::new(DeviceCache::default());
    let finder = asic_rs::MinerFactory::new()
        .with_connectivity_timeout(Duration::from_secs(8))
        .with_identification_timeout(Duration::from_secs(8));
    for ip in &ips {
        match finder.get_miner(*ip).await {
            Ok(Some(m)) => {
                let i = m.get_device_info();
                println!(
                    "{ip}: asic-rs found {} {} ({}) logs={} pw={} reset={}",
                    i.make,
                    i.model,
                    i.firmware,
                    m.supports_read_logs(),
                    m.supports_change_password(),
                    m.supports_factory_reset()
                );
                let mac = m.get_mac().await.map(|x| x.to_string());
                let fw = m.get_firmware_version().await;
                cache.record_discovery(*ip, Arc::from(m), mac, fw);
            }
            Ok(None) => println!("{ip}: asic-rs did not identify it"),
            Err(e) => println!("{ip}: asic-rs discovery error: {e}"),
        }
    }
    let factory: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::new(Duration::from_secs(10)));
    let resolver = Arc::new(Resolver::new(factory, cache));
    for action in [AdminAction::Hashboards, AdminAction::ReadLogs] {
        assert!(action.is_read(), "this test only reads");
        let mut req = AdminRequest::new(ips.iter().map(|i| i.to_string()).collect(), action);
        req.stagger_ms = 200;
        let out = run_admin_job(
            "hw-admin-reads",
            ips.clone(),
            req,
            resolver.clone(),
            Arc::new(MemoryStore::default()),
            Arc::new(NullAudit),
            AdminOptions::default(),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
        for o in out {
            let msg = match (&o.hashboards, &o.logs) {
                (_, Some(l)) => format!(
                    "{} lines, {} bytes{}",
                    l.lines().count(),
                    l.len(),
                    if o.logs_truncated { " (tail)" } else { "" }
                ),
                _ => o.msg.clone().unwrap_or_default(),
            };
            println!(
                "{} {:?}: {:?} via {:?}: {msg}",
                o.ip, action, o.status, o.route
            );
            for p in &o.paths_tried {
                println!("    tried: {p}");
            }
        }
    }
}
