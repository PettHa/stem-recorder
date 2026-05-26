const COMMANDS: &[&str] = &[
    "list_devices",
    "start_monitor",
    "stop_monitor",
    "start_record",
    "stop_record",
    "get_stats",
    "verify_wav",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
