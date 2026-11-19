//! User settings: a small JSON file next to the executable (created on first run), and on Windows a
//! native settings window to edit it.
//!
//! Precedence for every option: command line > environment > this file > built-in default.
//! That is achieved by exporting the file's values as `TT_*` environment variables for the options
//! that are not already set, so clap's `env` support does the rest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub const FILE_NAME: &str = "terminal-tunnel.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Folder the shared shell starts in ("" = no folder, no guard).
    pub root: String,
    /// Shell program ("" = PowerShell 7 / system default).
    pub shell: String,
    /// Port of the embedded relay server.
    pub port: u16,
    /// Name of the persistent Dev Tunnel.
    pub tunnel_id: String,
    /// Allow anonymous access to the tunnel (default: sign-in required).
    pub anonymous: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { root: String::new(), shell: String::new(), port: 8080, tunnel_id: "terminal-tunnel".into(), anonymous: false }
    }
}

fn appdata_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| Path::new(&a).join("terminal-tunnel"))
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// The settings file that exists, if any: next to the executable first, then in `%APPDATA%`.
pub fn find() -> Option<PathBuf> {
    [exe_dir(), appdata_dir()].into_iter().flatten().map(|d| d.join(FILE_NAME)).find(|p| p.is_file())
}

/// Where a new settings file should be created: next to the executable if that folder is writable
/// (portable use), otherwise in `%APPDATA%\terminal-tunnel\`.
pub fn new_file_location() -> Result<PathBuf> {
    if let Some(dir) = exe_dir() {
        let probe = dir.join(".tt-write-test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return Ok(dir.join(FILE_NAME));
        }
    }
    let dir = appdata_dir().context("no writable folder for the settings file")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir.join(FILE_NAME))
}

pub fn load(path: &Path) -> Result<Settings> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("{} is not valid JSON: {e}", path.display()))?;
    let d = Settings::default();
    let s = |k: &str, def: &str| v[k].as_str().unwrap_or(def).trim().to_string();
    let port = match &v["port"] {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        serde_json::Value::String(t) => t.trim().parse().ok(),
        _ => Some(d.port),
    }
    .ok_or_else(|| anyhow!("{}: \"port\" must be a number from 1 to 65535", path.display()))?;
    Ok(Settings {
        root: s("root", &d.root),
        shell: s("shell", &d.shell),
        port,
        tunnel_id: s("tunnelId", &d.tunnel_id),
        anonymous: v["anonymous"].as_bool().unwrap_or(d.anonymous),
    })
}

pub fn save(path: &Path, s: &Settings) -> Result<()> {
    let v = serde_json::json!({
        "root": s.root,
        "shell": s.shell,
        "port": s.port,
        "tunnelId": s.tunnel_id,
        "anonymous": s.anonymous,
    });
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Check values the way the settings window does before saving.
pub fn validate(s: &Settings) -> Result<()> {
    if !s.root.is_empty() && !Path::new(&s.root).is_dir() {
        bail!("The folder \"{}\" does not exist.", s.root);
    }
    if s.port == 0 {
        bail!("The port must be a number from 1 to 65535.");
    }
    if s.tunnel_id.is_empty() || !s.tunnel_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("The tunnel name may only contain letters, digits and '-'.");
    }
    Ok(())
}

/// Export the settings as `TT_*` variables, but only for options not already set (so the real
/// environment and the command line keep priority).
pub fn apply_env(s: &Settings) {
    let set = |k: &str, v: String| {
        if std::env::var_os(k).is_none() {
            // Called once at startup, before any other thread exists.
            unsafe { std::env::set_var(k, v) };
        }
    };
    if !s.root.is_empty() {
        set("TT_ROOT", s.root.clone());
    }
    if !s.shell.is_empty() {
        set("TT_SHELL", s.shell.clone());
    }
    set("TT_PORT", s.port.to_string());
    set("TT_DEVTUNNEL_ID", s.tunnel_id.clone());
    set("TT_DEVTUNNEL_ANONYMOUS", if s.anonymous { "1" } else { "0" }.into());
}

/// Load the settings file if there is one and export it. Returns its path.
pub fn load_and_apply() -> Result<Option<PathBuf>> {
    let Some(path) = find() else { return Ok(None) };
    let s = load(&path)?;
    apply_env(&s);
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("tt-settings-test");
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn save_then_load_roundtrips() {
        let p = temp("roundtrip.json");
        let s = Settings { root: "D:/Development".into(), shell: String::new(), port: 9090, tunnel_id: "my-tunnel".into(), anonymous: true };
        save(&p, &s).unwrap();
        assert_eq!(load(&p).unwrap(), s);
    }

    #[test]
    fn missing_fields_use_defaults_and_bad_values_error() {
        let p = temp("partial.json");
        std::fs::write(&p, r#"{ "root": "C:/x" }"#).unwrap();
        let s = load(&p).unwrap();
        assert_eq!((s.port, s.tunnel_id.as_str(), s.anonymous), (8080, "terminal-tunnel", false));

        std::fs::write(&p, r#"{ "port": 70000 }"#).unwrap();
        assert!(load(&p).is_err());
        std::fs::write(&p, "not json").unwrap();
        assert!(load(&p).is_err());
        std::fs::write(&p, r#"{ "port": "8081" }"#).unwrap();
        assert_eq!(load(&p).unwrap().port, 8081);
    }

    #[test]
    fn validation_catches_bad_input() {
        let ok = Settings::default();
        assert!(validate(&ok).is_ok());
        assert!(validate(&Settings { root: "Z:/no/such/dir/xyz".into(), ..ok.clone() }).is_err());
        assert!(validate(&Settings { port: 0, ..ok.clone() }).is_err());
        assert!(validate(&Settings { tunnel_id: "bad name!".into(), ..ok.clone() }).is_err());
        assert!(validate(&Settings { tunnel_id: String::new(), ..ok }).is_err());
    }
}

// ----------------------------------------------------------------------------------------------
// Windows settings window
// ----------------------------------------------------------------------------------------------

#[cfg(windows)]
pub use ui::edit;

#[cfg(windows)]
mod ui {
    use std::ffi::c_void;
    use std::sync::Mutex;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, DEFAULT_CHARSET, FW_NORMAL, GetStockObject, HBRUSH, HDC, SetBkMode, TRANSPARENT, WHITE_BRUSH,
        CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY, FF_DONTCARE, OUT_DEFAULT_PRECIS, DEFAULT_PITCH, HFONT,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::Shell::{
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON,
        BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL,
        GetDlgItem, GetMessageW, GetSystemMetrics, GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW,
        LoadCursorW, IDC_ARROW, IDCANCEL, IDOK, MB_ICONWARNING, MB_OK, MSG, MessageBoxW, PostQuitMessage,
        RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SendMessageW, SetForegroundWindow, SetWindowTextW,
        ShowWindow, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_SETFONT, WNDCLASSW,
        WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };
    use windows::core::{PCWSTR, w};

    use super::{Settings, validate};

    const ID_ROOT: i32 = 101;
    const ID_BROWSE: i32 = 102;
    const ID_SHELL: i32 = 103;
    const ID_TUNNEL: i32 = 104;
    const ID_PORT: i32 = 105;
    const ID_ANON: i32 = 106;
    const ID_SAVE: i32 = IDOK.0;
    const ID_CANCEL: i32 = IDCANCEL.0;

    const CLIENT_W: i32 = 560;
    const CLIENT_H: i32 = 330;

    struct State {
        initial: Settings,
        owner: Option<isize>,
        result: Option<Settings>,
        font: isize,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Show the settings window and wait for it. Returns the new settings if the user saved.
    /// `owner` is a window to disable while this one is open (the tray window), if any.
    pub fn edit(owner: Option<isize>, initial: &Settings) -> Option<Settings> {
        *STATE.lock().unwrap() = Some(State { initial: initial.clone(), owner, result: None, font: 0 });
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let hinst = GetModuleHandleW(None).expect("module handle");
            let class = w!("TerminalTunnelSettings");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
                ..Default::default()
            };
            RegisterClassW(&wc); // fails harmlessly if already registered

            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
            let mut rc = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
            let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_APPWINDOW);
            let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
            let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
            let y = (GetSystemMetrics(SM_CYSCREEN) - wh) / 3;
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_APPWINDOW,
                class,
                w!("terminal-tunnel settings"),
                style,
                x,
                y,
                ww,
                wh,
                None,
                None,
                Some(hinst.into()),
                None,
            ) else {
                return None;
            };
            if let Some(o) = owner {
                let _ = EnableWindow(HWND(o as *mut c_void), false);
            }
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_SHOW); // the first call may be overridden by the launcher
            let _ = SetForegroundWindow(hwnd);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if !IsDialogMessageW(hwnd, &msg).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        STATE.lock().unwrap().take().and_then(|s| s.result)
    }

    unsafe fn text(hwnd: HWND, id: i32) -> String {
        unsafe {
            let Ok(h) = GetDlgItem(Some(hwnd), id) else { return String::new() };
            let len = GetWindowTextLengthW(h) as usize;
            let mut buf = vec![0u16; len + 1];
            let n = GetWindowTextW(h, &mut buf) as usize;
            String::from_utf16_lossy(&buf[..n]).trim().to_string()
        }
    }

    unsafe fn add(hwnd: HWND, class: PCWSTR, label: &str, style: WINDOW_STYLE, id: i32, x: i32, y: i32, w: i32, h: i32, font: HFONT) -> HWND {
        unsafe {
            let hinst = GetModuleHandleW(None).expect("module handle");
            let t = wide(label);
            let c = CreateWindowExW(
                Default::default(),
                class,
                PCWSTR(t.as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(id as usize as *mut c_void)),
                Some(hinst.into()),
                None,
            )
            .expect("create control");
            SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            c
        }
    }

    unsafe fn build(hwnd: HWND) {
        unsafe {
            let font = CreateFontW(
                -16, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, w!("Segoe UI"),
            );
            let st = STATE.lock().unwrap();
            let init = st.as_ref().map(|s| s.initial.clone()).unwrap_or_default();
            drop(st);
            if let Some(s) = STATE.lock().unwrap().as_mut() {
                s.font = font.0 as isize;
            }

            let edit = WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
            let label = WINDOW_STYLE(0);
            let (l, fw) = (16, CLIENT_W - 32);

            add(hwnd, w!("STATIC"), "Shared folder: the shell starts here (leave empty for none)", label, 0, l, 14, fw, 22, font);
            let r = add(hwnd, w!("EDIT"), &init.root, edit, ID_ROOT, l, 38, fw - 96, 28, font);
            let _ = r;
            add(hwnd, w!("BUTTON"), "Browse...", WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32), ID_BROWSE, l + fw - 88, 37, 88, 30, font);

            add(hwnd, w!("STATIC"), "Shell program (leave empty for PowerShell 7)", label, 0, l, 80, fw, 22, font);
            add(hwnd, w!("EDIT"), &init.shell, edit, ID_SHELL, l, 104, fw, 28, font);

            add(hwnd, w!("STATIC"), "Dev Tunnel name", label, 0, l, 146, 240, 22, font);
            add(hwnd, w!("EDIT"), &init.tunnel_id, edit, ID_TUNNEL, l, 170, 240, 28, font);
            add(hwnd, w!("STATIC"), "Relay port", label, 0, l + 264, 146, 120, 22, font);
            add(hwnd, w!("EDIT"), &init.port.to_string(), edit, ID_PORT, l + 264, 170, 120, 28, font);

            let cb = add(hwnd, w!("BUTTON"), "Allow anonymous tunnel access (skips the Microsoft sign-in)", WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32), ID_ANON, l, 214, fw, 26, font);
            SendMessageW(cb, BM_SETCHECK, Some(WPARAM(init.anonymous as usize)), Some(LPARAM(0)));

            add(hwnd, w!("STATIC"), "Saved in terminal-tunnel.json; changes apply on restart.", label, 0, l, 252, fw, 22, font);
            add(hwnd, w!("BUTTON"), "Save", WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32), ID_SAVE, CLIENT_W - 16 - 184, CLIENT_H - 52, 88, 34, font);
            add(hwnd, w!("BUTTON"), "Cancel", WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32), ID_CANCEL, CLIENT_W - 16 - 88, CLIENT_H - 52, 88, 34, font);
        }
    }

    unsafe fn browse(hwnd: HWND) {
        unsafe {
            let mut name = [0u16; 260];
            let title = wide("Choose the shared folder");
            let bi = BROWSEINFOW {
                hwndOwner: hwnd,
                lpszTitle: PCWSTR(title.as_ptr()),
                ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
                pszDisplayName: windows::core::PWSTR(name.as_mut_ptr()),
                ..Default::default()
            };
            let pidl = SHBrowseForFolderW(&bi);
            if pidl.is_null() {
                return;
            }
            let mut path = [0u16; 260];
            if SHGetPathFromIDListW(pidl, &mut path).as_bool() {
                let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                let p = String::from_utf16_lossy(&path[..n]).replace('\\', "/");
                let w = wide(&p);
                if let Ok(h) = GetDlgItem(Some(hwnd), ID_ROOT) {
                    let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
                }
            }
            CoTaskMemFree(Some(pidl as *const c_void));
        }
    }

    unsafe fn save(hwnd: HWND) {
        unsafe {
            let port_text = text(hwnd, ID_PORT);
            let anonymous = GetDlgItem(Some(hwnd), ID_ANON)
                .map(|h| SendMessageW(h, BM_GETCHECK, None, None).0 == 1)
                .unwrap_or(false);
            let s = Settings {
                root: text(hwnd, ID_ROOT).replace('\\', "/"),
                shell: text(hwnd, ID_SHELL),
                port: port_text.parse().unwrap_or(0),
                tunnel_id: text(hwnd, ID_TUNNEL),
                anonymous,
            };
            if let Err(e) = validate(&s) {
                let m = wide(&e.to_string());
                MessageBoxW(Some(hwnd), PCWSTR(m.as_ptr()), w!("terminal-tunnel settings"), MB_OK | MB_ICONWARNING);
                return;
            }
            if let Some(st) = STATE.lock().unwrap().as_mut() {
                st.result = Some(s);
            }
            let _ = DestroyWindow(hwnd);
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_CREATE => {
                    build(hwnd);
                    LRESULT(0)
                }
                WM_COMMAND => {
                    let (id, code) = ((wparam.0 & 0xFFFF) as i32, ((wparam.0 >> 16) & 0xFFFF) as u32);
                    if code == BN_CLICKED {
                        match id {
                            ID_BROWSE => browse(hwnd),
                            ID_SAVE => save(hwnd),
                            ID_CANCEL => {
                                let _ = DestroyWindow(hwnd);
                            }
                            _ => {}
                        }
                    }
                    LRESULT(0)
                }
                WM_CTLCOLORSTATIC => {
                    // Labels and the checkbox: draw on the window's white background, not grey boxes.
                    SetBkMode(HDC(wparam.0 as *mut c_void), TRANSPARENT);
                    LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    let (owner, font) = {
                        let st = STATE.lock().unwrap();
                        st.as_ref().map(|s| (s.owner, s.font)).unwrap_or((None, 0))
                    };
                    if let Some(o) = owner {
                        let _ = EnableWindow(HWND(o as *mut c_void), true);
                        let _ = SetForegroundWindow(HWND(o as *mut c_void));
                    }
                    let _ = font; // the font handle is released with the process
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}
