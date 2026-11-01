use std::sync::Arc;

use crate::attempts::Attempts;
use crate::config::Config;
use crate::scenarios::FlakyCounters;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
    /// Recent sends over SMTP and the APIs, for `/attempts`.
    pub attempts: Arc<Attempts>,
    /// Failures so far of `flaky-<n>` sends.
    pub flaky: Arc<FlakyCounters>,
}

impl AppState {
    /// Opens `<data_dir>/messages.db`, or an in-memory SQLite database when there is no data dir.
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let db = config.data_dir.as_ref().map(|d| d.join("messages.db"));
        let store = Store::open(db.as_deref(), config.max_messages)?;
        Ok(Self { config: Arc::new(config), store: Arc::new(store), attempts: Arc::default(), flaky: Arc::default() })
    }

    /// Largest request body the provider APIs accept: the largest message, base64-encoded in JSON.
    pub fn body_limit(&self) -> usize {
        self.config.max_message_size.saturating_mul(4) / 3 + 1024 * 1024
    }
}
