//! A winit window on X11, Wayland or macOS, drawn through wgpu (Vulkan, GL
//! or Metal).
//!
//! winit owns the event loop; [`Handler`] owns the [`App`] and feeds it
//! window events, wakes from pane and `jet-cli` threads (as user events),
//! and the timers it asked for (as `WaitUntil` deadlines).

use std::collections::HashMap;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jet_config::{Config, Decorations};
use jet_core::{Key, KeyAction, KeyInput, Mods, MouseAction, MouseButton, Progress};
use jet_render::Gpu;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, Modifiers, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersKeyState, PhysicalKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{
    CursorIcon, ImePurpose, ResizeDirection, UserAttentionType, Window, WindowAttributes, WindowId,
};

use crate::app::{
    App, BoxError, ConfigFile, First, Host, Kind, Pointer, Start, Timer, Wake, WindowRequest,
};
use crate::chrome::Button;
use crate::layout::Rect;

mod quake;

/// Borderless windows resize from this close to an edge, in DIPs.
const EDGE_DIPS: f64 = 6.0;
/// A plain terminal's blink: X11 and Wayland have no system setting, and
/// macOS's isn't public.
const BLINK: Duration = Duration::from_millis(600);

/// A terminal window, or with `quake` the one that shows and hides (see
/// [`quake`]): toggling a running one instead, or with `background`,
/// leaving it be, and otherwise starting hidden.
pub fn run(quake: bool, background: bool, first: First) -> ExitCode {
    if quake && quake::signal_running(!background) {
        return ExitCode::SUCCESS;
    }
    match run_loop(quake, background, first) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jet: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The system's light or dark mode, where winit can tell (macOS).
fn appearance(theme: Option<winit::window::Theme>) -> jet_config::Appearance {
    match theme {
        Some(winit::window::Theme::Light) => jet_config::Appearance::Light,
        _ => jet_config::Appearance::Dark,
    }
}

fn run_loop(quake: bool, background: bool, first: First) -> Result<(), BoxError> {
    let settings = ConfigFile::load(quake.then_some(None));
    let start = Start::new(&settings.config, quake, first);
    let mut builder = EventLoop::<Wake>::with_user_event();
    #[cfg(target_os = "linux")]
    let quake = quake && {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let x11 = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
        if x11 {
            builder.with_x11();
        } else {
            eprintln!("jet: quake mode needs X11 or XWayland; opening a plain window");
        }
        x11
    };
    // No Dock icon or menu bar, unless it should show like other windows.
    #[cfg(target_os = "macos")]
    if quake && !settings.config.quake.taskbar {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build()?;
    let mut handler = Handler {
        proxy: event_loop.create_proxy(),
        settings: Some(settings),
        start: Some(start),
        quake,
        background,
        app: None,
        error: None,
    };
    event_loop.run_app(&mut handler)?;
    handler.error.map_or(Ok(()), Err)
}

/// Wakes from other threads, each sent once until the window handles it:
/// a busy pane shouldn't flood the event loop.
struct Wakes {
    proxy: Mutex<EventLoopProxy<Wake>>,
    panes: AtomicBool,
    cli: AtomicBool,
    quake: AtomicBool,
}

impl Wakes {
    fn flag(&self, wake: Wake) -> &AtomicBool {
        match wake {
            Wake::Panes => &self.panes,
            Wake::Cli => &self.cli,
            Wake::Quake => &self.quake,
        }
    }
}

/// The window and what winit leaves to the app: timers, the modifier
/// state, the clipboard.
pub struct Winit {
    window: Arc<Window>,
    wakes: Arc<Wakes>,
    /// Each running timer's period and next deadline.
    timers: HashMap<Timer, (Duration, Instant)>,
    clipboard: Option<arboard::Clipboard>,
    mods: Mods,
    /// Where the mouse last was, in client pixels.
    cursor: (i32, i32),
    /// Asked to close; the handler drops the app once the event is done.
    closing: bool,
    /// The title bar changed hands: lay out again.
    reframe: bool,
    /// This is `jet quake`'s window.
    quake: bool,
    /// The user dragged or sized the quake window: it comes back where
    /// they left it, this session, instead of per `[quake]`.
    moved: bool,
    #[cfg(target_os = "macos")]
    hotkey: Option<quake::mac::Hotkey>,
}

impl Winit {
    /// Take `quake.hotkey`, dropping the previous one; why not if it can't
    /// be. Only macOS has one: Linux desktops bind a key to `jet quake`.
    fn register_hotkey(&mut self, quake: &jet_config::Quake) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            self.hotkey = None;
            // One window and one chord here: the first of each.
            let chord = quake.hotkey.0.first()?;
            match quake::mac::Hotkey::register(chord, self.waker(Wake::Quake)) {
                Ok(hotkey) => self.hotkey = Some(hotkey),
                Err(problem) => return Some(problem),
            }
        }
        let _ = quake;
        None
    }
}

impl Host for Winit {
    fn waker(&self, wake: Wake) -> Arc<dyn Fn() + Send + Sync> {
        let wakes = Arc::clone(&self.wakes);
        Arc::new(move || {
            if !wakes.flag(wake).swap(true, Ordering::AcqRel)
                && let Ok(proxy) = wakes.proxy.lock()
            {
                // Fails only once the loop is gone, and then nobody's waiting.
                let _ = proxy.send_event(wake);
            }
        })
    }

    fn set_title(&mut self, title: &str) {
        self.window.set_title(title);
    }

    fn set_timer(&mut self, timer: Timer, every: Duration) {
        self.timers.insert(timer, (every, Instant::now() + every));
    }

    fn kill_timer(&mut self, timer: Timer) {
        self.timers.remove(&timer);
    }

    fn clipboard(&mut self) -> Option<String> {
        self.clipboard.as_mut()?.get_text().ok()
    }

    fn set_clipboard(&mut self, text: &str) -> Result<(), String> {
        let clipboard = self.clipboard.as_mut().ok_or("no clipboard")?;
        clipboard.set_text(text).map_err(|err| err.to_string())
    }

    fn maximized(&self) -> bool {
        self.window.is_maximized()
    }

    fn mods(&self) -> Mods {
        self.mods
    }

    fn blink_interval(&self) -> Option<Duration> {
        Some(BLINK)
    }

    // winit keeps sending a pressed button's moves to its window.
    fn capture(&mut self, _: bool) {}

    fn drag_threshold(&self) -> (i32, i32) {
        (4, 4)
    }

    fn request(&mut self, request: WindowRequest) {
        match request {
            // The quake window stays, for the next tab.
            WindowRequest::Close if self.quake => quake::hide(&self.window),
            WindowRequest::Close => self.closing = true,
            WindowRequest::Raise => self.window.focus_window(),
            WindowRequest::Move => {
                self.moved |= self.quake;
                if let Err(err) = self.window.drag_window() {
                    eprintln!("jet: moving the window: {err}");
                }
            }
            WindowRequest::Reframe => self.reframe = true,
            WindowRequest::Attention => self
                .window
                .request_user_attention(Some(UserAttentionType::Informational)),
        }
    }

    // No taskbar progress outside Windows yet.
    fn progress(&mut self, _: Progress) {}

    fn local_time(&self) -> (String, String) {
        local_time()
    }

    fn quake(&self) -> bool {
        self.quake
    }

    #[cfg(target_os = "macos")]
    fn autostart(&mut self, on: bool) {
        if let Err(err) = quake::mac::autostart(on) {
            eprintln!("jet: {err}");
        }
    }

    fn reconfigure_quake(&mut self, config: &jet_config::Quake) -> Option<String> {
        let problem = self.register_hotkey(config);
        self.moved = false;
        if self.window.is_visible().unwrap_or(false) {
            quake::place(&self.window, config, self.window.current_monitor());
        }
        problem
    }

    fn style_frame(&mut self, config: &Config) {
        let decorations = if self.quake {
            Decorations::None
        } else {
            config.window.decorations
        };
        self.window.set_decorations(system_frame(decorations));
    }

    /// macOS: the default text editor, as `.toml` may have no app.
    fn open_file(&mut self, path: &Path) {
        let path = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            run_detached("open", &["-t", &path]);
        } else {
            run_detached("xdg-open", &[&path]);
        }
    }

    fn open_url(&mut self, url: &str) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        run_detached(opener, &[url]);
    }

    /// Through `osascript` on macOS (shown as Script Editor's until jet
    /// has a notification API of its own) and `notify-send` elsewhere.
    fn notify(&mut self, title: &str, body: &str) {
        let title = if title.is_empty() { "jet" } else { title };
        if cfg!(target_os = "macos") {
            // AppleScript strings: backslash and quote escaped.
            let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                quote(body),
                quote(title)
            );
            run_detached("osascript", &["-e", &script]);
        } else {
            run_detached("notify-send", &["--app-name=jet", "--", title, body]);
        }
    }

    fn ime_area(&mut self, area: Rect) {
        self.window.set_ime_cursor_area(
            PhysicalPosition::new(area.x, area.y),
            PhysicalSize::new(area.w, area.h),
        );
    }
}

struct Handler {
    proxy: EventLoopProxy<Wake>,
    /// Loaded before the loop, for the window to take.
    settings: Option<ConfigFile>,
    /// Readied before the loop for the window to take.
    start: Option<Start>,
    quake: bool,
    /// `jet quake --background`: hidden until the hotkey.
    background: bool,
    app: Option<App<Winit>>,
    /// Why the window couldn't open, for `run` to report.
    error: Option<BoxError>,
}

impl Handler {
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<App<Winit>, BoxError> {
        let settings = self
            .settings
            .take()
            .unwrap_or_else(|| ConfigFile::load(None));
        let quake = self.quake;
        // The quake window is borderless, and shown once placed.
        let decorations = if quake {
            Decorations::None
        } else {
            settings.config.window.decorations
        };
        // On macOS a transparent window's own title bar goes see-through
        // too, so only when jet draws that bar or the window is to be.
        let transparent = !cfg!(target_os = "macos")
            || decorations != Decorations::System
            || settings.config.window.opacity < 1.0;
        let attributes = WindowAttributes::default()
            .with_title(if quake { "jet quake" } else { "jet" })
            .with_transparent(transparent)
            .with_visible(!quake)
            .with_decorations(system_frame(decorations));
        // jet's title bar under the system's buttons, set once: AppKit's
        // title bar can't change style after the window opens.
        #[cfg(target_os = "macos")]
        let attributes = attributes
            .with_titlebar_transparent(decorations == Decorations::Custom)
            .with_title_hidden(decorations == Decorations::Custom)
            .with_fullsize_content_view(decorations == Decorations::Custom)
            // Left Option is Alt (Meta) for the shell; right Option types
            // what the layout composes.
            .with_option_as_alt(OptionAsAlt::OnlyLeft);
        let window = Arc::new(event_loop.create_window(attributes)?);
        window.set_ime_allowed(true);
        window.set_ime_purpose(ImePurpose::Terminal);
        // The swapchain kind is DX12's; Vulkan has alpha either way.
        let instance = Gpu::instance(true);
        let surface = instance.create_surface(Arc::clone(&window))?;
        let host = Winit {
            window: Arc::clone(&window),
            wakes: Arc::new(Wakes {
                proxy: Mutex::new(self.proxy.clone()),
                panes: AtomicBool::new(false),
                cli: AtomicBool::new(false),
                quake: AtomicBool::new(false),
            }),
            timers: HashMap::new(),
            clipboard: arboard::Clipboard::new()
                .inspect_err(|err| eprintln!("jet: no clipboard: {err}"))
                .ok(),
            mods: Mods::empty(),
            cursor: (0, 0),
            closing: false,
            reframe: false,
            quake,
            moved: false,
            #[cfg(target_os = "macos")]
            hotkey: None,
        };
        let dpi = dpi_of(window.scale_factor());
        let focused = window.has_focus();
        let mut app = App::new(
            host,
            &instance,
            surface,
            dpi,
            focused,
            settings,
            self.start.take().unwrap_or_default(),
        )?;
        app.set_appearance(appearance(event_loop.system_theme()));
        if quake {
            #[cfg(target_os = "macos")]
            {
                quake::mac::join_all_spaces(&window);
                // With a Dock icon, clicking it toggles, as the hotkey.
                quake::mac::on_dock_click(app.host.waker(Wake::Quake));
            }
            let placed = app.settings.config.quake.clone();
            app.hotkey_problem = app.host.register_hotkey(&placed);
            app.refresh_banner();
            // Shown at once, there being no tray icon to say it's running,
            // unless started to wait for the hotkey; from the loop, once
            // macOS has finished launching the app.
            if !self.background {
                (app.host.waker(Wake::Quake))();
            }
            return Ok(app);
        }
        let on = app.settings.config.quake.autostart;
        app.host.autostart(on);
        let (w, h) = app.client_size_for(app.first_grid())?;
        let _ = window.request_inner_size(PhysicalSize::new(w as u32, h as u32));
        let size = window.inner_size();
        if size.width > 0 && size.height > 0 {
            app.resize(size.width, size.height);
        }
        Ok(app)
    }

    /// Close once asked to, else lay out again if the frame changed.
    fn settle(&mut self, event_loop: &ActiveEventLoop) {
        let Some(app) = &mut self.app else {
            return;
        };
        if app.host.closing {
            // Dropping the app ends its shells.
            self.app = None;
            event_loop.exit();
            return;
        }
        if std::mem::take(&mut app.host.reframe) {
            app.relayout(false);
            app.render();
        }
    }
}

impl ApplicationHandler<Wake> for Handler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }
        match self.open(event_loop) {
            Ok(app) => self.app = Some(app),
            Err(err) => {
                self.error = Some(err);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, wake: Wake) {
        let Some(app) = &mut self.app else {
            return;
        };
        // Cleared first: a wake during the work below sends another.
        app.host.wakes.flag(wake).store(false, Ordering::Release);
        match wake {
            Wake::Panes => app.pump(),
            Wake::Cli => app.serve_cli(),
            Wake::Quake => app.toggle_quake(),
        }
        self.settle(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(app) = &mut self.app else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                if app.close_requested() {
                    app.host.closing = true;
                }
            }
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    app.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                app.dpi = dpi_of(scale_factor);
                app.rebuild_renderer();
                // Cell sizes changed even where the cell count doesn't.
                app.relayout(true);
                app.render();
            }
            WindowEvent::RedrawRequested => app.render(),
            WindowEvent::ThemeChanged(theme) => app.set_appearance(appearance(Some(theme))),
            WindowEvent::Focused(focused) => {
                // Alt+Tab mid-drag: the release goes elsewhere.
                if !focused {
                    app.lost_capture();
                }
                // X11 reports a window that was never focused as unfocused.
                let lost = app.focused && !focused;
                app.set_focus(focused);
                if lost && app.host.quake && app.settings.config.quake.hide_on_blur {
                    quake::hide(&app.host.window);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                app.host.mods = mods_of(&modifiers);
                // Holding or letting go of the link modifier over a URL.
                app.hover_link();
                app.show_pointer();
            }
            WindowEvent::Ime(Ime::Preedit(text, cursor)) => {
                app.set_preedit(text, cursor.map(|(start, _)| start));
            }
            WindowEvent::Ime(Ime::Commit(text)) => app.commit_text(text),
            WindowEvent::Ime(Ime::Disabled) => app.set_preedit(String::new(), None),
            WindowEvent::Ime(Ime::Enabled) => {}
            WindowEvent::KeyboardInput { event, .. } => app.key(&event),
            WindowEvent::CursorMoved { position, .. } => app.moved(position),
            WindowEvent::CursorLeft { .. } => app.mouse_left(),
            WindowEvent::MouseInput { state, button, .. } => app.button(state, button),
            WindowEvent::MouseWheel { delta, .. } => {
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(p) => {
                        p.y / f64::from(app.renderer.cell_metrics().height.max(1))
                    }
                };
                let (x, y) = app.host.cursor;
                app.wheel(notches, x, y);
            }
            _ => {}
        }
        self.settle(event_loop);
    }

    /// Also on macOS's Quit, after which the process ends without
    /// returning from the loop: drop the app now to end the shells and
    /// remove the socket.
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.app = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(app) = &mut self.app else {
            return;
        };
        let now = Instant::now();
        let due: Vec<Timer> = app
            .host
            .timers
            .iter_mut()
            .filter(|(_, (_, at))| *at <= now)
            .map(|(timer, (every, at))| {
                *at = now + *every;
                *timer
            })
            .collect();
        for timer in due {
            app.timer(timer);
        }
        self.settle(event_loop);
        let next = self
            .app
            .as_ref()
            .and_then(|app| app.host.timers.values().map(|(_, at)| *at).min());
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

impl App<Winit> {
    /// Hide when in front, else show in front, with a fresh tab if the
    /// last one closed.
    fn toggle_quake(&mut self) {
        if !self.host.quake {
            return;
        }
        let window = &self.host.window;
        if window.is_visible().unwrap_or(false) && window.has_focus() {
            return quake::hide(window);
        }
        if self.tabs.is_empty() {
            self.new_tab(None);
        }
        quake::show(
            &self.host.window,
            &self.settings.config.quake,
            !self.host.moved,
        );
    }

    /// The input method's uncommitted text changed (empty when it's done).
    fn set_preedit(&mut self, text: String, cursor: Option<usize>) {
        if text != self.preedit || cursor != self.preedit_cursor {
            self.preedit = text;
            self.preedit_cursor = cursor;
            self.render();
        }
    }

    /// Text the input method committed, typed into the focused pane.
    fn commit_text(&mut self, text: String) {
        self.set_preedit(String::new(), None);
        self.reset_blink();
        self.send_key(KeyInput {
            key: Key::Unidentified,
            action: KeyAction::Press,
            mods: Mods::empty(),
            text: Some(text),
            unshifted: None,
        });
    }

    fn key(&mut self, event: &KeyEvent) {
        // Like Windows, only presses reach the terminal.
        if event.state != ElementState::Pressed {
            return;
        }
        self.reset_blink();
        let mods = self.host.mods;
        let action = if event.repeat {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        };
        let text = event
            .text
            .as_ref()
            .map(|t| t.to_string())
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
        let Some((key, kind)) = key_of(event.physical_key) else {
            // Keys the table lacks (on layouts with more) still type.
            if let Some(text) = text {
                self.send_key(KeyInput {
                    key: Key::Unidentified,
                    action,
                    mods: mods & Mods::SHIFT,
                    text: Some(text),
                    unshifted: None,
                });
            }
            return;
        };
        if kind != Kind::Modifier && self.run_binding(key, mods) {
            return;
        }
        // Command chords are the app's on macOS; unbound ones do nothing.
        if cfg!(target_os = "macos") && mods.contains(Mods::SUPER) {
            return;
        }
        let unshifted = match event.key_without_modifiers() {
            winit::keyboard::Key::Character(c) => c.chars().next().map(|c| c.to_ascii_lowercase()),
            _ => None,
        };
        let plain = !mods.intersects(Mods::CTRL | Mods::ALT | Mods::SUPER);
        let text = match kind {
            Kind::Modifier => return,
            // The text already reflects Shift and the layout.
            Kind::Text if plain => text,
            Kind::Text | Kind::Functional => None,
        };
        let mods = if text.is_some() {
            mods & Mods::SHIFT
        } else {
            mods
        };
        self.send_key(KeyInput {
            key,
            action,
            mods,
            text,
            unshifted,
        });
    }

    fn moved(&mut self, position: PhysicalPosition<f64>) {
        let (x, y) = (position.x as i32, position.y as i32);
        self.host.cursor = (x, y);
        if self.decorations() == Decorations::Custom && self.buttons == 0 {
            let button = self.chrome().button_at(x, y);
            self.set_hover(button);
        }
        self.mouse(MouseAction::Move, x, y);
        self.show_pointer();
    }

    /// The mouse pointer for what's under it.
    fn show_pointer(&mut self) {
        let (x, y) = self.host.cursor;
        let icon = match (self.edge_at(x, y), self.pointer_at(x, y)) {
            (Some(direction), _) => resize_icon(direction),
            (None, Pointer::ResizeColumns) => CursorIcon::ColResize,
            (None, Pointer::ResizeRows) => CursorIcon::RowResize,
            (None, Pointer::Arrow) => CursorIcon::Default,
            (None, Pointer::Text) => CursorIcon::Text,
            (None, Pointer::Link) => CursorIcon::Pointer,
            (None, Pointer::Shape(shape)) => shape.name().parse().unwrap_or_default(),
        };
        self.host.window.set_cursor(icon);
    }

    fn button(&mut self, state: ElementState, button: winit::event::MouseButton) {
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return,
        };
        let (x, y) = self.host.cursor;
        let pressed = state == ElementState::Pressed;
        // What Windows does for borderless windows outside the client area:
        // edges resize, jet's buttons, and the title bar moves the window.
        if button == MouseButton::Left && self.buttons == 0 && self.drag.is_none() {
            if pressed && let Some(direction) = self.edge_at(x, y) {
                self.host.moved |= self.host.quake;
                if let Err(err) = self.host.window.drag_resize_window(direction) {
                    eprintln!("jet: resizing the window: {err}");
                }
                return;
            }
            if self.decorations() == Decorations::Custom {
                let chrome = self.chrome();
                if let Some(hit) = chrome.button_at(x, y) {
                    if pressed {
                        self.pressed = Some(hit);
                    } else if self.pressed.take() == Some(hit) {
                        self.window_button(hit);
                    }
                    return;
                }
                if pressed && chrome.caption.contains(x, y) && self.tab_at(x, y).is_none() {
                    self.host.request(WindowRequest::Move);
                    return;
                }
            }
        }
        self.pressed = None;
        let action = if pressed {
            MouseAction::Press(button)
        } else {
            MouseAction::Release(button)
        };
        self.mouse(action, x, y);
    }

    fn window_button(&mut self, button: Button) {
        let window = &self.host.window;
        match button {
            Button::Minimize => window.set_minimized(true),
            Button::Maximize => window.set_maximized(!window.is_maximized()),
            Button::Close => {
                if self.close_requested() {
                    self.host.closing = true;
                }
            }
        }
    }

    /// The edge a borderless window resizes from at client `(x, y)`.
    /// macOS resizes every window from its edges itself.
    fn edge_at(&self, x: i32, y: i32) -> Option<ResizeDirection> {
        let window = &self.host.window;
        // The quake window has no frame either, so sizes the same way.
        let framed = self.decorations() == Decorations::System && !self.host.quake;
        if cfg!(target_os = "macos") || framed || window.is_maximized() {
            return None;
        }
        let edge = (EDGE_DIPS * window.scale_factor()).round() as i32;
        let (w, h) = (self.config.width as i32, self.config.height as i32);
        let (left, right) = (x < edge, x >= w - edge);
        let (top, bottom) = (y < edge, y >= h - edge);
        Some(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return None,
        })
    }
}

/// Start `program` with `args` and forget it: reaped off the window
/// thread, since openers and notifiers exit quickly.
fn run_detached(program: &str, args: &[&str]) {
    let spawned = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("jet-detached".into())
                .spawn(move || child.wait());
        }
        Err(err) => eprintln!("jet: {program}: {err}"),
    }
}

fn resize_icon(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
        ResizeDirection::West => CursorIcon::WResize,
    }
}

fn dpi_of(scale: f64) -> u32 {
    ((scale * 96.0).round() as u32).max(96)
}

fn mods_of(modifiers: &Modifiers) -> Mods {
    let state = modifiers.state();
    let mut mods = Mods::empty();
    mods.set(Mods::SHIFT, state.shift_key());
    mods.set(Mods::CTRL, state.control_key());
    mods.set(Mods::ALT, state.alt_key());
    mods.set(Mods::SUPER, state.super_key());
    // Right Option alone composes text on macOS rather than being Alt.
    if cfg!(target_os = "macos")
        && modifiers.lalt_state() != ModifiersKeyState::Pressed
        && modifiers.ralt_state() == ModifiersKeyState::Pressed
    {
        mods.remove(Mods::ALT);
    }
    mods
}

/// Whether the system draws the window's frame. macOS keeps its frame and
/// buttons for jet's title bar too, with the title bar made see-through.
fn system_frame(decorations: Decorations) -> bool {
    if cfg!(target_os = "macos") {
        decorations != Decorations::None
    } else {
        decorations == Decorations::System
    }
}

/// The physical key as libghostty names it, and how it reaches the
/// terminal. Both use the W3C key code names.
fn key_of(key: PhysicalKey) -> Option<(Key, Kind)> {
    use Kind::{Functional, Modifier, Text};
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    macro_rules! same {
        ($kind:ident: $($name:ident)*) => {
            match code {
                $(KeyCode::$name => return Some((Key::$name, $kind)),)*
                _ => {}
            }
        };
    }
    same!(Text: Backquote Backslash BracketLeft BracketRight Comma Digit0 Digit1 Digit2
        Digit3 Digit4 Digit5 Digit6 Digit7 Digit8 Digit9 Equal IntlBackslash IntlRo IntlYen
        Minus Period Quote Semicolon Slash Space Numpad0 Numpad1 Numpad2 Numpad3 Numpad4
        Numpad5 Numpad6 Numpad7 Numpad8 Numpad9 NumpadAdd NumpadComma NumpadDecimal
        NumpadDivide NumpadEqual NumpadMultiply NumpadSubtract);
    same!(Functional: Backspace Tab Enter Escape Delete End Help Home Insert PageDown PageUp
        ArrowDown ArrowLeft ArrowRight ArrowUp NumpadEnter ContextMenu F1 F2 F3 F4 F5 F6 F7
        F8 F9 F10 F11 F12 F13 F14 F15 F16 F17 F18 F19 F20 F21 F22 F23 F24);
    same!(Modifier: AltLeft AltRight ControlLeft ControlRight ShiftLeft ShiftRight CapsLock
        NumLock);
    Some(match code {
        KeyCode::KeyA => (Key::A, Text),
        KeyCode::KeyB => (Key::B, Text),
        KeyCode::KeyC => (Key::C, Text),
        KeyCode::KeyD => (Key::D, Text),
        KeyCode::KeyE => (Key::E, Text),
        KeyCode::KeyF => (Key::F, Text),
        KeyCode::KeyG => (Key::G, Text),
        KeyCode::KeyH => (Key::H, Text),
        KeyCode::KeyI => (Key::I, Text),
        KeyCode::KeyJ => (Key::J, Text),
        KeyCode::KeyK => (Key::K, Text),
        KeyCode::KeyL => (Key::L, Text),
        KeyCode::KeyM => (Key::M, Text),
        KeyCode::KeyN => (Key::N, Text),
        KeyCode::KeyO => (Key::O, Text),
        KeyCode::KeyP => (Key::P, Text),
        KeyCode::KeyQ => (Key::Q, Text),
        KeyCode::KeyR => (Key::R, Text),
        KeyCode::KeyS => (Key::S, Text),
        KeyCode::KeyT => (Key::T, Text),
        KeyCode::KeyU => (Key::U, Text),
        KeyCode::KeyV => (Key::V, Text),
        KeyCode::KeyW => (Key::W, Text),
        KeyCode::KeyX => (Key::X, Text),
        KeyCode::KeyY => (Key::Y, Text),
        KeyCode::KeyZ => (Key::Z, Text),
        KeyCode::SuperLeft => (Key::MetaLeft, Modifier),
        KeyCode::SuperRight => (Key::MetaRight, Modifier),
        _ => return None,
    })
}

/// Local `HH:MM` and `YYYY-MM-DD`, in the zone `TZ` or the system says.
fn local_time() -> (String, String) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let now = libc::time_t::try_from(now).unwrap_or_default();
    // SAFETY: all-zero is a valid `tm`; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return (String::new(), String::new());
    }
    (
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        format!(
            "{}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
    )
}

#[cfg(test)]
mod tests {
    use jet_core::Key;
    use winit::keyboard::{KeyCode, PhysicalKey};

    use crate::app::Kind;
    use crate::app::winit::key_of;

    #[test]
    fn keys_map_by_code() {
        let key = |code| key_of(PhysicalKey::Code(code));
        assert_eq!(key(KeyCode::KeyA), Some((Key::A, Kind::Text)));
        assert_eq!(key(KeyCode::Digit7), Some((Key::Digit7, Kind::Text)));
        assert_eq!(key(KeyCode::F24), Some((Key::F24, Kind::Functional)));
        assert_eq!(
            key(KeyCode::SuperLeft),
            Some((Key::MetaLeft, Kind::Modifier))
        );
        assert_eq!(key(KeyCode::MediaPlayPause), None);
    }
}
