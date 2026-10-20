//! SSH transport seam and the russh client.
//!
//! [`HostKeyPolicy`] decides inside the handshake, before any credential is
//! sent: `Capture` accepts any key only to report its fingerprint (the
//! first-use prompt; no authentication happens), `Pinned` accepts only the
//! fingerprint the operator already trusted.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::plan::is_allowed;
use super::store::{SshLogin, SshSecret};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// Report the key, authenticate nothing.
    Capture,
    /// Only this SHA256 fingerprint.
    Pinned(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit == Some(0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SshError {
    NotAllowed,
    HostKeyMismatch { pinned: String, got: String },
    AuthFailed,
    Connect(String),
    Channel(String),
    Timeout,
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("command refused by the SSH allow-list"),
            Self::HostKeyMismatch { pinned, got } => {
                write!(f, "host key changed: pinned {pinned}, got {got}")
            }
            Self::AuthFailed => f.write_str("authentication failed"),
            Self::Connect(e) => write!(f, "connect: {e}"),
            Self::Channel(e) => write!(f, "channel: {e}"),
            Self::Timeout => f.write_str("timed out"),
        }
    }
}

impl std::error::Error for SshError {}

pub trait SshSession: Send {
    fn exec<'a>(&'a mut self, command: &'a str) -> BoxFut<'a, Result<ExecOutput, SshError>>;
    fn close(self: Box<Self>) -> BoxFut<'static, ()>;
}

pub trait SshTransport: Send + Sync {
    /// Handshake only; return the server's SHA256 host-key fingerprint.
    fn fetch_host_key<'a>(&'a self, addr: &'a str) -> BoxFut<'a, Result<String, SshError>>;
    /// Handshake with `Pinned` policy, then authenticate.
    fn open<'a>(
        &'a self,
        addr: &'a str,
        pinned: &'a str,
        login: &'a SshLogin,
    ) -> BoxFut<'a, Result<Box<dyn SshSession>, SshError>>;
}

/// Allow-list guard around any session: the only path commands take.
pub async fn exec_allowed(s: &mut dyn SshSession, command: &str) -> Result<ExecOutput, SshError> {
    if !is_allowed(command) {
        return Err(SshError::NotAllowed);
    }
    s.exec(command).await
}

// ---------------- russh ----------------

use russh::client;
use russh::keys::{HashAlg, PublicKeyOrCertificate};

struct Handler {
    policy: HostKeyPolicy,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Certificates are not trusted by CA here; pin the key they carry.
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                key.fingerprint(HashAlg::Sha256).to_string()
            }
            PublicKeyOrCertificate::Certificate(c) => {
                c.public_key().fingerprint(HashAlg::Sha256).to_string()
            }
        };
        *self.seen.lock().unwrap() = Some(fp.clone());
        Ok(match &self.policy {
            HostKeyPolicy::Capture => true,
            HostKeyPolicy::Pinned(p) => *p == fp,
        })
    }
}

pub struct RusshTransport {
    pub timeout: Duration,
    /// Max bytes of output kept per command.
    pub max_output: usize,
}

impl Default for RusshTransport {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            max_output: 512 * 1024,
        }
    }
}

fn config(timeout: Duration) -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(timeout),
        ..Default::default()
    })
}

type Conn = client::Handle<Handler>;

async fn handshake(
    addr: &str,
    policy: HostKeyPolicy,
    timeout: Duration,
) -> Result<(Conn, String), SshError> {
    let seen = Arc::new(Mutex::new(None));
    let h = Handler {
        policy: policy.clone(),
        seen: seen.clone(),
    };
    let r = tokio::time::timeout(timeout, client::connect(config(timeout), addr, h))
        .await
        .map_err(|_| SshError::Timeout)?;
    let got = seen.lock().unwrap().clone();
    match r {
        Ok(conn) => Ok((conn, got.unwrap_or_default())),
        Err(e) => match (policy, got) {
            (HostKeyPolicy::Pinned(p), Some(g)) if p != g => {
                Err(SshError::HostKeyMismatch { pinned: p, got: g })
            }
            _ => Err(SshError::Connect(e.to_string())),
        },
    }
}

struct RusshSession {
    conn: Conn,
    timeout: Duration,
    max_output: usize,
}

impl SshSession for RusshSession {
    fn exec<'a>(&'a mut self, command: &'a str) -> BoxFut<'a, Result<ExecOutput, SshError>> {
        Box::pin(async move {
            let fut = async {
                let mut ch = self
                    .conn
                    .channel_open_session()
                    .await
                    .map_err(|e| SshError::Channel(e.to_string()))?;
                ch.exec(true, command)
                    .await
                    .map_err(|e| SshError::Channel(e.to_string()))?;
                let (mut out, mut err, mut exit) = (Vec::new(), Vec::new(), None);
                while let Some(msg) = ch.wait().await {
                    match msg {
                        russh::ChannelMsg::Data { data } if out.len() < self.max_output => {
                            out.extend_from_slice(&data)
                        }
                        russh::ChannelMsg::ExtendedData { data, .. }
                            if err.len() < self.max_output =>
                        {
                            err.extend_from_slice(&data)
                        }
                        russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                        _ => {}
                    }
                }
                Ok(ExecOutput {
                    exit,
                    stdout: String::from_utf8_lossy(&out).into_owned(),
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                })
            };
            tokio::time::timeout(self.timeout, fut)
                .await
                .map_err(|_| SshError::Timeout)?
        })
    }
    fn close(self: Box<Self>) -> BoxFut<'static, ()> {
        Box::pin(async move {
            let _ = self
                .conn
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
        })
    }
}

impl SshTransport for RusshTransport {
    fn fetch_host_key<'a>(&'a self, addr: &'a str) -> BoxFut<'a, Result<String, SshError>> {
        Box::pin(async move {
            let (conn, fp) = handshake(addr, HostKeyPolicy::Capture, self.timeout).await?;
            let _ = conn
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
            if fp.is_empty() {
                return Err(SshError::Connect("no host key offered".into()));
            }
            Ok(fp)
        })
    }

    fn open<'a>(
        &'a self,
        addr: &'a str,
        pinned: &'a str,
        login: &'a SshLogin,
    ) -> BoxFut<'a, Result<Box<dyn SshSession>, SshError>> {
        Box::pin(async move {
            let (mut conn, _) = handshake(
                addr,
                HostKeyPolicy::Pinned(pinned.to_string()),
                self.timeout,
            )
            .await?;
            let auth = match &login.secret {
                SshSecret::Password(p) => conn.authenticate_password(&login.username, p).await,
                SshSecret::PrivateKey(pem) => {
                    let key = russh::keys::decode_secret_key(pem, None)
                        .map_err(|_| SshError::AuthFailed)?;
                    let hash = conn
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    conn.authenticate_publickey(
                        &login.username,
                        russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                }
            };
            match auth {
                Ok(r) if r.success() => Ok(Box::new(RusshSession {
                    conn,
                    timeout: self.timeout,
                    max_output: self.max_output,
                }) as Box<dyn SshSession>),
                Ok(_) => Err(SshError::AuthFailed),
                Err(e) => Err(SshError::Connect(e.to_string())),
            }
        })
    }
}
