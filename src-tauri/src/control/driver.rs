//! Miner driver abstraction for start / stop / reboot.
//!
//! Assumptions (no hardware or firmware list available yet):
//! - "stop" = pause/sleep (miner stays reachable), "start" = resume.
//! - Firmware-specific behaviour is delegated to asic-rs via [`AsicRsDriver`];
//!   per-firmware overrides can implement [`MinerDriver`] directly.
//! - A reboot often drops the connection before replying, so a transport
//!   error or timeout on reboot is reported as "probably rebooting".

use std::net::IpAddr;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MinerAction {
    Start,
    Stop,
    Reboot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub start: bool,
    pub stop: bool,
    pub reboot: bool,
}

impl Capabilities {
    pub fn supports(&self, action: MinerAction) -> bool {
        match action {
            MinerAction::Start => self.start,
            MinerAction::Stop => self.stop,
            MinerAction::Reboot => self.reboot,
        }
    }
}

pub trait MinerDriver: Send + Sync {
    #[allow(dead_code)]
    fn ip(&self) -> IpAddr;
    fn caps(&self) -> Capabilities;
    /// Ok(true) = accepted, Ok(false) = firmware refused / no-op.
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>>;
}

/// Resolves an IP to a driver (None = no miner found).
pub trait DriverFactory: Send + Sync {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>>;
}

// ---------------- asic-rs backed driver ----------------

pub struct AsicRsDriver {
    ip: IpAddr,
    miner: Box<dyn asic_rs_core::traits::miner::Miner>,
}

impl AsicRsDriver {
    pub fn new(ip: IpAddr, miner: Box<dyn asic_rs_core::traits::miner::Miner>) -> Self {
        Self { ip, miner }
    }
}

impl MinerDriver for AsicRsDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        Capabilities {
            start: self.miner.supports_resume(),
            stop: self.miner.supports_pause(),
            reboot: self.miner.supports_restart(),
        }
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match action {
                MinerAction::Start => self.miner.resume(None).await,
                MinerAction::Stop => self.miner.pause(None).await,
                MinerAction::Reboot => self.miner.restart().await,
            }
        })
    }
}

pub struct AsicRsFactory {
    factory: asic_rs::MinerFactory,
}

impl AsicRsFactory {
    pub fn new(timeout: Duration) -> Self {
        Self {
            factory: asic_rs::MinerFactory::new()
                .with_connectivity_timeout(timeout)
                .with_identification_timeout(timeout),
        }
    }
}

impl DriverFactory for AsicRsFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            let m = self.factory.get_miner(ip).await?;
            Ok(m.map(|m| Box::new(AsicRsDriver::new(ip, m)) as Box<dyn MinerDriver>))
        })
    }
}

// ---------------- mock driver (tests / dry runs) ----------------

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockBehavior {
    Ok,
    Refuse,
    Error,
    Hang,
}

#[cfg(test)]
#[derive(Default)]
pub struct MockLog {
    pub calls: Mutex<Vec<(IpAddr, MinerAction, tokio::time::Instant)>>,
    pub in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
}

#[cfg(test)]
pub struct MockDriver {
    pub ip: IpAddr,
    pub caps: Capabilities,
    pub behavior: MockBehavior,
    pub delay: Duration,
    pub log: std::sync::Arc<MockLog>,
}

#[cfg(test)]
impl MinerDriver for MockDriver {
    fn ip(&self) -> IpAddr {
        self.ip
    }
    fn caps(&self) -> Capabilities {
        self.caps
    }
    fn execute(&self, action: MinerAction) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            let log = &self.log;
            log.calls.lock().unwrap().push((self.ip, action, tokio::time::Instant::now()));
            let now = log.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            log.max_in_flight.fetch_max(now, Ordering::SeqCst);
            if self.behavior == MockBehavior::Hang {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            } else {
                tokio::time::sleep(self.delay).await;
            }
            log.in_flight.fetch_sub(1, Ordering::SeqCst);
            match self.behavior {
                MockBehavior::Ok | MockBehavior::Hang => Ok(true),
                MockBehavior::Refuse => Ok(false),
                MockBehavior::Error => anyhow::bail!("mock transport error"),
            }
        })
    }
}
