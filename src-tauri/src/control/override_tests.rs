
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;

use super::driver::{Capabilities, CompositeDriver, MinerAction, MinerDriver, MockBehavior, MockDriver, MockLog, Route};
use super::http_mock::{spawn_axeos, spawn_s9, S9MockOpts};
use super::overrides::auth::DigestCredentials;
use super::overrides::bitaxe::{
    axeos_caps, caps_for_version_str, parse_axeos_version, BitaxeDriver, ASIC_RS_NATIVE_MIN, PAUSE_RESUME_MIN,
};
use super::overrides::s9::S9StockDriver;

fn ip() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

#[test]
fn axeos_2_10_reboot_only() {
    let caps = caps_for_version_str("2.10.0");
    assert_eq!(caps.as_flags(), (false, false, true));
    assert!(PAUSE_RESUME_MIN > semver::Version::new(2, 10, 0));
}

#[test]
fn axeos_2_14_full_caps() {
    let caps = caps_for_version_str("2.14.0");
    assert_eq!(caps.as_flags(), (true, true, true));
}

#[test]
fn axeos_pause_probe_overrides_old_version() {
    // Probe signal (miningPaused in /info) unlocks pause even if version parse failed.
    let caps = axeos_caps(None, true);
    assert_eq!(caps.as_flags(), (true, true, true));
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
    assert_eq!(d.caps().as_flags(), (false, false, true));

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
    assert_eq!(d.caps().as_flags(), (true, true, true));

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
    assert!(d.caps().supports(MinerAction::Start) && d.caps().supports(MinerAction::Stop));
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
    assert_eq!(d.caps().as_flags(), (false, false, true));
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
    assert_eq!(d.caps().as_flags(), (true, true, true));
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

// ---------------- capability map / composite (asic-rs 0.8.6) ----------------


#[test]
fn caps_prefer_native_then_fallback_and_serialize_for_ui() {
    let native = Capabilities::flags(true, true, false, Route::AsicRs);
    let fallback = Capabilities::flags(false, false, true, Route::Axeos);
    let c = Capabilities::prefer(&native, &fallback);
    assert_eq!(c.route(MinerAction::Start), Some(Route::AsicRs));
    assert_eq!(c.route(MinerAction::Stop), Some(Route::AsicRs));
    assert_eq!(c.route(MinerAction::Reboot), Some(Route::Axeos));
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"start": true, "stop": true, "reboot": true,
            "routes": {"start": "asic-rs", "stop": "asic-rs", "reboot": "axeos"}})
    );
    let none = serde_json::to_value(Capabilities::none()).unwrap();
    assert_eq!(none, serde_json::json!({"start": false, "stop": false, "reboot": false, "routes": {}}));
    // Native wins where both can do it.
    let both = Capabilities::prefer(&native, &Capabilities::flags(true, true, true, Route::Axeos));
    assert_eq!(both.route(MinerAction::Stop), Some(Route::AsicRs));
}

#[test]
fn native_backend_only_for_axeos_2_14_and_newer() {
    let client = reqwest::Client::new();
    let ip: IpAddr = "192.0.2.50".parse().unwrap();
    let mk = |v: Option<semver::Version>| BitaxeDriver::from_parts(ip, "http://192.0.2.50".into(), client.clone(), v, false);
    assert_eq!(ASIC_RS_NATIVE_MIN, semver::Version::new(2, 14, 0));
    assert!(mk(Some(semver::Version::new(2, 15, 3))).native_backend_available(), "fleet Bitaxes (2.15.3)");
    assert!(mk(Some(semver::Version::new(2, 14, 0))).native_backend_available());
    assert!(!mk(Some(semver::Version::new(2, 10, 1))).native_backend_available());
    assert!(!mk(None).native_backend_available());
}

#[tokio::test]
async fn composite_sends_each_action_on_its_route() {
    let ip: IpAddr = "192.0.2.51".parse().unwrap();
    let nlog = Arc::new(MockLog::default());
    let flog = Arc::new(MockLog::default());
    let mk = |caps, log: &Arc<MockLog>| MockDriver { ip, caps, behavior: MockBehavior::Ok, delay: Duration::ZERO, log: log.clone(), status: None };
    let d = CompositeDriver::new(
        ip,
        Box::new(mk(Capabilities::flags(true, true, false, Route::AsicRs), &nlog)),
        Box::new(mk(Capabilities::flags(true, true, true, Route::Axeos), &flog)),
    );
    for a in MinerAction::ALL {
        assert!(d.execute(a).await.unwrap());
    }
    let calls = |l: &Arc<MockLog>| l.calls.lock().unwrap().iter().map(|c| c.1).collect::<Vec<_>>();
    assert_eq!(calls(&nlog), [MinerAction::Start, MinerAction::Stop]);
    assert_eq!(calls(&flog), [MinerAction::Reboot]);
}
