//! What SSH is allowed to do, per firmware, and the command allow-list.
//!
//! SSH is a *fallback*: TerraMine prefers asic-rs for anything it covers
//! (Braiins restart and logs are native, for instance). SSH exists for the
//! gaps and for the stock-S9 "grant API write" flow, which no HTTP/RPC path
//! can do. Every string ever sent over the channel must match a rule here;
//! anything else is refused before a connection is made. Paths and the
//! service names are the documented ones for each firmware.

use serde::{Deserialize, Serialize};

use crate::control::safety::DeviceClass;

/// An SSH fallback action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAction {
    /// Restart the mining process (not the whole box).
    Restart,
    /// Read the miner's log.
    ReadLogs,
    /// Stock S9 only: add a `W:` api-allow rule so the bmminer RPC accepts
    /// privileged commands, with backup / validate / restart / rollback.
    GrantApiWrite,
}

impl SshAction {
    pub const ALL: [SshAction; 3] = [
        SshAction::Restart,
        SshAction::ReadLogs,
        SshAction::GrantApiWrite,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Restart => "ssh_restart",
            Self::ReadLogs => "ssh_read_logs",
            Self::GrantApiWrite => "ssh_grant_api_write",
        }
    }

    /// GrantApiWrite rewrites a config file and restarts mining: it is
    /// irreversible in the plan-policy sense (typed confirm, one device).
    pub fn irreversible(self) -> bool {
        matches!(self, Self::GrantApiWrite)
    }

    pub fn is_read(self) -> bool {
        matches!(self, Self::ReadLogs)
    }
}

/// How SSH reaches one firmware family: the documented restart command and
/// log file, and whether the stock-S9 bmminer config edit applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellProfile {
    pub class: DeviceClass,
    /// Label used in messages ("Braiins OS").
    pub label: &'static str,
    /// Default SSH username for this firmware.
    pub default_user: &'static str,
    /// Restart command, or None when SSH has no documented restart here.
    pub restart_cmd: Option<&'static str>,
    /// Log command (reads, bounded), or None.
    pub log_cmd: Option<&'static str>,
    /// Stock-S9 bmminer config edit lives here.
    pub bmminer_conf: bool,
    /// Run from TerraMine against real hardware and verified. Until then
    /// every SSH path for this shell is dry-run only in the app.
    pub hw_confirmed: bool,
}

pub const BRAIINS_RESTART: &str = "if [ -x /etc/init.d/bosminer ]; then /etc/init.d/bosminer restart; else /etc/init.d/S99bosminer stop; /etc/init.d/S99bosminer start; fi";

/// Why a class has no SSH path (shown in paths tried).
pub fn no_shell_reason(class: DeviceClass) -> String {
    match class {
        DeviceClass::LuxOs => "LuxOS documents no shell commands; restart and logs go through its API (resetminer, logs)".into(),
        DeviceClass::Vnish => "VNish: no documented shell commands".into(),
        DeviceClass::Whatsminer => "Whatsminer: no shell access; use the btminer API".into(),
        DeviceClass::AxeOs => "AxeOS (ESP32) has no shell".into(),
        other => format!("{other:?}: no known root shell"),
    }
}

/// The stock S9 bmminer config path (confirmed by hand on a stock S9).
pub const S9_CONF: &str = "/config/bmminer.conf";
/// The stock S9 bmminer service control (confirmed by hand on a stock S9).
pub const S9_BMMINER_SVC: &str = "/etc/init.d/bmminer.sh";

/// Known shells, by device class. Firmwares without a documented root shell
/// are absent, so [`shell_for`] returns None and every SSH action is
/// refused for them.
const SHELLS: &[ShellProfile] = &[
    // Braiins OS: Zynq boards ship /etc/init.d/bosminer; BBB/AML/CVITEK
    // boards ship S99bosminer (stop + start). One fixed compound string.
    // Logs: OpenWrt logread (plausible, not documented by Braiins).
    ShellProfile {
        class: DeviceClass::BraiinsOs,
        label: "Braiins OS",
        default_user: "root",
        restart_cmd: Some(BRAIINS_RESTART),
        log_cmd: Some("logread -l 400"),
        bmminer_conf: false,
        hw_confirmed: false,
    },
    // Stock Antminer (S9 and kin): dropbear shell, bmminer service and
    // config. Restart and config path confirmed on the S9 by hand; SSH runs
    // from TerraMine itself are not hardware-confirmed yet.
    ShellProfile {
        class: DeviceClass::AntminerStock,
        label: "Antminer stock (bmminer)",
        default_user: "root",
        restart_cmd: Some("/etc/init.d/bmminer.sh restart"),
        log_cmd: Some("dmesg"),
        bmminer_conf: true,
        hw_confirmed: false,
    },
];

pub fn shell_for(class: DeviceClass) -> Option<&'static ShellProfile> {
    SHELLS.iter().find(|s| s.class == class)
}

/// Can `class` do `action` over SSH? `Ok(the shell)` or a reason.
pub fn supports(class: DeviceClass, action: SshAction) -> Result<&'static ShellProfile, String> {
    let Some(sh) = shell_for(class) else {
        return Err(format!("{}; SSH is not offered", no_shell_reason(class)));
    };
    let ok = match action {
        SshAction::Restart => sh.restart_cmd.is_some(),
        SshAction::ReadLogs => sh.log_cmd.is_some(),
        SshAction::GrantApiWrite => sh.bmminer_conf,
    };
    if ok {
        Ok(sh)
    } else {
        Err(match action {
            SshAction::GrantApiWrite => {
                format!("{} has no bmminer api-allow config over SSH", sh.label)
            }
            _ => format!(
                "{} has no documented SSH {} command",
                sh.label,
                action.name()
            ),
        })
    }
}

/// A single command the runner intends to send. `redact` hides a data blob
/// (a base64 config) in logs and audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCmd {
    pub command: String,
    /// What to show instead of `command` in messages (no secrets, no config).
    pub display: String,
}

impl PlannedCmd {
    pub fn plain(cmd: impl Into<String>) -> Self {
        let cmd = cmd.into();
        Self {
            display: cmd.clone(),
            command: cmd,
        }
    }
    pub fn opaque(cmd: String, display: impl Into<String>) -> Self {
        Self {
            command: cmd,
            display: display.into(),
        }
    }
}

/// Every command string the allow-list recognises, independent of device.
/// A command the runner builds that is not matched here is a bug and is
/// refused before anything is sent.
pub fn is_allowed(cmd: &str) -> bool {
    let c = cmd.trim();
    // Exact service / log commands from the shell table.
    if SHELLS
        .iter()
        .any(|s| s.restart_cmd == Some(c) || s.log_cmd == Some(c))
    {
        return true;
    }
    // The stock-S9 bmminer config flow: a fixed, closed set of shapes over
    // the two known paths only.
    let conf = S9_CONF;
    if c == format!("cat {conf}") {
        return true;
    }
    if c.starts_with(&format!("cp {conf} {conf}.terrabak."))
        && !c[format!("cp {conf} {conf}.terrabak.").len()..]
            .contains(|ch: char| !ch.is_ascii_digit())
    {
        return true;
    }
    // Atomic write: base64 of the new config decoded into a temp file, JSON
    // re-checked on the box, then moved into place. Only these three shapes.
    if c.starts_with("printf %s ") && c.ends_with(&format!("| base64 -d > {conf}.terratmp")) {
        let mid = &c["printf %s ".len()..c.len() - format!("| base64 -d > {conf}.terratmp").len()];
        return is_base64_token(mid.trim());
    }
    if c == format!("mv {conf}.terratmp {conf}") {
        return true;
    }
    false
}

fn is_base64_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 1_048_576
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

/// Stock Antminer generations with the bmminer / cgminer-era root shell
/// (dropbear open by default). Later stock firmware locks SSH.
pub const STOCK_SHELL_MODELS: [&str; 4] = ["S9", "T9", "L3", "S7"];

/// [`supports`] plus the model gate for stock Antminers.
pub fn supports_model(
    class: DeviceClass,
    model: &str,
    action: SshAction,
) -> Result<&'static ShellProfile, String> {
    let sh = supports(class, action)?;
    if class == DeviceClass::AntminerStock {
        let m = model.to_ascii_uppercase();
        let hit = STOCK_SHELL_MODELS.iter().any(|g| {
            m.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w.starts_with(g))
        });
        if !hit {
            return Err(format!("stock Antminer {model}: Bitmain firmware for this generation has SSH locked; no shell path"));
        }
    }
    Ok(sh)
}

/// Plan the generic shell command for a non-destructive action.
pub fn simple_command(sh: &ShellProfile, action: SshAction) -> Option<PlannedCmd> {
    match action {
        SshAction::Restart => sh.restart_cmd.map(PlannedCmd::plain),
        SshAction::ReadLogs => sh.log_cmd.map(PlannedCmd::plain),
        SshAction::GrantApiWrite => None,
    }
}

// ---------------- stock S9: api-allow edit ----------------

/// Add a `W:<cidr>` rule to the front of a bmminer `api-allow` value.
/// cgminer matches the first rule that covers the caller, so the write
/// grant must come first. Returns the new value and whether it changed.
pub fn add_write_rule(current: &str, cidr: &str) -> (String, bool) {
    let rules: Vec<&str> = current
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();
    let want = format!("W:{cidr}");
    if rules
        .first()
        .map(|r| r.eq_ignore_ascii_case(&want))
        .unwrap_or(false)
    {
        return (current.to_string(), false);
    }
    // Drop any existing identical rule elsewhere, keep the rest in order.
    let mut out = vec![want.clone()];
    out.extend(
        rules
            .into_iter()
            .filter(|r| !r.eq_ignore_ascii_case(&want))
            .map(String::from),
    );
    (out.join(","), true)
}

/// Parse a bmminer.conf, add the write rule to `api-allow`, and return the
/// new config text plus whether it changed. Fails when the file is not an
/// object or `api-allow` is not a string.
pub fn grant_api_write_conf(conf: &str, cidr: &str) -> Result<(String, bool), String> {
    validate_cidr(cidr)?;
    let mut v: serde_json::Value =
        serde_json::from_str(conf).map_err(|e| format!("{S9_CONF} is not valid JSON: {e}"))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| format!("{S9_CONF} is not a JSON object"))?;
    let current = match obj.get("api-allow") {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => return Err(format!("api-allow is {other}, expected a string")),
    };
    let (next, changed) = add_write_rule(&current, cidr);
    obj.insert("api-allow".into(), serde_json::Value::String(next));
    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    // Re-validate the output parses (defence in depth before it is written).
    serde_json::from_str::<serde_json::Value>(&out)
        .map_err(|e| format!("rebuilt config did not re-parse: {e}"))?;
    Ok((out, changed))
}

/// A LAN CIDR like `198.51.100.0/24`, `10.0.0.0/8` or a single host `/32`.
pub fn validate_cidr(cidr: &str) -> Result<(), String> {
    let (ip, bits) = cidr
        .split_once('/')
        .ok_or_else(|| format!("{cidr} is not a CIDR (expected a.b.c.d/len)"))?;
    let len: u8 = bits
        .parse()
        .map_err(|_| format!("{cidr}: bad prefix length"))?;
    if len > 32 {
        return Err(format!("{cidr}: prefix length > 32"));
    }
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 || octets.iter().any(|o| o.parse::<u8>().is_err()) {
        return Err(format!("{cidr}: not an IPv4 address"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_shells_and_actions() {
        assert_eq!(
            shell_for(DeviceClass::BraiinsOs).unwrap().label,
            "Braiins OS"
        );
        assert!(shell_for(DeviceClass::Whatsminer).is_none());
        assert!(
            shell_for(DeviceClass::AxeOs).is_none(),
            "AxeOS has no shell"
        );
        assert!(supports(DeviceClass::BraiinsOs, SshAction::Restart).is_ok());
        assert!(supports(DeviceClass::BraiinsOs, SshAction::GrantApiWrite).is_err());
        assert!(supports(DeviceClass::AntminerStock, SshAction::GrantApiWrite).is_ok());
        assert!(supports(DeviceClass::Vnish, SshAction::ReadLogs)
            .unwrap_err()
            .contains("no documented shell"));
        assert!(supports(DeviceClass::LuxOs, SshAction::Restart)
            .unwrap_err()
            .contains("resetminer"));
        assert!(
            SHELLS.iter().all(|s| !s.hw_confirmed),
            "nothing is hardware-confirmed yet"
        );
    }

    #[test]
    fn stock_shell_only_on_bmminer_era_models() {
        assert!(supports_model(
            DeviceClass::AntminerStock,
            "Antminer S9",
            SshAction::GrantApiWrite
        )
        .is_ok());
        assert!(supports_model(
            DeviceClass::AntminerStock,
            "Antminer S9j",
            SshAction::Restart
        )
        .is_ok());
        assert!(supports_model(
            DeviceClass::AntminerStock,
            "Antminer S19 Pro",
            SshAction::Restart
        )
        .unwrap_err()
        .contains("SSH locked"));
        assert!(supports_model(DeviceClass::BraiinsOs, "Antminer S19", SshAction::Restart).is_ok());
    }

    #[test]
    fn allowlist_accepts_only_the_planned_shapes() {
        assert!(is_allowed(BRAIINS_RESTART));
        assert!(
            !is_allowed("/etc/init.d/bosminer restart"),
            "only the full fixed string"
        );
        assert!(is_allowed("dmesg"));
        assert!(is_allowed("/etc/init.d/bmminer.sh restart"));
        assert!(is_allowed("logread -l 400"));
        assert!(is_allowed("cat /config/bmminer.conf"));
        assert!(is_allowed(
            "cp /config/bmminer.conf /config/bmminer.conf.terrabak.1730000000"
        ));
        assert!(is_allowed(
            "printf %s QUJD | base64 -d > /config/bmminer.conf.terratmp"
        ));
        assert!(is_allowed(
            "mv /config/bmminer.conf.terratmp /config/bmminer.conf"
        ));
        // Refused:
        for bad in [
            "rm -rf /",
            "/etc/init.d/bmminer.sh restart; rm -rf /",
            "/etc/init.d/luxminer restart",
            "cat /etc/shadow",
            "cp /config/bmminer.conf /tmp/x",
            "cp /config/bmminer.conf /config/bmminer.conf.terrabak.; rm x",
            "printf %s not$b64 | base64 -d > /config/bmminer.conf.terratmp",
            "printf %s QUJD | base64 -d > /config/bmminer.conf",
            "mv /config/bmminer.conf.terratmp /etc/passwd",
            "reboot",
        ] {
            assert!(!is_allowed(bad), "{bad} must be refused");
        }
    }

    #[test]
    fn write_rule_goes_first_and_is_idempotent() {
        assert_eq!(
            add_write_rule("R:0/0", "198.51.100.0/24"),
            ("W:198.51.100.0/24,R:0/0".into(), true)
        );
        assert_eq!(
            add_write_rule("", "10.0.0.0/8"),
            ("W:10.0.0.0/8".into(), true)
        );
        assert_eq!(
            add_write_rule("W:198.51.100.0/24,R:0/0", "198.51.100.0/24"),
            ("W:198.51.100.0/24,R:0/0".into(), false)
        );
        // An existing W rule that is not first is moved to the front.
        assert_eq!(
            add_write_rule("R:0/0,W:198.51.100.0/24", "198.51.100.0/24"),
            ("W:198.51.100.0/24,R:0/0".into(), true)
        );
    }

    #[test]
    fn grant_edit_only_touches_api_allow_and_stays_valid_json() {
        let conf = r#"{"pools":[{"url":"stratum+tcp://p:3333","user":"w"}],"api-listen":"true","api-network":"true","api-allow":"R:0/0"}"#;
        let (out, changed) = grant_api_write_conf(conf, "198.51.100.0/24").unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["api-allow"], "W:198.51.100.0/24,R:0/0");
        assert_eq!(v["pools"][0]["user"], "w", "other keys untouched");
        assert_eq!(v["api-listen"], "true");
        // Idempotent second pass.
        let (_, changed2) = grant_api_write_conf(&out, "198.51.100.0/24").unwrap();
        assert!(!changed2);
        // Missing api-allow is created.
        let (out, _) = grant_api_write_conf(r#"{"pools":[]}"#, "10.0.0.0/8").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap()["api-allow"],
            "W:10.0.0.0/8"
        );
        assert!(grant_api_write_conf("not json", "10.0.0.0/8")
            .unwrap_err()
            .contains("not valid JSON"));
        assert!(grant_api_write_conf(r#"{"api-allow":5}"#, "10.0.0.0/8")
            .unwrap_err()
            .contains("expected a string"));
        assert!(grant_api_write_conf("{}", "bad").is_err());
    }
}
