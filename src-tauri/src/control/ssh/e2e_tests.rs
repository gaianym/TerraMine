//! SSH fallbacks end to end: the real russh client against a real russh
//! server (the mock shell), with the stock-S9 cgminer RPC mock behind it
//! for verification. Nothing here touches a real miner.

use std::sync::Arc;
use std::time::Duration;

use super::mock_ssh::{MockShell, MockSsh, MockSshFault, MOCK_CONF};
use super::plan::{is_allowed, SshAction, S9_CONF};
use super::store::{MemoryKv, SecretKv, SshLogin, SshSecret};
use super::transport::{exec_allowed, RusshTransport, SshError, SshTransport};
use super::{SshOptions, SshOutcome, SshRequest, SshRunner, SshStatus, SshTarget};
use crate::control::mock_miner::{MockKind, MockMiner};
use crate::control::safety::{DeviceClass, MemoryAudit};

struct Rig {
    miner: MockMiner,
    ssh: MockSsh,
    kv: Arc<MemoryKv>,
    audit: Arc<MemoryAudit>,
    target: SshTarget,
}

impl Rig {
    fn runner(&self, allow_unconfirmed: bool) -> SshRunner {
        SshRunner {
            transport: Arc::new(RusshTransport {
                timeout: Duration::from_secs(10),
                max_output: 64 * 1024,
            }),
            kv: self.kv.clone(),
            audit: self.audit.clone(),
            opts: SshOptions {
                allow_unconfirmed,
                verify_timeout: Duration::from_secs(5),
                poll: Duration::from_millis(50),
                rpc_timeout: Duration::from_secs(2),
            },
        }
    }
    fn kv(&self) -> &dyn SecretKv {
        self.kv.as_ref()
    }
    fn login(&self) {
        let l = SshLogin {
            username: self.ssh.user.clone(),
            secret: SshSecret::Password(self.ssh.password.clone()),
        };
        self.kv().set_login(self.target.class, None, &l).unwrap();
    }
    fn pin(&self) {
        self.kv()
            .pin(&self.target.key(), &self.ssh.fingerprint)
            .unwrap();
    }
    async fn run(&self, req: SshRequest) -> SshOutcome {
        self.runner(true).run("ssh-test", &self.target, &req).await
    }
}

/// A miner whose cgminer RPC is the verification target and whose SSH shell
/// restarts it. `privileged` = the API already grants write.
async fn rig(shell: MockShell, privileged: bool) -> Rig {
    let miner = MockMiner::spawn(
        MockKind::S9Stock { miner_mode: false },
        Duration::from_millis(100),
    )
    .await
    .unwrap();
    miner.set_rpc_denied(!privileged);
    let ssh = MockSsh::spawn(shell, 1, Some(miner.bmminer_restart_hook()))
        .await
        .unwrap();
    let (class, model, fw) = match shell {
        MockShell::S9Stock => (DeviceClass::AntminerStock, "Antminer S9", "bmminer 2.0.0"),
        MockShell::Braiins => (DeviceClass::BraiinsOs, "Antminer S19", "Braiins OS 25.07"),
    };
    let mut target = SshTarget::new(
        "192.0.2.10",
        class,
        model,
        fw,
        Some("02:00:00:00:00:10".into()),
    );
    target.ssh_addr = ssh.addr.to_string();
    target.rpc_addr = miner.cgminer.unwrap().to_string();
    Rig {
        miner,
        ssh,
        kv: Arc::new(MemoryKv::default()),
        audit: Arc::new(MemoryAudit::default()),
        target,
    }
}

fn live(action: SshAction) -> SshRequest {
    SshRequest {
        action,
        dry_run: false,
        typed: None,
        allow_irreversible: false,
        cidr: None,
    }
}

fn grant(cidr: &str) -> SshRequest {
    SshRequest {
        action: SshAction::GrantApiWrite,
        dry_run: false,
        typed: Some("ssh_grant_api_write".into()),
        allow_irreversible: true,
        cidr: Some(cidr.into()),
    }
}

fn conf_allow(conf: &str) -> String {
    serde_json::from_str::<serde_json::Value>(conf).unwrap()["api-allow"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_list_refuses_unknown_commands_on_a_real_session() {
    let r = rig(MockShell::S9Stock, false).await;
    let t = RusshTransport::default();
    let l = SshLogin {
        username: r.ssh.user.clone(),
        secret: SshSecret::Password(r.ssh.password.clone()),
    };
    let mut s = t
        .open(&r.target.ssh_addr, &r.ssh.fingerprint, &l)
        .await
        .unwrap();
    for bad in [
        "rm -rf /",
        "reboot",
        "cat /etc/shadow",
        "/etc/init.d/bmminer.sh restart; reboot",
    ] {
        assert_eq!(
            exec_allowed(s.as_mut(), bad).await.unwrap_err(),
            SshError::NotAllowed,
            "{bad}"
        );
    }
    assert!(
        r.ssh.commands().is_empty(),
        "nothing refused reached the miner: {:?}",
        r.ssh.commands()
    );
    let o = exec_allowed(s.as_mut(), "cat /config/bmminer.conf")
        .await
        .unwrap();
    assert!(o.ok() && o.stdout.contains("api-allow"));
    s.close().await;
    assert_eq!(r.ssh.commands(), vec!["cat /config/bmminer.conf"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_by_default_and_unconfirmed_paths_never_connect() {
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    for (json, action) in [
        (serde_json::json!({"action": "restart"}), SshAction::Restart),
        (
            serde_json::json!({"action": "read_logs"}),
            SshAction::ReadLogs,
        ),
        (
            serde_json::json!({"action": "grant_api_write", "cidr": "192.0.2.0/24"}),
            SshAction::GrantApiWrite,
        ),
    ] {
        let req: SshRequest = serde_json::from_value(json).unwrap();
        assert!(req.dry_run, "{action:?} is dry run by default");
        // The app's runner: hardware-unconfirmed SSH paths are dry run only.
        let o = r.runner(false).run("t", &r.target, &req).await;
        assert_eq!(o.status, SshStatus::DryRun, "{o:?}");
        assert!(!o.plan.is_empty());
        assert!(
            o.msg.contains("not yet confirmed on real hardware"),
            "{}",
            o.msg
        );
        let o = r
            .runner(false)
            .run(
                "t",
                &r.target,
                &SshRequest {
                    dry_run: false,
                    typed: Some(action.name().into()),
                    allow_irreversible: true,
                    ..req
                },
            )
            .await;
        assert!(
            matches!(o.status, SshStatus::Refused),
            "{action:?} live in the app: {o:?}"
        );
    }
    let g = r
        .runner(false)
        .run(
            "t",
            &r.target,
            &SshRequest {
                dry_run: true,
                ..grant("192.0.2.0/24")
            },
        )
        .await;
    assert!(
        g.plan.iter().any(|l| l.contains("W:192.0.2.0/24 first")),
        "{:?}",
        g.plan
    );
    assert!(g.plan.iter().any(|l| l.contains("restore the original")));
    assert_eq!(r.ssh.logins(), 0, "nothing logged in");
    assert!(r.ssh.commands().is_empty());
    // A live stock-S9 restart in the app may still use the confirmed RPC
    // route; with write denied it then stops at the SSH gate.
    let o = r
        .runner(false)
        .run("t", &r.target, &live(SshAction::Restart))
        .await;
    assert!(
        o.paths_tried
            .iter()
            .any(|p| p.contains("cgminer-rpc restart: skipped (API denies write")),
        "{:?}",
        o.paths_tried
    );
    assert!(o
        .paths_tried
        .iter()
        .any(|p| p.contains("not yet confirmed on real hardware")));
    assert_eq!(r.ssh.logins(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_firmwares_list_paths_tried() {
    let r = rig(MockShell::S9Stock, false).await;
    for (class, model, needle) in [
        (DeviceClass::LuxOs, "Antminer S19", "resetminer"),
        (DeviceClass::AxeOs, "Bitaxe Gamma", "no shell"),
        (DeviceClass::Whatsminer, "M30S", "btminer API"),
        (DeviceClass::Vnish, "Antminer S19", "no documented shell"),
        (DeviceClass::AntminerStock, "Antminer S19 Pro", "SSH locked"),
        (DeviceClass::Avalon, "Avalon 1246", "no known root shell"),
    ] {
        let t = SshTarget {
            class,
            model: model.into(),
            ..r.target.clone()
        };
        for action in SshAction::ALL {
            let o = r
                .runner(true)
                .run(
                    "t",
                    &t,
                    &SshRequest {
                        action,
                        dry_run: true,
                        typed: None,
                        allow_irreversible: false,
                        cidr: Some("192.0.2.0/24".into()),
                    },
                )
                .await;
            assert_eq!(o.status, SshStatus::Unsupported, "{class:?} {action:?}");
            assert!(
                o.paths_tried.iter().any(|p| p.contains(needle)),
                "{class:?}: {:?}",
                o.paths_tried
            );
            assert!(
                o.paths_tried.len() >= 2,
                "prior paths listed first: {:?}",
                o.paths_tried
            );
        }
    }
    // Braiins has no bmminer config to edit.
    let t = SshTarget {
        class: DeviceClass::BraiinsOs,
        ..r.target.clone()
    };
    let o = r
        .runner(true)
        .run(
            "t",
            &t,
            &SshRequest {
                dry_run: true,
                ..grant("192.0.2.0/24")
            },
        )
        .await;
    assert_eq!(o.status, SshStatus::Unsupported);
    assert!(o.paths_tried[0].contains("asic-rs: no api-allow setter"));
    assert_eq!(r.ssh.logins(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn host_key_trust_on_first_use_then_pinned_then_changed() {
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    // First use: fingerprint reported, no login, nothing run.
    let o = r.run(live(SshAction::Restart)).await;
    assert_eq!(o.status, SshStatus::HostKeyUnpinned, "{o:?}");
    assert_eq!(o.fingerprint.as_deref(), Some(r.ssh.fingerprint.as_str()));
    assert!(o.msg.contains(&r.ssh.fingerprint));
    assert_eq!(
        (r.ssh.logins(), r.ssh.commands().len(), r.ssh.restarts()),
        (0, 0, 0)
    );
    // The prompt's data source agrees.
    let (fp, pinned) = r.runner(true).host_key(&r.target).await.unwrap();
    assert_eq!((fp.as_str(), pinned), (r.ssh.fingerprint.as_str(), None));
    // Operator trusts it.
    r.pin();
    let o = r.run(live(SshAction::Restart)).await;
    assert_eq!(o.status, SshStatus::Verified, "{o:?}");
    assert!(
        o.paths_tried
            .iter()
            .any(|p| p == "ssh (Antminer stock (bmminer)): used"),
        "{:?}",
        o.paths_tried
    );
    assert_eq!((r.ssh.restarts(), r.miner.mining_restarts()), (1, 1));
    // Key changes (another box answering on that address): refused.
    let other = MockSsh::spawn(MockShell::S9Stock, 2, None).await.unwrap();
    assert_ne!(other.fingerprint, r.ssh.fingerprint);
    let t = SshTarget {
        ssh_addr: other.addr.to_string(),
        ..r.target.clone()
    };
    let o = r
        .runner(true)
        .run("t", &t, &live(SshAction::ReadLogs))
        .await;
    assert_eq!(o.status, SshStatus::HostKeyChanged, "{o:?}");
    assert_eq!(o.fingerprint.as_deref(), Some(other.fingerprint.as_str()));
    assert!(o.msg.contains("HOST KEY CHANGED"));
    assert_eq!(
        (other.logins(), other.commands().len()),
        (0, 0),
        "no credentials sent to a changed key"
    );
    assert_eq!(
        r.kv().pinned(&r.target.key()).unwrap().as_deref(),
        Some(r.ssh.fingerprint.as_str()),
        "pin unchanged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_or_wrong_credentials() {
    let r = rig(MockShell::S9Stock, false).await;
    r.pin();
    let o = r.run(live(SshAction::ReadLogs)).await;
    assert_eq!(o.status, SshStatus::NoCredentials, "{o:?}");
    assert_eq!(r.ssh.logins(), 0);
    r.kv()
        .set_login(
            r.target.class,
            None,
            &SshLogin {
                username: "root".into(),
                secret: SshSecret::Password("wrong".into()),
            },
        )
        .unwrap();
    let o = r.run(live(SshAction::ReadLogs)).await;
    assert_eq!(o.status, SshStatus::Refused);
    assert!(o.msg.contains("login rejected"), "{}", o.msg);
    assert!(
        !serde_json::to_string(&o).unwrap().contains("wrong"),
        "password never in an outcome"
    );
    assert!(!format!("{:?}", r.audit.records()).contains("mock-ssh-pass"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stock_s9_restart_prefers_the_rpc_when_it_grants_write() {
    let r = rig(MockShell::S9Stock, true).await;
    let o = r
        .runner(false)
        .run("t", &r.target, &live(SshAction::Restart))
        .await;
    assert_eq!(o.status, SshStatus::Verified, "{o:?}");
    assert!(
        o.paths_tried
            .contains(&"cgminer-rpc restart: used".to_string()),
        "{:?}",
        o.paths_tried
    );
    assert_eq!(r.miner.mining_restarts(), 1);
    assert_eq!(r.ssh.logins(), 0, "SSH not needed");
}

#[tokio::test(flavor = "multi_thread")]
async fn braiins_restart_and_logs_over_ssh() {
    let r = rig(MockShell::Braiins, false).await;
    r.login();
    r.pin();
    let o = r.run(live(SshAction::Restart)).await;
    assert_eq!(o.status, SshStatus::Verified, "{o:?}");
    assert!(o.paths_tried[0].contains("asic-rs restart(): reboots the whole controller"));
    assert_eq!(r.ssh.restarts(), 1);
    let o = r.run(live(SshAction::ReadLogs)).await;
    assert_eq!(o.status, SshStatus::Ok, "{o:?}");
    assert!(o.output.as_deref().unwrap().contains("bosminer"));
    assert!(o.paths_tried[0].contains("asic-rs read_logs()"));
    assert!(
        r.ssh.commands().iter().all(|c| is_allowed(c)),
        "{:?}",
        r.ssh.commands()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_grant_api_write_backs_up_edits_only_api_allow_restarts_and_verifies() {
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    let o = r.run(grant("127.0.0.0/8")).await;
    assert_eq!(o.status, SshStatus::Verified, "{o:?}");
    let conf = r.ssh.file(S9_CONF).unwrap();
    assert_eq!(conf_allow(&conf), "W:127.0.0.0/8,R:0/0");
    // Only api-allow changed.
    let (mut a, mut b): (serde_json::Value, serde_json::Value) = (
        serde_json::from_str(&conf).unwrap(),
        serde_json::from_str(MOCK_CONF).unwrap(),
    );
    a.as_object_mut().unwrap().remove("api-allow");
    b.as_object_mut().unwrap().remove("api-allow");
    assert_eq!(a, b);
    // Backup holds the original bytes.
    let backup = o.backup.clone().unwrap();
    assert_eq!(r.ssh.file(&backup).as_deref(), Some(MOCK_CONF));
    assert_eq!((r.ssh.restarts(), r.miner.mining_restarts()), (1, 1));
    assert!(
        !r.ssh.files().iter().any(|f| f.ends_with(".terratmp")),
        "temp file moved into place"
    );
    let cmds = r.ssh.commands();
    assert!(cmds.iter().all(|c| is_allowed(c)), "{cmds:?}");
    assert!(
        cmds[0].starts_with("cat ") && cmds[1].starts_with("cp "),
        "read + backup first: {cmds:?}"
    );
    assert!(
        o.plan.iter().all(|l| !l.contains("base64")),
        "config blob not echoed: {:?}",
        o.plan
    );
    // Audited (attempt + outcome).
    let recs = r.audit.records();
    assert_eq!(
        recs.iter().map(|x| x.phase.as_str()).collect::<Vec<_>>(),
        ["attempt", "outcome"]
    );
    assert_eq!(recs[1].outcome, "verified");
    assert!(recs[1].irreversible);
    // Second run: already granted, nothing written.
    let n = r.ssh.commands().len();
    let o = r.run(grant("127.0.0.0/8")).await;
    assert_eq!(o.status, SshStatus::Verified, "{o:?}");
    assert!(o.msg.contains("nothing changed"));
    assert_eq!(r.ssh.commands().len(), n + 1, "only the read");
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_grant_rolls_back_on_bad_read_back_or_no_effect() {
    // Write silently lost: read-back fails before any restart.
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    r.ssh.set_fault(Some(MockSshFault::WriteIgnored));
    let o = r.run(grant("127.0.0.0/8")).await;
    assert_eq!(o.status, SshStatus::RolledBack, "{o:?}");
    assert!(
        o.msg.contains("does not start with W:127.0.0.0/8"),
        "{}",
        o.msg
    );
    r.ssh.set_fault(None);
    assert_eq!(r.ssh.file(S9_CONF).as_deref(), Some(MOCK_CONF));

    // Applied and restarted, but the range does not cover this computer:
    // the API still denies write, so the original is restored.
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    let o = r.run(grant("192.0.2.0/24")).await;
    assert_eq!(o.status, SshStatus::RolledBack, "{o:?}");
    assert!(o.msg.contains("still denies write"), "{}", o.msg);
    assert_eq!(
        r.ssh.file(S9_CONF).as_deref(),
        Some(MOCK_CONF),
        "original restored byte for byte"
    );
    assert_eq!(
        r.ssh.restarts(),
        2,
        "restart after the edit and after the restore"
    );
    assert!(
        r.ssh.file(o.backup.as_deref().unwrap()).is_some(),
        "backup kept"
    );

    // Restart failing also rolls back.
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    r.ssh.set_fault(Some(MockSshFault::RestartFails));
    let o = r.run(grant("127.0.0.0/8")).await;
    assert_eq!(
        o.status,
        SshStatus::RollbackFailed,
        "restore restart fails too: {o:?}"
    );
    assert_eq!(
        r.ssh.file(S9_CONF).as_deref(),
        Some(MOCK_CONF),
        "file restored even so"
    );
    assert!(o.msg.contains("original is at"), "{}", o.msg);

    // Write itself failing: rolled back (file never changed).
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    r.ssh.set_fault(Some(MockSshFault::WriteFails));
    let o = r.run(grant("127.0.0.0/8")).await;
    assert!(
        matches!(o.status, SshStatus::RolledBack | SshStatus::RollbackFailed),
        "{o:?}"
    );
    assert_eq!(r.ssh.file(S9_CONF).as_deref(), Some(MOCK_CONF));
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_grant_needs_typed_confirmation_one_device_and_a_valid_range() {
    let r = rig(MockShell::S9Stock, false).await;
    r.login();
    r.pin();
    let no_type = SshRequest {
        typed: None,
        ..grant("127.0.0.0/8")
    };
    assert_eq!(r.run(no_type).await.status, SshStatus::Refused);
    let wrong = SshRequest {
        typed: Some("yes".into()),
        ..grant("127.0.0.0/8")
    };
    assert!(r
        .run(wrong)
        .await
        .msg
        .contains("type `ssh_grant_api_write`"));
    let no_allow = SshRequest {
        allow_irreversible: false,
        ..grant("127.0.0.0/8")
    };
    assert_eq!(r.run(no_allow).await.status, SshStatus::Refused);
    assert!(super::preflight(&grant("127.0.0.0/8"), 2)
        .unwrap_err()
        .contains("exactly one"));
    let dry_bad = SshRequest {
        dry_run: true,
        ..grant("not-a-range")
    };
    assert_eq!(r.run(dry_bad).await.status, SshStatus::Refused);
    let no_range = SshRequest {
        cidr: None,
        ..grant("x")
    };
    assert!(r.run(no_range).await.msg.contains("LAN range"));
    assert_eq!(r.ssh.logins(), 0);
    // Malformed config on the miner: refused, nothing written.
    r.ssh.set_file(S9_CONF, "{ broken");
    let o = r.run(grant("127.0.0.0/8")).await;
    assert_eq!(o.status, SshStatus::Refused, "{o:?}");
    assert_eq!(r.ssh.commands(), vec![format!("cat {S9_CONF}")]);
}
