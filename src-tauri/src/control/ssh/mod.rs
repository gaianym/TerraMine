//! SSH fallbacks (plan K2): restart the mining process, read logs, and the
//! stock-S9 "grant API write" (add a `W:` api-allow rule).
//!
//! Order of preference, listed in `paths_tried` on every outcome: asic-rs
//! first, then the existing RPC route, then SSH. Every SSH path:
//! - is gated per firmware (and per model for stock Antminers),
//! - sends only allow-listed command strings ([`plan::is_allowed`]),
//! - is dry run by default, and dry-run only in the app until confirmed on
//!   real hardware ([`plan::ShellProfile::hw_confirmed`]),
//! - authenticates with a login from the OS keychain, and
//! - refuses to authenticate to a host whose key is not pinned. The first
//!   run reports the fingerprint for a visible trust prompt; a changed key
//!   is refused outright.

pub mod plan;
pub mod store;
pub mod transport;

#[cfg(test)]
mod e2e_tests;
#[cfg(any(test, feature = "mock-miner"))]
pub mod mock_ssh;

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::control::overrides::cgminer_rpc::{parse_reply, status_of, summary_elapsed, CgminerRpc};
use crate::control::safety::audit::now_ms;
use crate::control::safety::{AuditRecord, AuditSink, DeviceClass};
use plan::{
    grant_api_write_conf, simple_command, supports_model, validate_cidr, PlannedCmd, ShellProfile,
    SshAction, S9_BMMINER_SVC, S9_CONF,
};
use store::{device_key, SecretKv};
use transport::{exec_allowed, SshError, SshSession, SshTransport};

/// One device, as identified by discovery.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub ip: String,
    pub class: DeviceClass,
    pub model: String,
    pub firmware: String,
    pub mac: Option<String>,
    /// `ip:22` in the app; a mock server in tests.
    pub ssh_addr: String,
    /// cgminer-compatible API, `ip:4028` in the app.
    pub rpc_addr: String,
}

impl SshTarget {
    pub fn new(
        ip: &str,
        class: DeviceClass,
        model: &str,
        firmware: &str,
        mac: Option<String>,
    ) -> Self {
        Self {
            ip: ip.into(),
            class,
            model: model.into(),
            firmware: firmware.into(),
            mac,
            ssh_addr: format!("{ip}:22"),
            rpc_addr: format!("{ip}:4028"),
        }
    }
    pub fn key(&self) -> String {
        device_key(&self.ip, self.mac.as_deref())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshRequest {
    pub action: SshAction,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Typed confirmation (the action name) for GrantApiWrite.
    #[serde(default)]
    pub typed: Option<String>,
    #[serde(default)]
    pub allow_irreversible: bool,
    /// GrantApiWrite: the LAN range given write access, e.g. `a.b.c.0/24`.
    #[serde(default)]
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshStatus {
    DryRun,
    Ok,
    Verified,
    Unverified,
    RolledBack,
    RollbackFailed,
    Refused,
    Failed,
    Unsupported,
    NoCredentials,
    /// First use: the fingerprint is in the outcome; pin it to continue.
    HostKeyUnpinned,
    /// The key differs from the pinned one. Nothing was sent.
    HostKeyChanged,
}

impl SshStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Ok => "ok",
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
            Self::NoCredentials => "no_credentials",
            Self::HostKeyUnpinned => "host_key_unpinned",
            Self::HostKeyChanged => "host_key_changed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SshOutcome {
    pub ip: String,
    pub action: SshAction,
    pub status: SshStatus,
    pub msg: String,
    /// Commands planned (dry run) or sent, as displayed (no secrets/blobs).
    pub plan: Vec<String>,
    pub paths_tried: Vec<String>,
    /// Host-key fingerprint seen or pinned.
    pub fingerprint: Option<String>,
    /// Log text (ReadLogs).
    pub output: Option<String>,
    /// Backup path on the miner (GrantApiWrite).
    pub backup: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SshOptions {
    /// Mock tests only: run SSH paths not confirmed on real hardware.
    pub allow_unconfirmed: bool,
    /// How long to wait for mining to come back after a restart.
    pub verify_timeout: Duration,
    pub poll: Duration,
    pub rpc_timeout: Duration,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            allow_unconfirmed: false,
            verify_timeout: Duration::from_secs(120),
            poll: Duration::from_secs(2),
            rpc_timeout: Duration::from_secs(5),
        }
    }
}

pub struct SshRunner {
    pub transport: Arc<dyn SshTransport>,
    pub kv: Arc<dyn SecretKv>,
    pub audit: Arc<dyn AuditSink>,
    pub opts: SshOptions,
}

/// Validate a request against the batch before anything runs.
pub fn preflight(req: &SshRequest, n: usize) -> Result<(), String> {
    if req.action == SshAction::GrantApiWrite {
        validate_cidr(
            req.cidr
                .as_deref()
                .ok_or("grant API write needs the LAN range (cidr) to give write access")?,
        )?;
    }
    if req.dry_run || !req.action.irreversible() {
        return Ok(());
    }
    if !req.allow_irreversible {
        return Err(format!(
            "{} rewrites the miner's config: needs allow_irreversible",
            req.action.name()
        ));
    }
    if req.typed.as_deref() != Some(req.action.name()) {
        return Err(format!("type `{}` to confirm", req.action.name()));
    }
    if n != 1 {
        return Err(format!(
            "{} runs on exactly one miner at a time ({n} selected)",
            req.action.name()
        ));
    }
    Ok(())
}

/// What else covers this action before SSH (listed first in paths tried).
fn prior_paths(t: &SshTarget, action: SshAction) -> Vec<String> {
    match action {
        SshAction::Restart => {
            let mut v = vec!["asic-rs restart(): reboots the whole controller (that is Reboot), not a mining-process restart".to_string()];
            if t.class == DeviceClass::AntminerStock {
                v.push(
                    "cgminer-rpc restart: preferred when the API grants write (privileged)".into(),
                );
            }
            v
        }
        SshAction::ReadLogs => vec![
            "asic-rs read_logs(): preferred path for logs (the Logs action); SSH is the fallback"
                .into(),
        ],
        SshAction::GrantApiWrite => vec![
            "asic-rs: no api-allow setter in any backend".into(),
            "cgminer-rpc: no command edits api-allow (privilege cannot grant itself)".into(),
            "web CGI set_miner_conf: rewrites the whole config; not used for this".into(),
        ],
    }
}

struct Ctx<'a> {
    runner: &'a SshRunner,
    run_id: &'a str,
    t: &'a SshTarget,
    req: &'a SshRequest,
    out: SshOutcome,
}

impl Ctx<'_> {
    fn audit(&self, phase: &str) -> std::io::Result<()> {
        self.runner.audit.record(&AuditRecord {
            ts_ms: now_ms(),
            run_id: self.run_id.into(),
            ip: self.t.ip.clone(),
            action: self.req.action.name().into(),
            phase: phase.into(),
            dry_run: self.req.dry_run,
            irreversible: self.req.action.irreversible(),
            device_class: Some(self.t.class),
            model: Some(self.t.model.clone()),
            firmware: Some(self.t.firmware.clone()),
            requested: serde_json::to_value(
                serde_json::json!({ "action": self.req.action, "cidr": self.req.cidr }),
            )
            .ok(),
            snapshot_id: self.out.backup.clone(),
            pre_state: None,
            paths_tried: self.out.paths_tried.clone(),
            outcome: if phase == "attempt" {
                "pending".into()
            } else {
                self.out.status.as_str().into()
            },
            msg: Some(self.out.msg.clone()),
        })
    }
    fn finish(mut self, status: SshStatus, msg: impl Into<String>) -> SshOutcome {
        self.out.status = status;
        self.out.msg = msg.into();
        if let Err(e) = self.audit("outcome") {
            eprintln!(
                "audit: failed to record {} outcome for {}: {e}",
                self.req.action.name(),
                self.t.ip
            );
        }
        self.out
    }
    async fn exec(
        &mut self,
        s: &mut dyn SshSession,
        c: &PlannedCmd,
    ) -> Result<transport::ExecOutput, String> {
        self.out.plan.push(c.display.clone());
        match exec_allowed(s, &c.command).await {
            Ok(o) => Ok(o),
            Err(SshError::NotAllowed) => Err(format!(
                "`{}` refused by the SSH allow-list; not sent",
                c.display
            )),
            Err(e) => Err(format!("`{}`: {e}", c.display)),
        }
    }
}

// ---- cgminer-compatible API reads used for verification ----

async fn rpc_call(
    addr: &str,
    cmd: &'static str,
    to: Duration,
) -> anyhow::Result<serde_json::Value> {
    // Verification reads only.
    anyhow::ensure!(
        matches!(cmd, "summary" | "privileged"),
        "not a verification read"
    );
    let mut sock = tokio::time::timeout(to, tokio::net::TcpStream::connect(addr)).await??;
    sock.write_all(serde_json::json!({ "command": cmd }).to_string().as_bytes())
        .await?;
    let mut buf = Vec::new();
    tokio::time::timeout(to, sock.read_to_end(&mut buf)).await??;
    parse_reply(&buf)
}

async fn rpc_elapsed(addr: &str, to: Duration) -> Option<u64> {
    rpc_call(addr, "summary", to)
        .await
        .ok()
        .as_ref()
        .and_then(summary_elapsed)
}

async fn rpc_privileged(addr: &str, to: Duration) -> Option<bool> {
    let v = rpc_call(addr, "privileged", to).await.ok()?;
    Some(matches!(status_of(&v), Some(("S", _)) | Some(("I", _))))
}

fn write_conf_cmds(conf: &str) -> [PlannedCmd; 2] {
    let b64 = base64::engine::general_purpose::STANDARD.encode(conf.as_bytes());
    [
        PlannedCmd::opaque(
            format!("printf %s {b64} | base64 -d > {S9_CONF}.terratmp"),
            format!("write {S9_CONF}.terratmp ({} bytes)", conf.len()),
        ),
        PlannedCmd::plain(format!("mv {S9_CONF}.terratmp {S9_CONF}")),
    ]
}

impl SshRunner {
    /// Handshake only, no login: the fingerprint for the trust prompt, plus
    /// the pin on record. Never authenticates or runs anything.
    pub async fn host_key(&self, t: &SshTarget) -> Result<(String, Option<String>), String> {
        let fp = self
            .transport
            .fetch_host_key(&t.ssh_addr)
            .await
            .map_err(|e| e.to_string())?;
        let pinned = self
            .kv
            .pinned(&t.key())
            .map_err(|e| format!("keychain: {e}"))?;
        Ok((fp, pinned))
    }

    pub async fn run(&self, run_id: &str, t: &SshTarget, req: &SshRequest) -> SshOutcome {
        let out = SshOutcome {
            ip: t.ip.clone(),
            action: req.action,
            status: SshStatus::Refused,
            msg: String::new(),
            plan: Vec::new(),
            paths_tried: prior_paths(t, req.action),
            fingerprint: None,
            output: None,
            backup: None,
        };
        let mut c = Ctx {
            runner: self,
            run_id,
            t,
            req,
            out,
        };
        let sh = match supports_model(t.class, &t.model, req.action) {
            Ok(sh) => sh,
            Err(reason) => {
                c.out.paths_tried.push(format!("ssh: {reason}"));
                let msg = format!(
                    "{} not supported; tried: {}",
                    req.action.name(),
                    c.out.paths_tried.join("; ")
                );
                return c.finish(SshStatus::Unsupported, msg);
            }
        };
        if let Err(e) = preflight(req, 1) {
            return c.finish(SshStatus::Refused, e);
        }
        let dev = t.key();
        let pinned = self.kv.pinned(&dev).ok().flatten();
        let login = self.kv.login(t.class, &dev).ok().flatten();
        c.out.fingerprint = pinned.clone();
        let plan = match self.plan_lines(t, sh, req) {
            Ok(p) => p,
            Err(e) => return c.finish(SshStatus::Refused, e),
        };

        if req.dry_run {
            c.out
                .paths_tried
                .push(format!("ssh ({}): planned", sh.label));
            c.out.plan = plan;
            let mut notes = Vec::new();
            if !sh.hw_confirmed && !self.opts.allow_unconfirmed {
                notes.push(
                    "SSH path not yet confirmed on real hardware: a live run is refused"
                        .to_string(),
                );
            }
            notes.push(match &pinned {
                Some(fp) => format!("host key pinned ({fp})"),
                None => "host key not pinned yet: the first live run asks you to trust it".into(),
            });
            notes.push(match &login {
                Some(l) => format!("SSH login on file for {}", l.username),
                None => format!("no SSH login on file (default user {})", sh.default_user),
            });
            return c.finish(
                SshStatus::DryRun,
                format!("dry run, nothing sent; {}", notes.join("; ")),
            );
        }

        // Stock S9 restart: the API route first, when it grants write.
        if req.action == SshAction::Restart && t.class == DeviceClass::AntminerStock {
            match self.rpc_restart(t).await {
                Ok(outcome) => {
                    c.out.paths_tried.push("cgminer-rpc restart: used".into());
                    let (status, msg) = outcome;
                    return c.finish(status, msg);
                }
                Err(reason) => c
                    .out
                    .paths_tried
                    .push(format!("cgminer-rpc restart: skipped ({reason})")),
            }
        }

        if !sh.hw_confirmed && !self.opts.allow_unconfirmed {
            c.out.paths_tried.push(format!(
                "ssh ({}): skipped (not yet confirmed on real hardware; dry run only)",
                sh.label
            ));
            c.out.plan = plan;
            return c.finish(
                SshStatus::Refused,
                "SSH path not yet confirmed on real hardware: dry run only",
            );
        }
        let Some(login) = login else {
            c.out
                .paths_tried
                .push(format!("ssh ({}): no login in the keychain", sh.label));
            return c.finish(
                SshStatus::NoCredentials,
                format!(
                    "no SSH login on file; add one (default user {})",
                    sh.default_user
                ),
            );
        };
        let Some(pinned) = pinned else {
            // Trust on first use: show the key, run nothing.
            return match self.transport.fetch_host_key(&t.ssh_addr).await {
                Ok(fp) => {
                    c.out.fingerprint = Some(fp.clone());
                    c.out
                        .paths_tried
                        .push(format!("ssh ({}): host key not pinned", sh.label));
                    c.finish(SshStatus::HostKeyUnpinned, format!("first SSH connection: confirm host key {fp} to trust it, then run again"))
                }
                Err(e) => c.finish(SshStatus::Failed, format!("ssh: {e}")),
            };
        };
        if let Err(e) = c.audit("attempt") {
            return c.finish(
                SshStatus::Refused,
                format!("audit log unavailable, refusing: {e}"),
            );
        }
        let mut session = match self.transport.open(&t.ssh_addr, &pinned, &login).await {
            Ok(s) => s,
            Err(SshError::HostKeyMismatch { pinned, got }) => {
                c.out.fingerprint = Some(got.clone());
                c.out
                    .paths_tried
                    .push(format!("ssh ({}): host key changed", sh.label));
                return c.finish(SshStatus::HostKeyChanged, format!("HOST KEY CHANGED: pinned {pinned}, miner offered {got}. Nothing sent. Re-pin only if you know why it changed (e.g. a firmware reinstall)."));
            }
            Err(SshError::AuthFailed) => {
                return c.finish(
                    SshStatus::Refused,
                    "SSH login rejected; update the stored login",
                )
            }
            Err(e) => return c.finish(SshStatus::Failed, format!("ssh: {e}")),
        };
        c.out.paths_tried.push(format!("ssh ({}): used", sh.label));
        let (status, msg) = match req.action {
            SshAction::Restart => self.ssh_restart(&mut c, session.as_mut(), sh).await,
            SshAction::ReadLogs => self.ssh_logs(&mut c, session.as_mut(), sh).await,
            SshAction::GrantApiWrite => {
                self.ssh_grant(
                    &mut c,
                    session.as_mut(),
                    req.cidr.as_deref().unwrap_or_default(),
                )
                .await
            }
        };
        session.close().await;
        c.finish(status, msg)
    }

    fn plan_lines(
        &self,
        _t: &SshTarget,
        sh: &ShellProfile,
        req: &SshRequest,
    ) -> Result<Vec<String>, String> {
        Ok(match req.action {
            SshAction::Restart | SshAction::ReadLogs => {
                vec![simple_command(sh, req.action).ok_or("no command")?.display]
            }
            SshAction::GrantApiWrite => {
                let cidr = req.cidr.as_deref().unwrap_or_default();
                vec![
                    format!("cat {S9_CONF}"),
                    format!("cp {S9_CONF} {S9_CONF}.terrabak.<time>"),
                    format!("edit api-allow only: put W:{cidr} first; validate JSON"),
                    format!("write {S9_CONF}.terratmp, then mv {S9_CONF}.terratmp {S9_CONF}"),
                    format!("cat {S9_CONF} (read back: W:{cidr} first, valid JSON)"),
                    format!("{S9_BMMINER_SVC} restart"),
                    "check the API answers and grants write (privileged)".into(),
                    "on any failure: restore the original, restart, read back".into(),
                ]
            }
        })
    }

    /// Ok((status, msg)) = the RPC handled it; Err(reason) = fall through.
    async fn rpc_restart(&self, t: &SshTarget) -> Result<(SshStatus, String), String> {
        let to = self.opts.rpc_timeout;
        match rpc_privileged(&t.rpc_addr, to).await {
            None => return Err("API unreachable".into()),
            Some(false) => return Err("API denies write (no W: api-allow rule)".into()),
            Some(true) => {}
        }
        let before = rpc_elapsed(&t.rpc_addr, to).await;
        let rpc = CgminerRpc::for_class(DeviceClass::AntminerStock, t.rpc_addr.clone(), to)?;
        match rpc.restart().await {
            Ok(true) => {}
            Ok(false) => return Err("API refused restart".into()),
            Err(e) => return Err(format!("restart failed: {e}")),
        }
        Ok(self
            .verify_restart(&t.rpc_addr, before, Instant::now())
            .await)
    }

    async fn verify_restart(
        &self,
        rpc_addr: &str,
        before: Option<u64>,
        sent: Instant,
    ) -> (SshStatus, String) {
        let deadline = sent + self.opts.verify_timeout;
        loop {
            if let Some(e) = rpc_elapsed(rpc_addr, self.opts.rpc_timeout).await {
                let reset = match before {
                    Some(b) => e < b,
                    None => e <= sent.elapsed().as_secs() + 5,
                };
                if reset {
                    return (
                        SshStatus::Verified,
                        format!("mining restarted (API uptime {e}s)"),
                    );
                }
            }
            if Instant::now() >= deadline {
                return (
                    SshStatus::Unverified,
                    "restart sent; the API did not show a fresh mining uptime in time".into(),
                );
            }
            tokio::time::sleep(self.opts.poll).await;
        }
    }

    async fn ssh_restart(
        &self,
        c: &mut Ctx<'_>,
        s: &mut dyn SshSession,
        sh: &ShellProfile,
    ) -> (SshStatus, String) {
        let before = rpc_elapsed(&c.t.rpc_addr, self.opts.rpc_timeout).await;
        let cmd = simple_command(sh, SshAction::Restart).expect("gated");
        match c.exec(s, &cmd).await {
            Ok(o) if o.ok() => {
                self.verify_restart(&c.t.rpc_addr, before, Instant::now())
                    .await
            }
            Ok(o) => (
                SshStatus::Failed,
                format!("restart exited {:?}: {}", o.exit, o.stderr.trim()),
            ),
            Err(e) => (SshStatus::Failed, e),
        }
    }

    async fn ssh_logs(
        &self,
        c: &mut Ctx<'_>,
        s: &mut dyn SshSession,
        sh: &ShellProfile,
    ) -> (SshStatus, String) {
        let cmd = simple_command(sh, SshAction::ReadLogs).expect("gated");
        match c.exec(s, &cmd).await {
            Ok(o) if o.ok() => {
                let n = o.stdout.lines().count();
                c.out.output = Some(o.stdout);
                (SshStatus::Ok, format!("{n} log lines"))
            }
            Ok(o) => (
                SshStatus::Failed,
                format!("log read exited {:?}: {}", o.exit, o.stderr.trim()),
            ),
            Err(e) => (SshStatus::Failed, e),
        }
    }

    async fn ssh_grant(
        &self,
        c: &mut Ctx<'_>,
        s: &mut dyn SshSession,
        cidr: &str,
    ) -> (SshStatus, String) {
        let cat = PlannedCmd::plain(format!("cat {S9_CONF}"));
        let original = match c.exec(s, &cat).await {
            Ok(o) if o.ok() => o.stdout,
            Ok(o) => {
                return (
                    SshStatus::Failed,
                    format!("cannot read {S9_CONF} (exit {:?}); nothing changed", o.exit),
                )
            }
            Err(e) => return (SshStatus::Failed, format!("{e}; nothing changed")),
        };
        let (next, changed) = match grant_api_write_conf(&original, cidr) {
            Ok(x) => x,
            Err(e) => return (SshStatus::Refused, format!("{e}; nothing changed")),
        };
        if !changed {
            return match rpc_privileged(&c.t.rpc_addr, self.opts.rpc_timeout).await {
                Some(true) => (SshStatus::Verified, format!("W:{cidr} is already first in api-allow and the API grants write; nothing changed")),
                _ => (SshStatus::Ok, format!("W:{cidr} is already first in api-allow; nothing changed (API write not confirmed)")),
            };
        }
        let backup = format!("{S9_CONF}.terrabak.{}", now_ms() / 1000);
        match c
            .exec(s, &PlannedCmd::plain(format!("cp {S9_CONF} {backup}")))
            .await
        {
            Ok(o) if o.ok() => c.out.backup = Some(backup.clone()),
            Ok(o) => {
                return (
                    SshStatus::Failed,
                    format!("backup failed (exit {:?}); nothing changed", o.exit),
                )
            }
            Err(e) => return (SshStatus::Failed, format!("{e}; nothing changed")),
        }
        let failure = self.grant_apply(c, s, &next, cidr).await;
        let Err(reason) = failure else {
            return (SshStatus::Verified, format!("api-allow now starts with W:{cidr}; bmminer restarted; API grants write. Backup: {backup}"));
        };
        // Roll back to the original bytes.
        match self.grant_restore(c, s, &original).await {
            Ok(()) => (SshStatus::RolledBack, format!("{reason}; restored the original {S9_CONF} and restarted bmminer (backup kept: {backup})")),
            Err(e) => (SshStatus::RollbackFailed, format!("{reason}; ROLLBACK FAILED ({e}). The original is at {backup} on the miner")),
        }
    }

    async fn grant_apply(
        &self,
        c: &mut Ctx<'_>,
        s: &mut dyn SshSession,
        next: &str,
        cidr: &str,
    ) -> Result<(), String> {
        for cmd in write_conf_cmds(next) {
            match c.exec(s, &cmd).await {
                Ok(o) if o.ok() => {}
                Ok(o) => return Err(format!("`{}` exited {:?}", cmd.display, o.exit)),
                Err(e) => return Err(e),
            }
        }
        let back = match c
            .exec(s, &PlannedCmd::plain(format!("cat {S9_CONF}")))
            .await
        {
            Ok(o) if o.ok() => o.stdout,
            Ok(o) => return Err(format!("read-back exited {:?}", o.exit)),
            Err(e) => return Err(e),
        };
        let v: serde_json::Value =
            serde_json::from_str(&back).map_err(|e| format!("read-back is not valid JSON: {e}"))?;
        let want = format!("W:{cidr}");
        let first = v
            .get("api-allow")
            .and_then(|a| a.as_str())
            .and_then(|a| a.split(',').next())
            .map(str::trim);
        if first != Some(want.as_str()) {
            return Err(format!("read-back api-allow does not start with {want}"));
        }
        let before = rpc_elapsed(&c.t.rpc_addr, self.opts.rpc_timeout).await;
        match c
            .exec(s, &PlannedCmd::plain(format!("{S9_BMMINER_SVC} restart")))
            .await
        {
            Ok(o) if o.ok() => {}
            Ok(o) => return Err(format!("bmminer restart exited {:?}", o.exit)),
            Err(e) => return Err(e),
        }
        let (st, msg) = self
            .verify_restart(&c.t.rpc_addr, before, Instant::now())
            .await;
        if st != SshStatus::Verified {
            return Err(format!("after restart: {msg}"));
        }
        match rpc_privileged(&c.t.rpc_addr, self.opts.rpc_timeout).await {
            Some(true) => Ok(()),
            Some(false) => Err(format!("bmminer is back but still denies write; W:{cidr} may not cover this computer's address")),
            None => Err("API unreachable after restart".into()),
        }
    }

    async fn grant_restore(
        &self,
        c: &mut Ctx<'_>,
        s: &mut dyn SshSession,
        original: &str,
    ) -> Result<(), String> {
        for cmd in write_conf_cmds(original) {
            match c.exec(s, &cmd).await {
                Ok(o) if o.ok() => {}
                Ok(o) => return Err(format!("`{}` exited {:?}", cmd.display, o.exit)),
                Err(e) => return Err(e),
            }
        }
        let back = c
            .exec(s, &PlannedCmd::plain(format!("cat {S9_CONF}")))
            .await?;
        if back.stdout != original {
            return Err("restored file does not read back identical".into());
        }
        match c
            .exec(s, &PlannedCmd::plain(format!("{S9_BMMINER_SVC} restart")))
            .await
        {
            Ok(o) if o.ok() => Ok(()),
            Ok(o) => Err(format!("bmminer restart exited {:?}", o.exit)),
            Err(e) => Err(e),
        }
    }
}
