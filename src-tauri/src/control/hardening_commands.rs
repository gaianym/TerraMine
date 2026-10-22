//! Tauri commands for the capability matrix and the audit log viewer.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::audit_view::{self, AuditFilter, AuditPage, ExportFormat};
use super::cache::{DeviceCache, Resolved, Resolver};
use super::driver::{DriverFactory, OverlayFactory};
use super::matrix::{self, MatrixRow};
use super::{creds, parse_ips, safety, AUDIT_FILE};

/// The matrix for `ips`, or for every discovered miner when absent.
#[tauri::command]
pub async fn capability_matrix(ips: Option<Vec<String>>) -> Result<Vec<MatrixRow>, String> {
    let cache = DeviceCache::global();
    let ips: Vec<IpAddr> = match ips {
        Some(v) if !v.is_empty() => parse_ips(&v)?,
        _ => {
            let mut v = cache.others(&[], usize::MAX);
            v.sort();
            v
        }
    };
    let f: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::with_store(
        Duration::from_secs(5),
        &creds::KeyringStore,
    ));
    let resolver = Arc::new(Resolver::new(f, cache.clone()));
    // App options: unconfirmed backends show as dry-run only.
    let engine = Arc::new(safety::SafetyEngine::new(
        Arc::new(safety::NullAudit),
        Arc::new(safety::snapshot::SnapshotStore::in_memory(1)),
        safety::SafetyOptions::default(),
    ));
    let rows = stream::iter(ips)
        .map(|ip| {
            let (resolver, engine, cache) = (resolver.clone(), engine.clone(), cache.clone());
            async move { matrix_for(&resolver, &engine, &cache, ip).await }
        })
        .buffered(8)
        .collect::<Vec<_>>()
        .await;
    Ok(rows)
}

pub async fn matrix_for(
    resolver: &Resolver,
    engine: &safety::SafetyEngine,
    cache: &DeviceCache,
    ip: IpAddr,
) -> MatrixRow {
    match resolver.resolve(ip, Duration::from_secs(5)).await {
        Resolved::Ready { driver, .. } => {
            let miner = cache.get(ip).and_then(|c| c.miner);
            matrix::matrix_row(engine, ip, driver, miner)
        }
        Resolved::Changed(m) | Resolved::Failed(m) => matrix::not_found(ip, Some(m)),
        Resolved::NotFound(m) => matrix::not_found(ip, m),
    }
}

fn audit_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join(AUDIT_FILE))
}

#[tauri::command]
pub async fn audit_query(app: AppHandle, filter: AuditFilter) -> Result<AuditPage, String> {
    let path = audit_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let (records, bad) =
            audit_view::load(&path).map_err(|e| format!("cannot read the audit log: {e}"))?;
        Ok(audit_view::page(records, bad, &filter))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub path: String,
    pub count: usize,
}

/// Writes the filtered, redacted log to the Downloads folder (falls back
/// to `<app data>/audit/exports`).
#[tauri::command]
pub async fn audit_export(
    app: AppHandle,
    filter: AuditFilter,
    format: ExportFormat,
) -> Result<ExportResult, String> {
    let path = audit_path(&app)?;
    let dir = match app.path().download_dir() {
        Ok(d) => d,
        Err(_) => app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("audit/exports"),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let (records, _) =
            audit_view::load(&path).map_err(|e| format!("cannot read the audit log: {e}"))?;
        let (p, count) =
            audit_view::export(records, &filter, format, &dir, safety::audit::now_ms())
                .map_err(|e| format!("export failed: {e}"))?;
        Ok(ExportResult {
            path: p.display().to_string(),
            count,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
