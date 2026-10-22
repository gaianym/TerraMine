pub mod control;
mod discovery;
mod logger;
#[cfg(test)]
mod test_hygiene;

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
            control::run_settings_job,
            control::settings_support,
            control::admin_support,
            control::run_admin_action,
            control::cancel_action_run,
            control::list_miner_credentials,
            control::set_miner_credentials,
            control::delete_miner_credentials,
            control::hardening_commands::capability_matrix,
            control::hardening_commands::audit_query,
            control::hardening_commands::audit_export,
            control::ssh_commands::ssh_support,
            control::ssh_commands::ssh_fetch_host_key,
            control::ssh_commands::ssh_pin_host_key,
            control::ssh_commands::ssh_unpin_host_key,
            control::ssh_commands::set_ssh_login,
            control::ssh_commands::delete_ssh_login,
            control::ssh_commands::run_ssh_action,
            control::ssh_commands::run_pool_divert,
        ])
        .run(tauri::generate_context!())
        .expect("error while running terra-mine");
}
