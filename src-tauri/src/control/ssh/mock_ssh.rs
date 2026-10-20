//! A real SSH server (russh) with a tiny fake shell, for end-to-end tests:
//! it understands exactly the command shapes TerraMine's allow-list can
//! produce, keeps files in memory (the stock-S9 `/config/bmminer.conf`),
//! counts mining restarts and logs every command it received. Anything else
//! exits 127, so a test can prove nothing unexpected was sent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::plan::{BRAIINS_RESTART, S9_BMMINER_SVC, S9_CONF};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockShell {
    Braiins,
    S9Stock,
}

/// Simulated faults for rollback tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockSshFault {
    /// `mv` into the config path fails (exit 1).
    WriteFails,
    /// After a write the config reads back unchanged (write silently lost).
    WriteIgnored,
    /// bmminer restart exits 1.
    RestartFails,
}

pub type RestartHook = Arc<dyn Fn(Option<&str>) + Send + Sync>;

#[derive(Default)]
struct State {
    files: HashMap<String, String>,
    commands: Vec<String>,
    restarts: u32,
    logins: u32,
    fault: Option<MockSshFault>,
}

pub struct MockSsh {
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub user: String,
    pub password: String,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
}

impl Drop for MockSsh {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub const MOCK_CONF: &str = r#"{
"pools" : [
{ "url" : "stratum+tcp://pool.example.com:3333", "user" : "acct.worker1", "pass" : "x" }
],
"api-listen" : true,
"api-network" : true,
"api-allow" : "R:0/0",
"bitmain-use-vil" : true,
"bitmain-freq" : "550"
}
"#;

impl MockSsh {
    /// `on_restart` is told the current config whenever the miner process
    /// restarts (wire it to the cgminer RPC mock).
    /// `key_seed` picks the host key (a different seed = a "changed" key).
    pub async fn spawn(
        shell: MockShell,
        key_seed: u8,
        on_restart: Option<RestartHook>,
    ) -> std::io::Result<Self> {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[key_seed; 32]));
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: std::time::Duration::from_millis(10),
            auth_rejection_time_initial: Some(std::time::Duration::ZERO),
            ..Default::default()
        });
        let mut st = State::default();
        if shell == MockShell::S9Stock {
            st.files.insert(S9_CONF.into(), MOCK_CONF.into());
        }
        let state = Arc::new(Mutex::new(st));
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let stop = CancellationToken::new();
        let (user, password) = ("root".to_string(), "mock-ssh-pass".to_string());
        let h = Shell {
            shell,
            state: state.clone(),
            on_restart,
            user: user.clone(),
            password: password.clone(),
        };
        let stop2 = stop.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = tokio::select! {
                    _ = stop2.cancelled() => return,
                    r = l.accept() => match r { Ok(x) => x, Err(_) => return },
                };
                let (config, h) = (config.clone(), h.clone());
                tokio::spawn(async move {
                    if let Ok(s) = server::run_stream(config, sock, h).await {
                        let _ = s.await;
                    }
                });
            }
        });
        Ok(Self {
            addr,
            fingerprint,
            user,
            password,
            state,
            stop,
        })
    }

    pub fn commands(&self) -> Vec<String> {
        self.state.lock().unwrap().commands.clone()
    }
    pub fn restarts(&self) -> u32 {
        self.state.lock().unwrap().restarts
    }
    pub fn logins(&self) -> u32 {
        self.state.lock().unwrap().logins
    }
    pub fn file(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().files.get(path).cloned()
    }
    pub fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = self.state.lock().unwrap().files.keys().cloned().collect();
        v.sort();
        v
    }
    pub fn set_file(&self, path: &str, body: &str) {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.into(), body.into());
    }
    pub fn set_fault(&self, f: Option<MockSshFault>) {
        self.state.lock().unwrap().fault = f;
    }
}

#[derive(Clone)]
struct Shell {
    shell: MockShell,
    state: Arc<Mutex<State>>,
    on_restart: Option<RestartHook>,
    user: String,
    password: String,
}

impl Shell {
    /// (exit, stdout, stderr)
    fn run(&self, cmd: &str) -> (u32, String, String) {
        let mut s = self.state.lock().unwrap();
        s.commands.push(cmd.to_string());
        let restart = match self.shell {
            MockShell::Braiins => cmd == BRAIINS_RESTART,
            MockShell::S9Stock => cmd == format!("{S9_BMMINER_SVC} restart"),
        };
        if restart {
            if s.fault == Some(MockSshFault::RestartFails) {
                return (1, String::new(), "restart failed".into());
            }
            s.restarts += 1;
            let conf = s.files.get(S9_CONF).cloned();
            drop(s);
            if let Some(h) = &self.on_restart {
                h(conf.as_deref());
            }
            return (0, "Restarting miner\n".into(), String::new());
        }
        if cmd == "logread -l 400" && self.shell == MockShell::Braiins {
            return (
                0,
                "bosminer[1]: started\nbosminer[1]: pool connected\n".into(),
                String::new(),
            );
        }
        if cmd == "dmesg" && self.shell == MockShell::S9Stock {
            return (
                0,
                "[0.000] Linux version\n[1.000] bitmain: chain 6 ok\n".into(),
                String::new(),
            );
        }
        if let Some(path) = cmd.strip_prefix("cat ") {
            return match s.files.get(path) {
                Some(b) => (0, b.clone(), String::new()),
                None => (1, String::new(), format!("cat: {path}: No such file\n")),
            };
        }
        if let Some(rest) = cmd.strip_prefix("cp ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            if let [from, to] = parts[..] {
                if let Some(b) = s.files.get(from).cloned() {
                    s.files.insert(to.into(), b);
                    return (0, String::new(), String::new());
                }
            }
            return (1, String::new(), "cp failed\n".into());
        }
        if let Some(rest) = cmd.strip_prefix("printf %s ") {
            if let Some((b64, path)) = rest.split_once(" | base64 -d > ") {
                return match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) => {
                        s.files
                            .insert(path.into(), String::from_utf8_lossy(&b).into_owned());
                        (0, String::new(), String::new())
                    }
                    Err(_) => (1, String::new(), "base64: invalid input\n".into()),
                };
            }
        }
        if let Some(rest) = cmd.strip_prefix("mv ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            if let [from, to] = parts[..] {
                match s.fault {
                    Some(MockSshFault::WriteFails) if to == S9_CONF => {
                        return (1, String::new(), "mv: read-only\n".into())
                    }
                    Some(MockSshFault::WriteIgnored) if to == S9_CONF => {
                        s.files.remove(from);
                        return (0, String::new(), String::new());
                    }
                    _ => {}
                }
                if let Some(b) = s.files.remove(from) {
                    s.files.insert(to.into(), b);
                    return (0, String::new(), String::new());
                }
            }
            return (1, String::new(), "mv failed\n".into());
        }
        (127, String::new(), format!("sh: {cmd}: not found\n"))
    }
}

impl server::Handler for Shell {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == self.user && password == self.password {
            self.state.lock().unwrap().logins += 1;
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        let (code, out, err) = self.run(&cmd);
        session.channel_success(channel)?;
        if !out.is_empty() {
            session.data(channel, out.into_bytes())?;
        }
        if !err.is_empty() {
            session.extended_data(channel, 1, err.into_bytes())?;
        }
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}
