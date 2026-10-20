//! AxeOS (Bitaxe / NerdAxe) admin path. asic-rs 0.8.6 reports
//! `supports_read_logs() = false` for every Bitaxe backend; ESP-Miner
//! streams its log over the `GET /api/ws` WebSocket instead (the AxeOS
//! web UI's log view). TerraMine reads that socket for a short window,
//! sends nothing on it but the closing frame, and returns the lines.
//! Read-only: nothing is changed on the miner.
//!
//! AxeOS has no login and no factory-reset endpoint, so password change
//! and reset are reported as impossible here. Hashboard detail comes from
//! asic-rs (`get_data`), not from this path.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{AdminAction, AdminBackend};
use crate::control::overrides::DigestCredentials;

pub const NO_PASSWORD: &str = "AxeOS has no login or password (its HTTP API is open on the LAN)";
pub const NO_RESET: &str = "the AxeOS HTTP API has no factory-reset endpoint";
pub const NO_BOARDS: &str = "hashboard detail comes from asic-rs get_data";

pub struct AxeOsAdmin {
    /// `http://host[:port]`.
    pub base: String,
    pub window: Duration,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl AxeOsAdmin {
    pub fn new(base: String) -> Self {
        Self {
            base,
            window: Duration::from_secs(4),
            timeout: Duration::from_secs(10),
            max_bytes: super::DEFAULT_MAX_LOG_BYTES,
        }
    }
}

impl AdminBackend for AxeOsAdmin {
    fn id(&self) -> &'static str {
        "axeos"
    }
    fn supports(&self, action: AdminAction) -> Result<(), String> {
        match action {
            AdminAction::ReadLogs => Ok(()),
            AdminAction::Hashboards => Err(NO_BOARDS.into()),
            AdminAction::ChangePassword => Err(NO_PASSWORD.into()),
            AdminAction::FactoryReset => Err(NO_RESET.into()),
        }
    }
    fn read_logs<'a>(
        &'a self,
        _login: Option<&'a DigestCredentials>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(capture_ws_log(
            &self.base,
            "/api/ws",
            self.window,
            self.timeout,
            self.max_bytes,
        ))
    }
}

/// `host:port` from `http://host[:port][/…]`.
fn authority(base: &str) -> anyhow::Result<(String, String)> {
    let rest = base
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("axeos log socket needs an http:// base, got {base}"))?;
    let host = rest.split('/').next().unwrap_or_default();
    anyhow::ensure!(!host.is_empty(), "empty host in {base}");
    let addr = if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
    {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    Ok((host.to_string(), addr))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One parsed server frame: (fin, opcode, payload, bytes consumed), or
/// `None` when `buf` does not hold a whole frame yet.
pub fn parse_frame(buf: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let fin = buf[0] & 0x80 != 0;
    let op = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = u64::from(buf[1] & 0x7f);
    let mut at = 2usize;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u64::from(u16::from_be_bytes([buf[2], buf[3]]));
        at = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?);
        at = 10;
    }
    let mask = if masked {
        if buf.len() < at + 4 {
            return None;
        }
        let m = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(m)
    } else {
        None
    };
    let len = usize::try_from(len).ok()?;
    if buf.len() < at + len {
        return None;
    }
    let mut p = buf[at..at + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Some((fin, op, p, at + len))
}

/// Read the ESP-Miner log socket for `window` (or until `max_bytes`).
pub async fn capture_ws_log(
    base: &str,
    path: &str,
    window: Duration,
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let (host, addr) = authority(base)?;
    let mut sock = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    let key = base64(&rand::random::<[u8; 16]>());
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        anyhow::ensure!(
            buf.len() < 16 * 1024,
            "log socket: oversized handshake reply"
        );
        let n = tokio::time::timeout(timeout, sock.read(&mut chunk))
            .await
            .map_err(|_| anyhow::anyhow!("log socket: handshake timeout"))??;
        anyhow::ensure!(n > 0, "log socket closed during handshake");
        buf.extend_from_slice(&chunk[..n]);
    };
    let status = String::from_utf8_lossy(&buf[..head_end])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    anyhow::ensure!(
        status.split_whitespace().nth(1) == Some("101"),
        "log socket not available ({})",
        status.trim()
    );
    let mut data = buf.split_off(head_end);
    let mut text = Vec::new();
    let mut msg = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    'read: loop {
        while let Some((fin, op, payload, used)) = parse_frame(&data) {
            data.drain(..used);
            match op {
                0 | 1 => {
                    msg.extend_from_slice(&payload);
                    if fin {
                        text.extend_from_slice(&msg);
                        if !msg.ends_with(b"\n") {
                            text.push(b'\n');
                        }
                        msg.clear();
                    }
                }
                8 => break 'read,
                _ => {} // ping / pong / binary: ignored
            }
            if text.len() >= max_bytes {
                break 'read;
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => data.extend_from_slice(&chunk[..n]),
        }
    }
    // Client close frame (masked, empty payload), then drop the socket.
    let m: [u8; 4] = rand::random();
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        sock.write_all(&[0x88, 0x80, m[0], m[1], m[2], m[3]]),
    )
    .await;
    Ok(String::from_utf8_lossy(&text).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_examples() {
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn parses_short_extended_and_masked_frames() {
        let (fin, op, p, used) = parse_frame(&[0x81, 0x03, b'a', b'b', b'c', 0xff]).unwrap();
        assert!(fin);
        assert_eq!((op, p.as_slice(), used), (1, &b"abc"[..], 5));
        let long = vec![b'x'; 300];
        let mut f = vec![0x81, 126, 0x01, 0x2c];
        f.extend_from_slice(&long);
        assert_eq!(parse_frame(&f).unwrap().2.len(), 300);
        assert!(
            parse_frame(&f[..100]).is_none(),
            "partial frame waits for more"
        );
        let m = [1u8, 2, 3, 4];
        let masked: Vec<u8> = b"hi"
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ m[i % 4])
            .collect();
        let mut f = vec![0x81, 0x82];
        f.extend_from_slice(&m);
        f.extend_from_slice(&masked);
        assert_eq!(parse_frame(&f).unwrap().2, b"hi");
    }

    #[test]
    fn authority_defaults_to_port_80() {
        assert_eq!(authority("http://192.0.2.10").unwrap().1, "192.0.2.10:80");
        assert_eq!(
            authority("http://127.0.0.1:4040").unwrap().1,
            "127.0.0.1:4040"
        );
        assert!(authority("https://x").is_err());
    }

    #[test]
    fn only_logs_are_offered() {
        let a = AxeOsAdmin::new("http://192.0.2.10".into());
        assert!(a.supports(AdminAction::ReadLogs).is_ok());
        for x in [
            AdminAction::Hashboards,
            AdminAction::ChangePassword,
            AdminAction::FactoryReset,
        ] {
            assert!(a.supports(x).is_err(), "{x:?}");
        }
    }
}
