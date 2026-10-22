//! Stock Antminer S9 admin path, used only where asic-rs has nothing:
//!
//! - Hashboards: asic-rs `antminer/v2020` reads `chain_acn1..3` /
//!   `chain_rate1..3` from bmminer `stats`, but the S9's bmminer numbers
//!   its chains 6..8 (`chain_acn6`, `temp2_6`, …), so asic-rs returns
//!   boards with no data there. This path reads the same allowlisted
//!   `stats` call and maps every `chain_acnN` it finds. It runs after
//!   asic-rs, and only when asic-rs returned no board data.
//! - Logs: asic-rs reads `/cgi-bin/log.cgi` (2020+ stock firmware); the
//!   2018 S9 web UI serves its kernel log as `/cgi-bin/get_kernel_log.cgi`
//!   (digest auth, read-only). Tried after asic-rs.
//! - Password change and factory reset: no S9-specific override. asic-rs's
//!   stock Antminer path (`passwd.cgi` / `reset_conf.cgi`) is the only
//!   route, dry run only until hardware-confirmed.

use diqwest::WithDigestAuth;
use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::Value;

use super::{AdminAction, AdminBackend, Hashboard};
use crate::control::overrides::cgminer_rpc::CgminerRpc;
use crate::control::overrides::DigestCredentials;

pub const NO_WRITE_OVERRIDE: &str =
    "no S9-specific override; asic-rs's stock Antminer path is the only route";
pub const RPC_NO_LOGS: &str = "bmminer RPC has no log command";
/// Chips per S9-family hashboard (BM1387 × 63).
pub const S9_CHIPS: u16 = 63;

/// `/cgi-bin/get_kernel_log.cgi` over the S9's digest CGI (logs only).
pub struct S9CgiLogs {
    pub base: String,
    pub client: Client,
    pub creds: DigestCredentials,
}

impl AdminBackend for S9CgiLogs {
    fn id(&self) -> &'static str {
        "s9-cgi"
    }
    fn supports(&self, action: AdminAction) -> Result<(), String> {
        match action {
            AdminAction::ReadLogs => Ok(()),
            AdminAction::Hashboards => {
                Err("hashboards come from asic-rs, else bmminer stats".into())
            }
            AdminAction::ChangePassword | AdminAction::FactoryReset => {
                Err(NO_WRITE_OVERRIDE.into())
            }
        }
    }
    fn read_logs<'a>(
        &'a self,
        login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let c = login.unwrap_or(&self.creds);
            let r = self
                .client
                .get(format!("{}/cgi-bin/get_kernel_log.cgi", self.base))
                .send_digest_auth((c.username.as_str(), c.password.as_str()))
                .await?;
            anyhow::ensure!(
                r.status().is_success(),
                "get_kernel_log.cgi: {}",
                r.status()
            );
            Ok(r.text().await?)
        })
    }
}

/// Hashboards from bmminer `stats` (allowlisted read; hashboards only).
pub struct BmminerBoards {
    pub rpc: CgminerRpc,
    pub model: String,
}

impl AdminBackend for BmminerBoards {
    fn id(&self) -> &'static str {
        "cgminer-rpc"
    }
    fn supports(&self, action: AdminAction) -> Result<(), String> {
        match action {
            AdminAction::Hashboards => Ok(()),
            AdminAction::ReadLogs => Err(RPC_NO_LOGS.into()),
            AdminAction::ChangePassword | AdminAction::FactoryReset => {
                Err("bmminer RPC has no password or reset command (never sent)".into())
            }
        }
    }
    fn hashboards(&self) -> BoxFuture<'_, anyhow::Result<Vec<Hashboard>>> {
        Box::pin(async move {
            let v = self.rpc.call("stats").await?;
            let expected = self
                .model
                .to_ascii_uppercase()
                .contains("S9")
                .then_some(S9_CHIPS);
            let b = parse_bmminer_chains(&v, expected);
            anyhow::ensure!(!b.is_empty(), "bmminer stats reported no chains");
            Ok(b)
        })
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Boards from bmminer `stats`: one per `chain_acnN` key, in N order.
/// Working chips = `o` marks in `chain_acsN` (else `chain_acnN`); `x`
/// marks are dead chips. Hashrate `chain_rateN` (GH/s), board temperature
/// `tempN`, chip temperature `temp2_N`; zeros are empty sensors.
pub fn parse_bmminer_chains(v: &Value, expected: Option<u16>) -> Vec<Hashboard> {
    let Some(stats) = v.get("STATS").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let Some(o) = stats
        .iter()
        .filter_map(|x| x.as_object())
        .find(|o| o.keys().any(|k| k.starts_with("chain_acn")))
    else {
        return Vec::new();
    };
    let mut chains: Vec<u32> = o
        .keys()
        .filter_map(|k| k.strip_prefix("chain_acn").and_then(|n| n.parse().ok()))
        .collect();
    chains.sort_unstable();
    let pos = |t: Option<f64>| t.filter(|t| *t > 0.0);
    chains
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let get = |k: String| o.get(&k);
            let acn = get(format!("chain_acn{n}")).and_then(num).map(|x| x as u16);
            let acs = get(format!("chain_acs{n}")).and_then(|s| s.as_str());
            let working = match acs {
                Some(s) if s.chars().any(|c| c == 'o' || c == 'x') => {
                    Some(s.chars().filter(|c| *c == 'o').count() as u16)
                }
                _ => acn,
            };
            let rate = get(format!("chain_rate{n}")).and_then(num);
            Hashboard {
                position: i as u8,
                hashrate_ghs: rate,
                board_temp_c: pos(get(format!("temp{n}")).and_then(num)),
                outlet_chip_temp_c: pos(get(format!("temp2_{n}")).and_then(num)),
                working_chips: working,
                expected_chips: expected.or(acn),
                active: Some(working.unwrap_or(0) > 0 || rate.unwrap_or(0.0) > 0.0),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_s9_chains_6_to_8() {
        let raw = br#"{"STATUS":[{"STATUS":"S"}],"STATS":[{"BMMiner":"2.0.0"}{"chain_acn6":63,"chain_acn7":29,"chain_acn8":0,"chain_acs6":" oooooooo oooooooo oooooooo oooooooo oooooooo oooooooo oooooooo ooooooo","chain_acs7":" oooooooo oooooooo oooooooo ooooo","chain_acs8":"","chain_rate6":"4610.5","chain_rate7":"0.00","chain_rate8":"","temp6":55,"temp2_6":71,"temp7":52,"temp2_7":0,"temp8":0,"temp2_8":0}]}"#;
        let v = crate::control::overrides::cgminer_rpc::parse_reply(raw).unwrap();
        let b = parse_bmminer_chains(&v, Some(S9_CHIPS));
        assert_eq!(b.len(), 3);
        assert_eq!(
            (b[0].working_chips, b[0].expected_chips, b[0].hashrate_ghs),
            (Some(63), Some(63), Some(4610.5))
        );
        assert_eq!(b[0].outlet_chip_temp_c, Some(71.0));
        assert!(b[0].problem().is_none());
        assert_eq!(b[1].working_chips, Some(29));
        assert_eq!(b[1].problem().as_deref(), Some("29/63 chips, 0 hashrate"));
        assert_eq!(b[1].outlet_chip_temp_c, None, "0 = empty sensor");
        assert_eq!(b[2].working_chips, Some(0));
        assert_eq!(b[2].active, Some(false));
        let s = super::super::summarize_boards(&b);
        assert!(s.contains("2 degraded"), "{s}");
    }

    #[test]
    fn x_marks_are_dead_chips() {
        let v = serde_json::json!({"STATS":[{"chain_acn1":4,"chain_acs1":"ooxo"}]});
        assert_eq!(parse_bmminer_chains(&v, None)[0].working_chips, Some(3));
        assert_eq!(parse_bmminer_chains(&v, None)[0].expected_chips, Some(4));
        assert!(parse_bmminer_chains(&serde_json::json!({"STATS":[{}]}), None).is_empty());
    }
}
