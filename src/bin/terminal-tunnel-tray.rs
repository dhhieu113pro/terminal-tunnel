//! Windows GUI program: no console window, always runs in the notification area.
//! Double-click it, pin it, or put a shortcut in `shell:startup`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    std::process::exit(terminal_tunnel::tray_main());
    #[cfg(not(windows))]
    eprintln!("terminal-tunnel-tray is only available on Windows; use `terminal-tunnel host-web`.");
}
