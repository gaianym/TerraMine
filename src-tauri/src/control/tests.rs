use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::driver::*;
use super::job::*;
use super::{job_config, parse_ips, ActionRequest};

struct MockFactory {
    miners: HashMap<IpAddr, (Capabilities, MockBehavior)>,
    delay: Duration,
    log: Arc<MockLog>,
}

impl DriverFactory for MockFactory {
    fn connect(&self, ip: IpAddr) -> BoxFuture<'_, anyhow::Result<Option<Box<dyn MinerDriver>>>> {
        Box::pin(async move {
            Ok(self.miners.get(&ip).map(|(caps, b)| {
                Box::new(MockDriver { ip, caps: caps.clone(), behavior: *b, delay: self.delay, log: self.log.clone(), status: None })
                    as Box<dyn MinerDriver>
            }))
        })
    }
}

fn all_caps() -> Capabilities {
    Capabilities::flags(true, true, true, Route::Mock)
}

fn ip(n: u8) -> IpAddr {
    IpAddr::from([192, 0, 2, n])
}

fn setup(miners: Vec<(u8, Capabilities, MockBehavior)>, delay_ms: u64) -> (Arc<MockFactory>, Arc<MockLog>) {
    let log = Arc::new(MockLog::default());
    let f = MockFactory {
        miners: miners.into_iter().map(|(n, c, b)| (ip(n), (c, b))).collect(),
        delay: Duration::from_millis(delay_ms),
        log: log.clone(),
    };
    (Arc::new(f), log)
}

fn cfg(action: MinerAction) -> JobConfig {
    JobConfig { action, concurrency: 4, stagger: Duration::ZERO, timeout: Duration::from_secs(1), dry_run: false, verify: None }
}

fn collecting_sink() -> (Sink, Arc<Mutex<Vec<ActionRowEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

fn statuses(r: &[ActionRowEvent]) -> Vec<ActionStatus> {
    r.iter().map(|e| e.status).collect()
}

#[tokio::test]
async fn start_stop_reboot_success_map_to_driver_calls() {
    for (action, expected) in [
        (MinerAction::Start, ActionStatus::Ok),
        (MinerAction::Stop, ActionStatus::Ok),
        (MinerAction::Reboot, ActionStatus::Rebooting),
    ] {
        let (f, log) = setup(vec![(1, all_caps(), MockBehavior::Ok)], 0);
        let (sink, _) = collecting_sink();
        let r = run_action(vec![ip(1)], cfg(action), f, CancellationToken::new(), sink).await;
        assert_eq!(statuses(&r), vec![expected]);
        let calls = log.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, action);
    }
}

#[tokio::test]
async fn unsupported_action_is_not_sent() {
    let caps = Capabilities::flags(false, false, true, Route::Mock);
    let (f, log) = setup(vec![(1, caps, MockBehavior::Ok)], 0);
    let (sink, _) = collecting_sink();
    let r = run_action(vec![ip(1)], cfg(MinerAction::Stop), f, CancellationToken::new(), sink).await;
    assert_eq!(statuses(&r), vec![ActionStatus::Unsupported]);
    assert!(log.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dry_run_sends_nothing() {
    let (f, log) = setup(vec![(1, all_caps(), MockBehavior::Ok)], 0);
    let (sink, _) = collecting_sink();
    let mut c = cfg(MinerAction::Reboot);
    c.dry_run = true;
    let r = run_action(vec![ip(1)], c, f, CancellationToken::new(), sink).await;
    assert_eq!(statuses(&r), vec![ActionStatus::DryRun]);
    assert!(log.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_refused_and_errors() {
    let (f, _) = setup(vec![(1, all_caps(), MockBehavior::Refuse), (2, all_caps(), MockBehavior::Error)], 0);
    let (sink, _) = collecting_sink();
    let r = run_action(vec![ip(1), ip(2), ip(3)], cfg(MinerAction::Stop), f, CancellationToken::new(), sink).await;
    assert_eq!(statuses(&r), vec![ActionStatus::Refused, ActionStatus::Failed, ActionStatus::NotFound]);
}

#[tokio::test(start_paused = true)]
async fn timeout_on_stop_fails_but_on_reboot_assumes_rebooting() {
    let (f, _) = setup(vec![(1, all_caps(), MockBehavior::Hang)], 0);
    let (sink, _) = collecting_sink();
    let r = run_action(vec![ip(1)], cfg(MinerAction::Stop), f.clone(), CancellationToken::new(), sink.clone()).await;
    assert_eq!(statuses(&r), vec![ActionStatus::Failed]);
    let r = run_action(vec![ip(1)], cfg(MinerAction::Reboot), f, CancellationToken::new(), sink).await;
    assert_eq!(statuses(&r), vec![ActionStatus::Rebooting]);
}

#[tokio::test]
async fn reboot_connection_drop_assumes_rebooting() {
    let (f, _) = setup(vec![(1, all_caps(), MockBehavior::Error)], 0);
    let (sink, _) = collecting_sink();
    let r = run_action(vec![ip(1)], cfg(MinerAction::Reboot), f, CancellationToken::new(), sink).await;
    assert_eq!(statuses(&r), vec![ActionStatus::Rebooting]);
}

#[tokio::test(start_paused = true)]
async fn concurrency_is_bounded() {
    let miners = (1..=10).map(|n| (n, all_caps(), MockBehavior::Ok)).collect();
    let (f, log) = setup(miners, 100);
    let (sink, _) = collecting_sink();
    let mut c = cfg(MinerAction::Start);
    c.concurrency = 3;
    let r = run_action((1..=10).map(ip).collect(), c, f, CancellationToken::new(), sink).await;
    assert!(r.iter().all(|e| e.status == ActionStatus::Ok));
    assert_eq!(log.max_in_flight.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn dispatches_are_staggered() {
    let miners = (1..=4).map(|n| (n, all_caps(), MockBehavior::Ok)).collect();
    let (f, log) = setup(miners, 0);
    let (sink, _) = collecting_sink();
    let mut c = cfg(MinerAction::Stop);
    c.stagger = Duration::from_millis(250);
    run_action((1..=4).map(ip).collect(), c, f, CancellationToken::new(), sink).await;
    let mut t: Vec<_> = log.calls.lock().unwrap().iter().map(|c| c.2).collect();
    t.sort();
    for w in t.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(250), "gap {:?}", w[1] - w[0]);
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_stops_pending_miners() {
    let miners = (1..=5).map(|n| (n, all_caps(), MockBehavior::Ok)).collect();
    let (f, log) = setup(miners, 0);
    let (sink, _) = collecting_sink();
    let mut c = cfg(MinerAction::Reboot);
    c.stagger = Duration::from_secs(10);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        c2.cancel();
    });
    let r = run_action((1..=5).map(ip).collect(), c, f, cancel, sink).await;
    assert_eq!(log.calls.lock().unwrap().len(), 2);
    assert_eq!(r.iter().filter(|e| e.status == ActionStatus::Cancelled).count(), 3);
}

#[tokio::test]
async fn events_queued_then_sent_then_terminal_in_input_order() {
    let (f, _) = setup(vec![(1, all_caps(), MockBehavior::Ok), (2, all_caps(), MockBehavior::Ok)], 0);
    let (sink, events) = collecting_sink();
    let r = run_action(vec![ip(2), ip(1)], cfg(MinerAction::Start), f, CancellationToken::new(), sink).await;
    assert_eq!(r[0].ip, "192.0.2.2");
    let ev = events.lock().unwrap();
    for target in ["192.0.2.1", "192.0.2.2"] {
        let s: Vec<_> = ev.iter().filter(|e| e.ip == target).map(|e| e.status).collect();
        assert_eq!(s, vec![ActionStatus::Queued, ActionStatus::Sent, ActionStatus::Ok]);
    }
}

#[test]
fn parse_ips_dedups_and_rejects_bad() {
    assert_eq!(parse_ips(&["192.0.2.1".into(), " 192.0.2.1 ".into()]).unwrap().len(), 1);
    assert!(parse_ips(&["nope".into()]).is_err());
    assert!(parse_ips(&[]).is_err());
}

#[test]
fn request_defaults_and_clamps() {
    let req: ActionRequest = serde_json::from_str(r#"{"ips":["192.0.2.1"],"action":"reboot","concurrency":0,"timeout_sec":0}"#).unwrap();
    let c = job_config(&req);
    assert_eq!(c.action, MinerAction::Reboot);
    assert_eq!(c.concurrency, 1);
    assert_eq!(c.timeout, Duration::from_secs(1));
    assert_eq!(c.stagger, Duration::from_millis(500));
    assert!(!c.dry_run);
}
