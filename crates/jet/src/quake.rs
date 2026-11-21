//! `jet quake`: windows that global hotkeys show and hide, placed by
//! fractions of a monitor's work area.
//!
//! `jet quake` itself (the coordinator) shows nothing: it holds the
//! hotkeys, the tray icon and `[quake] autostart`'s sign-in entry, and
//! starts a process per quake window (`[quake]`, or each `[quake.NAME]`),
//! which it shows and hides: together, when they share a hotkey. Another
//! `jet quake` (or `jet quake NAME`) asks the running one to toggle and exits.

use std::cell::RefCell;
use std::error::Error;

use jet_config::{Monitor, Quake, keybinds};
use jet_core::Mods;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromRect, MonitorFromWindow,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, BringWindowToTop, CallNextHookEx, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
    GWL_EXSTYLE, GWLP_USERDATA, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowThreadProcessId, HC_ACTION, HICON, HWND_NOTOPMOST, HWND_TOPMOST,
    KBDLLHOOKSTRUCT, KillTimer, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PM_NOREMOVE, PeekMessageW,
    PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassExW, RegisterWindowMessageW,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SendMessageTimeoutW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SetWindowsHookExW, ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_COPYDATA, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONUP,
    WM_QUIT, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSEXW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::app::ConfigFile;
use crate::keymap;

/// The coordinator's window class, so another `jet quake` can find it.
pub const CLASS: PCWSTR = w!("jet-quake");
/// Quake windows' classes start so, then the window's name (empty for
/// `[quake]` alone).
const WINDOW_CLASS: &str = "jet-quake:";
/// Toggle: to the coordinator, the first window's group (from another
/// `jet quake`); to a quake window, itself (`jet-cli quake`).
pub const WM_APP_TOGGLE: u32 = WM_APP + 2;
/// Tray icon clicks, to the coordinator.
pub const WM_APP_TRAY: u32 = WM_APP + 3;
/// Hide, to a quake window.
pub const WM_APP_HIDE: u32 = WM_APP + 4;
/// Settings changed: move to the new place if showing.
pub const WM_APP_PLACE: u32 = WM_APP + 5;
/// Quit, from `jet quake --uninstall` or the tray. Above win32.rs's own
/// WM_APP ids, like the rest.
pub const WM_APP_QUIT: u32 = WM_APP + 20;
/// A quake window lost the focus: decided once the new window is in front.
pub const WM_APP_BLUR: u32 = WM_APP + 21;
/// Show, to a quake window; activating it when wparam is 1.
pub const WM_APP_SHOW: u32 = WM_APP + 22;
/// From the hook: hotkey wparam was pressed.
const WM_APP_KEY: u32 = WM_APP + 23;
/// To the coordinator: quake window wparam started.
pub const WM_APP_READY: u32 = WM_APP + 24;
/// To the coordinator: the focus left the quake windows for another.
pub const WM_APP_LEFT: u32 = WM_APP + 25;
/// WM_COPYDATA kinds: toggle the named window (UTF-8 name, to the
/// coordinator), and the hotkey problem (UTF-8, empty for none, to a
/// quake window).
const COPY_TOGGLE: usize = 1;
pub const COPY_PROBLEM: usize = 2;
const MENU_QUIT: usize = 1;
/// Tray menu ids from here on toggle the window of that index.
const MENU_WINDOW: usize = 100;
/// How often the coordinator looks for config edits.
const RELOAD_MS: u32 = 1000;
const RELOAD_TIMER: usize = 1;

type BoxError = Box<dyn Error>;

/// Quake window `name`'s class.
pub fn window_class(name: Option<&str>) -> HSTRING {
    HSTRING::from(format!("{WINDOW_CLASS}{}", name.unwrap_or("")))
}

/// Whether `hwnd` is one of jet's quake windows.
fn is_quake_window(hwnd: HWND) -> bool {
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    let class = String::from_utf16_lossy(&class[..usize::try_from(len).unwrap_or(0)]);
    class.starts_with(WINDOW_CLASS)
}

fn find(class: &HSTRING) -> Option<HWND> {
    unsafe { FindWindowW(class, PCWSTR::null()) }.ok()
}

/// The running coordinator's window, if one is.
fn running() -> Option<HWND> {
    unsafe { FindWindowW(CLASS, PCWSTR::null()) }.ok()
}

/// Held while this process is the one coordinator, or the one process of
/// a quake window; `None` if another already is.
pub struct Claim(HANDLE);

impl Claim {
    /// The coordinator's with `None`, else quake window `name`'s.
    pub fn take(window: Option<Option<&str>>) -> Option<Self> {
        let name = match window {
            None => HSTRING::from(r"Local\jet-quake"),
            Some(name) => HSTRING::from(format!(r"Local\jet-quake:{}", name.unwrap_or(""))),
        };
        let handle = unsafe { CreateMutexW(None, false, &name) }.ok()?;
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

/// Ask the running coordinator to toggle window `name`, or with `None`
/// the first window's group. False if none is running.
fn signal_running(name: Option<&str>) -> bool {
    let Some(hwnd) = running() else {
        return false;
    };
    // It's the coordinator that brings a window forward, with the right
    // to that this process has for being just started.
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let _ = unsafe { AllowSetForegroundWindow(pid) };
    let Some(name) = name else {
        return unsafe { PostMessageW(Some(hwnd), WM_APP_TOGGLE, WPARAM(0), LPARAM(0)) }.is_ok();
    };
    send_copy(hwnd, COPY_TOGGLE, name.as_bytes());
    true
}

/// `bytes` to `hwnd` as WM_COPYDATA of kind `kind`, waiting a moment at
/// most for it to be taken. Not answered while the wait lasts, so the
/// receiver must never send back.
fn send_copy(hwnd: HWND, kind: usize, bytes: &[u8]) {
    let data = COPYDATASTRUCT {
        dwData: kind,
        cbData: u32::try_from(bytes.len()).unwrap_or(0),
        lpData: bytes.as_ptr().cast_mut().cast(),
    };
    let _ = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&raw const data as isize),
            SMTO_BLOCK | SMTO_ABORTIFHUNG,
            1000,
            None,
        )
    };
}

/// A WM_COPYDATA's kind and bytes.
///
/// # Safety
/// `lparam` is a WM_COPYDATA's, used before the handler returns.
pub unsafe fn copied<'a>(lparam: LPARAM) -> Option<(usize, &'a [u8])> {
    let data = lparam.0 as *const COPYDATASTRUCT;
    // SAFETY: WM_COPYDATA's lparam points at its COPYDATASTRUCT, whose
    // bytes stay mapped until the handler returns.
    let data = unsafe { data.as_ref() }?;
    if data.lpData.is_null() {
        return Some((data.dwData, &[]));
    }
    let len = usize::try_from(data.cbData).ok()?;
    Some((data.dwData, unsafe {
        std::slice::from_raw_parts(data.lpData.cast::<u8>(), len)
    }))
}

/// Where Windows keeps what runs at sign-in, and jet's entry there.
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("jet quake");

/// What an uninstaller runs: `jet quake --uninstall`.
pub const UNINSTALL: &str = "--uninstall";
/// What the installer runs once done: `jet quake --autostart`, which does
/// what `quake.autostart` says and exits.
pub const AUTOSTART: &str = "--autostart";
/// What the coordinator starts a quake window with: `jet quake --window
/// [NAME]`.
pub const WINDOW: &str = "--window";

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

/// What a quake window's process keeps of being one.
pub struct QuakeWindow {
    /// Where the user last dragged or sized the window to, kept for the
    /// session in place of `[quake]`'s placement.
    pub moved: Option<RECT>,
    /// Why a hotkey couldn't be had, from the coordinator.
    pub problem: Option<String>,
}

impl QuakeWindow {
    pub fn new() -> Self {
        Self {
            moved: None,
            problem: None,
        }
    }

    /// Started: the coordinator answers with the hotkey problem, if any.
    pub fn ready(hwnd: HWND) {
        if let Some(coordinator) = running() {
            let _ = unsafe {
                PostMessageW(
                    Some(coordinator),
                    WM_APP_READY,
                    WPARAM(hwnd.0 as usize),
                    LPARAM(0),
                )
            };
        }
    }

    /// Lost the focus (`WM_APP_BLUR`): whether to hide, telling the
    /// coordinator when the focus left for something that's not a quake
    /// window. Decided now and not on the deactivation, which can be one
    /// that's undone at once while coming forward.
    pub fn blurred(hwnd: HWND, hide_on_blur: bool) -> bool {
        let front = unsafe { GetForegroundWindow() };
        if front.is_invalid() || front == hwnd || is_quake_window(front) {
            return false;
        }
        if let Some(coordinator) = running() {
            let _ = unsafe { PostMessageW(Some(coordinator), WM_APP_LEFT, WPARAM(0), LPARAM(0)) };
        }
        hide_on_blur
    }
}

/// What `jet quake [NAME]` runs: the coordinator, or with one running, a
/// toggle of the first window's group (or window `name`) by it. Hidden
/// with `background` until a hotkey, else shown at once.
pub fn coordinate(background: bool, name: Option<String>) -> Result<(), BoxError> {
    let Some(_claim) = Claim::take(None) else {
        if !background {
            signal_running(name.as_deref());
        }
        return Ok(());
    };
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    let icon = unsafe { LoadIconW(Some(instance), crate::app::win32::ICON) }.unwrap_or_default();
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>())?,
        lpfnWndProc: Some(coordinator_proc),
        hInstance: instance,
        hIcon: icon,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(windows::core::Error::from_thread().into());
    }
    // Never shown: a top-level window, not a message-only one, so that
    // FindWindow finds it and Explorer's broadcasts reach it.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS,
            w!("jet quake"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    let mut coordinator = Coordinator {
        hwnd,
        icon,
        taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        settings: ConfigFile::load(None),
        windows: Vec::new(),
        keys: Vec::new(),
        hook: None,
        problem: None,
        last: None,
    };
    coordinator.add_tray_icon();
    coordinator.apply();
    // Started by hand: show what was asked for.
    let shown: Vec<Option<String>> = match (&name, background) {
        (_, true) => Vec::new(),
        (Some(name), false) => vec![Some(name.clone())],
        (None, false) => coordinator.first_group(),
    };
    for (window, _) in &coordinator.windows {
        if find(&window_class(window.as_deref())).is_none() {
            spawn(window.as_deref(), !shown.contains(window));
        }
    }
    let _ = unsafe { SetTimer(Some(hwnd), RELOAD_TIMER, RELOAD_MS, None) };
    let cell = Box::into_raw(Box::new(RefCell::new(coordinator)));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cell as isize) };
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), RELOAD_TIMER);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        // SAFETY: from Box::into_raw above; the window no longer has it.
        drop(Box::from_raw(cell));
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

/// Start quake window `name`'s process, hidden until a hotkey with
/// `background`.
fn spawn(name: Option<&str>, background: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return eprintln!("jet: can't find jet.exe to start a quake window");
    };
    let mut command = std::process::Command::new(exe);
    command.args(["quake", WINDOW]);
    command.args(name);
    if background {
        command.arg(crate::app::BACKGROUND);
    }
    if let Err(err) = command.spawn() {
        eprintln!("jet: starting quake window {}: {err}", name.unwrap_or(""));
    }
}

/// A hotkey and the windows it toggles.
struct Key {
    /// As written, the first time.
    chord: String,
    vk: u16,
    mods: HOT_KEY_MODIFIERS,
    windows: Vec<Option<String>>,
}

/// The coordinator's state, owned by its window.
struct Coordinator {
    hwnd: HWND,
    icon: HICON,
    /// Explorer's broadcast after it restarts: the tray icon is gone.
    taskbar_created: u32,
    settings: ConfigFile,
    /// The quake windows the config has, by name.
    windows: Vec<(Option<String>, Quake)>,
    /// Registered as hotkey id index + 1.
    keys: Vec<Key>,
    /// `quake.hotkey-hook`'s keyboard hook, when on.
    hook: Option<Hook>,
    /// Why a hotkey couldn't be had, shown in the quake windows.
    problem: Option<String>,
    /// The window last in front when its group hid, to come back to.
    last: Option<Option<String>>,
}

unsafe extern "system" fn coordinator_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Coordinator>;
    // SAFETY: set from Box::into_raw in `coordinate`, cleared before it's
    // dropped, and only used on this thread.
    let Some(cell) = (unsafe { ptr.as_ref() }) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    // Re-entered from the tray menu's own loop: default handling.
    let handled = cell
        .try_borrow_mut()
        .ok()
        .and_then(|mut c| c.handle(msg, wparam, lparam));
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

impl Coordinator {
    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_HOTKEY => self.toggle_key(wparam.0.wrapping_sub(1)),
            WM_APP_KEY => self.toggle_key(wparam.0),
            WM_APP_TOGGLE => self.toggle(self.first_group()),
            WM_COPYDATA => {
                // SAFETY: a WM_COPYDATA's lparam, used in this handler only.
                if let Some((COPY_TOGGLE, name)) = unsafe { copied(lparam) } {
                    let name = String::from_utf8_lossy(name);
                    self.toggle(vec![(!name.is_empty()).then(|| name.into_owned())]);
                }
                return Some(LRESULT(1));
            }
            WM_APP_TRAY => match (lparam.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP => self.toggle(self.first_group()),
                WM_RBUTTONUP => self.tray_menu(),
                _ => {}
            },
            WM_APP_READY => self.send_problem(HWND(wparam.0 as *mut _)),
            WM_APP_LEFT => self.hide_on_blur(),
            WM_TIMER if wparam.0 == RELOAD_TIMER => {
                if let Some(config) = self.settings.reload(false) {
                    self.settings.config = config;
                    self.apply();
                }
            }
            WM_APP_QUIT => self.quit(),
            msg if msg == self.taskbar_created => self.add_tray_icon(),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    /// Put the config in effect: windows started or quit, hotkeys taken.
    fn apply(&mut self) {
        let windows = match self.settings.config.quake.resolve() {
            Ok(windows) => windows,
            Err(err) => {
                eprintln!("jet: {err}");
                return;
            }
        };
        for (gone, _) in &self.windows {
            if !windows.iter().any(|(name, _)| name == gone) {
                post(gone.as_deref(), WM_APP_QUIT, 0);
            }
        }
        // Only on a change: the first start starts them as asked.
        if !self.windows.is_empty() {
            for (new, _) in &windows {
                if !self.windows.iter().any(|(name, _)| name == new) {
                    spawn(new.as_deref(), true);
                }
            }
        }
        self.windows = windows;
        let problem = self.register().err();
        if problem != self.problem {
            self.problem = problem;
            for (name, _) in &self.windows {
                if let Some(hwnd) = find(&window_class(name.as_deref())) {
                    self.send_problem(hwnd);
                }
            }
        }
    }

    /// Take every window's hotkeys, a chord shared by windows once for
    /// them all, dropping the previous ones; why not if one can't be.
    fn register(&mut self) -> Result<(), String> {
        self.hook = None;
        for id in 1..=self.keys.len() {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), i32::try_from(id).unwrap_or(0)) };
        }
        self.keys.clear();
        let mut problems = Vec::new();
        for (name, quake) in &self.windows {
            for chord in &quake.hotkey.0 {
                match parse_chord(chord) {
                    Ok((vk, mods)) => {
                        match self.keys.iter_mut().find(|k| (k.vk, k.mods) == (vk, mods)) {
                            Some(key) => key.windows.push(name.clone()),
                            None => self.keys.push(Key {
                                chord: chord.clone(),
                                vk,
                                mods,
                                windows: vec![name.clone()],
                            }),
                        }
                    }
                    Err(problem) => problems.push(problem),
                }
            }
        }
        for (id, key) in (1..).zip(&self.keys) {
            // Kept with the hook too: it still claims the chord, and it
            // still fires where the hook can't hear (an elevated window in
            // front).
            let mods = key.mods | MOD_NOREPEAT;
            if unsafe { RegisterHotKey(Some(self.hwnd), id, mods, u32::from(key.vk)) }.is_err() {
                problems.push(format!(
                    "quake.hotkey: {} is taken by another app",
                    key.chord
                ));
            }
        }
        if self.settings.config.quake.hotkey_hook && !self.keys.is_empty() {
            let chords = self.keys.iter().map(|k| (k.vk, k.mods)).collect();
            match Hook::start(self.hwnd, chords) {
                Ok(hook) => self.hook = Some(hook),
                Err(problem) => problems.push(problem),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// The first window's group: the windows sharing its first hotkey, or
    /// it alone.
    fn first_group(&self) -> Vec<Option<String>> {
        let Some((first, _)) = self.windows.first() else {
            return Vec::new();
        };
        self.keys
            .iter()
            .find(|k| k.windows.first() == Some(first))
            .map_or_else(|| vec![first.clone()], |k| k.windows.clone())
    }

    fn toggle_key(&mut self, index: usize) {
        if let Some(key) = self.keys.get(index) {
            self.toggle(key.windows.clone());
        }
    }

    /// Hide `group` if one of it is in front, else show it all, the one
    /// last in front (or the first) focused. Ones not running are started.
    fn toggle(&mut self, group: Vec<Option<String>>) {
        let mut running = Vec::new();
        for name in group {
            match find(&window_class(name.as_deref())) {
                Some(hwnd) => running.push((name, hwnd)),
                None if self.windows.iter().any(|(n, _)| *n == name) => {
                    spawn(name.as_deref(), false);
                }
                None => {}
            }
        }
        let front = unsafe { GetForegroundWindow() };
        if let Some((name, _)) = running.iter().find(|(_, hwnd)| *hwnd == front) {
            self.last = Some(name.clone());
            for (_, hwnd) in &running {
                post_to(*hwnd, WM_APP_HIDE, 0);
            }
            return;
        }
        let focus = running
            .iter()
            .position(|(name, _)| Some(name) == self.last.as_ref())
            .unwrap_or(0);
        for (i, (_, hwnd)) in running.iter().enumerate() {
            if i != focus {
                post_to(*hwnd, WM_APP_SHOW, 0);
            }
        }
        if let Some((_, hwnd)) = running.get(focus) {
            // Lends it the right to come forward this hotkey gave us.
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(*hwnd, Some(&mut pid)) };
            let _ = unsafe { AllowSetForegroundWindow(pid) };
            post_to(*hwnd, WM_APP_SHOW, 1);
        }
    }

    /// The focus left the quake windows: hide those that hide on blur.
    fn hide_on_blur(&self) {
        if is_quake_window(unsafe { GetForegroundWindow() }) {
            return;
        }
        for (name, quake) in &self.windows {
            if quake.hide_on_blur {
                post(name.as_deref(), WM_APP_HIDE, 0);
            }
        }
    }

    fn send_problem(&self, hwnd: HWND) {
        let problem = self.problem.as_deref().unwrap_or("");
        send_copy(hwnd, COPY_PROBLEM, problem.as_bytes());
    }

    /// Quit the quake windows and then itself.
    fn quit(&self) {
        for (name, _) in &self.windows {
            post(name.as_deref(), WM_APP_QUIT, 0);
        }
        unsafe { PostQuitMessage(0) };
    }

    fn add_tray_icon(&self) {
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

    /// The tray's right-click menu: a window to show or hide, or quit.
    /// Runs its own message loop.
    fn tray_menu(&mut self) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let mut added = true;
        for (i, (name, _)) in self.windows.iter().enumerate() {
            let label = match name {
                Some(name) => HSTRING::from(format!("Show / hide {name}")),
                None => HSTRING::from("Show / hide"),
            };
            let id = MENU_WINDOW + i;
            added &= unsafe { AppendMenuW(menu, MF_STRING, id, &label) }.is_ok();
        }
        added &= unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }.is_ok();
        added &= unsafe { AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit")) }.is_ok();
        let mut at = POINT::default();
        let _ = unsafe { GetCursorPos(&mut at) };
        // Without this the menu doesn't close when clicking elsewhere.
        let _ = unsafe { SetForegroundWindow(self.hwnd) };
        let picked = if added {
            let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON;
            let picked = unsafe { TrackPopupMenu(menu, flags, at.x, at.y, None, self.hwnd, None) };
            picked.0 as usize
        } else {
            0
        };
        let _ = unsafe { DestroyMenu(menu) };
        if picked == MENU_QUIT {
            self.quit();
        } else if let Some((name, _)) = picked
            .checked_sub(MENU_WINDOW)
            .and_then(|i| self.windows.get(i))
        {
            self.toggle(vec![name.clone()]);
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.hook = None;
        for id in 1..=self.keys.len() {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), i32::try_from(id).unwrap_or(0)) };
        }
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.tray_data()) };
    }
}

/// Post `msg` to quake window `name`, if it's running.
fn post(name: Option<&str>, msg: u32, wparam: usize) {
    if let Some(hwnd) = find(&window_class(name)) {
        post_to(hwnd, msg, wparam);
    }
}

fn post_to(hwnd: HWND, msg: u32, wparam: usize) {
    let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), LPARAM(0)) };
}

/// `chord` as a virtual key and RegisterHotKey's modifiers.
fn parse_chord(chord: &str) -> Result<(u16, HOT_KEY_MODIFIERS), String> {
    let parsed = keybinds::parse_chord(chord)
        .ok_or_else(|| format!("quake.hotkey: {chord:?} isn't a key chord"))?;
    let vk = keymap::vk_of(parsed.key)
        .ok_or_else(|| format!("quake.hotkey: {chord:?} can't be a global hotkey"))?;
    let mut mods = HOT_KEY_MODIFIERS(0);
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
    Ok((vk, mods))
}

/// The hotkeys seen by a low-level keyboard hook, on a thread of its own.
/// Games can turn hotkeys off while in front (raw input's
/// RIDEV_NOHOTKEYS: League of Legends does), but not a hook, which sees
/// keys before hotkeys are looked for. The thread does nothing else, so
/// it answers every key at once: Windows drops a hook that's slow.
struct Hook {
    thread: u32,
}

/// What the hook thread's hook looks for. A hook gets no context of its
/// own, so it lives with the thread that installed it.
struct Watch {
    hwnd: isize,
    chords: Vec<(u16, HOT_KEY_MODIFIERS)>,
    /// The chord whose key is down: its repeats and its release are
    /// swallowed too.
    held: Option<usize>,
}

thread_local! {
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

impl Hook {
    fn start(hwnd: HWND, chords: Vec<(u16, HOT_KEY_MODIFIERS)>) -> Result<Self, String> {
        let watch = Watch {
            hwnd: hwnd.0 as isize,
            chords,
            held: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("jet-quake-hook".into())
            .spawn(move || hook_thread(watch, &tx))
            .map_err(|e| format!("quake.hotkey-hook: {e}"))?;
        let thread = rx
            .recv()
            .map_err(|_| "quake.hotkey-hook: the hook thread quit".to_owned())??;
        Ok(Self { thread })
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// Install the hook, say so (with the thread to stop) and pump messages,
/// which is what runs the hook, until told to quit.
fn hook_thread(watch: Watch, started: &std::sync::mpsc::Sender<Result<u32, String>>) {
    WATCH.set(Some(watch));
    let mut msg = MSG::default();
    // Makes the queue, so the quit can't be posted before it exists.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    let module = unsafe { GetModuleHandleW(None) }.ok();
    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), module.map(Into::into), 0) };
    let hook = match hook {
        Ok(hook) => hook,
        Err(err) => {
            let _ = started.send(Err(format!("quake.hotkey-hook: {err}")));
            return;
        }
    };
    let _ = started.send(Ok(unsafe { GetCurrentThreadId() }));
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {}
    let _ = unsafe { UnhookWindowsHookEx(hook) };
}

/// The modifiers down now, as RegisterHotKey's.
fn mods_down() -> HOT_KEY_MODIFIERS {
    let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
    let mut mods = HOT_KEY_MODIFIERS(0);
    for (vks, m) in [
        (&[VK_CONTROL][..], MOD_CONTROL),
        (&[VK_SHIFT], MOD_SHIFT),
        (&[VK_MENU], MOD_ALT),
        (&[VK_LWIN, VK_RWIN], MOD_WIN),
    ] {
        if vks.iter().any(|vk| down(vk.0)) {
            mods |= m;
        }
    }
    mods
}

/// A chord toggles its windows and goes no further: the window in front
/// sees neither its key going down nor up.
unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points at the event's KBDLLHOOKSTRUCT.
        let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let ours = WATCH.with_borrow_mut(|watch| {
            let watch = watch.as_mut()?;
            let vk = u16::try_from(key.vkCode).ok()?;
            if let Some(held) = watch.held {
                if watch.chords.get(held).is_some_and(|c| c.0 == vk) {
                    if !down {
                        watch.held = None;
                    }
                    return Some(());
                }
                return None;
            }
            if !down {
                return None;
            }
            let mods = mods_down();
            let index = watch.chords.iter().position(|c| *c == (vk, mods))?;
            watch.held = Some(index);
            let hwnd = HWND(watch.hwnd as *mut std::ffi::c_void);
            post_to(hwnd, WM_APP_KEY, index);
            Some(())
        });
        if ours.is_some() {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
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
