//! `jet quake` on X11 and macOS: one borderless window that shows and
//! hides, placed per `[quake]`. Running `jet quake` again toggles the
//! running one, so a desktop's own shortcut can drive it; macOS also gets
//! the global hotkey. Wayland can't hide, place or raise a window, so on
//! Linux this runs on X11 (XWayland under a Wayland desktop).

use jet_cli::transport::{self, Client};
use jet_cli::{Answer, Body, Request};
use jet_config::{Monitor, Quake};
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowLevel};

/// Whether a `jet quake` is running, asking it to toggle with `toggle`.
/// Not this jet: its main thread, which would answer, is the one asking.
/// And briefly: a stuck jet mustn't hang this one's start too.
pub fn signal_running(toggle: bool) -> bool {
    let Ok(running) = transport::running() else {
        return false;
    };
    let me = std::process::id();
    running
        .iter()
        .filter(|(pid, _)| *pid != me)
        .any(|(_, address)| {
            let Ok(mut client) = Client::connect(address) else {
                return false;
            };
            if client
                .set_timeout(std::time::Duration::from_secs(1))
                .is_err()
            {
                return false;
            }
            let is_quake = matches!(
                client.call(Request::Hello).map(|r| r.body),
                Ok(Body::Ok(Answer::Hello(hello))) if hello.quake
            );
            is_quake && (!toggle || client.call(Request::ToggleQuake).is_ok())
        })
}

/// Place per `quake` on its monitor (or with `!replace`, leave it where
/// it was) and bring to the front, focused.
pub fn show(window: &Window, quake: &Quake, replace: bool) {
    if replace {
        let monitor = match quake.monitor {
            Monitor::Cursor => under_cursor(window),
            Monitor::Primary => window.primary_monitor(),
        };
        place(window, quake, monitor);
    }
    window.set_window_level(if quake.always_on_top {
        WindowLevel::AlwaysOnTop
    } else {
        WindowLevel::Normal
    });
    window.set_visible(true);
    window.focus_window();
}

/// Out of sight. On macOS the app hides too, which hands the focus back
/// to the app that had it.
pub fn hide(window: &Window) {
    window.set_visible(false);
    #[cfg(target_os = "macos")]
    mac::hide_app();
}

/// Move and size per `quake` on `monitor`, or the window's own.
pub fn place(window: &Window, quake: &Quake, monitor: Option<MonitorHandle>) {
    let Some(monitor) = monitor.or_else(|| window.current_monitor()) else {
        return eprintln!("jet: can't find the monitor to show on");
    };
    let [x, y, w, h] = work_area(&monitor);
    let at = |start: f64, len: f64, fraction: f32| start + (len * f64::from(fraction)).round();
    let (left, top) = (at(x, w, quake.x), at(y, h, quake.y));
    let right = at(x, w, quake.x + quake.width);
    let bottom = at(y, h, quake.y + quake.height);
    let (position, size) = rect(left, top, (right - left).max(1.0), (bottom - top).max(1.0));
    // Sized first: AppKit keeps the bottom edge where it was on a resize.
    let _ = window.request_inner_size(size);
    window.set_outer_position(position);
}

/// The monitor's area less the menu bar and Dock: in points on macOS,
/// whose windows are placed in points across monitors of any scale.
#[cfg(target_os = "macos")]
fn work_area(monitor: &MonitorHandle) -> [f64; 4] {
    let scale = monitor.scale_factor();
    let at = monitor.position().to_logical::<f64>(scale);
    let size = monitor.size().to_logical::<f64>(scale);
    let [left, top, right, bottom] = mac::insets(monitor);
    [
        at.x + left,
        at.y + top,
        size.width - left - right,
        size.height - top - bottom,
    ]
}

/// The whole monitor, in pixels: X11 says nothing of panels per monitor.
#[cfg(not(target_os = "macos"))]
fn work_area(monitor: &MonitorHandle) -> [f64; 4] {
    let (at, size) = (monitor.position(), monitor.size());
    [
        f64::from(at.x),
        f64::from(at.y),
        f64::from(size.width),
        f64::from(size.height),
    ]
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> (Position, Size) {
    if cfg!(target_os = "macos") {
        (
            LogicalPosition::new(x, y).into(),
            LogicalSize::new(w, h).into(),
        )
    } else {
        (
            PhysicalPosition::new(x as i32, y as i32).into(),
            PhysicalSize::new(w as u32, h as u32).into(),
        )
    }
}

/// The monitor under the mouse. Only macOS says where the mouse is when
/// it's outside the window; elsewhere this is the window's monitor.
fn under_cursor(window: &Window) -> Option<MonitorHandle> {
    #[cfg(target_os = "macos")]
    if let Some((x, y)) = mac::cursor() {
        return window.available_monitors().find(|monitor| {
            let scale = monitor.scale_factor();
            let at = monitor.position().to_logical::<f64>(scale);
            let size = monitor.size().to_logical::<f64>(scale);
            (at.x..at.x + size.width).contains(&x) && (at.y..at.y + size.height).contains(&y)
        });
    }
    window.current_monitor()
}

/// The few AppKit, Core Graphics and Carbon calls winit doesn't make,
/// through their C interfaces.
#[cfg(target_os = "macos")]
pub mod mac {
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::null_mut;
    use std::sync::Arc;

    use jet_config::keybinds;
    use jet_core::{Key, Mods};
    use winit::monitor::MonitorHandle;
    use winit::platform::macos::MonitorHandleExtMacOS;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    type Id = *mut c_void;
    type OsStatus = i32;
    type Wake = Arc<dyn Fn() + Send + Sync>;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CgPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CgRect {
        origin: CgPoint,
        size: CgPoint,
    }

    #[repr(C)]
    struct EventTypeSpec {
        class: u32,
        kind: u32,
    }

    #[repr(C)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }

    type Handler = unsafe extern "C" fn(Id, Id, *mut c_void) -> OsStatus;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Id;
        fn objc_msgSend();
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: Id);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: Id) -> Id;
        fn CGEventGetLocation(event: Id) -> CgPoint;
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetApplicationEventTarget() -> Id;
        fn InstallEventHandler(
            target: Id,
            handler: Handler,
            count: usize,
            types: *const EventTypeSpec,
            data: *mut c_void,
            out: *mut Id,
        ) -> OsStatus;
        fn RemoveEventHandler(handler: Id) -> OsStatus;
        fn RegisterEventHotKey(
            code: u32,
            modifiers: u32,
            id: EventHotKeyId,
            target: Id,
            options: u32,
            out: *mut Id,
        ) -> OsStatus;
        fn UnregisterEventHotKey(hotkey: Id) -> OsStatus;
    }

    type AeHandler = unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> i16;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn AEInstallEventHandler(
            class: u32,
            id: u32,
            handler: AeHandler,
            refcon: *mut c_void,
            is_system: u8,
        ) -> i16;
    }

    /// `[receiver selector]`, for a method taking nothing and returning `R`.
    /// arm64 returns structs like CGRect in registers; x86_64 would need
    /// `objc_msgSend_stret` for them.
    unsafe fn send<R>(receiver: Id, selector: &CStr) -> R {
        // SAFETY: objc_msgSend takes the receiver and selector, then the
        // method's arguments, and returns what the method does.
        let send: unsafe extern "C" fn(Id, Id) -> R =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { send(receiver, sel_registerName(selector.as_ptr())) }
    }

    /// `[receiver selector:arg]`, for a method returning nothing.
    unsafe fn send_with<A>(receiver: Id, selector: &CStr, arg: A) {
        // SAFETY: as in `send`.
        let send: unsafe extern "C" fn(Id, Id, A) =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { send(receiver, sel_registerName(selector.as_ptr()), arg) }
    }

    /// Where the mouse is, in points from the main display's top left.
    pub fn cursor() -> Option<(f64, f64)> {
        let event = unsafe { CGEventCreate(null_mut()) };
        if event.is_null() {
            return None;
        }
        let at = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event) };
        Some((at.x, at.y))
    }

    /// The menu bar and Dock's share of `monitor`'s edges, in points:
    /// left, top, right, bottom.
    pub fn insets(monitor: &MonitorHandle) -> [f64; 4] {
        let Some(screen) = monitor.ns_screen().filter(|s| !s.is_null()) else {
            return [0.0; 4];
        };
        if cfg!(not(target_arch = "aarch64")) {
            return [0.0; 4];
        }
        // Both in AppKit's coordinates, whose y grows upward.
        let frame: CgRect = unsafe { send(screen, c"frame") };
        let visible: CgRect = unsafe { send(screen, c"visibleFrame") };
        let (f, v) = (frame, visible);
        [
            v.origin.x - f.origin.x,
            (f.origin.y + f.size.y) - (v.origin.y + v.size.y),
            (f.origin.x + f.size.x) - (v.origin.x + v.size.x),
            v.origin.y - f.origin.y,
        ]
    }

    /// On every Space, and over full-screen apps, so it drops down where
    /// the user is.
    pub fn join_all_spaces(window: &Window) {
        const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
        const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        let ns_window: Id = unsafe { send(handle.ns_view.as_ptr(), c"window") };
        if !ns_window.is_null() {
            let behavior = CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY;
            unsafe { send_with(ns_window, c"setCollectionBehavior:", behavior) };
        }
    }

    pub fn hide_app() {
        let class = unsafe { objc_getClass(c"NSApplication".as_ptr()) };
        if class.is_null() {
            return;
        }
        let app: Id = unsafe { send(class, c"sharedApplication") };
        if !app.is_null() {
            unsafe { send_with(app, c"hide:", null_mut::<c_void>()) };
        }
    }

    /// Have clicks on the Dock icon (the "reopen" Apple Event, which winit
    /// doesn't pass on) call `wake`, for the rest of the process. Replaces
    /// AppKit's handler, so call once the app has finished launching.
    pub fn on_dock_click(wake: Wake) {
        const CORE: u32 = u32::from_be_bytes(*b"aevt");
        const REOPEN: u32 = u32::from_be_bytes(*b"rapp");
        // Kept for as long as the handler is installed: the process.
        let wake = Box::into_raw(Box::new(wake));
        let status = unsafe { AEInstallEventHandler(CORE, REOPEN, reopened, wake.cast(), 0) };
        if status != 0 {
            drop(unsafe { Box::from_raw(wake) });
            eprintln!("jet: listening for Dock clicks failed ({status})");
        }
    }

    unsafe extern "C" fn reopened(_: *const c_void, _: *mut c_void, wake: *mut c_void) -> i16 {
        // SAFETY: the `Wake` on_dock_click boxed and never frees.
        let wake = unsafe { &*wake.cast::<Wake>() };
        wake();
        0
    }

    /// `quake.autostart`: have launchd run `jet quake --background` (this
    /// jet) at login, and start it now if it isn't running; or neither.
    pub fn autostart(on: bool) -> Result<(), String> {
        let home = std::env::var_os("HOME").ok_or("quake.autostart: HOME isn't set")?;
        let plist = std::path::Path::new(&home).join("Library/LaunchAgents/dev.jet.quake.plist");
        if !on {
            return match std::fs::remove_file(&plist) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(format!(
                    "quake.autostart: removing {}: {err}",
                    plist.display()
                )),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| format!("quake.autostart: {e}"))?;
        let exe = exe.to_string_lossy();
        if !super::signal_running(false) {
            crate::app::winit::run_detached(&exe, &["quake", crate::app::BACKGROUND]);
        }
        let escaped = exe
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by jet for [quake] autostart; removed when that's false. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.jet.quake</string>
  <key>ProgramArguments</key>
  <array>
    <string>{escaped}</string>
    <string>quake</string>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
            crate::app::BACKGROUND
        );
        // Rewritten only when it changes (another jet.app, say), as every
        // window does this.
        if std::fs::read_to_string(&plist).is_ok_and(|old| old == text) {
            return Ok(());
        }
        let write = || {
            if let Some(dir) = plist.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&plist, &text)
        };
        write().map_err(|err| format!("quake.autostart: writing {}: {err}", plist.display()))
    }

    /// A global hotkey through Carbon, which needs no Accessibility
    /// permission. Calls `wake` on the main thread when pressed.
    pub struct Hotkey {
        hotkey: Id,
        handler: Id,
        wake: *mut Wake,
    }

    impl Hotkey {
        pub fn register(chord: &str, wake: Wake) -> Result<Self, String> {
            const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
            const HOT_KEY_PRESSED: u32 = 5;
            let parsed = keybinds::parse_chord(chord)
                .ok_or_else(|| format!("quake.hotkey: {chord:?} isn't a key chord"))?;
            let code = key_code(parsed.key)
                .ok_or_else(|| format!("quake.hotkey: {chord:?} can't be a global hotkey"))?;
            let mut modifiers = 0;
            for (mod_, carbon) in [
                (Mods::SUPER, 1 << 8),
                (Mods::SHIFT, 1 << 9),
                (Mods::ALT, 1 << 11),
                (Mods::CTRL, 1 << 12),
            ] {
                if parsed.mods.contains(mod_) {
                    modifiers |= carbon;
                }
            }
            let wake = Box::into_raw(Box::new(wake));
            let target = unsafe { GetApplicationEventTarget() };
            let spec = EventTypeSpec {
                class: KEYBOARD,
                kind: HOT_KEY_PRESSED,
            };
            let mut handler = null_mut();
            let status = unsafe {
                InstallEventHandler(target, pressed, 1, &spec, wake.cast(), &mut handler)
            };
            if status != 0 {
                drop(unsafe { Box::from_raw(wake) });
                return Err(format!(
                    "quake.hotkey: listening for {chord} failed ({status})"
                ));
            }
            let id = EventHotKeyId {
                signature: u32::from_be_bytes(*b"jetq"),
                id: 1,
            };
            let mut hotkey = null_mut();
            let status =
                unsafe { RegisterEventHotKey(code, modifiers, id, target, 0, &mut hotkey) };
            if status != 0 {
                unsafe { RemoveEventHandler(handler) };
                drop(unsafe { Box::from_raw(wake) });
                return Err(format!("quake.hotkey: {chord} is taken by another app"));
            }
            Ok(Self {
                hotkey,
                handler,
                wake,
            })
        }
    }

    impl Drop for Hotkey {
        fn drop(&mut self) {
            unsafe {
                UnregisterEventHotKey(self.hotkey);
                RemoveEventHandler(self.handler);
                drop(Box::from_raw(self.wake));
            }
        }
    }

    unsafe extern "C" fn pressed(_: Id, _: Id, wake: *mut c_void) -> OsStatus {
        // SAFETY: the `Wake` Hotkey::register boxed, alive until the
        // handler is removed.
        let wake = unsafe { &*wake.cast::<Wake>() };
        wake();
        0
    }

    /// The Mac virtual key code (`kVK_*`) at `key`'s position.
    fn key_code(key: Key) -> Option<u32> {
        Some(match key {
            Key::A => 0x00,
            Key::S => 0x01,
            Key::D => 0x02,
            Key::F => 0x03,
            Key::H => 0x04,
            Key::G => 0x05,
            Key::Z => 0x06,
            Key::X => 0x07,
            Key::C => 0x08,
            Key::V => 0x09,
            Key::B => 0x0B,
            Key::Q => 0x0C,
            Key::W => 0x0D,
            Key::E => 0x0E,
            Key::R => 0x0F,
            Key::Y => 0x10,
            Key::T => 0x11,
            Key::Digit1 => 0x12,
            Key::Digit2 => 0x13,
            Key::Digit3 => 0x14,
            Key::Digit4 => 0x15,
            Key::Digit6 => 0x16,
            Key::Digit5 => 0x17,
            Key::Equal => 0x18,
            Key::Digit9 => 0x19,
            Key::Digit7 => 0x1A,
            Key::Minus => 0x1B,
            Key::Digit8 => 0x1C,
            Key::Digit0 => 0x1D,
            Key::BracketRight => 0x1E,
            Key::O => 0x1F,
            Key::U => 0x20,
            Key::BracketLeft => 0x21,
            Key::I => 0x22,
            Key::P => 0x23,
            Key::Enter => 0x24,
            Key::L => 0x25,
            Key::J => 0x26,
            Key::Quote => 0x27,
            Key::K => 0x28,
            Key::Semicolon => 0x29,
            Key::Backslash => 0x2A,
            Key::Comma => 0x2B,
            Key::Slash => 0x2C,
            Key::N => 0x2D,
            Key::M => 0x2E,
            Key::Period => 0x2F,
            Key::Tab => 0x30,
            Key::Space => 0x31,
            Key::Backquote => 0x32,
            Key::Backspace => 0x33,
            Key::Escape => 0x35,
            Key::F17 => 0x40,
            Key::F18 => 0x4F,
            Key::F19 => 0x50,
            Key::F20 => 0x5A,
            Key::F5 => 0x60,
            Key::F6 => 0x61,
            Key::F7 => 0x62,
            Key::F3 => 0x63,
            Key::F8 => 0x64,
            Key::F9 => 0x65,
            Key::F11 => 0x67,
            Key::F13 => 0x69,
            Key::F16 => 0x6A,
            Key::F14 => 0x6B,
            Key::F10 => 0x6D,
            Key::F12 => 0x6F,
            Key::F15 => 0x71,
            Key::Home => 0x73,
            Key::PageUp => 0x74,
            Key::Delete => 0x75,
            Key::F4 => 0x76,
            Key::End => 0x77,
            Key::F2 => 0x78,
            Key::PageDown => 0x79,
            Key::F1 => 0x7A,
            Key::ArrowLeft => 0x7B,
            Key::ArrowRight => 0x7C,
            Key::ArrowDown => 0x7D,
            Key::ArrowUp => 0x7E,
            _ => return None,
        })
    }
}
