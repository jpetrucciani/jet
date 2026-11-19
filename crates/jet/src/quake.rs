//! `jet quake`: one window that a global hotkey shows and hides, placed by
//! fractions of a monitor's work area, with a tray icon to reach it while
//! hidden. A second `jet quake` toggles the running one and exits.

use jet_config::{Monitor, Quake, keybinds};
use jet_core::Mods;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromRect, MonitorFromWindow,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CreatePopupMenu, DestroyMenu, DestroyWindow, FindWindowW,
    GWL_EXSTYLE, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, HICON,
    HWND_NOTOPMOST, HWND_TOPMOST, MF_STRING, PostMessageW, RegisterWindowMessageW, SW_HIDE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::keymap;

/// The quake window's class, so a second `jet quake` can find it.
pub const CLASS: PCWSTR = w!("jet-quake");
/// Show or hide: from a second `jet quake` or the tray.
pub const WM_APP_TOGGLE: u32 = WM_APP + 2;
/// Tray icon clicks.
pub const WM_APP_TRAY: u32 = WM_APP + 3;
/// Hide, posted when the window loses the focus.
pub const WM_APP_HIDE: u32 = WM_APP + 4;
/// Settings changed: move to the new place if showing.
pub const WM_APP_PLACE: u32 = WM_APP + 5;
/// Quit, from `jet quake --uninstall`. Above win32.rs's own WM_APP ids.
pub const WM_APP_QUIT: u32 = WM_APP + 20;
const HOTKEY: i32 = 1;
const MENU_TOGGLE: usize = 1;
const MENU_QUIT: usize = 2;

/// The running `jet quake`'s window, if one is.
fn running() -> Option<HWND> {
    unsafe { FindWindowW(CLASS, PCWSTR::null()) }.ok()
}

/// Held while this process is the one `jet quake`; `None` if another
/// already is.
pub struct Claim(HANDLE);

impl Claim {
    pub fn take() -> Option<Self> {
        let handle = unsafe { CreateMutexW(None, false, w!(r"Local\jet-quake")) }.ok()?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Ask a running `jet quake` to toggle. False if none is running.
pub fn signal_running() -> bool {
    let Some(hwnd) = running() else {
        return false;
    };
    unsafe { PostMessageW(Some(hwnd), WM_APP_TOGGLE, WPARAM(0), LPARAM(0)) }.is_ok()
}

/// Where Windows keeps what runs at sign-in, and jet's entry there.
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("jet quake");

/// What an uninstaller runs: `jet quake --uninstall`.
pub const UNINSTALL: &str = "--uninstall";
/// What the installer runs once done: `jet quake --autostart`, which does
/// what `quake.autostart` says and exits.
pub const AUTOSTART: &str = "--autostart";

/// Undo autostart for an uninstall: drop the sign-in entry and quit the
/// running quake, so nothing points at or holds the files being removed.
pub fn uninstall() {
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
    if let Some(hwnd) = running() {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// `quake.autostart`: have `jet quake` (this jet.exe) run at sign-in, and
/// start it now if it isn't running; or neither.
pub fn autostart(on: bool) -> Result<(), String> {
    if !on {
        // Absent already is fine.
        let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("quake.autostart: {e}"))?;
    if running().is_none() {
        std::process::Command::new(&exe)
            .args(["quake", crate::app::BACKGROUND])
            .spawn()
            .map_err(|e| format!("quake.autostart: starting jet quake: {e}"))?;
    }
    let line = HSTRING::from(format!(
        "\"{}\" quake {}",
        exe.display(),
        crate::app::BACKGROUND
    ));
    let bytes = u32::try_from((line.len() + 1) * 2).map_err(|e| e.to_string())?;
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(line.as_ptr().cast()),
            bytes,
        )
    };
    err.ok().map_err(|e| format!("quake.autostart: {e}"))
}

/// What `jet quake` holds while running: the hotkey and the tray icon.
pub struct QuakeWindow {
    hwnd: HWND,
    icon: HICON,
    /// Explorer's broadcast after it restarts: the tray icon is gone.
    pub taskbar_created: u32,
    /// Where the user last dragged or sized the window to, kept for the
    /// session in place of `[quake]`'s placement.
    pub moved: Option<RECT>,
}

impl QuakeWindow {
    pub fn new(hwnd: HWND, icon: HICON) -> Self {
        let this = Self {
            hwnd,
            icon,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            moved: None,
        };
        this.add_tray_icon();
        this
    }

    pub fn add_tray_icon(&self) {
        let mut data = self.tray_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon;
        for (slot, unit) in data.szTip.iter_mut().zip("jet quake".encode_utf16()) {
            *slot = unit;
        }
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            eprintln!("jet: adding the tray icon failed");
        }
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    /// Take `chord` as the hotkey, dropping the previous one; why not if it
    /// can't be.
    pub fn register(&self, chord: &str) -> Result<(), String> {
        let _ = unsafe { UnregisterHotKey(Some(self.hwnd), HOTKEY) };
        let parsed = keybinds::parse_chord(chord)
            .ok_or_else(|| format!("quake.hotkey: {chord:?} isn't a key chord"))?;
        let vk = keymap::vk_of(parsed.key)
            .ok_or_else(|| format!("quake.hotkey: {chord:?} can't be a global hotkey"))?;
        let mut mods = MOD_NOREPEAT;
        for (mod_, win) in [
            (Mods::CTRL, MOD_CONTROL),
            (Mods::SHIFT, MOD_SHIFT),
            (Mods::ALT, MOD_ALT),
            (Mods::SUPER, MOD_WIN),
        ] {
            if parsed.mods.contains(mod_) {
                mods |= win;
            }
        }
        unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY, mods, u32::from(vk)) }
            .map_err(|_| format!("quake.hotkey: {chord} is taken by another app"))
    }
}

impl Drop for QuakeWindow {
    fn drop(&mut self) {
        let _ = unsafe { UnregisterHotKey(Some(self.hwnd), HOTKEY) };
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.tray_data()) };
    }
}

/// Where `quake` goes on `monitor`'s work area.
fn placement(quake: &Quake, monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).ok()?,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let work = if quake.over_taskbar {
        info.rcMonitor
    } else {
        info.rcWork
    };
    let (w, h) = (
        (work.right - work.left) as f32,
        (work.bottom - work.top) as f32,
    );
    let at = |start: i32, len: f32, fraction: f32| start + (len * fraction).round() as i32;
    Some(RECT {
        left: at(work.left, w, quake.x),
        top: at(work.top, h, quake.y),
        right: at(work.left, w, quake.x + quake.width),
        bottom: at(work.top, h, quake.y + quake.height),
    })
}

/// The rect for the monitor Windows suggests after a DPI change.
pub fn placement_near(quake: &Quake, suggested: &RECT) -> Option<RECT> {
    placement(quake, unsafe {
        MonitorFromRect(suggested, MONITOR_DEFAULTTONEAREST)
    })
}

/// Show (or with `!activate`, just move) the window at `moved`, or per
/// `quake` when it wasn't moved or that's on no monitor now. Sends
/// messages: call with `App` released.
pub fn show(hwnd: HWND, quake: &Quake, moved: Option<RECT>, activate: bool) {
    let on_screen =
        moved.filter(|r| !unsafe { MonitorFromRect(r, MONITOR_DEFAULTTONULL) }.is_invalid());
    let monitor = if !activate {
        unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
    } else {
        match quake.monitor {
            Monitor::Cursor => {
                let mut at = POINT::default();
                let _ = unsafe { GetCursorPos(&mut at) };
                unsafe { MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST) }
            }
            Monitor::Primary => unsafe {
                MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)
            },
        }
    };
    let Some(r) = on_screen.or_else(|| placement(quake, monitor)) else {
        return eprintln!("jet: can't find the monitor to show on");
    };
    let ex = if quake.taskbar {
        WS_EX_APPWINDOW
    } else {
        WS_EX_TOOLWINDOW
    };
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex.0 as isize) };
    let order = if quake.always_on_top {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    // Shown without activating: activating here, from the background, can
    // be refused, and a refused activation comes back as a deactivation
    // that hide-on-blur takes for leaving: the window flashes and hides.
    let flags = SWP_SHOWWINDOW | SWP_FRAMECHANGED | SWP_NOACTIVATE;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if let Err(err) = unsafe { SetWindowPos(hwnd, Some(order), r.left, r.top, w, h, flags) } {
        eprintln!("jet: placing the quake window: {err}");
    }
    if activate {
        come_forward(hwnd);
    }
}

/// Into the foreground. The hotkey (or tray click) being the last input
/// usually allows it, but Windows' foreground lock can still refuse (an
/// elevated window or the Start menu in front); sharing the foreground
/// thread's input for the moment gets past that.
fn come_forward(hwnd: HWND) {
    if unsafe { SetForegroundWindow(hwnd) }.as_bool() && unsafe { GetForegroundWindow() } == hwnd {
        return;
    }
    let front = unsafe { GetForegroundWindow() };
    let theirs = unsafe { GetWindowThreadProcessId(front, None) };
    let ours = unsafe { GetCurrentThreadId() };
    if theirs == 0 || theirs == ours {
        return;
    }
    let joined = unsafe { AttachThreadInput(ours, theirs, true) }.as_bool();
    let _ = unsafe { BringWindowToTop(hwnd) };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    if joined {
        let _ = unsafe { AttachThreadInput(ours, theirs, false) };
    }
}

pub fn hide(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

/// The tray's right-click menu; runs its own message loop.
pub fn tray_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let added = unsafe {
        AppendMenuW(menu, MF_STRING, MENU_TOGGLE, w!("Show / hide")).is_ok()
            && AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit")).is_ok()
    };
    let mut at = POINT::default();
    let _ = unsafe { GetCursorPos(&mut at) };
    // Without this the menu doesn't close when clicking elsewhere.
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let picked = if added {
        let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON;
        unsafe { TrackPopupMenu(menu, flags, at.x, at.y, None, hwnd, None) }.0 as usize
    } else {
        0
    };
    let _ = unsafe { DestroyMenu(menu) };
    match picked {
        MENU_TOGGLE => {
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_TOGGLE, WPARAM(0), LPARAM(0)) };
        }
        MENU_QUIT => {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        _ => {}
    }
}
