use std::sync::Arc;

use super::creds::{summarize, CredentialStore, FirmwareKind, MemoryStore};
use super::driver::{MinerAction, MinerStatus};
use super::job::{check_status, Check};
use super::overrides::bitaxe::parse_axeos_status;
use super::overrides::s9::parse_cgminer_elapsed;
use super::overrides::DigestCredentials;

fn st(uptime: Option<u64>, paused: Option<bool>) -> MinerStatus {
    MinerStatus { uptime_s: uptime, paused }
}

#[test]
fn reboot_verified_only_when_uptime_drops() {
    let before = Some(st(Some(5000), None));
    assert_eq!(check_status(MinerAction::Reboot, before, st(Some(12), None)), Check::Done);
    assert_eq!(check_status(MinerAction::Reboot, before, st(Some(5010), None)), Check::Pending);
    assert!(matches!(check_status(MinerAction::Reboot, None, st(Some(1), None)), Check::Impossible(_)));
    assert!(matches!(check_status(MinerAction::Reboot, before, st(None, None)), Check::Impossible(_)));
}

#[test]
fn pause_resume_verified_by_reported_state() {
    assert_eq!(check_status(MinerAction::Stop, None, st(None, Some(true))), Check::Done);
    assert_eq!(check_status(MinerAction::Stop, None, st(None, Some(false))), Check::Pending);
    assert_eq!(check_status(MinerAction::Start, None, st(None, Some(false))), Check::Done);
    assert!(matches!(check_status(MinerAction::Start, None, st(None, None)), Check::Impossible(_)));
}

#[test]
fn axeos_status_parsing() {
    let s = parse_axeos_status(&serde_json::json!({"uptimeSeconds": 42, "miningPaused": true}));
    assert_eq!(s, st(Some(42), Some(true)));
    let s = parse_axeos_status(&serde_json::json!({"uptimeSeconds": 7}));
    assert_eq!(s, st(Some(7), None));
}

#[test]
fn cgminer_elapsed_parsing_handles_nul_terminator() {
    let mut raw = br#"{"STATUS":[{"STATUS":"S"}],"SUMMARY":[{"Elapsed":1234}]}"#.to_vec();
    raw.push(0);
    assert_eq!(parse_cgminer_elapsed(&raw), Some(1234));
    assert_eq!(parse_cgminer_elapsed(b"garbage"), None);
}

#[test]
fn credential_store_roundtrip_and_summary_hides_secret() {
    let store = MemoryStore::default();
    let c = DigestCredentials { username: "ops".into(), password: "hunter2".into() };
    store.set(FirmwareKind::AntminerStock, &c).unwrap();
    assert_eq!(store.get(FirmwareKind::AntminerStock).unwrap(), Some(c.clone()));
    let sum = summarize(&store);
    let row = sum.iter().find(|r| r.firmware == FirmwareKind::AntminerStock).unwrap();
    assert_eq!(row.username.as_deref(), Some("ops"));
    assert!(row.has_password);
    assert!(!serde_json::to_string(&sum).unwrap().contains("hunter2"));
    store.delete(FirmwareKind::AntminerStock).unwrap();
    assert_eq!(store.get(FirmwareKind::AntminerStock).unwrap(), None);
}

#[test]
fn debug_output_redacts_password() {
    let c = DigestCredentials { username: "ops".into(), password: "hunter2".into() };
    assert!(!format!("{c:?}").contains("hunter2"));
}

#[test]
fn overlay_factory_uses_stored_s9_creds() {
    let store = Arc::new(MemoryStore::default());
    store.set(FirmwareKind::AntminerStock, &DigestCredentials { username: "u".into(), password: "p".into() }).unwrap();
    let f = super::driver::OverlayFactory::with_store(std::time::Duration::from_secs(1), store.as_ref());
    assert_eq!(f.s9_credentials().username, "u");
    let f = super::driver::OverlayFactory::with_store(std::time::Duration::from_secs(1), &MemoryStore::default());
    assert_eq!(f.s9_credentials(), &DigestCredentials::antminer_stock_default());
}
