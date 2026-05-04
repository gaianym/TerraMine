mod discovery;
mod logger;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logger::init();
    tauri::Builder::default()
        .manage(discovery::AppState::default())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            discovery::filter_ips_matching_targets,
            discovery::start_discovery_run,
            discovery::enrich_known_devices_run,
            discovery::stop_discovery_run,
            discovery::stop_all_discovery_runs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running terra-mine");
}
