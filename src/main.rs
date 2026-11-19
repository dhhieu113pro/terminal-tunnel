//! Console program: `terminal-tunnel host-web` / `terminal-tunnel signal`.
//! (The double-clickable tray program is `terminal-tunnel-tray`, see src/bin/.)

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    terminal_tunnel::cli_main().await
}
