use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct AppState {
    inner: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl AppState {
    pub async fn register_run(
        &self,
        run_id: String,
        cancel: CancellationToken,
        max_active: usize,
    ) -> Result<(), String> {
        let mut map = self.inner.lock().await;
        if map.len() >= max_active {
            return Err("Too many active discovery runs".to_string());
        }
        map.insert(run_id, cancel);
        Ok(())
    }

    pub async fn remove_run(&self, run_id: &str) {
        let mut map = self.inner.lock().await;
        map.remove(run_id);
    }

    pub async fn cancel_token(&self, run_id: &str) -> Option<CancellationToken> {
        let map = self.inner.lock().await;
        map.get(run_id).cloned()
    }

    pub async fn take_all_cancel_tokens(&self) -> Vec<CancellationToken> {
        let mut map = self.inner.lock().await;
        let tokens: Vec<_> = map.values().cloned().collect();
        map.clear();
        tokens
    }
}
