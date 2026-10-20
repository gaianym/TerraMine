//! Audit log viewer (plan step I): read the append-only JSONL audit trail,
//! filter by miner / action / result / time / text, page, and export as CSV
//! or JSON Lines. Read only: the audit file is never rewritten.
//!
//! Secrets never leave this module: every record is passed through
//! [`redact`] before it is returned or exported. Writers already redact
//! (pool passwords serialise as `<redacted>`, logins are never audited), so
//! this is defence in depth for old lines and future fields.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::safety::AuditRecord;

pub const REDACTED: &str = "<redacted>";
pub const MAX_LIMIT: usize = 5000;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditFilter {
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    /// Final status (`verified`, `rolled_back`, `refused`, `dry_run`, ...).
    #[serde(default)]
    pub outcome: Option<String>,
    /// `attempt` or `outcome`; absent = both.
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub since_ms: Option<u64>,
    #[serde(default)]
    pub until_ms: Option<u64>,
    /// Case-insensitive match on message, model, firmware, run id, paths.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditPage {
    /// Newest first, redacted.
    pub records: Vec<AuditRecord>,
    /// Matching records before paging.
    pub total: usize,
    /// Lines that did not parse (shown so nothing is silently hidden).
    pub skipped_lines: usize,
    /// Distinct values across the whole file, for the filter dropdowns.
    pub ips: Vec<String>,
    pub actions: Vec<String>,
    pub outcomes: Vec<String>,
}

fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase().replace('-', "_");
    k == "pass"
        || k == "pwd"
        || k.contains("password")
        || k.contains("passphrase")
        || k.contains("secret")
        || k.contains("token")
        || k.contains("private_key")
        || k.contains("privatekey")
        || k == "authorization"
        || k == "cookie"
        || k == "api_key"
        || k == "apikey"
}

/// Replace the value of every secret-looking key, at any depth.
pub fn redact_value(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if secret_key(k) {
                    if !val.is_null() {
                        *val = Value::String(REDACTED.into());
                    }
                } else {
                    redact_value(val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        _ => {}
    }
}

/// Mask `password=...`, `password: ...`, `"password":"..."` style pairs in
/// free text (messages), keeping the key so the line stays readable.
pub fn redact_text(s: &str) -> String {
    const KEYS: [&str; 6] = [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "private_key",
    ];
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        for k in KEYS {
            if lower[i..].starts_with(k) {
                let mut j = i + k.len();
                // Skip to the separator: optional quote and spaces, then = or :
                while j < s.len() && (bytes[j] == b'"' || bytes[j] == b'\'' || bytes[j] == b' ') {
                    j += 1;
                }
                if j < s.len() && (bytes[j] == b'=' || bytes[j] == b':') {
                    j += 1;
                    while j < s.len() && (bytes[j] == b' ' || bytes[j] == b'"' || bytes[j] == b'\'')
                    {
                        j += 1;
                    }
                    let start = j;
                    while j < s.len()
                        && !matches!(
                            bytes[j],
                            b' ' | b'"' | b'\'' | b',' | b';' | b'}' | b')' | b'\n'
                        )
                    {
                        j += 1;
                    }
                    if j > start && &s[start..j] != REDACTED {
                        out.push_str(&s[i..start]);
                        out.push_str(REDACTED);
                        i = j;
                        continue 'outer;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn redact(mut r: AuditRecord) -> AuditRecord {
    for v in [&mut r.requested, &mut r.pre_state].into_iter().flatten() {
        redact_value(v);
    }
    r.msg = r.msg.map(|m| redact_text(&m));
    r.paths_tried = r.paths_tried.into_iter().map(|p| redact_text(&p)).collect();
    r
}

/// Every parseable record in file order, plus the count of bad lines. A
/// missing file is an empty log.
pub fn load(path: &Path) -> std::io::Result<(Vec<AuditRecord>, usize)> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let (mut out, mut bad) = (Vec::new(), 0);
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditRecord>(&line) {
            Ok(r) => out.push(r),
            Err(_) => bad += 1,
        }
    }
    Ok((out, bad))
}

fn eq(a: &Option<String>, b: &str) -> bool {
    a.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none_or(|a| a.eq_ignore_ascii_case(b))
}

pub fn matches(r: &AuditRecord, f: &AuditFilter) -> bool {
    if !eq(&f.ip, &r.ip)
        || !eq(&f.action, &r.action)
        || !eq(&f.outcome, &r.outcome)
        || !eq(&f.phase, &r.phase)
    {
        return false;
    }
    if f.since_ms.is_some_and(|t| r.ts_ms < t) || f.until_ms.is_some_and(|t| r.ts_ms > t) {
        return false;
    }
    if f.dry_run.is_some_and(|d| d != r.dry_run) {
        return false;
    }
    if let Some(t) = f.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let t = t.to_ascii_lowercase();
        let hay = [
            r.msg.clone().unwrap_or_default(),
            r.model.clone().unwrap_or_default(),
            r.firmware.clone().unwrap_or_default(),
            r.run_id.clone(),
            r.paths_tried.join(" "),
        ]
        .join(" ")
        .to_ascii_lowercase();
        if !hay.contains(&t) {
            return false;
        }
    }
    true
}

/// Matching records, newest first, redacted.
pub fn filtered(records: Vec<AuditRecord>, f: &AuditFilter) -> Vec<AuditRecord> {
    let mut v: Vec<AuditRecord> = records
        .into_iter()
        .filter(|r| matches(r, f))
        .map(redact)
        .collect();
    // Stable: equal timestamps keep file order reversed (attempt before outcome reads naturally newest-first).
    v.reverse();
    v.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    v
}

pub fn page(records: Vec<AuditRecord>, skipped_lines: usize, f: &AuditFilter) -> AuditPage {
    let distinct = |g: fn(&AuditRecord) -> &str| {
        records
            .iter()
            .map(g)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let (ips, actions, outcomes) = (
        distinct(|r| &r.ip),
        distinct(|r| &r.action),
        distinct(|r| &r.outcome),
    );
    let all = filtered(records, f);
    let total = all.len();
    let limit = f.limit.unwrap_or(500).clamp(1, MAX_LIMIT);
    let records = all
        .into_iter()
        .skip(f.offset.unwrap_or(0))
        .take(limit)
        .collect();
    AuditPage {
        records,
        total,
        skipped_lines,
        ips,
        actions,
        outcomes,
    }
}

/// UTC ISO-8601 from epoch milliseconds (no extra date crate).
pub fn iso_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

fn csv_field(s: &str) -> String {
    // Leading = + - @ would run as a formula in a spreadsheet.
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub const CSV_HEADER: &str = "time_utc,ts_ms,run_id,ip,action,phase,outcome,dry_run,irreversible,device_class,model,firmware,snapshot_id,paths_tried,requested,msg";

pub fn to_csv(records: &[AuditRecord]) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for r in records {
        let class = r
            .device_class
            .and_then(|c| serde_json::to_value(c).ok())
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let cols = [
            iso_utc(r.ts_ms),
            r.ts_ms.to_string(),
            r.run_id.clone(),
            r.ip.clone(),
            r.action.clone(),
            r.phase.clone(),
            r.outcome.clone(),
            r.dry_run.to_string(),
            r.irreversible.to_string(),
            class,
            r.model.clone().unwrap_or_default(),
            r.firmware.clone().unwrap_or_default(),
            r.snapshot_id.clone().unwrap_or_default(),
            r.paths_tried.join(" | "),
            r.requested
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.msg.clone().unwrap_or_default(),
        ];
        out.push_str(
            &cols
                .iter()
                .map(|c| csv_field(c))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

pub fn to_jsonl(records: &[AuditRecord]) -> String {
    records
        .iter()
        .filter_map(|r| serde_json::to_string(r).ok())
        .map(|l| l + "\n")
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Jsonl,
}

/// Write the filtered (unpaged), redacted records to `dir`; returns the path.
pub fn export(
    records: Vec<AuditRecord>,
    f: &AuditFilter,
    format: ExportFormat,
    dir: &Path,
    now_ms: u64,
) -> std::io::Result<(std::path::PathBuf, usize)> {
    let all = filtered(records, f);
    let (body, ext) = match format {
        ExportFormat::Csv => (to_csv(&all), "csv"),
        ExportFormat::Jsonl => (to_jsonl(&all), "jsonl"),
    };
    std::fs::create_dir_all(dir)?;
    let stamp = iso_utc(now_ms).replace([':', '.'], "-");
    let path = dir.join(format!("terramine-audit-{stamp}.{ext}"));
    std::fs::write(&path, body)?;
    Ok((path, all.len()))
}
