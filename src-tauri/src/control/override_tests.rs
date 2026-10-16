
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reqwest::Client;

use super::driver::{Capabilities, MinerAction, MinerDriver};
use super::http_mock::{spawn_axeos, spawn_s9, S9MockOpts};
use super::overrides::auth::DigestCredentials;
use super::overrides::bitaxe::{axeos_caps, caps_for_version_str, parse_axeos_version, BitaxeDriver, PAUSE_RESUME_MIN};
use super::overrides::s9::S9StockDriver;

fn ip() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

#[test]
fn axeos_2_10_reboot_only() {
    let caps = caps_for_version_str("2.10.0");
    assert_eq!(caps, Capabilities { start: false, stop: false, reboot: true });
    assert!(PAUSE_RESUME_MIN > semver::Version::new(2, 10, 0));
}

#[test]
fn axeos_2_14_full_caps() {
    let caps = caps_for_version_str("2.14.0");
    assert_eq!(caps, Capabilities { start: true, stop: true, reboot: true });
}

#[test]
fn axeos_pause_probe_overrides_old_version() {
    // Probe signal (miningPaused in /info) unlocks pause even if version parse failed.
    let caps = axeos_caps(None, true);
    assert!(caps.start && caps.stop && caps.reboot);
}

#[test]
fn parse_version_from_info_json() {
    let v = parse_axeos_version(&serde_json::json!({"version":"2.10.1","boardVersion":"401"}));
    assert_eq!(v.unwrap().to_string(), "2.10.1");
    let v = parse_axeos_version(&serde_json::json!({"axeOSVersion":"v2.14.0"}));
    assert_eq!(v.unwrap().to_string(), "2.14.0");
}

#[tokio::test]
async fn mock_axeos_2_10_restart_only() {
    let mock = spawn_axeos("2.10.0", false).await;
    let d = BitaxeDriver::probe_at(ip(), mock.base.clone(), Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected");
    assert_eq!(d.caps(), Capabilities { start: false, stop: false, reboot: true });

    assert!(d.execute(MinerAction::Reboot).await.unwrap());
    assert_eq!(mock.counters["restart"].load(Ordering::SeqCst), 1);

    // Start/stop reported unsupported by caps — job layer would skip; driver returns false.
    assert!(!d.execute(MinerAction::Stop).await.unwrap());
    assert!(!d.execute(MinerAction::Start).await.unwrap());
    assert_eq!(mock.counters["pause"].load(Ordering::SeqCst), 0);
    assert_eq!(mock.counters["resume"].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mock_axeos_2_14_pause_resume_restart() {
    let mock = spawn_axeos("2.14.2", false).await;
    let d = BitaxeDriver::probe_at(ip(), mock.base.clone(), Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected");
    assert_eq!(d.caps(), Capabilities { start: true, stop: true, reboot: true });

    assert!(d.execute(MinerAction::Stop).await.unwrap());
    assert!(d.execute(MinerAction::Start).await.unwrap());
    assert!(d.execute(MinerAction::Reboot).await.unwrap());
    assert_eq!(mock.counters["pause"].load(Ordering::SeqCst), 1);
    assert_eq!(mock.counters["resume"].load(Ordering::SeqCst), 1);
    assert_eq!(mock.counters["restart"].load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mock_axeos_pause_via_mining_paused_field() {
    // Version string intentionally old-looking but info advertises miningPaused.
    let mock = spawn_axeos("2.10.0", true).await;
    let d = BitaxeDriver::probe_at(ip(), mock.base.clone(), Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected");
    assert!(d.caps().start && d.caps().stop);
    assert!(d.execute(MinerAction::Stop).await.unwrap());
}

#[tokio::test]
async fn mock_s9_without_miner_mode_reboot_only() {
    let mock = spawn_s9(S9MockOpts { has_miner_mode: false, ..Default::default() }).await;
    let creds = DigestCredentials::antminer_stock_default();
    let d = S9StockDriver::probe_at(ip(), mock.base.clone(), creds, Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected");
    assert_eq!(d.caps(), Capabilities { start: false, stop: false, reboot: true });
    assert!(d.execute(MinerAction::Reboot).await.unwrap());
    assert_eq!(mock.counters["reboot"].load(Ordering::SeqCst), 1);
    assert!(!d.execute(MinerAction::Stop).await.unwrap());
    assert_eq!(mock.counters["set_conf"].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mock_s9_with_miner_mode_allows_pause() {
    let mock = spawn_s9(S9MockOpts { has_miner_mode: true, ..Default::default() }).await;
    let creds = DigestCredentials::antminer_stock_default();
    let d = S9StockDriver::probe_at(ip(), mock.base.clone(), creds, Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected");
    assert_eq!(d.caps(), Capabilities { start: true, stop: true, reboot: true });
    assert!(d.execute(MinerAction::Stop).await.unwrap());
    assert!(d.execute(MinerAction::Start).await.unwrap());
    assert_eq!(mock.counters["set_conf"].load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn mock_s9_digest_deny_fails_reboot() {
    let mock = spawn_s9(S9MockOpts {
        accept_auth: false,
        password: "wrong-default-should-not-matter".into(),
        ..Default::default()
    }).await;
    // Client still sends root:root; server rejects every digest attempt.
    let creds = DigestCredentials::antminer_stock_default();
    let client = Client::builder().timeout(Duration::from_secs(2)).no_proxy().build().unwrap();
    let d = S9StockDriver::from_parts(ip(), mock.base.clone(), client, creds, false);
    let err = d.execute(MinerAction::Reboot).await;
    assert!(err.is_err() || err.ok() == Some(false));
    // At least one deny recorded (challenge and/or rejected response).
    assert!(mock.counters["deny"].load(Ordering::SeqCst) >= 1 || mock.hits.lock().unwrap().iter().any(|h| h.contains("reboot")));
}

#[tokio::test]
async fn mock_s9_pluggable_credentials_not_hardcoded_secret_path() {
    // Non-default creds work when the mock expects them — proves we don't
    // bake a single password into the request path.
    let mock = spawn_s9(S9MockOpts {
        username: "admin".into(),
        password: "s3cret".into(),
        has_miner_mode: false,
        accept_auth: true,
    }).await;
    let creds = DigestCredentials {
        username: "admin".into(),
        password: "s3cret".into(),
    };
    let d = S9StockDriver::probe_at(ip(), mock.base.clone(), creds, Duration::from_secs(2))
        .await
        .unwrap()
        .expect("detected with custom creds");
    assert!(d.execute(MinerAction::Reboot).await.unwrap());
}

#[test]
fn digest_default_is_documented_factory_fallback() {
    let d = DigestCredentials::antminer_stock_default();
    assert_eq!(d.username, "root");
    assert_eq!(d.password, "root");
}
