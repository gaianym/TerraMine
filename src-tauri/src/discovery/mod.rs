use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use tauri::{AppHandle, State};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod helpers;
mod job;
mod limits;
mod rate_limiter;
mod sanitize;
mod state;
mod targets;
mod types;
mod ui;

pub use state::AppState;
pub use types::DiscoverySettings;

use limits::{MAX_ACTIVE_RUNS, MAX_TARGETS_LEN_BYTES};

#[tauri::command]
pub async fn start_discovery_run(
    app: AppHandle,
    state: State<'_, AppState>,
    run_id: Option<String>,
    targets: String,
    settings: DiscoverySettings,
) -> Result<String, String> {
    let targets = targets.trim().to_string();
    if targets.is_empty() {
        return Err("targets must not be empty".to_string());
    }
    if targets.as_bytes().len() > MAX_TARGETS_LEN_BYTES {
        return Err(format!(
            "targets too large (max {MAX_TARGETS_LEN_BYTES} bytes)"
        ));
    }

    settings.validate().map_err(|e| e.to_string())?;

    let run_id = run_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let cancel = CancellationToken::new();

    state
        .register_run(run_id.clone(), cancel.clone(), MAX_ACTIVE_RUNS)
        .await?;

    let app2 = app.clone();
    let state2 = (*state).clone();
    let run_id2 = run_id.clone();
    let targets2 = targets.clone();
    let settings2 = settings.clone();

    tauri::async_runtime::spawn(async move {
        let res = AssertUnwindSafe(job::run_discovery_job(
            app2,
            run_id2.clone(),
            targets2,
            settings2,
            cancel,
        ))
        .catch_unwind()
        .await;

        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("discovery run {run_id2} failed: {e:?}");
            }
            Err(panic) => {
                let detail = if let Some(s) = panic.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                log::error!("discovery run {run_id2} panicked: {detail}");
            }
        }

        state2.remove_run(&run_id2).await;
    });

    Ok(run_id)
}

#[tauri::command]
pub async fn stop_discovery_run(state: State<'_, AppState>, run_id: String) -> Result<(), String> {
    match state.cancel_token(&run_id).await {
        Some(t) => {
            t.cancel();
            Ok(())
        }
        None => Err("Unknown run id".to_string()),
    }
}

#[tauri::command]
pub async fn stop_all_discovery_runs(state: State<'_, AppState>) -> Result<usize, String> {
    let tokens = state.take_all_cancel_tokens().await;
    let n = tokens.len();
    for t in tokens {
        t.cancel();
    }
    Ok(n)
}
