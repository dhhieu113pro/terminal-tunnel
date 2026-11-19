//! terminal-tunnel: share a shell with a phone browser over an end-to-end encrypted WebSocket.
//!
//! Two programs use this library:
//!   `terminal-tunnel`       (console)  host-web | signal  -> [`cli_main`]
//!   `terminal-tunnel-tray`  (Windows GUI, no console)     -> [`tray_main`]: double-click to run in the tray

mod pty;
mod settings;
#[cfg(windows)]
mod tray;
mod web;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Remote terminal for a phone browser (end-to-end encrypted WebSocket)")]
struct Cli {
    /// Directory the shared shell starts in (e.g. D:\Development). For PowerShell, `cd` out of it
    /// is blocked (soft guard, not a sandbox). Also settable as "root" in the settings file or TT_ROOT.
    #[arg(long, env = "TT_ROOT", global = true)]
    root: Option<std::path::PathBuf>,
    /// Settings file (JSON) to use instead of the one next to the executable
    /// (`terminal-tunnel.json`) or in %APPDATA%\terminal-tunnel. Options given on the command line
    /// or in the environment override its values. Example:
    /// {"root":"D:/Development","shell":"","port":8080,"tunnelId":"terminal-tunnel","anonymous":false}
    #[arg(long, env = "TT_CONFIG", global = true)]
    config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Share a shell to a phone browser: prints a QR code.
    HostWeb {
        /// Program to run (default: PowerShell 7 / system shell).
        #[arg(long, env = "TT_SHELL")]
        shell: Option<String>,
        /// Base URL of a relay server you run elsewhere (`terminal-tunnel signal`), which must be
        /// https, e.g. https://tt.example.com. Alternative to --devtunnel.
        #[arg(long)]
        signal: Option<String>,
        /// Port for the embedded relay server.
        #[arg(long, env = "TT_PORT", default_value_t = 8080)]
        port: u16,
        /// Expose the embedded relay server through a Microsoft Dev Tunnel (default: on; needs the
        /// `devtunnel` CLI, logged in). This is how the QR link works over the internet. An explicit
        /// --signal takes precedence; turn off with `--devtunnel=false` or TT_DEVTUNNEL=0.
        #[arg(long, env = "TT_DEVTUNNEL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        devtunnel: bool,
        /// With --devtunnel: allow anonymous access to the tunnel. By default the tunnel requires
        /// signing in as the owner (the account used by `devtunnel user login`).
        #[arg(long, env = "TT_DEVTUNNEL_ANONYMOUS", value_parser = clap::builder::BoolishValueParser::new())]
        devtunnel_anonymous: bool,
        /// Name of the persistent Dev Tunnel used by --devtunnel (created on first use, then reused).
        #[arg(long, env = "TT_DEVTUNNEL_ID", default_value = "terminal-tunnel")]
        devtunnel_id: String,
        /// Windows: live in the notification area. Keeps the tunnel up and creates a fresh
        /// single-use link (QR window / "Copy link" in the tray menu) after every session.
        #[arg(long, env = "TT_TRAY", value_parser = clap::builder::BoolishValueParser::new())]
        tray: bool,
        /// With --tray: detach from the console so no terminal window stays open.
        #[arg(long, env = "TT_HIDE_CONSOLE", value_parser = clap::builder::BoolishValueParser::new())]
        hide_console: bool,
    },
    /// Run only the relay + web page server (deploy on a public host).
    Signal {
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Address to listen on. Loopback by default (put an https reverse proxy on the same machine
        /// in front); use 0.0.0.0 only to expose the relay directly (Windows Firewall will then ask).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
}

/// `--config <file>` / `--config=<file>` / `TT_CONFIG`. It has to be read before clap parses the
/// rest, because the file's values are exported as `TT_*` variables that clap then picks up.
fn explicit_config_path() -> Option<std::path::PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(Into::into);
        }
        if let Some(v) = a.to_str().and_then(|s| s.strip_prefix("--config=")) {
            return Some(v.into());
        }
    }
    std::env::var_os("TT_CONFIG").map(Into::into)
}

/// Load the settings file (the explicit one, which must exist, or the one found next to the
/// executable / in %APPDATA%) and export its values. Returns the file used, if any.
fn load_settings(explicit: Option<&std::path::Path>) -> Result<Option<std::path::PathBuf>> {
    match explicit {
        Some(p) => {
            if !p.is_file() {
                bail!("settings file {} not found", p.display());
            }
            settings::apply_env(&settings::load(p)?);
            Ok(Some(p.to_path_buf()))
        }
        None => settings::load_and_apply(),
    }
}

/// Entry point of the console program (`terminal-tunnel`).
pub async fn cli_main() -> Result<()> {
    let settings_file = load_settings(explicit_config_path().as_deref())?;
    run(Cli::parse(), settings_file).await
}

/// First launch of the tray app (no settings file, no `TT_ROOT`): ask for the settings in a window
/// and write the file. `None` if the user cancelled.
#[cfg(windows)]
fn first_run_setup() -> Result<Option<std::path::PathBuf>> {
    let Some(s) = settings::edit(None, &settings::Settings::default()) else { return Ok(None) };
    let path = settings::new_file_location()?;
    settings::save(&path, &s)?;
    settings::apply_env(&s);
    Ok(Some(path))
}

/// Entry point of the Windows GUI program (`terminal-tunnel-tray`): no console window, always tray
/// mode. Problems (bad config, already running, no tunnel) are shown in a message box because there
/// is no console to print them. Returns the process exit code.
#[cfg(windows)]
pub fn tray_main() -> i32 {
    let result = (|| -> Result<()> {
        let _single = single_instance()?;
        let mut settings_file = load_settings(explicit_config_path().as_deref())?;
        if settings_file.is_none() && std::env::var_os("TT_ROOT").is_none() {
            // First launch: nothing configured yet, so ask (and create the settings file).
            match first_run_setup()? {
                Some(p) => settings_file = Some(p),
                None => return Ok(()), // cancelled: nothing to run
            }
        }
        // Same options as `host-web --tray`, so a shortcut can pass e.g. `--root D:/Development`.
        let args = std::env::args_os()
            .next()
            .into_iter()
            .chain(["host-web".into(), "--tray".into()])
            .chain(std::env::args_os().skip(1));
        let cli = Cli::try_parse_from(args).map_err(|e| anyhow!(e.to_string()))?;
        tokio::runtime::Runtime::new()?.block_on(run(cli, settings_file))
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            tray::error_box("terminal-tunnel", &format!("{e:#}"));
            1
        }
    }
}

/// Only one tray app per user: a second launch tells the user where the first one is.
#[cfg(windows)]
fn single_instance() -> Result<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;
    let handle = unsafe { CreateMutexW(None, true, w!("Local\\terminal-tunnel-tray")) }
        .map_err(|e| anyhow!("cannot create the single-instance lock: {e}"))?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if tray::show_existing() {
            std::process::exit(0); // the running copy shows its window; nothing to report
        }
        bail!("terminal-tunnel is already running. Look for its icon in the notification area (click the ^ arrow if it is hidden).");
    }
    Ok(handle)
}

async fn run(cli: Cli, settings_file: Option<std::path::PathBuf>) -> Result<()> {
    if matches!(cli.cmd, Cmd::HostWeb { .. }) {
        match &settings_file {
            Some(f) => eprintln!("config: settings file {}", f.display()),
            None => eprintln!("config: no settings file ({} next to the exe or in %APPDATA%\\terminal-tunnel)", settings::FILE_NAME),
        }
        match &cli.root {
            Some(r) => eprintln!("config: root = {}", r.display()),
            None => eprintln!("config: root = (none: shell starts in the current directory, no guard)"),
        }
    }
    let root = match cli.root {
        Some(p) => {
            if !p.is_dir() {
                bail!("root {} does not exist or is not a directory", p.display());
            }
            Some(p)
        }
        None => None,
    };
    match cli.cmd {
        Cmd::HostWeb { shell, signal, port, devtunnel, devtunnel_anonymous, devtunnel_id, tray, hide_console } => {
            let shell = shell.unwrap_or_else(default_shell);
            // The tray is for real use from a phone: it always goes through the Dev Tunnel unless a
            // relay URL is given explicitly.
            let devtunnel = signal.is_none() && (devtunnel || tray);
            if tray {
                #[cfg(windows)]
                return web::host_tray(shell, root, signal, port, devtunnel, devtunnel_anonymous, devtunnel_id, hide_console).await;
                #[cfg(not(windows))]
                {
                    let _ = hide_console;
                    bail!("--tray is only available on Windows");
                }
            }
            web::host_web(shell, root, signal, port, devtunnel, devtunnel_anonymous, devtunnel_id).await
        }
        Cmd::Signal { port, bind } => web::run_signal(port, &bind).await,
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        // Prefer PowerShell 7 when installed, else Windows PowerShell 5.1.
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        if std::path::Path::new(pwsh).exists() { pwsh.into() } else { "powershell.exe".into() }
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}
