//! Run local mock miners for manual UI testing:
//!   cargo run --features mock-miner --bin mock-miner
use std::time::Duration;

use terra_mine_lib::control::mock_miner::{Fault, MockKind, MockMiner};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let down = Duration::from_secs(5);
    let miners = vec![
        ("AxeOS 2.10", MockMiner::spawn(MockKind::AxeOs210, down).await?),
        ("AxeOS 2.15", MockMiner::spawn(MockKind::AxeOs215, down).await?),
        ("S9 stock", MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, down).await?),
        ("S9 stock + miner-mode", MockMiner::spawn(MockKind::S9Stock { miner_mode: true }, down).await?),
        (
            "AxeOS 2.15 unhealthy",
            MockMiner::spawn(MockKind::AxeOs215, down).await?,
        ),
    ];
    // Hashrate drops to 0 once frequency/voltage leave 525 MHz / 1150 mV.
    miners[4].1.set_fault(Some(Fault::HashrateDrop));
    for (name, m) in &miners {
        println!("{name:24} http={} cgminer={:?}", m.base(), m.cgminer);
    }
    println!("S9 digest user root / password mock-pass. Ctrl-C to stop.");
    tokio::signal::ctrl_c().await
}
