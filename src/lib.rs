use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

mod commands;

#[cfg(target_os = "windows")]
mod loopback;

pub use commands::*;

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("stem-recorder")
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::start_monitor,
            commands::stop_monitor,
            commands::start_record,
            commands::stop_record,
            commands::get_stats,
            commands::verify_wav,
        ])
        .build()
}
