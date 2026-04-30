use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::types::{
    DiscoveryErrorCode, DiscoveryErrorEvent, DiscoveryProgress, DiscoveryRowEvent, RowStatus,
};

#[derive(Clone)]
pub struct DiscoveryUi {
    app: AppHandle,
    events: Arc<EventNames>,
}

struct EventNames {
    row: String,
    progress: String,
    done: String,
    error: String,
}

impl EventNames {
    fn sanitize_run_id(raw: &str) -> String {
        const MAX_LEN: usize = 64;
        let mut out = String::with_capacity(raw.len().min(MAX_LEN));
        let mut prev_underscore = false;
        for ch in raw.chars() {
            if out.len() >= MAX_LEN {
                break;
            }
            let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
            if ok {
                out.push(ch);
                prev_underscore = false;
            } else if !prev_underscore {
                out.push('_');
                prev_underscore = true;
            }
        }
        if out.is_empty() {
            "run".to_string()
        } else {
            out
        }
    }

    fn new(run_id: &str) -> Self {
        let id = Self::sanitize_run_id(run_id);
        Self {
            row: format!("tm:discovery:row:{id}"),
            progress: format!("tm:discovery:progress:{id}"),
            done: format!("tm:discovery:done:{id}"),
            error: format!("tm:discovery:error:{id}"),
        }
    }
}

impl DiscoveryUi {
    pub fn new(app: AppHandle, run_id: &str) -> Self {
        Self {
            app,
            events: Arc::new(EventNames::new(run_id)),
        }
    }

    fn emit<P: Serialize + Clone>(&self, name: &str, payload: P) {
        if let Err(e) = self.app.emit(name, payload) {
            log::warn!("emit {name} failed: {e:?}");
        }
    }

    pub fn emit_row(&self, event: DiscoveryRowEvent) {
        self.emit(&self.events.row, event);
    }

    pub fn emit_row_ip_status(&self, id: String, status: RowStatus, ip: std::net::IpAddr) {
        self.emit_row(DiscoveryRowEvent {
            id,
            status,
            row: Some(super::helpers::ip_row(ip)),
        });
    }

    pub fn emit_progress(&self, progress: &DiscoveryProgress) {
        self.emit(&self.events.progress, progress);
    }

    pub fn emit_done(&self) {
        self.emit(&self.events.done, ());
    }

    pub fn emit_error(&self, code: DiscoveryErrorCode, message: impl Into<String>) {
        let message = super::sanitize::sanitize_error_message(&message.into());
        self.emit(
            &self.events.error,
            DiscoveryErrorEvent { code, message },
        );
    }
}
