use std::sync::OnceLock;

static INIT: OnceLock<()> = OnceLock::new();

pub fn init() {
    INIT.get_or_init(|| {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init();
    });
}
