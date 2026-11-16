//! Raw Win32 window: our own message loop and a wgpu DX12 surface.
//!
//! Lifecycle: `App` lives in a `Box<RefCell<_>>` behind `GWLP_USERDATA` from
//! window creation until `WM_DESTROY`, where it is dropped (closing the shell)
//! while the window, and with it the surface's HWND, is still valid. Nothing
//! that can synchronously re-enter the window procedure runs while `App` is
//! borrowed; such calls are returned as an [`After`], or queued in
//! [`Win32::later`], and run after.

use std::cell::RefCell;
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::Thread;
use std::time::Duration;

use jet_config::{Config, Decorations};
use jet_core::{Key, KeyAction, KeyInput, Mods, MouseAction, MouseButton, Progress};
use jet_render::Gpu;
use wgpu::rwh::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow, PAINTSTRUCT,
    ScreenToClient,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
    GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, ImmGetContext, ImmReleaseContext,
    ImmSetCandidateWindow, ImmSetCompositionWindow,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, MAPVK_VK_TO_CHAR, MapVirtualKeyW, ReleaseCapture, SetCapture, TME_LEAVE,
    TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx, GWLP_USERDATA, GetCaretBlinkTime,
    GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON,
    HTCAPTION, HTCLIENT, HTCLOSE, HTMAXBUTTON, HTMINBUTTON, HTTOP, IDC_ARROW, IDC_HAND, IDC_IBEAM,
    IDC_SIZENS, IDC_SIZEWE, IsIconic, IsWindowVisible, IsZoomed, KillTimer, LoadCursorW, LoadIconW,
    MSG, NCCALCSIZE_PARAMS, PostMessageW, PostQuitMessage, RegisterClassExW, SM_CXDRAG,
    SM_CXPADDEDBORDER, SM_CYDRAG, SM_CYFRAME, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
    SW_SHOWNORMAL, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED,
    WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SIZE, WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
    WNDCLASSEXW, WS_CAPTION, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::app::Kind;
use crate::app::{App, BoxError, ConfigFile, Host, Pointer, Timer, Wake, WindowRequest};
use crate::chrome::Button;
use crate::dwm;
use crate::keymap;
use crate::layout::Rect;
use crate::quake::{self, QuakeWindow};
use crate::taskbar::Taskbar;

/// Posted by the pane thread when a snapshot or event is ready.
const WM_APP_WAKE: u32 = WM_APP + 1;
/// The title bar changed hands: recompute the client area.
const WM_APP_FRAME: u32 = WM_APP + 6;
/// A `jet-cli` request is waiting.
const WM_APP_CLI: u32 = WM_APP + 7;
/// Come to the front, for `jet-cli activate`.
const WM_APP_RAISE: u32 = WM_APP + 8;
/// The window's monitor just had a vblank (see [`Vblank`]).
const WM_APP_VBLANK: u32 = WM_APP + 9;
/// A notification's balloon was clicked, timed out or closed.
const WM_APP_NOTIFY: u32 = WM_APP + 10;
/// The tray icon carrying notifications; quake's own is 1.
const NOTIFY_ICON: u32 = 2;
/// `MAKEINTRESOURCE(1)`: the icon in `assets/jet.rc`.
const ICON: PCWSTR = PCWSTR(std::ptr::without_provenance(1));
/// Wheel units per notch.
const WHEEL_DELTA: i32 = 120;
const CF_UNICODETEXT: u32 = 13;
const TIMERS: [Timer; 4] = [Timer::Blink, Timer::Autoscroll, Timer::Flash, Timer::Config];

fn timer_id(timer: Timer) -> usize {
    match timer {
        Timer::Blink => 1,
        Timer::Autoscroll => 2,
        Timer::Flash => 3,
        Timer::Config => 4,
    }
}

/// The window, and the keyboard state Windows spreads over messages.
pub struct Win32 {
    hwnd: HWND,
    /// Set when this is `jet quake`'s window.
    quake: Option<QuakeWindow>,
    taskbar: Option<Taskbar>,
    /// Paces frames for see-through windows, whose DirectComposition
    /// swapchain's vsync runs on the wrong clock across mixed refresh rates.
    vblank: Option<Vblank>,
    /// Calls that send messages back, like moving the window: run once
    /// `App` is released.
    later: Vec<Box<dyn FnOnce(HWND)>>,
    /// The last text key pressed, paired with the `WM_CHAR` it produces.
    pending_key: Option<(Key, Option<char>)>,
    /// Drop the next `WM_CHAR`: its key was already handled.
    suppress_char: bool,
    high_surrogate: Option<u16>,
    /// The window class's icon, for notifications.
    icon: HICON,
    /// A notification's tray icon is up, until its balloon goes away.
    notifying: bool,
}

impl Win32 {
    fn post(&self, msg: u32) {
        let _ = unsafe { PostMessageW(Some(self.hwnd), msg, WPARAM(0), LPARAM(0)) };
    }

    fn notify_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: self.hwnd,
            uID: NOTIFY_ICON,
            ..Default::default()
        }
    }

    /// Drop the notification's tray icon (and its toast with it).
    fn end_notification(&mut self) {
        if std::mem::take(&mut self.notifying) {
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.notify_data()) };
        }
    }
}

impl Drop for Win32 {
    fn drop(&mut self) {
        self.end_notification();
    }
}

/// `text` into a fixed UTF-16 buffer, cut to fit with its terminating NUL.
fn fill(buf: &mut [u16], text: &str) {
    let room = buf.len().saturating_sub(1);
    for (slot, unit) in buf.iter_mut().take(room).zip(text.encode_utf16()) {
        *slot = unit;
    }
}

impl Host for Win32 {
    fn waker(&self, wake: Wake) -> Arc<dyn Fn() + Send + Sync> {
        let msg = match wake {
            Wake::Panes => WM_APP_WAKE,
            Wake::Cli => WM_APP_CLI,
            Wake::Quake => quake::WM_APP_TOGGLE,
        };
        // HWND isn't Send; posting to it from another thread is fine.
        let target = self.hwnd.0 as isize;
        Arc::new(move || {
            let hwnd = HWND(target as *mut c_void);
            let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)) };
        })
    }

    fn set_title(&mut self, title: &str) {
        let _ = unsafe { SetWindowTextW(self.hwnd, &HSTRING::from(title)) };
    }

    fn set_timer(&mut self, timer: Timer, every: Duration) {
        let ms = u32::try_from(every.as_millis()).unwrap_or(u32::MAX);
        unsafe { SetTimer(Some(self.hwnd), timer_id(timer), ms, None) };
    }

    fn kill_timer(&mut self, timer: Timer) {
        let _ = unsafe { KillTimer(Some(self.hwnd), timer_id(timer)) };
    }

    fn clipboard(&mut self) -> Option<String> {
        clipboard_text(self.hwnd)
    }

    fn set_clipboard(&mut self, text: &str) -> Result<(), String> {
        set_clipboard_text(self.hwnd, text).map_err(|err| err.to_string())
    }

    fn maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd) }.as_bool()
    }

    fn mods(&self) -> Mods {
        current_mods()
    }

    fn blink_interval(&self) -> Option<Duration> {
        let ms = unsafe { GetCaretBlinkTime() };
        (ms != u32::MAX).then(|| Duration::from_millis(u64::from(ms)))
    }

    fn capture(&mut self, on: bool) {
        if on {
            unsafe { SetCapture(self.hwnd) };
        } else {
            let _ = unsafe { ReleaseCapture() };
        }
    }

    fn drag_threshold(&self) -> (i32, i32) {
        unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) }
    }

    fn request(&mut self, request: WindowRequest) {
        match request {
            // Async: closing re-enters the window procedure.
            WindowRequest::Close => self.post(WM_CLOSE),
            WindowRequest::Raise => self.post(WM_APP_RAISE),
            WindowRequest::Move => self.later.push(Box::new(move_window)),
            // A new client area; that sends messages.
            WindowRequest::Reframe => self.post(WM_APP_FRAME),
            WindowRequest::Attention => {
                let info = FLASHWINFO {
                    cbSize: size_of::<FLASHWINFO>() as u32,
                    hwnd: self.hwnd,
                    dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                    uCount: 0,
                    dwTimeout: 0,
                };
                let _ = unsafe { FlashWindowEx(&info) };
            }
        }
    }

    fn progress(&mut self, progress: Progress) {
        if let Some(taskbar) = &mut self.taskbar {
            taskbar.show(self.hwnd, progress);
        }
    }

    fn local_time(&self) -> (String, String) {
        local_time()
    }

    fn quake(&self) -> bool {
        self.quake.is_some()
    }

    fn paces_frames(&self) -> bool {
        self.vblank.is_some()
    }

    fn want_vblank(&mut self, on: bool) {
        if let Some(vblank) = &self.vblank {
            vblank.want(on);
        }
    }

    fn reconfigure_quake(&mut self, config: &jet_config::Quake) -> Option<String> {
        let problem = self.quake.as_ref()?.register(&config.hotkey).err();
        self.post(quake::WM_APP_PLACE);
        problem
    }

    fn style_frame(&mut self, config: &Config) {
        let vt = config.vt_settings();
        dwm::apply(self.hwnd, &config.window, vt.background, vt.foreground);
    }

    fn open_url(&mut self, url: &str) {
        let done = unsafe {
            ShellExecuteW(
                Some(self.hwnd),
                w!("open"),
                &HSTRING::from(url),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        // Above 32 is success; below is an error code.
        if done.0 as usize <= 32 {
            eprintln!("jet: opening {url} failed ({})", done.0 as usize);
        }
    }

    /// A balloon from a tray icon of its own, which Windows 10 and later
    /// show as a toast. The icon stays until the balloon is clicked (which
    /// raises the window), times out or is closed.
    fn notify(&mut self, title: &str, body: &str) {
        let mut data = self.notify_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_INFO;
        data.uCallbackMessage = WM_APP_NOTIFY;
        data.hIcon = self.icon;
        data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        fill(&mut data.szTip, "jet");
        fill(
            &mut data.szInfoTitle,
            if title.is_empty() { "jet" } else { title },
        );
        fill(&mut data.szInfo, body);
        let op = if self.notifying { NIM_MODIFY } else { NIM_ADD };
        if unsafe { Shell_NotifyIconW(op, &data) }.as_bool() {
            self.notifying = true;
        } else {
            eprintln!("jet: showing a notification failed");
        }
    }

    /// The IME draws its composition itself, here: at the cursor, with the
    /// candidate list kept off the cursor's cell.
    fn ime_area(&mut self, area: Rect) {
        let context = unsafe { ImmGetContext(self.hwnd) };
        if context.is_invalid() {
            return;
        }
        let at = POINT {
            x: area.x,
            y: area.y,
        };
        let cell = RECT {
            left: area.x,
            top: area.y,
            right: area.x + area.w,
            bottom: area.y + area.h,
        };
        let composition = COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: at,
            rcArea: RECT::default(),
        };
        let candidates = CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_EXCLUDE,
            ptCurrentPos: at,
            rcArea: cell,
        };
        unsafe {
            let _ = ImmSetCompositionWindow(context, &composition);
            let _ = ImmSetCandidateWindow(context, &candidates);
            let _ = ImmReleaseContext(self.hwnd, context);
        }
    }

    fn track_leave(&mut self) {
        // WM_MOUSELEAVE when the mouse leaves the window (or the bar's tabs
        // for its draggable part).
        let mut track = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        let _ = unsafe { TrackMouseEvent(&mut track) };
    }
}

/// A terminal window, or with `quake` the hotkey-toggled one.
pub fn run(quake: bool) -> ExitCode {
    match run_window(quake) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jet: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_window(quake: bool) -> Result<(), BoxError> {
    if quake && quake::signal_running() {
        return Ok(());
    }
    // For the taskbar button's progress (ITaskbarList3). Without it that's
    // all that's lost.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    // Best effort: fails harmlessly if a manifest already set awareness.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>())?,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(None, IDC_IBEAM)? },
        // Windows picks the small size from the same icon. A missing icon
        // only costs the default one.
        hIcon: unsafe { LoadIconW(Some(instance), ICON) }.unwrap_or_default(),
        lpszClassName: if quake { quake::CLASS } else { w!("jet") },
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(windows::core::Error::from_thread().into());
    }
    // The quake window is placed and shown by its hotkey.
    let style = if quake { WS_POPUP } else { WS_OVERLAPPEDWINDOW };
    let hwnd = unsafe {
        CreateWindowExW(
            // Everything is drawn through the composition swapchain; a GDI
            // surface under it would hide whatever transparency shows.
            WS_EX_NOREDIRECTIONBITMAP,
            class.lpszClassName,
            w!("jet"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            None,
        )?
    };

    let settings = ConfigFile::load();
    // Fixed for the window's life: an opacity edit that crosses 1 applies
    // on the next start.
    let see_through = settings.config.window.opacity < 1.0;
    let instance = Gpu::instance(see_through);
    let handle = NonZeroIsize::new(hwnd.0 as isize).ok_or("null window handle")?;
    // SAFETY: the surface is dropped with App in WM_DESTROY, before the
    // window it draws to is gone.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::Win32(Win32WindowHandle::new(handle)),
        })?
    };
    let host = Win32 {
        hwnd,
        quake: quake.then(|| QuakeWindow::new(hwnd, class.hIcon)),
        taskbar: Taskbar::new(),
        vblank: see_through.then(|| Vblank::start(hwnd)).flatten(),
        later: Vec::new(),
        pending_key: None,
        suppress_char: false,
        high_surrogate: None,
        icon: class.hIcon,
        notifying: false,
    };
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let focused = unsafe { GetForegroundWindow() } == hwnd;
    let mut app = App::new(host, &instance, surface, dpi, focused, settings)?;
    // Shown at once when the hotkey can't be had, to say so.
    let mut show_quake = false;
    if let Some(window) = &app.host.quake {
        app.hotkey_problem = window.register(&app.settings.config.quake.hotkey).err();
        show_quake = app.hotkey_problem.is_some();
        app.refresh_banner();
    }
    let size = if quake {
        None
    } else {
        Some(app.window_size_for(app.first_grid())?)
    };
    let placed = app.settings.config.quake.clone();
    let app = Box::into_raw(Box::new(RefCell::new(app)));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
        if let Some((width, height)) = size {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_FRAMECHANGED,
            )?;
            let _ = ShowWindow(hwnd, SW_SHOW);
        } else if show_quake {
            quake::show(hwnd, &placed, true);
        }
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            // Queues WM_CHAR for text keys; key_down suppresses it for keys
            // it already encoded.
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Work that may re-enter the window procedure, run once `App` is released.
enum After {
    Nothing,
    Default,
    MoveTo(RECT),
    /// Calls that send messages back, like showing or hiding the window.
    Then(Box<dyn FnOnce(HWND)>),
    /// Answer with this.
    Result(isize),
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RefCell<App<Win32>>;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    if msg == WM_DESTROY {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            // SAFETY: created by Box::into_raw in run_window, and never
            // borrowed here: WM_DESTROY only arrives via DefWindowProc, which
            // is never called while App is borrowed.
            drop(Box::from_raw(ptr));
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }
    // SAFETY: valid until WM_DESTROY above, on this thread only.
    let cell = unsafe { &*ptr };
    let (after, later) = match cell.try_borrow_mut() {
        Ok(mut app) => {
            let after = app.handle(msg, wparam, lparam);
            (after, std::mem::take(&mut app.host.later))
        }
        // Re-entered from inside a handler (e.g. WM_SETTEXT): default handling.
        Err(_) => (After::Default, Vec::new()),
    };
    for work in later {
        work(hwnd);
    }
    match after {
        After::Nothing => LRESULT(0),
        After::Default => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        After::MoveTo(r) => {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if let Err(err) = unsafe { SetWindowPos(hwnd, None, r.left, r.top, w, h, SWP_NOZORDER) }
            {
                eprintln!("jet: moving window after DPI change: {err}");
            }
            LRESULT(0)
        }
        After::Then(work) => {
            work(hwnd);
            LRESULT(0)
        }
        After::Result(value) => LRESULT(value),
    }
}

impl App<Win32> {
    /// Outer window size whose client area fits `grid` exactly.
    fn window_size_for(&self, grid: (u16, u16)) -> Result<(i32, i32), BoxError> {
        let (w, h) = self.client_size_for(grid)?;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let custom = self.decorations() != Decorations::System;
        // jet's own title bar is client area, over where Windows' would be.
        let style = if custom {
            WS_OVERLAPPEDWINDOW & !WS_CAPTION
        } else {
            WS_OVERLAPPEDWINDOW
        };
        unsafe {
            AdjustWindowRectExForDpi(
                &mut rect,
                style,
                false,
                WINDOW_EX_STYLE::default(),
                self.dpi,
            )?;
        }
        if custom {
            rect.top = 0;
        }
        Ok((rect.right - rect.left, rect.bottom - rect.top))
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> After {
        match msg {
            WM_APP_WAKE => self.pump(),
            WM_APP_VBLANK => self.vblank(),

            WM_SIZE => {
                let (w, h) = (lparam.0 & 0xFFFF, (lparam.0 >> 16) & 0xFFFF);
                if w > 0 && h > 0 {
                    self.resize(w as u32, h as u32);
                }
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                unsafe { BeginPaint(self.host.hwnd, &mut paint) };
                self.render();
                let _ = unsafe { EndPaint(self.host.hwnd, &paint) };
            }
            // The surface covers the whole client area; erasing only flickers.
            WM_ERASEBKGND => {}
            WM_KEYDOWN | WM_SYSKEYDOWN => return self.key_down(wparam, lparam),
            // Keep Alt from activating the (nonexistent) window menu.
            WM_SYSKEYUP => {}
            WM_KEYUP => {
                // Letting go of the link modifier over a URL.
                if (wparam.0 & 0xFFFF) as u16 == VK_CONTROL.0 {
                    self.hover_link();
                    self.set_cursor();
                }
                return After::Default;
            }
            WM_CHAR | WM_SYSCHAR => self.char(wparam),
            WM_MOUSEWHEEL => self.wheel_msg(wparam, lparam),
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_RBUTTONDOWN
            | WM_RBUTTONUP | WM_MOUSEMOVE => self.mouse_msg(msg, lparam),
            // Capture taken away mid-drag (Alt+Tab, a dialog): end the drag.
            WM_CAPTURECHANGED => self.lost_capture(),
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => return self.set_cursor(),
            WM_NCCALCSIZE => return self.calc_size(wparam, lparam),
            WM_MOUSELEAVE => self.mouse_left(),
            WM_NCHITTEST => return self.hit_test(lparam),
            WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK
            | WM_NCLBUTTONUP => return self.nc_mouse(msg, wparam),
            WM_APP_FRAME => return After::Then(Box::new(frame_changed)),
            WM_APP_CLI => self.serve_cli(),
            WM_APP_RAISE => {
                if self.host.quake.is_some() {
                    let placed = self.settings.config.quake.clone();
                    return After::Then(Box::new(move |hwnd| quake::show(hwnd, &placed, true)));
                }
                return After::Then(Box::new(raise));
            }
            // Active means focused for a one-window app. WM_SETFOCUS isn't
            // enough: a window opened from the background gets thread focus
            // without being in front. The caption follows WM_NCACTIVATE, and
            // so do we; Windows still draws the caption.
            WM_NCACTIVATE => {
                self.set_focus(wparam.0 != 0);
                if wparam.0 == 0
                    && self.host.quake.is_some()
                    && self.settings.config.quake.hide_on_blur
                {
                    // Hiding sends messages; not from in here.
                    self.host.post(quake::WM_APP_HIDE);
                }
                return After::Default;
            }
            WM_TIMER => {
                if let Some(timer) = TIMERS.iter().find(|t| timer_id(**t) == wparam.0) {
                    self.timer(*timer);
                }
            }
            WM_DPICHANGED => return self.dpi_changed(wparam, lparam),
            WM_HOTKEY | quake::WM_APP_TOGGLE => return self.toggle_quake(),
            WM_CLOSE | quake::WM_APP_HIDE if self.host.quake.is_some() => {
                return After::Then(Box::new(quake::hide));
            }
            quake::WM_APP_PLACE => {
                let placed = self.settings.config.quake.clone();
                if self.host.quake.is_some() && unsafe { IsWindowVisible(self.host.hwnd) }.as_bool()
                {
                    return After::Then(Box::new(move |hwnd| quake::show(hwnd, &placed, false)));
                }
            }
            WM_APP_NOTIFY => match (lparam.0 & 0xFFFF) as u32 {
                NIN_BALLOONUSERCLICK => {
                    self.host.end_notification();
                    return After::Then(Box::new(raise));
                }
                NIN_BALLOONTIMEOUT | NIN_BALLOONHIDE => self.host.end_notification(),
                _ => {}
            },
            quake::WM_APP_TRAY => match (lparam.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP => return self.toggle_quake(),
                WM_RBUTTONUP => return After::Then(Box::new(quake::tray_menu)),
                _ => {}
            },
            msg if self
                .host
                .quake
                .as_ref()
                .is_some_and(|q| q.taskbar_created == msg) =>
            {
                if let Some(quake) = &self.host.quake {
                    quake.add_tray_icon();
                }
            }
            _ => return After::Default,
        }
        After::Nothing
    }

    /// The pointer for what's under the mouse. Set outright rather than
    /// left to the class cursor, so a modifier key can change it too.
    fn set_cursor(&self) -> After {
        let mut at = POINT::default();
        if unsafe { GetCursorPos(&mut at) }.is_err()
            || !unsafe { ScreenToClient(self.host.hwnd, &mut at) }.as_bool()
        {
            return After::Default;
        }
        let shape = match self.pointer_at(at.x, at.y) {
            Pointer::ResizeColumns => IDC_SIZEWE,
            Pointer::ResizeRows => IDC_SIZENS,
            Pointer::Arrow => IDC_ARROW,
            Pointer::Link => IDC_HAND,
            Pointer::Text => IDC_IBEAM,
        };
        match unsafe { LoadCursorW(None, shape) } {
            Ok(cursor) => {
                unsafe { SetCursor(Some(cursor)) };
                After::Nothing
            }
            Err(_) => After::Default,
        }
    }

    /// Windows' resize border, which jet's title bar has to leave for it.
    fn resize_border(&self) -> i32 {
        unsafe {
            GetSystemMetricsForDpi(SM_CYFRAME, self.dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, self.dpi)
        }
    }

    /// Without Windows' title bar, the client area takes its place: all of
    /// the window but the side and bottom resize borders, and when
    /// maximized, the part hanging off the screen.
    fn calc_size(&self, wparam: WPARAM, lparam: LPARAM) -> After {
        if wparam.0 == 0 || self.decorations() == Decorations::System {
            return After::Default;
        }
        // SAFETY: with wparam TRUE, lparam is an NCCALCSIZE_PARAMS.
        let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
        let top = params.rgrc[0].top;
        unsafe { DefWindowProcW(self.host.hwnd, WM_NCCALCSIZE, wparam, lparam) };
        let zoomed = unsafe { IsZoomed(self.host.hwnd) }.as_bool();
        params.rgrc[0].top = top + if zoomed { self.resize_border() } else { 0 };
        After::Result(0)
    }

    /// What's under the mouse, for Windows: resize edges, jet's window
    /// buttons (so Snap Layouts shows over maximize), and the empty parts
    /// of the title and tab bars to drag the window by.
    fn hit_test(&self, lparam: LPARAM) -> After {
        let decorations = self.decorations();
        if decorations == Decorations::System {
            return After::Default;
        }
        let edge = unsafe { DefWindowProcW(self.host.hwnd, WM_NCHITTEST, WPARAM(0), lparam) };
        if edge.0 != HTCLIENT as isize {
            return After::Result(edge.0);
        }
        let mut at = POINT {
            x: i32::from((lparam.0 & 0xFFFF) as u16 as i16),
            y: i32::from(((lparam.0 >> 16) & 0xFFFF) as u16 as i16),
        };
        if !unsafe { ScreenToClient(self.host.hwnd, &mut at) }.as_bool() {
            return After::Default;
        }
        let zoomed = unsafe { IsZoomed(self.host.hwnd) }.as_bool();
        if !zoomed && at.y < self.resize_border() {
            return After::Result(HTTOP as isize);
        }
        let chrome = self.chrome();
        let hit = match chrome.button_at(at.x, at.y) {
            Some(Button::Minimize) => HTMINBUTTON,
            Some(Button::Maximize) => HTMAXBUTTON,
            Some(Button::Close) => HTCLOSE,
            None if self.tab_at(at.x, at.y).is_some() => HTCLIENT,
            None if chrome.caption.contains(at.x, at.y) => HTCAPTION,
            None if decorations == Decorations::None && chrome.tabs.contains(at.x, at.y) => {
                HTCAPTION
            }
            None => HTCLIENT,
        };
        After::Result(hit as isize)
    }

    /// Mouse over jet's window buttons; Windows sends these, as the hit
    /// test calls them non-client.
    fn nc_mouse(&mut self, msg: u32, wparam: WPARAM) -> After {
        let button = match wparam.0 as u32 {
            HTMINBUTTON => Some(Button::Minimize),
            HTMAXBUTTON => Some(Button::Maximize),
            HTCLOSE => Some(Button::Close),
            _ => None,
        };
        if self.decorations() != Decorations::Custom {
            return After::Default;
        }
        if msg == WM_NCMOUSELEAVE {
            self.set_hover(None);
            self.pressed = None;
            return After::Default;
        }
        if msg == WM_NCMOUSEMOVE {
            if button.is_some() && self.hover.is_none() {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: self.host.hwnd,
                    dwHoverTime: 0,
                };
                let _ = unsafe { TrackMouseEvent(&mut track) };
            }
            self.set_hover(button);
            return After::Default;
        }
        // Presses on the buttons are ours: Windows would draw its own.
        let Some(button) = button else {
            return After::Default;
        };
        if msg == WM_NCLBUTTONDOWN || msg == WM_NCLBUTTONDBLCLK {
            self.pressed = Some(button);
            return After::Result(0);
        }
        if msg != WM_NCLBUTTONUP || self.pressed.take() != Some(button) {
            return After::Result(0);
        }
        self.set_hover(None);
        After::Then(Box::new(move |hwnd| {
            let show = match button {
                Button::Minimize => SW_MINIMIZE,
                Button::Maximize if unsafe { IsZoomed(hwnd) }.as_bool() => SW_RESTORE,
                Button::Maximize => SW_MAXIMIZE,
                Button::Close => {
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                    return;
                }
            };
            let _ = unsafe { ShowWindow(hwnd, show) };
        }))
    }

    fn dpi_changed(&mut self, wparam: WPARAM, lparam: LPARAM) -> After {
        self.dpi = ((wparam.0 >> 16) & 0xFFFF) as u32;
        self.rebuild_renderer();
        // Cell sizes changed even where the cell count doesn't.
        self.relayout(true);
        // SAFETY: for WM_DPICHANGED, lparam points to the suggested RECT.
        let suggested = unsafe { *(lparam.0 as *const RECT) };
        // The quake window keeps its share of the monitor instead.
        let placed = self
            .host
            .quake
            .as_ref()
            .and_then(|_| quake::placement_near(&self.settings.config.quake, &suggested));
        After::MoveTo(placed.unwrap_or(suggested))
    }

    fn key_down(&mut self, wparam: WPARAM, lparam: LPARAM) -> After {
        let vk = (wparam.0 & 0xFFFF) as u16;
        let mods = current_mods();
        let (ctrl, alt) = (mods.contains(Mods::CTRL), mods.contains(Mods::ALT));
        // Alt+F4 stays Windows' to close the window.
        if alt && vk == 0x73 {
            return After::Default;
        }
        self.reset_blink();
        let Some((key, kind)) = keymap::lookup(vk) else {
            self.host.suppress_char = false;
            return After::Default;
        };
        if kind != Kind::Modifier && self.run_binding(key, mods) {
            self.host.suppress_char = true;
            return After::Nothing;
        }
        let unshifted = unshifted_char(vk);
        match kind {
            Kind::Modifier => {
                // Holding the link modifier over a URL.
                if vk == VK_CONTROL.0 {
                    self.hover_link();
                    self.set_cursor();
                }
                return After::Nothing;
            }
            // Plain text, or AltGr (Ctrl+Alt): let WM_CHAR bring the
            // layout's character.
            Kind::Text if ctrl == alt => {
                self.host.suppress_char = false;
                self.host.pending_key = Some((key, unshifted));
                return After::Nothing;
            }
            Kind::Text | Kind::Functional => {}
        }
        self.host.suppress_char = true;
        // Bit 30: the key was already down, so this is auto-repeat.
        let action = if lparam.0 & (1 << 30) != 0 {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        };
        // Ctrl+C always interrupts: selections are already copied on select.
        self.send_key(KeyInput {
            key,
            action,
            mods,
            text: None,
            unshifted,
        });
        After::Nothing
    }

    /// The hotkey: hide when in front, else show in front, with a fresh
    /// tab if the last one closed.
    fn toggle_quake(&mut self) -> After {
        if self.host.quake.is_none() {
            return After::Nothing;
        }
        let hwnd = self.host.hwnd;
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        if visible && unsafe { GetForegroundWindow() } == hwnd {
            return After::Then(Box::new(quake::hide));
        }
        if self.tabs.is_empty() {
            self.new_tab(None);
        }
        let placed = self.settings.config.quake.clone();
        After::Then(Box::new(move |hwnd| quake::show(hwnd, &placed, true)))
    }

    fn wheel_msg(&mut self, wparam: WPARAM, lparam: LPARAM) {
        // Positive is away from the user.
        let delta = i32::from(((wparam.0 >> 16) & 0xFFFF) as u16 as i16);
        // lparam is in screen coordinates.
        let mut point = POINT {
            x: i32::from((lparam.0 & 0xFFFF) as u16 as i16),
            y: i32::from(((lparam.0 >> 16) & 0xFFFF) as u16 as i16),
        };
        let _ = unsafe { ScreenToClient(self.host.hwnd, &mut point) };
        self.wheel(f64::from(delta) / f64::from(WHEEL_DELTA), point.x, point.y);
    }

    fn mouse_msg(&mut self, msg: u32, lparam: LPARAM) {
        let action = match msg {
            WM_LBUTTONDOWN => MouseAction::Press(MouseButton::Left),
            WM_LBUTTONUP => MouseAction::Release(MouseButton::Left),
            WM_MBUTTONDOWN => MouseAction::Press(MouseButton::Middle),
            WM_MBUTTONUP => MouseAction::Release(MouseButton::Middle),
            WM_RBUTTONDOWN => MouseAction::Press(MouseButton::Right),
            WM_RBUTTONUP => MouseAction::Release(MouseButton::Right),
            _ => MouseAction::Move,
        };
        // Client coordinates; negative past the left or top edge while
        // captured.
        let x = i32::from((lparam.0 & 0xFFFF) as u16 as i16);
        let y = i32::from(((lparam.0 >> 16) & 0xFFFF) as u16 as i16);
        self.mouse(action, x, y);
    }

    fn char(&mut self, wparam: WPARAM) {
        if std::mem::take(&mut self.host.suppress_char) {
            return;
        }
        let unit = (wparam.0 & 0xFFFF) as u16;
        let code = match unit {
            0xD800..=0xDBFF => {
                self.host.high_surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => match self.host.high_surrogate.take() {
                Some(high) => {
                    0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00)
                }
                None => return,
            },
            _ => u32::from(unit),
        };
        let Some(c) = char::from_u32(code).filter(|c| !c.is_control()) else {
            return;
        };
        let (key, unshifted) = self
            .host
            .pending_key
            .take()
            .unwrap_or((Key::Unidentified, None));
        self.send_key(KeyInput {
            key,
            action: KeyAction::Press,
            // The character already reflects Shift and AltGr.
            mods: current_mods() & Mods::SHIFT,
            text: Some(c.to_string()),
            unshifted,
        });
    }
}

/// Put `text` on the clipboard, with Windows line endings.
fn set_clipboard_text(hwnd: HWND, text: &str) -> windows::core::Result<()> {
    let wide: Vec<u16> = text
        .replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe { OpenClipboard(Some(hwnd))? };
    let result = (|| unsafe {
        EmptyClipboard()?;
        let global = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let ptr = GlobalLock(global).cast::<u16>();
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(windows::core::Error::from_thread());
        }
        // SAFETY: the block holds `wide.len()` u16s, freshly allocated.
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(global);
        // On success the clipboard owns the memory.
        if let Err(err) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))) {
            let _ = GlobalFree(Some(global));
            return Err(err);
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

/// The clipboard's text, if it has any and nobody else holds it open.
fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe { OpenClipboard(Some(hwnd)) }.ok()?;
    let text = (|| {
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
        let global = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(global) }.cast::<u16>();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: a locked CF_UNICODETEXT block holds GlobalSize bytes of
        // UTF-16, normally NUL-terminated; we stop at the NUL or the end.
        let units = unsafe { std::slice::from_raw_parts(ptr, GlobalSize(global) / 2) };
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = String::from_utf16_lossy(units.get(..end).unwrap_or_default());
        let _ = unsafe { GlobalUnlock(global) };
        Some(text)
    })();
    let _ = unsafe { CloseClipboard() };
    text
}

fn current_mods() -> Mods {
    let down = |vk: i32| unsafe { GetKeyState(vk) } < 0;
    let mut mods = Mods::empty();
    mods.set(Mods::SHIFT, down(0x10));
    mods.set(Mods::CTRL, down(0x11));
    mods.set(Mods::ALT, down(0x12));
    mods.set(Mods::SUPER, down(0x5B) || down(0x5C));
    mods
}

/// The key's character with no modifiers, per the active layout.
fn unshifted_char(vk: u16) -> Option<char> {
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_CHAR) } & 0x7FFF;
    char::from_u32(mapped)
        .filter(|c| !c.is_control() && *c != '\0')
        .map(|c| c.to_ascii_lowercase())
}

/// Have Windows ask for the client area again (`WM_NCCALCSIZE`).
fn frame_changed(hwnd: HWND) {
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    let _ = unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, flags) };
}

/// Move the window with the mouse, as dragging its title bar does.
fn move_window(hwnd: HWND) {
    /// `SC_MOVE | HTCAPTION`: move by the mouse rather than the keyboard.
    const SC_DRAGMOVE: usize = 0xF012;
    let _ = unsafe { ReleaseCapture() };
    unsafe { SendMessageW(hwnd, WM_SYSCOMMAND, Some(WPARAM(SC_DRAGMOVE)), None) };
}

/// Local `HH:MM` and `YYYY-MM-DD`.
fn local_time() -> (String, String) {
    let t = unsafe { GetLocalTime() };
    (
        format!("{:02}:{:02}", t.wHour, t.wMinute),
        format!("{}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay),
    )
}

/// Come to the front, restored if minimized.
fn raise(hwnd: HWND) {
    if unsafe { IsIconic(hwnd) }.as_bool() {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    }
    let _ = unsafe { SetForegroundWindow(hwnd) };
}

/// A thread posting `WM_APP_VBLANK` at each vblank of the window's monitor
/// while frames are wanted, parked otherwise.
struct Vblank {
    wanted: Arc<AtomicBool>,
    thread: Thread,
}

impl Vblank {
    fn start(hwnd: HWND) -> Option<Self> {
        let wanted = Arc::new(AtomicBool::new(false));
        // HWND isn't Send; posting to it from another thread is fine.
        let target = hwnd.0 as isize;
        let spawned = std::thread::Builder::new()
            .name("jet-vblank".into())
            .spawn({
                let wanted = Arc::clone(&wanted);
                move || pace(HWND(target as *mut c_void), &wanted)
            });
        match spawned {
            Ok(handle) => Some(Self {
                wanted,
                thread: handle.thread().clone(),
            }),
            Err(err) => {
                eprintln!("jet: no frame pacing thread: {err}");
                None
            }
        }
    }

    fn want(&self, on: bool) {
        self.wanted.store(on, Ordering::Release);
        if on {
            self.thread.unpark();
        }
    }
}

/// Wait for vblanks on whichever monitor `hwnd` is on, following it when it
/// moves; ends with the window.
fn pace(hwnd: HWND, wanted: &AtomicBool) {
    let mut output: Option<(HMONITOR, IDXGIOutput)> = None;
    loop {
        while !wanted.load(Ordering::Acquire) {
            std::thread::park();
        }
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        if output.as_ref().is_none_or(|(on, _)| *on != monitor) {
            output = output_of(monitor).map(|o| (monitor, o));
        }
        let waited = output
            .as_ref()
            .is_some_and(|(_, o)| unsafe { o.WaitForVBlank() }.is_ok());
        if !waited {
            // No output (display asleep, adapter reset): a 120 Hz-ish beat
            // until there is one again.
            output = None;
            std::thread::sleep(Duration::from_millis(8));
        }
        if unsafe { PostMessageW(Some(hwnd), WM_APP_VBLANK, WPARAM(0), LPARAM(0)) }.is_err() {
            return;
        }
    }
}

/// The DXGI output showing `monitor`.
fn output_of(monitor: HMONITOR) -> Option<IDXGIOutput> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    (0..)
        .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
        .flat_map(|adapter| (0..).map_while(move |j| unsafe { adapter.EnumOutputs(j) }.ok()))
        .find(|output| unsafe { output.GetDesc() }.is_ok_and(|d| d.Monitor == monitor))
}
