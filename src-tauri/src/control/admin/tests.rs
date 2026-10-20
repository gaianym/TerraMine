//! Admin unit tests: defaults, gating, path selection, text handling.

use super::*;

struct Fake {
    id: &'static str,
    ok: Vec<AdminAction>,
    confirmed: bool,
}

impl AdminBackend for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn supports(&self, action: AdminAction) -> Result<(), String> {
        if self.ok.contains(&action) {
            Ok(())
        } else {
            Err(format!("{} lacks {}", self.id, action.name()))
        }
    }
    fn confirmed(&self, _: AdminAction) -> bool {
        self.confirmed
    }
}

#[test]
fn reads_run_live_writes_default_to_dry_run() {
    for a in AdminAction::ALL {
        let r = AdminRequest::new(vec![], a);
        assert_eq!(r.dry_run(), !a.is_read(), "{a:?}");
        assert_eq!(a.kind().irreversible(), !a.is_read(), "{a:?}");
    }
    let mut r = AdminRequest::new(vec![], AdminAction::FactoryReset);
    r.dry_run = Some(false);
    assert!(!r.dry_run());
}

#[test]
fn request_json_defaults_and_password_never_echoed() {
    let r: AdminRequest = serde_json::from_value(serde_json::json!({
        "ips": ["192.0.2.1"], "action": "change_password", "new_password": "s3cret-pw!"
    }))
    .unwrap();
    assert!(r.dry_run(), "no dry_run field = dry run");
    assert_eq!(check_new_password(&r.new_password), Ok("s3cret-pw!"));
    assert!(
        !format!("{r:?}").contains("s3cret"),
        "Debug redacts the new password"
    );
    assert_eq!(
        serde_json::to_string(&r.new_password).unwrap(),
        "\"<redacted>\""
    );
    let r: AdminRequest =
        serde_json::from_value(serde_json::json!({"ips": [], "action": "read_logs"})).unwrap();
    assert!(!r.dry_run());
    assert_eq!(
        (r.concurrency, r.stagger_ms, r.max_log_bytes),
        (4, 500, DEFAULT_MAX_LOG_BYTES)
    );
}

#[test]
fn new_password_rules() {
    assert!(check_new_password(&Secret::Unknown).is_err());
    assert!(check_new_password(&Secret::Redacted)
        .unwrap_err()
        .contains("type it again"));
    assert!(check_new_password(&Secret::Value(String::new())).is_err());
    assert!(check_new_password(&Secret::Value("a".repeat(65))).is_err());
    assert!(check_new_password(&Secret::Value("a\nb".into())).is_err());
    assert!(check_new_password(&Secret::Value("a".repeat(64))).is_ok());
}

#[test]
fn support_picks_first_capable_path_and_lists_the_rest() {
    let b: Vec<Box<dyn AdminBackend>> = vec![
        Box::new(Fake {
            id: "asic-rs",
            ok: vec![AdminAction::Hashboards],
            confirmed: false,
        }),
        Box::new(Fake {
            id: "axeos",
            ok: vec![AdminAction::ReadLogs],
            confirmed: false,
        }),
    ];
    let s = support(&b, AdminAction::ReadLogs);
    assert_eq!(s.path.as_deref(), Some("axeos"));
    assert!(s.confirmed, "reads need no hardware confirmation");
    assert_eq!(s.tried.len(), 2);
    assert!(s.tried[0].contains("asic-rs: skipped (asic-rs lacks read_logs)"));
    let s = support(&b, AdminAction::FactoryReset);
    assert!(s.path.is_none());
    assert_eq!(s.tried.len(), 2);
    assert!(support(&[], AdminAction::ReadLogs).tried[0].contains("no admin path"));
    let w: Vec<Box<dyn AdminBackend>> = vec![Box::new(Fake {
        id: "x",
        ok: vec![AdminAction::ChangePassword],
        confirmed: false,
    })];
    let s = support(&w, AdminAction::ChangePassword);
    assert!(!s.confirmed);
    assert!(s.tried[0].contains("not yet confirmed on real hardware"));
}

#[test]
fn asic_rs_gating_follows_the_backend_flags() {
    use super::asic::{AsicRsAdmin, NativeAdmin, NativeAdminFlags};
    struct N(NativeAdminFlags);
    impl NativeAdmin for N {
        fn flags(&self) -> NativeAdminFlags {
            self.0
        }
        fn label(&self) -> String {
            "Make Model (Fw)".into()
        }
        fn firmware(&self) -> String {
            "Braiins".into()
        }
        fn boards(&self) -> BoxFuture<'_, anyhow::Result<Vec<BoardData>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn read_logs<'a>(
            &'a self,
            _: Option<&'a asic_rs_core::traits::miner::MinerAuth>,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn change_password<'a>(
            &'a self,
            _: Option<&'a asic_rs_core::traits::miner::MinerAuth>,
            _: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async { panic!("must not be called when unsupported") })
        }
        fn factory_reset<'a>(
            &'a self,
            _: Option<&'a asic_rs_core::traits::miner::MinerAuth>,
        ) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async { panic!("must not be called when unsupported") })
        }
    }
    let a = AsicRsAdmin::new(Arc::new(N(NativeAdminFlags {
        read_logs: true,
        change_password: true,
        factory_reset: false,
    })));
    assert!(a.supports(AdminAction::Hashboards).is_ok());
    assert!(a.supports(AdminAction::ReadLogs).is_ok());
    assert!(a.supports(AdminAction::ChangePassword).is_ok());
    assert_eq!(
        a.supports(AdminAction::FactoryReset).unwrap_err(),
        "supports_factory_reset() = false on Make Model (Fw)"
    );
    assert!(
        !a.confirmed(AdminAction::ChangePassword),
        "never hardware-confirmed"
    );
    assert_eq!(a.default_username().as_deref(), Some("root"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(
        rt.block_on(a.factory_reset(None)).is_err(),
        "gated before any call"
    );
    let none = AsicRsAdmin::new(Arc::new(N(NativeAdminFlags::default())));
    assert!(rt
        .block_on(none.check_login(&DigestCredentials::default()))
        .unwrap()
        .is_none());
}

#[test]
fn default_usernames_per_family() {
    use super::asic::default_username as u;
    assert_eq!(u("AntMiner Stock"), Some("root"));
    assert_eq!(u("VNish"), Some(""));
    assert_eq!(u("ESP-Miner"), None);
}

#[test]
fn ansi_is_stripped_and_tail_keeps_the_end() {
    assert_eq!(
        strip_ansi("\x1b[0;32mI (10) x: ok\x1b[0m\r\n"),
        "I (10) x: ok\n"
    );
    let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
    let (t, cut) = tail(text.clone(), 1024);
    assert!(cut);
    assert!(t.len() <= 1024);
    assert!(t.starts_with("line "), "starts at a line: {:?}", &t[..10]);
    assert!(t.ends_with("line 499\n"));
    assert_eq!(tail("short".into(), 1024), ("short".into(), false));
    let (t, _) = tail("é".repeat(600), 1001);
    assert!(t.chars().all(|c| c == 'é'), "cut on a char boundary");
}

#[test]
fn hashboard_problems_and_summary() {
    let ok = Hashboard {
        position: 0,
        hashrate_ghs: Some(4600.0),
        working_chips: Some(63),
        expected_chips: Some(63),
        outlet_chip_temp_c: Some(70.0),
        ..Default::default()
    };
    let bad = Hashboard {
        position: 1,
        hashrate_ghs: Some(0.0),
        working_chips: Some(29),
        expected_chips: Some(63),
        ..Default::default()
    };
    assert!(ok.problem().is_none());
    assert_eq!(bad.problem().as_deref(), Some("29/63 chips, 0 hashrate"));
    assert!(!Hashboard::default().has_data());
    let s = summarize_boards(&[ok, bad]);
    assert!(s.contains("1 degraded"), "{s}");
}

#[test]
fn device_keys_prefer_the_mac() {
    let ip: IpAddr = "192.0.2.7".parse().unwrap();
    assert_eq!(
        creds::device_key(ip, Some("02:00:00:00:AB:CD")),
        "device:02000000abcd"
    );
    assert_eq!(creds::device_key(ip, None), "device:192.0.2.7");
    assert_eq!(creds::pending_key("device:x"), "device:x:pending");
}
