pub mod control;
mod discovery;
mod logger;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logger::init();
    tauri::Builder::default()
        .manage(discovery::AppState::default())
        .manage(control::ControlState::default())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            discovery::filter_ips_matching_targets,
            discovery::start_discovery_run,
            discovery::enrich_known_devices_run,
            discovery::stop_discovery_run,
            discovery::stop_all_discovery_runs,
            control::miner_capabilities,
            control::run_miner_action,
            control::cancel_action_run,
            control::list_miner_credentials,
            control::set_miner_credentials,
            control::delete_miner_credentials,
        ])
        .run(tauri::generate_context!())
        .expect("error while running terra-mine");
}
