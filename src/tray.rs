//! Windows notification-area (tray) icon, built on the Microsoft `windows` crate (windows-rs).
//!
//! One hidden-by-default top-level window does everything: it owns the tray icon, receives its
//! mouse messages, shows the QR code (painted with GDI) and answers the menu commands. Win32 requires
//! the window and its message loop to live on one thread, so that is a dedicated thread; the rest of
//! the program talks to it through [`TrayHandle`] (state + `PostMessageW`) and receives menu choices
//! as [`Command`]s on a tokio channel.

use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc;
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ScreenToClient,
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_PATH_ELLIPSIS,
    DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DRAW_TEXT_FORMAT, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    HDC, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_NULL, RoundRect, SRCCOPY, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    HICON, ICON_BIG, ICON_SMALL, IDC_ARROW, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadCursorW,
    LoadIconW, LoadImageW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SendMessageW, WM_SETICON,
    MB_ICONERROR, MB_OK, MF_DISABLED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONUP, WM_TIMER, SetTimer, KillTimer, WM_PAINT,
    WM_RBUTTONUP, WNDCLASSW, WS_EX_APPWINDOW, WS_POPUP, HTCAPTION, HTCLIENT, WM_NCHITTEST,
};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute};
use windows::core::{PCWSTR, w};

/// Messages from the tray menu to the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Drop the current link and create a fresh single-use one.
    NewSession,
    Quit,
}

/// A QR code as a square matrix of dark/light modules.
#[derive(Clone, Debug)]
pub struct Qr {
    pub width: usize,
    pub dark: Vec<bool>,
}

impl Qr {
    pub fn for_url(url: &str) -> Option<Self> {
        let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
        Some(Self {
            width: code.width(),
            dark: code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect(),
        })
    }
}

/// What the window and the tooltip display.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub status: String,
    pub link: Option<String>,
    pub qr: Option<Qr>,
}

struct Shared {
    view: Mutex<View>,
    balloon: Mutex<Option<(String, String)>>,
    cmd: mpsc::UnboundedSender<Command>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const WM_BALLOON: u32 = WM_APP + 3;
const WM_SHOW: u32 = WM_APP + 4;
const WM_QUIT_TRAY: u32 = WM_APP + 5;

const ID_SHOW: usize = 1;
const ID_COPY: usize = 2;
const ID_NEW: usize = 3;
const ID_SETTINGS: usize = 6;
const ID_QUIT: usize = 5;

const CLIENT_W: i32 = 380;
const CLIENT_H: i32 = 548;
/// "Copy link" button (client coordinates).
const BTN: RECT = RECT { left: 32, top: 490, right: CLIENT_W - 32, bottom: 528 };
/// Set for a moment after the button copied the link, to show "Copied".
static COPIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const TIMER_COPIED: usize = 1;
/// Left edge of the close button hit area (top-right corner).
const CLOSE_X: i32 = CLIENT_W - 52;

/// Handle for the rest of the program. Cheap to use from any thread.
pub struct TrayHandle {
    hwnd: isize,
    shared: Arc<Shared>,
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn fill<const N: usize>(dst: &mut [u16; N], s: &str) {
    let mut n = 0;
    for u in s.encode_utf16().take(N - 1) {
        dst[n] = u;
        n += 1;
    }
    dst[n] = 0;
}

impl TrayHandle {
    /// Change what is displayed and refresh the window and tooltip.
    pub fn update(&self, f: impl FnOnce(&mut View)) {
        f(&mut self.shared.view.lock().unwrap());
        self.post(WM_REFRESH);
    }

    /// Show a new link (`None` clears it) and its QR code.
    pub fn set_link(&self, link: Option<String>) {
        self.update(|v| {
            v.qr = link.as_deref().and_then(Qr::for_url);
            v.link = link;
        });
    }

    /// A Windows balloon/toast notification from the tray icon.
    pub fn notify(&self, title: &str, text: &str) {
        *self.shared.balloon.lock().unwrap() = Some((title.to_string(), text.to_string()));
        self.post(WM_BALLOON);
    }

    /// Bring the QR window to the front.
    pub fn show(&self) {
        self.post(WM_SHOW);
    }

    /// Remove the icon and end the window thread.
    pub fn quit(&self) {
        self.post(WM_QUIT_TRAY);
    }

    fn post(&self, msg: u32) {
        unsafe {
            let _ = PostMessageW(Some(hwnd_of(self.hwnd)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Start the tray icon and its window thread. Menu choices arrive on `cmd`.
pub fn spawn(cmd: mpsc::UnboundedSender<Command>) -> TrayHandle {
    let shared = Arc::new(Shared { view: Mutex::new(View::default()), balloon: Mutex::new(None), cmd });
    let _ = SHARED.set(shared.clone());
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    std::thread::spawn(move || unsafe { run(tx) });
    let hwnd = rx.recv().expect("tray thread failed to create its window");
    TrayHandle { hwnd, shared }
}

/// A modal error box, for failures when no console is visible.
pub fn error_box(title: &str, text: &str) {
    unsafe {
        let (t, x) = (wide(title), wide(text));
        MessageBoxW(None, PCWSTR(x.as_ptr()), PCWSTR(t.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

unsafe fn run(ready: std::sync::mpsc::Sender<isize>) {
    unsafe {
        let hinst = GetModuleHandleW(None).expect("module handle");
        let class = w!("TerminalTunnelTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Fixed-size window; the client area is what we lay out.
        let style = WS_POPUP;
        let (ww, wh) = (CLIENT_W, CLIENT_H);
        let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - wh) / 3;
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            w!("terminal-tunnel"),
            style,
            x,
            y,
            ww,
            wh,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("create tray window");
        // The first ShowWindow call of a process is overridden by the launcher's start-up setting
        // (a hidden/minimized launch would swallow our first real show). Spend it here, on a hide.
        let _ = ShowWindow(hwnd, SW_HIDE);

        let round: i32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const c_void, 4);
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const c_void, 4);
        // Title bar / Alt-Tab (small) and taskbar (big) icons, from the icon embedded by build.rs.
        let small = load_icon(hinst.into(), GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let big = load_icon(hinst.into(), GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));

        let mut nid = nid(hwnd);
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = small;
        fill(&mut nid.szTip, "terminal-tunnel: starting");
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);

        let _ = ready.send(hwnd.0 as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The app icon embedded as resource id 1 (see build.rs), at the requested size; falls back to the
/// stock application icon if the resource is missing (e.g. built without the resource compiler).
unsafe fn load_icon(hinst: HINSTANCE, cx: i32, cy: i32) -> HICON {
    unsafe {
        match LoadImageW(Some(hinst), PCWSTR(1usize as *const u16), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn shared() -> &'static Arc<Shared> {
    SHARED.get().expect("tray state is set before the window exists")
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let status = shared().view.lock().unwrap().status.clone();
        let has_link = shared().view.lock().unwrap().link.is_some();
        let status_w = wide(&if status.is_empty() { "terminal-tunnel".to_string() } else { status });
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED | MF_DISABLED, 0, PCWSTR(status_w.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("Show QR code"));
        let copy_flags = if has_link { MF_STRING } else { MF_STRING | MF_GRAYED | MF_DISABLED };
        let _ = AppendMenuW(menu, copy_flags, ID_COPY, w!("Copy link"));
        let _ = AppendMenuW(menu, MF_STRING, ID_NEW, w!("New session"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("Settings..."));
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

unsafe fn show_window(hwnd: HWND) {
    unsafe {
        // The first ShowWindow call of a process uses the launcher's start-up setting instead of the
        // requested one (so a process started hidden or minimized would swallow it): call it twice.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

unsafe fn copy_link(hwnd: HWND) {
    let Some(link) = shared().view.lock().unwrap().link.clone() else { return };
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        let data = wide(&link);
        let bytes = data.len() * 2;
        if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let dst = GlobalLock(hmem) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                let _ = GlobalUnlock(hmem);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

/// "Settings...": edit the settings file in the settings window. Changes apply on the next start.
unsafe fn open_settings(hwnd: HWND) {
    use crate::settings;
    let path = match settings::find() {
        Some(p) => Ok(p),
        None => settings::new_file_location(),
    };
    let current = path.as_ref().ok().and_then(|p| settings::load(p).ok()).unwrap_or_default();
    let Some(new) = settings::edit(Some(hwnd.0 as isize), &current) else { return };
    match path.and_then(|p| settings::save(&p, &new).map(|_| p)) {
        Ok(p) => {
            *shared().balloon.lock().unwrap() =
                Some(("Settings saved".into(), format!("Restart the app to apply them ({}).", p.display())));
            unsafe { balloon(hwnd) };
        }
        Err(e) => error_box("terminal-tunnel settings", &format!("{e:#}")),
    }
}

unsafe fn refresh(hwnd: HWND) {
    unsafe {
        let status = shared().view.lock().unwrap().status.clone();
        let mut n = nid(hwnd);
        n.uFlags = NIF_TIP;
        fill(&mut n.szTip, &format!("terminal-tunnel: {status}"));
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

unsafe fn balloon(hwnd: HWND) {
    let Some((title, text)) = shared().balloon.lock().unwrap().take() else { return };
    unsafe {
        let mut n = nid(hwnd);
        n.uFlags = NIF_INFO;
        n.dwInfoFlags = NIIF_INFO;
        fill(&mut n.szInfoTitle, &title);
        fill(&mut n.szInfo, &text);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
}

fn rgb(r: u8, g: u8, b: u8) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// A GDI font (Segoe UI Variable if installed, else Segoe UI via font fallback).
unsafe fn font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY, DEFAULT_PITCH.0 as u32, w!("Segoe UI Variable Text"),
        )
    }
}

unsafe fn text_out(hdc: HDC, s: &str, mut r: RECT, f: HFONT, color: windows::Win32::Foundation::COLORREF, flags: DRAW_TEXT_FORMAT) {
    unsafe {
        let old = SelectObject(hdc, f.into());
        SetTextColor(hdc, color);
        let mut t: Vec<u16> = s.encode_utf16().collect();
        DrawTextW(hdc, &mut t, &mut r, flags);
        SelectObject(hdc, old);
    }
}

unsafe fn round_fill(hdc: HDC, r: RECT, radius: i32, color: windows::Win32::Foundation::COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_NULL, 0, color);
        let (ob, op) = (SelectObject(hdc, brush.into()), SelectObject(hdc, pen.into()));
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(pen.into());
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let wdc = BeginPaint(hwnd, &mut ps);
        // Draw off-screen, then blit once: no flicker on refresh.
        let hdc = CreateCompatibleDC(Some(wdc));
        let bmp = CreateCompatibleBitmap(wdc, CLIENT_W, CLIENT_H);
        let old_bmp = SelectObject(hdc, bmp.into());
        SetBkMode(hdc, TRANSPARENT);

        let (bg, card, fg, dim) = (rgb(24, 26, 31), rgb(255, 255, 255), rgb(236, 238, 242), rgb(140, 146, 158));
        let full = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
        let bgb = CreateSolidBrush(bg);
        FillRect(hdc, &full, bgb);
        let _ = DeleteObject(bgb.into());

        let view = shared().view.lock().unwrap().clone();
        let (f_title, f_body, f_small) = (font(20, 600), font(14, 400), font(12, 400));

        // Header.
        text_out(hdc, "terminal-tunnel", RECT { left: 32, top: 22, right: CLIENT_W - 32, bottom: 52 }, f_title, fg, DT_LEFT | DT_SINGLELINE);

        text_out(hdc, "\u{2715}", RECT { left: CLOSE_X, top: 14, right: CLIENT_W - 14, bottom: 44 }, f_body, dim, DT_CENTER | DT_SINGLELINE);

        // Status pill: a coloured dot + the status text.
        let lower = view.status.to_lowercase();
        let dot = if lower.contains("connected") {
            rgb(52, 199, 89)
        } else if lower.contains("cannot") || lower.contains("error") || lower.contains(": ") && lower.starts_with("session ended:") {
            rgb(255, 69, 58)
        } else if view.link.is_some() {
            rgb(10, 132, 255)
        } else {
            rgb(255, 214, 10)
        };
        round_fill(hdc, RECT { left: 32, top: 64, right: 42, bottom: 74 }, 10, dot);
        text_out(hdc, &view.status, RECT { left: 52, top: 58, right: CLIENT_W - 32, bottom: 100 }, f_body, dim, DT_LEFT | DT_WORDBREAK);

        // QR card: white rounded panel, code scaled to whole pixels with a quiet zone.
        let card_r = RECT { left: 32, top: 108, right: CLIENT_W - 32, bottom: 108 + (CLIENT_W - 64) };
        round_fill(hdc, card_r, 24, card);
        if let Some(qr) = &view.qr {
            let quiet = 2usize;
            let cells = qr.width + 2 * quiet;
            let area = CLIENT_W - 64 - 24;
            let scale = (area / cells as i32).max(1);
            let side = scale * cells as i32;
            let (ox, oy) = (card_r.left + (CLIENT_W - 64 - side) / 2, card_r.top + (CLIENT_W - 64 - side) / 2);
            let ink = CreateSolidBrush(rgb(17, 19, 24));
            for (i, &d) in qr.dark.iter().enumerate() {
                if d {
                    let (cx, cy) = ((i % qr.width + quiet) as i32, (i / qr.width + quiet) as i32);
                    let r = RECT { left: ox + cx * scale, top: oy + cy * scale, right: ox + (cx + 1) * scale, bottom: oy + (cy + 1) * scale };
                    FillRect(hdc, &r, ink);
                }
            }
            let _ = DeleteObject(ink.into());
        } else {
            text_out(hdc, "No link yet", card_r, f_body, rgb(120, 126, 138), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }

        // Footer: hint and the (truncated) link.
        let hint = if view.qr.is_some() { "Scan with your phone camera. Single-use link." } else { "Please wait..." };
        text_out(hdc, hint, RECT { left: 32, top: 436, right: CLIENT_W - 32, bottom: 456 }, f_small, dim, DT_CENTER | DT_SINGLELINE);
        if let Some(link) = &view.link {
            text_out(hdc, link, RECT { left: 32, top: 458, right: CLIENT_W - 32, bottom: 478 }, f_small, rgb(90, 96, 108), DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_PATH_ELLIPSIS);
            let copied = COPIED.load(std::sync::atomic::Ordering::Relaxed);
            round_fill(hdc, BTN, 12, if copied { rgb(52, 199, 89) } else { rgb(10, 132, 255) });
            text_out(hdc, if copied { "Copied" } else { "Copy link" }, BTN, f_body, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }

        for f in [f_title, f_body, f_small] {
            let _ = DeleteObject(f.into());
        }
        let _ = BitBlt(wdc, 0, 0, CLIENT_W, CLIENT_H, Some(hdc), 0, 0, SRCCOPY);
        SelectObject(hdc, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(hdc);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                match (lparam.0 as u32) & 0xFFFF {
                    WM_LBUTTONUP => show_window(hwnd),
                    WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_SHOW => show_window(hwnd),
                    ID_COPY => copy_link(hwnd),
                    ID_NEW => {
                        let _ = shared().cmd.send(Command::NewSession);
                    }
                    ID_SETTINGS => open_settings(hwnd),
                    ID_QUIT => {
                        let _ = shared().cmd.send(Command::Quit);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_REFRESH => {
                refresh(hwnd);
                LRESULT(0)
            }
            WM_BALLOON => {
                balloon(hwnd);
                LRESULT(0)
            }
            WM_SHOW => {
                show_window(hwnd);
                LRESULT(0)
            }
            WM_QUIT_TRAY => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_NCHITTEST => {
                let (x, y) = (((lparam.0 as i32) << 16) >> 16, lparam.0 as i32 >> 16);
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                LRESULT(if p.y < 56 && p.x < CLOSE_X { HTCAPTION as isize } else { HTCLIENT as isize })
            }
            WM_LBUTTONUP => {
                let (x, y) = (lparam.0 as i32 & 0xFFFF, (lparam.0 as i32 >> 16) & 0xFFFF);
                if x >= BTN.left && x < BTN.right && y >= BTN.top && y < BTN.bottom && shared().view.lock().unwrap().link.is_some() {
                    copy_link(hwnd);
                    COPIED.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = SetTimer(Some(hwnd), TIMER_COPIED, 1500, None);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                if (lparam.0 as i32 & 0xFFFF) >= CLOSE_X && (lparam.0 as i32 >> 16) < 56 {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let _ = KillTimer(Some(hwnd), TIMER_COPIED);
                COPIED.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                // The close button only hides the window; Quit is in the tray menu.
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => {
                let n = nid(hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &n);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_matrix_is_square_and_nonempty() {
        let qr = Qr::for_url("https://example.devtunnels.ms/w/abcdef0123456789#00112233445566778899aabbccddeeff").unwrap();
        assert!(qr.width >= 21);
        assert_eq!(qr.dark.len(), qr.width * qr.width);
        assert!(qr.dark.iter().any(|&d| d) && qr.dark.iter().any(|&d| !d));
    }

    #[test]
    fn fill_truncates_and_terminates() {
        let mut buf = [0xffffu16; 8];
        fill(&mut buf, "abcdefghijkl");
        assert_eq!(&buf[..7], &"abcdefg".encode_utf16().collect::<Vec<_>>()[..]);
        assert_eq!(buf[7], 0);
    }
}

/// Ask an already-running tray app to show its QR window. Returns false if none was found.
pub fn show_existing() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        match FindWindowW(w!("TerminalTunnelTray"), PCWSTR::null()) {
            Ok(hwnd) => PostMessageW(Some(hwnd), WM_SHOW, WPARAM(0), LPARAM(0)).is_ok(),
            Err(_) => false,
        }
    }
}
