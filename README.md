# terminal-tunnel

Remote terminal in Rust for a phone browser: `host-web` shows a QR code, the phone opens it and gets
a shell over an **end-to-end encrypted WebSocket**. Because it only needs the page to load over
https, it works through firewalls, NAT, VPNs and mobile networks (e.g. via a Microsoft Dev Tunnel).

(Earlier versions also had a CLI-to-CLI mode over iroh and a WebRTC mode; both were removed.)

## Quick start

```
# one-time: winget install Microsoft.devtunnel ; devtunnel user login
cargo run -q -- host-web            # the Dev Tunnel is the default
```

Scan the QR code with the phone camera, sign in when asked, and you get a shell. The link works
until you press **Exit** on the phone (reloading the page or a dropped connection reconnect by
themselves). Restart `host-web` and rescan for a new session. Press Ctrl+C to stop.

## Tray app (Windows): no command line needed

Build once, then just **double-click `dist\terminal-tunnel-tray.exe`**:

```
cargo build --release
copy target\release\terminal-tunnel-tray.exe dist\      # (dist\ already holds the last build)
```

It is a Windows **GUI program**: no console window ever opens. Its settings come from the settings
window (tray menu: **Settings...**, saved as `terminal-tunnel.json` next to the exe or in
`%APPDATA%\terminal-tunnel`) or from options in a shortcut (`--root D:/Development`). Problems
(bad config, tunnel not logged in, already running) are shown in a message box. Only one copy can run:
a second launch tells you to look in the notification area (click the `^` arrow if the icon is hidden).

`terminal-tunnel.exe` is the console build of the same program (`terminal-tunnel host-web --tray` also
works from a terminal; add `--hide-console` to detach).

It runs in the notification area (built on the Microsoft [`windows`](https://github.com/microsoft/windows-rs)
crate) and keeps the Dev Tunnel and relay up, so nothing needs restarting:

- A window with the **QR code** and status opens at startup; closing it only hides it.
- Left-click the tray icon to show the QR window; right-click for the menu: **Show QR code**,
  **Copy link**, **New session**, **Quit**.
- After the phone presses Exit (or you pick New session) a **fresh link** is created automatically
  and a notification says so. Until then the same link stays valid, so the phone can reload or
  reconnect with it (a newer connection replaces the older one); each connection shows a match code
  in the status and in a notification.
- Tray mode always uses the Dev Tunnel unless you pass `--signal`. If it cannot start (not logged in,
  tunnel limit, ...) an error box says why.
- Release builds are stripped and use thin LTO (`[profile.release]` in `Cargo.toml`), about 4.7 MB each.

## App icon

A dark rounded square with a blue `>` prompt and a yellow cursor block. `assets/make-icon.ps1`
generates `assets/icon.ico` (16 to 256 px) and `assets/icon-180.png`; run `pwsh assets/make-icon.ps1`
after changing the design. The icon is embedded in `terminal-tunnel.exe` by `build.rs` (Explorer,
taskbar, tray icon, window title bar; needs the Windows SDK resource compiler, otherwise the build
warns and the app falls back to the stock icon) and served by the relay as the page's favicon and
iPhone home-screen icon.

## On the phone

- Extra keys in the Termux layout above the keyboard: `ESC / - HOME ↑ END Exit` and
  `TAB CTRL ALT ← ↓ → Enter`. CTRL and ALT are sticky (tap, then the next key); arrows repeat while
  held; tapping a key never closes the on-screen keyboard. **Exit** asks for confirmation, then
  disconnects and retires the link (the shells keep running on the PC).
- The page follows the keyboard: it is pinned to the visible area, so the extra keys sit right above
  the keyboard, the terminal refits and the host PTY is resized to match.
- With `TT_ROOT` set, the PowerShell prompt shows the root as `~`: `PS ~>`, `PS ~\sub>`.
- **Tabs:** the bar above the terminal has one tab per shell, a **+** button (up to 8) and **Close** for the
  active tab. Shells run on the PC, not in the page: if the phone disconnects they keep running, and
  scanning the next link brings every tab back with its screen. Typing `exit` closes that tab (a fresh
  shell opens when none are left); quitting the tray app stops all of them.

## Configuration

Options come from the command line, the settings window (tray menu: **Settings...**, stored in
`terminal-tunnel.json`) or `TT_*` environment variables. Precedence: command line > environment >
settings file > default.

| Variable | Flag | Meaning |
|----------|------|---------|
| `TT_ROOT` | `--root` | Directory the shared shell starts in, e.g. `D:\Development`. For PowerShell, `cd`/`Set-Location`/`Push-Location` that would leave it are undone (**soft guard**). Startup fails if it doesn't exist. |
| `TT_DEVTUNNEL` | `--devtunnel` | Expose the embedded relay through a Dev Tunnel (default on; `--devtunnel=false` or `TT_DEVTUNNEL=0` to turn off; see below) |
| `TT_TRAY` | `--tray` | Windows tray mode (see above) |
| `TT_HIDE_CONSOLE` | `--hide-console` | With `--tray`: detach from the console |
| `TT_DEVTUNNEL_ID` | `--devtunnel-id` | Name of the persistent Dev Tunnel (default `terminal-tunnel`) |
| `TT_DEVTUNNEL_ANONYMOUS` | `--devtunnel-anonymous` | Allow anonymous access to the tunnel (default: sign-in required) |

`host-web` prints the settings file used and the effective root at startup, so check those lines.
In this repo `.cargo/config.toml` sets `TT_DEVTUNNEL=1` for `cargo run`; override with
`TT_DEVTUNNEL=0 cargo run -- host-web --signal https://...`.

**`TT_ROOT` is not a sandbox.** It is a convenience guard only: full paths (`dir C:\`, `type C:\...`),
`cmd`, other programs, or `Microsoft.PowerShell.Management\Set-Location` all bypass it, and non-PowerShell
shells only get the start directory. For real isolation, run `host-web` as a limited Windows account
that only has NTFS rights on the root, or inside a container with only that folder mounted.

The default shell on Windows is PowerShell 7 (`C:\Program Files\PowerShell\7\pwsh.exe`) if installed,
else Windows PowerShell.

## Ways to get an https link

The page needs https because the browser's WebCrypto (used for the encryption) only exists on https
pages and on localhost. `host-web` refuses plain-http links with a clear error.

### 1. Microsoft Dev Tunnels (no server needed)

`terminal-tunnel host-web --devtunnel` runs the embedded relay on loopback, hosts one **persistent**
Dev Tunnel (`terminal-tunnel`, created on first use with a 30-day expiry and reused afterwards, so
runs never pile up tunnels) and prints a QR code for its
`https://<id>-8080.<region>.devtunnels.ms` URL.

- **Sign-in required by default.** The phone is redirected to a Microsoft login and must sign in as
  the account used for `devtunnel user login`; anyone else gets the login page, not ours. The
  access rules are re-applied on every start. The host itself talks to the relay over loopback, so
  it needs no login.
- If the login redirect drops the `#token` part of the link, the page says "Missing token": rescan,
  or use `--devtunnel-anonymous` (anyone with the URL can then load the page, but still cannot get a
  shell without the token).
- Dev Tunnels have an account limit on the number of tunnels. If creation fails with a limit
  error, remove old ones with `devtunnel list` / `devtunnel delete <id>`.

### 2. Your own server

```
# on a public host: relay + web page (put an https reverse proxy in front)
terminal-tunnel signal --port 8080

# on the machine to share:
terminal-tunnel host-web --signal https://tt.example.com
```

Caddy example:
```
tt.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## Security model

- The token (128 bits, random per run) lives in the URL fragment, which browsers never send to any
  server. The Dev Tunnel and the relay never see it.
- **End-to-end encryption.** Every frame is AES-256-GCM encrypted with keys derived (HKDF-SHA256)
  from the token and two fresh nonces, one key per direction. The tunnel (which terminates TLS) and the
  relay only see ciphertext. Tampering, replay and reordering are rejected (strict frame counters).
- **Mutual authentication.** The host proves it knows the token (HMAC over both nonces) and the page
  verifies it before enabling input. The host starts the shell only after the first encrypted frame
  from the phone decrypts, which proves the phone knows the token.
- **Match code.** Once connected, the host terminal prints `match code: 123 456` and the phone's status
  bar shows the same 6 digits. Compare them; if they differ, close the session. It is a visual check
  only and does not block anything.
- Anyone who scans the QR code gets a shell on the host. Don't show it on shared screens.
- The browser page is fully self-contained: xterm.js 5.3.0 / xterm-addon-fit 0.8.0 are bundled into
  the binary (`src/web/vendor/`, no CDN), and responses carry a strict CSP, `no-referrer`, `nosniff`,
  `X-Frame-Options: DENY` and `no-store`.
- The relay limits abuse: at most 64 rooms, host rooms expire after 10 min while waiting, paired
  sessions after 30 min idle (the page pings every 20 s). With `--devtunnel` it listens on loopback only.
- Someone who has the link but not the token can connect and fail the handshake; that only replaces the
  phone connection (it reconnects by itself) and gives no shell. Anyone who has the whole link (with the
  token after the `#`) can use it until you press Exit, so do not share it; each connection shows a match
  code and a notification on the PC.
- The page shows errors in a banner and keeps a timestamped **Log** (tap the Log button, then Copy
  log) for troubleshooting.

## Status

Tested: build, unit tests (HKDF against the RFC 5869 vector, frame round-trip, replay / tamper /
wrong-token rejection, PowerShell folder guard and `~` prompt on 5.1 and 7, QR matrix), a full browser
session over the encrypted WebSocket on localhost (handshake, shell I/O, match code), the extra keys
and the keyboard-sized viewport (including the host PTY resize), and the tray on localhost (QR window,
Copy link, link used up while connected, automatic new link, New session, Quit, hidden launch).

Not yet tested: a session through the real Dev Tunnel with sign-in on an iPhone (binary WebSocket
frames through Microsoft's relay, and whether the `#token` survives the login redirect).
