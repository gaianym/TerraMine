//! Minimal HTTP mock servers for Bitaxe/AxeOS and stock S9 CGI tests.
#![cfg(test)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub struct MockHandle {
    #[allow(dead_code)]
    pub addr: SocketAddr,
    pub base: String,
    pub hits: Arc<Mutex<Vec<String>>>,
    pub counters: Arc<HashMap<&'static str, AtomicUsize>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// AxeOS mock: version string controls pause/resume availability.
pub async fn spawn_axeos(version: &str, include_mining_paused_field: bool) -> MockHandle {
    let version = version.to_string();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let counters: Arc<HashMap<&'static str, AtomicUsize>> = Arc::new(HashMap::from([
        ("restart", AtomicUsize::new(0)),
        ("pause", AtomicUsize::new(0)),
        ("resume", AtomicUsize::new(0)),
        ("info", AtomicUsize::new(0)),
    ]));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel::<()>();
    let hits_c = hits.clone();
    let counters_c = counters.clone();
    let paused_field = include_mining_paused_field;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                acc = listener.accept() => {
                    let Ok((mut sock, _)) = acc else { break };
                    let mut buf = vec![0u8; 4096];
                    let n = match sock.read(&mut buf).await { Ok(n) if n > 0 => n, _ => continue };
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let line = req.lines().next().unwrap_or("");
                    let parts: Vec<_> = line.split_whitespace().collect();
                    let (method, path) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or(""));
                    hits_c.lock().unwrap().push(format!("{method} {path}"));

                    let (status, body) = if method == "GET" && path.starts_with("/api/system/info") {
                        counters_c["info"].fetch_add(1, Ordering::SeqCst);
                        let mut info = format!(
                            r#"{{"version":"{version}","axeOSVersion":"{version}","ASICModel":"BM1366","boardVersion":"401"}}"#
                        );
                        if paused_field {
                            info = format!(
                                r#"{{"version":"{version}","axeOSVersion":"{version}","ASICModel":"BM1366","boardVersion":"401","miningPaused":false}}"#
                            );
                        }
                        ("200 OK", info)
                    } else if method == "POST" && path.starts_with("/api/system/restart") {
                        counters_c["restart"].fetch_add(1, Ordering::SeqCst);
                        ("200 OK", r#"{"message":"System will restart shortly."}"#.into())
                    } else if method == "POST" && path.starts_with("/api/system/pause") {
                        // Emulate 2.10 (no route) vs 2.14+.
                        let ver_ok = {
                            let v = semver::Version::parse(
                                version.trim_start_matches('v').split('-').next().unwrap_or("")
                            ).ok().or_else(|| {
                                let bits: Vec<_> = version.trim_start_matches('v').split('.').collect();
                                match bits.as_slice() {
                                    [a,b] => semver::Version::parse(&format!("{a}.{b}.0")).ok(),
                                    [a,b,c] => semver::Version::parse(&format!("{a}.{b}.{c}")).ok(),
                                    _ => None,
                                }
                            });
                            v.map(|v| v >= semver::Version::new(2,14,0)).unwrap_or(false)
                        };
                        if ver_ok || paused_field {
                            counters_c["pause"].fetch_add(1, Ordering::SeqCst);
                            ("200 OK", r#"{"message":"Mining paused"}"#.into())
                        } else {
                            ("404 Not Found", r#"{"error":"not found"}"#.into())
                        }
                    } else if method == "POST" && path.starts_with("/api/system/resume") {
                        let ver_ok = {
                            let v = semver::Version::parse(
                                version.trim_start_matches('v').split('-').next().unwrap_or("")
                            ).ok().or_else(|| {
                                let bits: Vec<_> = version.trim_start_matches('v').split('.').collect();
                                match bits.as_slice() {
                                    [a,b] => semver::Version::parse(&format!("{a}.{b}.0")).ok(),
                                    [a,b,c] => semver::Version::parse(&format!("{a}.{b}.{c}")).ok(),
                                    _ => None,
                                }
                            });
                            v.map(|v| v >= semver::Version::new(2,14,0)).unwrap_or(false)
                        };
                        if ver_ok || paused_field {
                            counters_c["resume"].fetch_add(1, Ordering::SeqCst);
                            ("200 OK", r#"{"message":"Mining resumed"}"#.into())
                        } else {
                            ("404 Not Found", r#"{"error":"not found"}"#.into())
                        }
                    } else {
                        ("404 Not Found", "not found".into())
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        }
    });

    MockHandle {
        addr,
        base: format!("http://{addr}"),
        hits,
        counters,
        shutdown: Some(tx),
    }
}

/// Stock S9 CGI mock with optional miner-mode and digest auth.
pub async fn spawn_s9(opts: S9MockOpts) -> MockHandle {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let counters: Arc<HashMap<&'static str, AtomicUsize>> = Arc::new(HashMap::from([
        ("reboot", AtomicUsize::new(0)),
        ("conf", AtomicUsize::new(0)),
        ("set_conf", AtomicUsize::new(0)),
        ("deny", AtomicUsize::new(0)),
    ]));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel::<()>();
    let hits_c = hits.clone();
    let counters_c = counters.clone();
    let user = opts.username;
    let pass = opts.password;
    let has_mode = opts.has_miner_mode;
    let accept = opts.accept_auth;
    let realm = "antminer";
    let nonce = "deadbeefcafebabe";

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                acc = listener.accept() => {
                    let Ok((mut sock, _)) = acc else { break };
                    let mut buf = vec![0u8; 8192];
                    let n = match sock.read(&mut buf).await { Ok(n) if n > 0 => n, _ => continue };
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let line = req.lines().next().unwrap_or("");
                    let parts: Vec<_> = line.split_whitespace().collect();
                    let (method, path) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or(""));
                    hits_c.lock().unwrap().push(format!("{method} {path}"));

                    let auth_header = req.lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .map(|l| l.to_string());

                    let authorized = match &auth_header {
                        Some(h) if h.to_ascii_lowercase().contains("digest") => {
                            // Minimal check: username + response present; verify HA1 path.
                            let ok_user = h.contains(&format!("username=\"{user}\""));
                            if !ok_user { false } else {
                                // Recompute digest response if we can parse uri + response.
                                let uri = extract_quoted(h, "uri").unwrap_or_else(|| path.to_string());
                                let client_resp = extract_quoted(h, "response").unwrap_or_default();
                                let cnonce = extract_quoted(h, "cnonce").unwrap_or_default();
                                let nc = extract_param(h, "nc").unwrap_or_else(|| "00000001".into());
                                let qop = extract_quoted(h, "qop").or_else(|| extract_param(h, "qop"));
                                let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
                                let ha2 = md5_hex(&format!("{method}:{uri}"));
                                let expected = if let Some(qop) = qop {
                                    md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
                                } else {
                                    md5_hex(&format!("{ha1}:{nonce}:{ha2}"))
                                };
                                client_resp == expected
                            }
                        }
                        _ => false,
                    };

                    if !authorized {
                        if !accept {
                            counters_c["deny"].fetch_add(1, Ordering::SeqCst);
                        }
                        let body = "Unauthorized";
                        let resp = format!(
                            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"{realm}\", nonce=\"{nonce}\", qop=\"auth\", algorithm=MD5\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        continue;
                    }

                    if !accept {
                        counters_c["deny"].fetch_add(1, Ordering::SeqCst);
                        let body = "Unauthorized";
                        let resp = format!(
                            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"{realm}\", nonce=\"{nonce}\", qop=\"auth\", algorithm=MD5\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        continue;
                    }

                    let (status, body): (&str, String) = if path.starts_with("/cgi-bin/get_miner_conf.cgi") {
                        counters_c["conf"].fetch_add(1, Ordering::SeqCst);
                        if has_mode {
                            ("200 OK", r#"{"miner-mode":"0","pools":[]}"#.into())
                        } else {
                            ("200 OK", r#"{"pools":[],"bitmain-fan-ctrl":false}"#.into())
                        }
                    } else if path.starts_with("/cgi-bin/reboot.cgi") {
                        counters_c["reboot"].fetch_add(1, Ordering::SeqCst);
                        ("200 OK", r#"{"result":"ok"}"#.into())
                    } else if path.starts_with("/cgi-bin/set_miner_conf.cgi") {
                        counters_c["set_conf"].fetch_add(1, Ordering::SeqCst);
                        ("200 OK", r#"{"result":"ok"}"#.into())
                    } else {
                        ("404 Not Found", "no".into())
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        }
    });

    MockHandle {
        addr,
        base: format!("http://{addr}"),
        hits,
        counters,
        shutdown: Some(tx),
    }
}

#[derive(Clone)]
pub struct S9MockOpts {
    pub username: String,
    pub password: String,
    pub has_miner_mode: bool,
    pub accept_auth: bool,
}

impl Default for S9MockOpts {
    fn default() -> Self {
        Self {
            username: "root".into(),
            password: "root".into(),
            has_miner_mode: false,
            accept_auth: true,
        }
    }
}

fn extract_quoted(header: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let idx = header.find(&pat)?;
    let rest = &header[idx + pat.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_param(header: &str, key: &str) -> Option<String> {
    for part in header.split([',', ' ']) {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{key}=")) {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}
