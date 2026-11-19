//! The terminal window, whatever draws it: tabs of split panes, jet's own
//! title and tab bars, the status bar and `jet-cli`. The window system
//! (Win32, or winit on Linux and macOS) is behind [`Host`]; its backend turns native
//! events into calls here.

mod jetmux;
#[cfg(windows)]
pub mod win32;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod winit;

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use jet_cli::transport::{self as cli_transport, Incoming, Message};
use jet_cli::{
    Answer, Body, Done, Event, Events, Failure, Hello, Listing, Opened, PaneInfo, Request, TabInfo,
    Text, What,
};
use jet_config::keybinds::{self, Action, Binding, Direction, SplitTarget, TabColor};
use jet_config::plumb;
use jet_config::status::{Shown, StatusPosition, mix};
use jet_config::{Bell, Config, Decorations, Icon, TabBarPosition, TabBarShow, TabColorStyle};
use jet_core::{
    Cell, CellFlags, CursorShape, Key, KeyInput, Line, Location, Mods, MouseAction, MouseButton,
    MouseInput, Occasion, Progress, Rgb, Snapshot, Underline,
};

use crate::pane::{Pane, PaneEvent, PaneOptions};
use jet_font::Fonts;
use jet_pty::{Command, Size};
use jet_render::{Gpu, Grid, Image, ImageId, Quad, Renderer};

use crate::chrome::{self, BUTTONS, Button, Chrome, StatusAt};
use crate::icon;
use crate::integration;
use crate::layout::{self, Divider, Node, PaneId, Rect};
use crate::overlay::{
    self, Choice, Closing, Command as PaletteCommand, Find, Outcome, Overlay, Palette, Rename,
};
use crate::session::{self, Session};
use crate::status;

/// jet's title bar and window buttons, in DIPs, as Windows 11 draws its own.
const CAPTION_DIPS: f32 = 32.0;
const BUTTON_DIPS: f32 = 46.0;
/// Title bar left free of tabs however many there are, to drag by.
const DRAG_GAP_DIPS: f32 = 64.0;
/// macOS draws its close, minimize and zoom buttons over jet's title bar,
/// at its start; jet draws none.
const SYSTEM_BUTTONS: bool = cfg!(target_os = "macos");
/// Held to underline and click URLs: Command on macOS, as there Ctrl+click
/// is a right click.
const LINK_MODS: Mods = if cfg!(target_os = "macos") {
    Mods::SUPER
} else {
    Mods::CTRL
};
const TRAFFIC_LIGHTS_DIPS: f32 = 76.0;
/// jetmux tabs' icon when their profile has none: a split pane.
pub(crate) const JETMUX_ICON: &str = r##"<svg viewBox="0 0 24 24"><g fill="none" stroke="#7f9fd0" stroke-width="2.2"><rect x="2.5" y="4" width="19" height="16" rx="2.5"/><path d="M12 4v16M12 12h9.5"/></g></svg>"##;
const CLOSE_HOVER: Rgb = Rgb {
    r: 0xc4,
    g: 0x2b,
    b: 0x1c,
};
/// Renderer id of jet's title bar.
const CAPTION: u64 = u64::MAX - 1;
/// How often the config file is checked for edits.
const CONFIG_EVERY: Duration = Duration::from_secs(1);
/// How often the session is saved, when it changed: closing the window
/// saves it too, but logging out or a crash doesn't close it.
const SESSION_EVERY: Duration = Duration::from_secs(10);
/// Selection autoscroll speed while dragging past an edge.
const AUTOSCROLL_EVERY: Duration = Duration::from_millis(40);
/// The visual bell: a faint white wash this long, at most this often.
const FLASH: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
const FLASH_FOR: Duration = Duration::from_millis(90);
const BELL_GAP: Duration = Duration::from_millis(250);
/// Renderer id of the tab bar's grid; panes count up from 1.
const TAB_BAR: u64 = u64::MAX;
const STATUS: u64 = u64::MAX - 2;
/// Smallest size the font size keys go to, in points.
const MIN_FONT_PT: f32 = 4.0;
/// How far from a divider, in gaps, still grabs it.
const DIVIDER_SLOP: i32 = 4;

type BoxError = Box<dyn Error>;

/// What the app asks of the window system.
trait Host {
    /// Something other threads call when a pane has output, or a `jet-cli`
    /// request is waiting: it should lead to [`App::pump`] or
    /// [`App::serve_cli`] on the window's thread.
    fn waker(&self, wake: Wake) -> Arc<dyn Fn() + Send + Sync>;
    fn set_title(&mut self, title: &str);
    /// Call [`App::timer`] every `every` until killed; setting it again
    /// restarts it.
    fn set_timer(&mut self, timer: Timer, every: Duration);
    fn kill_timer(&mut self, timer: Timer);
    fn clipboard(&mut self) -> Option<String>;
    fn set_clipboard(&mut self, text: &str) -> Result<(), String>;
    fn maximized(&self) -> bool;
    /// Modifiers held now.
    fn mods(&self) -> Mods;
    /// The cursor's blink half-period; `None` when the system says not to.
    fn blink_interval(&self) -> Option<Duration>;
    /// Keep getting the mouse while a button is down outside the window.
    fn capture(&mut self, on: bool);
    /// How far the mouse moves before a press becomes a drag.
    fn drag_threshold(&self) -> (i32, i32);
    /// Window-level work, which may run after the current event.
    fn request(&mut self, request: WindowRequest);
    fn progress(&mut self, progress: Progress);
    /// Local `HH:MM` and `YYYY-MM-DD`.
    fn local_time(&self) -> (String, String);
    /// Whether frames should be paced by [`App::vblank`] calls: when the
    /// swapchain's own vsync is on the wrong clock.
    fn paces_frames(&self) -> bool {
        false
    }
    /// Start or stop calling [`App::vblank`] at the monitor's vblanks.
    fn want_vblank(&mut self, _on: bool) {}
    /// Whether this is the quake window.
    fn quake(&self) -> bool;
    /// The quake settings changed, or the window is to go back to them:
    /// re-register the hotkey, re-place the window where they put it
    /// (forgetting where it was dragged). Why the hotkey can't be had, if
    /// not.
    fn reconfigure_quake(&mut self, quake: &jet_config::Quake) -> Option<String>;
    /// `quake.autostart` is `on`: run `jet quake` at sign-in and now, or
    /// stop running it at sign-in.
    fn autostart(&mut self, _on: bool) {}
    /// Open a text file in the system's editor for it.
    fn open_file(&mut self, path: &Path);
    /// The system's alert sound, where there is one.
    fn beep(&mut self) {}
    /// Caption, border, corners and backdrop per the config.
    fn style_frame(&mut self, config: &Config);
    /// Say when the mouse leaves the window, which [`App::mouse_left`]
    /// hears.
    fn track_leave(&mut self) {}
    /// Open an http(s) URL in the browser.
    fn open_url(&mut self, url: &str);
    /// Where the cursor is, in client pixels, for the input method's
    /// composition and candidate windows.
    fn ime_area(&mut self, area: Rect);
    /// A desktop notification; `title` may be empty.
    fn notify(&mut self, title: &str, body: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Wake {
    Panes,
    Cli,
    /// Show or hide the quake window.
    Quake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Timer {
    Blink,
    Autoscroll,
    Flash,
    Config,
}

/// Window-level work for [`Host::request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowRequest {
    Close,
    /// To the front, for `jet-cli activate`.
    Raise,
    /// Follow the mouse with the window, as dragging its title bar does.
    Move,
    /// The title bar changed hands: recompute the client area.
    Reframe,
    /// Flash the taskbar until looked at.
    Attention,
}

/// How a key reaches the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Produces text, which the platform's keyboard layout works out (with
    /// AltGr and dead keys). Ctrl or Alt combos are encoded as keys.
    Text,
    /// Never text (arrows, F-keys, Enter, ...): always encoded as a key.
    Functional,
    /// Shift, Ctrl, Alt, Win on their own.
    Modifier,
}

/// What the mouse shows over a spot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pointer {
    Text,
    /// Over a URL with the link modifier held.
    Link,
    Arrow,
    ResizeColumns,
    ResizeRows,
    /// What the program in the pane asked for (OSC 22).
    Shape(jet_core::PointerShape),
}

/// The settings in effect and the files they came from.
struct ConfigFile {
    dirs: jet_config::Dirs,
    /// Files whose edits mean a reload, and their last modification times.
    watched: Vec<PathBuf>,
    stamps: Vec<Option<SystemTime>>,
    config: Config,
    /// Why the files on disk didn't load, shown in the window.
    error: Option<String>,
}

impl ConfigFile {
    fn load() -> Self {
        let mut file = Self {
            dirs: jet_config::Dirs::from_env(),
            watched: Vec::new(),
            stamps: Vec::new(),
            config: Config::default(),
            error: None,
        };
        if let Some(config) = file.read() {
            file.config = config;
        }
        file
    }

    /// The new config if any watched file changed, appeared or went away.
    /// One that doesn't load keeps the config in effect.
    fn reload(&mut self, force: bool) -> Option<Config> {
        if !force && jet_config::stamps(&self.watched) == self.stamps {
            return None;
        }
        self.read()
    }

    /// Load and remember what to watch: also on failure, so a broken file
    /// reports once per save.
    fn read(&mut self) -> Option<Config> {
        let result = jet_config::load(&self.dirs);
        // A broken include doesn't list its sources: keep watching the last
        // good set so fixing it reloads.
        let extra = match &result {
            Ok(loaded) => [loaded.sources.as_slice(), &loaded.absent].concat(),
            Err(_) => std::mem::take(&mut self.watched),
        };
        self.watched = self.dirs.watched();
        for path in extra {
            if !self.watched.contains(&path) {
                self.watched.push(path);
            }
        }
        self.stamps = jet_config::stamps(&self.watched);
        match result {
            Ok(loaded) => {
                for source in &loaded.sources {
                    eprintln!("jet: config from {}", source.display());
                }
                for warning in &loaded.warnings {
                    eprintln!("jet: {warning}");
                }
                self.error = None;
                Some(loaded.config)
            }
            Err(err) => {
                eprintln!("jet: {err}");
                self.error = Some(err.to_string());
                None
            }
        }
    }
}

/// One shell and what was last drawn of it.
struct PaneSlot {
    pane: Pane,
    snapshot: Option<Snapshot>,
    title: String,
    /// From its profile, shown before the title in the tab bar.
    icon: Option<Icon>,
    progress: Progress,
    /// Where the shell last said it is (OSC 7 and friends).
    cwd: Option<PathBuf>,
    /// The profile it was opened with; `None` is the default shell.
    profile: Option<String>,
    /// The `[hosts]` rule its host matches, whose look it has.
    host_rule: Option<String>,
    /// The same report, remote or not, and when it came.
    location: Option<Location>,
    location_at: Instant,
    /// User variables the shell set (OSC 1337 SetUserVar).
    vars: HashMap<String, String>,
    /// The last command's exit status (OSC 133;D).
    exit: Option<i32>,
    /// The program in front and its working directory, looked up while
    /// it's focused and the status shows.
    front: Option<Front>,
    /// The mouse pointer its program asked for (OSC 22).
    pointer: jet_core::PointerShape,
    /// The innermost context its programs announced (OSC 3008).
    context: Option<jet_core::Context>,
    front_cwd: Option<PathBuf>,
    /// Area in client pixels, padding included.
    rect: Rect,
    grid: (u16, u16),
}

struct Front {
    process: jet_pty::Process,
    /// Where to, when it's `ssh`.
    ssh: Option<jet_core::Ssh>,
}

/// What a Ctrl+click on some text would do.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    pane: PaneId,
    /// Underlined while hovered.
    cells: Vec<(u16, std::ops::Range<u16>)>,
    what: Clicked,
}

#[derive(Clone, Debug, PartialEq)]
enum Clicked {
    Url(String),
    /// A `[plumb]` rule matched.
    Plumb(plumb::Hit),
}

/// What a `jet-cli` request asks to run in a new pane.
struct Opening {
    cwd: Option<PathBuf>,
    profile: Option<String>,
    command: Option<jet_config::Command>,
}

/// A tab: a split tree and which of its panes has the keyboard.
struct Tab {
    /// Fixed for the tab's life, for the CLI.
    id: u64,
    root: Node,
    focused: PaneId,
    /// Set from the CLI over what the shell says and the profile's icon.
    title: Option<String>,
    icon: Option<Icon>,
    /// Over its profile's or host's tab color: from `set_tab_color`.
    color: Option<Rgb>,
    /// The focused split fills the tab; the others are hidden.
    zoomed: bool,
    /// A jetmux session's tab, which the daemon lays out.
    remote: Option<jetmux::RemoteTab>,
}

impl Tab {
    fn new(id: u64, pane: PaneId) -> Self {
        Self {
            id,
            root: Node::Leaf(pane),
            focused: pane,
            title: None,
            icon: None,
            color: None,
            zoomed: false,
            remote: None,
        }
    }

    /// The panes that show: all of them, or the zoomed one.
    fn shown(&self) -> Vec<PaneId> {
        if self.zoomed {
            vec![self.focused]
        } else {
            self.root.panes()
        }
    }
}

struct App<H: Host> {
    host: H,
    settings: ConfigFile,
    bindings: Vec<Binding>,
    gpu: Gpu,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    renderer: Renderer,
    /// Points added by the font size keys.
    font_delta: f32,
    /// Why the configured font couldn't be used.
    font_problem: Option<String>,
    /// Why the quake hotkey couldn't be registered.
    hotkey_problem: Option<String>,
    /// Problem rows drawn over the top of the focused pane, until fixed or
    /// clicked.
    banner: Vec<Arc<Line>>,
    banner_dismissed: bool,
    /// The release of a click the app took (banner, tab bar).
    swallow_release: bool,
    /// Where the left button went down on a tab: moving far enough from
    /// there drags the window instead.
    tab_press: Option<(i32, i32)>,
    panes: HashMap<PaneId, PaneSlot>,
    tabs: Vec<Tab>,
    active: usize,
    next_pane: PaneId,
    /// The tab bar's and title's rows, kept while unchanged so their frames
    /// stay cached.
    tab_bar: Vec<Arc<Line>>,
    caption_line: Vec<Arc<Line>>,
    /// jet's window button under the mouse, and the one pressed.
    hover: Option<Button>,
    pressed: Option<Button>,
    /// The tab under the mouse, and whether it's over that tab's close
    /// button.
    tab_hover: Option<(usize, bool)>,
    /// Requests from `jet-cli`, and who wants to hear about events.
    cli: Option<Receiver<Incoming>>,
    socket: String,
    subscribers: Vec<(Events, Sender<Message>)>,
    next_tab: u64,
    /// Status segments as last filled in, and their rows as drawn.
    status: Vec<Shown>,
    status_lines: Vec<Arc<Line>>,
    /// When the focused pane's program was last looked up.
    polled: Instant,
    /// The session as last saved, and when that was checked.
    saved: Option<Session>,
    saved_at: Instant,
    local_user: String,
    local_host: String,
    /// Image icons in the tab bar as last built: cell, row, file.
    icon_spots: Vec<(usize, usize, Icon)>,
    /// Loaded icon files; cleared with the renderer, whose atlas holds them.
    icons: HashMap<Icon, Option<ImageId>>,
    dpi: u32,
    /// Wheel movement not yet a whole notch (touchpads send less).
    wheel_rest: f64,
    focused: bool,
    /// Blink phase: the cursor is showing.
    blink_on: bool,
    blink_timer: bool,
    /// Buttons we hold capture for, and the pane they went to.
    buttons: u8,
    mouse_pane: Option<PaneId>,
    /// The divider being dragged, by its index in the active tab's layout.
    drag: Option<(usize, Divider)>,
    /// Last move sent, in half cells, so moves within one aren't resent.
    last_move: Option<(PaneId, i32, i32)>,
    /// Where the mouse last was, in client pixels.
    pointer: (i32, i32),
    /// The URL or plumbable text under the mouse while the link modifier
    /// is held.
    link: Option<Target>,
    /// The input method's uncommitted text and its cursor (a byte offset),
    /// drawn at the focused pane's cursor. Win32's IME draws its own.
    preedit: String,
    preedit_cursor: Option<usize>,
    /// The cursor's last reported spot, for [`Host::ime_area`].
    ime_area: Option<Rect>,
    autoscroll_timer: bool,
    last_bell: Option<Instant>,
    /// The config from before the theme palette opened, to go back to.
    previewing: Option<Config>,
    /// The window may close though something's running: the user said so.
    close_confirmed: bool,
    /// Frames are paced by [`App::vblank`] (mailbox presents), rather than
    /// by blocking in vsync.
    paced: bool,
    /// Paced: a frame went out since the last vblank, so the next one waits
    /// for it, and one is owed.
    drew_since_vblank: bool,
    frame_pending: bool,
    /// The find bar or command palette, which takes the keys while open.
    overlay: Option<Overlay>,
    /// jetmux sessions shown here, by a key of jet's.
    remotes: HashMap<u64, jetmux::Attached>,
    next_remote: u64,
    /// Why the last jetmux session ended or couldn't start.
    remote_problem: Option<String>,
}

impl<H: Host> App<H> {
    /// The app for a window with `surface` at `dpi`, with a first tab.
    fn new(
        host: H,
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        dpi: u32,
        focused: bool,
        settings: ConfigFile,
        start: Start,
    ) -> Result<Self, BoxError> {
        let Start {
            early,
            session,
            first,
        } = start;
        let config_now = &settings.config;
        let grid = (config_now.window.columns, config_now.window.rows);
        let socket = cli_transport::address_for(std::process::id());
        // Making the device is most of startup (D3D12 takes ~270 ms), so
        // the shell starts meanwhile and its prompt is about ready for the
        // first frame.
        let first = match &early {
            Some(early) => First {
                profile: early.profile.clone(),
                ..First::default()
            },
            None => first,
        };
        let (gpu, (fonts, font_problem), pane) = std::thread::scope(|s| -> Result<_, BoxError> {
            let gpu = s.spawn(|| Gpu::new(instance, Some(&surface), false));
            let started = (|| -> Result<_, BoxError> {
                let fonts = load_fonts(dpi, config_now, config_now.font.size)?;
                let size = pty_size(grid, fonts.0.metrics())?;
                let wake = host.waker(Wake::Panes);
                let pane = match early {
                    Some(early) => {
                        early.pane.resize(size);
                        let _ = early.wake.set(Arc::clone(&wake));
                        // For what it said while nobody was listening.
                        wake();
                        early.pane
                    }
                    None => {
                        let env = pane_env(&socket, FIRST_PANE);
                        let (cwd, command) = first.launch(config_now);
                        let vt = config_now.profile_vt_settings(first.profile.as_deref());
                        spawn_shell(size, wake, config_now, vt, cwd, command.as_ref(), &env)?
                    }
                };
                Ok((fonts, pane))
            })();
            let gpu = gpu.join().map_err(|_| "GPU setup panicked")??;
            let (fonts, pane) = started?;
            Ok((gpu, fonts, pane))
        })?;
        let info = gpu.adapter.get_info();
        eprintln!("jet: rendering on {} ({:?})", info.name, info.backend);

        let mut config = surface
            .get_default_config(&gpu.adapter, 1, 1)
            .ok_or("surface not supported by adapter")?;
        // Opaque pixels look the same either way; this lets the rest show
        // what's behind.
        let alpha = wgpu::CompositeAlphaMode::PreMultiplied;
        if surface
            .get_capabilities(&gpu.adapter)
            .alpha_modes
            .contains(&alpha)
        {
            config.alpha_mode = alpha;
        }
        // Draw through a non-sRGB view: colors are blended as given (see jet-render).
        let view_format = config.format.remove_srgb_suffix();
        config.view_formats = vec![view_format];
        // Paced windows present without blocking, and the newest frame
        // wins at each vblank (mailbox). Low latency doesn't wait for the
        // vblank at all (immediate): a keystroke's frame goes straight out.
        let modes = surface.get_capabilities(&gpu.adapter).present_modes;
        let immediate =
            settings.config.window.low_latency && modes.contains(&wgpu::PresentMode::Immediate);
        let paced =
            host.paces_frames() && (immediate || modes.contains(&wgpu::PresentMode::Mailbox));
        config.present_mode = match (paced, immediate) {
            (false, _) => wgpu::PresentMode::AutoVsync,
            (true, true) => wgpu::PresentMode::Immediate,
            (true, false) => wgpu::PresentMode::Mailbox,
        };
        config.desired_maximum_frame_latency = 1;
        // Usable before the first resize sizes it.
        surface.configure(&gpu.device, &config);

        let renderer = make_renderer(&gpu, view_format, dpi, config_now, fonts);
        let bindings = config_now.bindings().unwrap_or_default();
        let (local_user, local_host) = local_names();
        let mut app = Self {
            host,
            settings,
            bindings,
            gpu,
            surface,
            config,
            view_format,
            renderer,
            font_delta: 0.0,
            font_problem,
            hotkey_problem: None,
            banner: Vec::new(),
            banner_dismissed: false,
            swallow_release: false,
            tab_press: None,
            panes: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            next_pane: FIRST_PANE,
            tab_bar: Vec::new(),
            caption_line: Vec::new(),
            hover: None,
            pressed: None,
            tab_hover: None,
            cli: None,
            socket,
            subscribers: Vec::new(),
            next_tab: 1,
            status: Vec::new(),
            status_lines: Vec::new(),
            polled: Instant::now(),
            saved: None,
            saved_at: Instant::now(),
            local_user,
            local_host,
            icon_spots: Vec::new(),
            icons: HashMap::new(),
            dpi,
            wheel_rest: 0.0,
            focused,
            blink_on: true,
            blink_timer: false,
            buttons: 0,
            mouse_pane: None,
            drag: None,
            last_move: None,
            pointer: (0, 0),
            link: None,
            preedit: String::new(),
            preedit_cursor: None,
            ime_area: None,
            autoscroll_timer: false,
            last_bell: None,
            previewing: None,
            close_confirmed: false,
            paced,
            drew_since_vblank: false,
            frame_pending: false,
            overlay: None,
            remotes: HashMap::new(),
            next_remote: 0,
            remote_problem: None,
        };
        app.listen();
        let id = app.adopt(pane, grid, first.profile.as_deref());
        let tab = app.take_tab_id();
        app.tabs.push(Tab::new(tab, id));
        if let Some(session) = session {
            app.restore(session, id);
        }
        app.refresh_banner();
        app.host.style_frame(&app.settings.config);
        app.host.set_timer(Timer::Config, CONFIG_EVERY);
        Ok(app)
    }

    /// Client area size whose panes fit `grid` exactly.
    fn client_size_for(&self, grid: (u16, u16)) -> Result<(i32, i32), BoxError> {
        let (w, h) = self.renderer.pixel_size(grid.0, grid.1);
        let (w, h) = (i32::try_from(w)?, i32::try_from(h)?);
        // Room for the title and tab bars around the grid.
        let probe = self.chrome_for(w * 4, h * 4).panes;
        Ok((w + w * 4 - probe.w, h + h * 4 - probe.h))
    }

    /// A new shell in `cwd` (or `profile`'s), not yet in any tab.
    fn spawn(
        &mut self,
        grid: (u16, u16),
        cwd: Option<PathBuf>,
        name: Option<&str>,
        command: Option<jet_config::Command>,
    ) -> Result<PaneId, BoxError> {
        let config = &self.settings.config;
        let (cwd, profiled) = launch(config, cwd, name);
        let command = command.as_ref().or(profiled);
        let size = pty_size(grid, self.renderer.cell_metrics())?;
        let env = pane_env(&self.socket, self.next_pane);
        let wake = self.host.waker(Wake::Panes);
        let vt = config.profile_vt_settings(name);
        let pane = spawn_shell(size, wake, config, vt, cwd, command, &env)?;
        Ok(self.adopt(pane, grid, name))
    }

    /// Give `pane`, started for the next id (from `profile`), its slot.
    fn adopt(&mut self, pane: Pane, grid: (u16, u16), profile: Option<&str>) -> PaneId {
        let config = &self.settings.config;
        let icon = profile
            .and_then(|name| config.profiles.get(name))
            .map_or(&config.shell.icon, |p| &p.icon)
            .clone()
            // A session's tabs stand out from local ones.
            .or_else(|| pane.is_remote().then(|| Icon::Svg(JETMUX_ICON.into())));
        // A daemon's panes hear about focus as jet moves it.
        if self.focused && !pane.is_remote() {
            pane.focus(true);
        }
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.insert(
            id,
            PaneSlot {
                // Until the shell sets one: the bundled ConPTY never does.
                title: pane.program().to_owned(),
                pane,
                snapshot: None,
                icon,
                progress: Progress::Hidden,
                cwd: None,
                profile: profile.map(str::to_owned),
                host_rule: None,
                location: None,
                location_at: Instant::now(),
                vars: HashMap::new(),
                exit: None,
                front: None,
                front_cwd: None,
                pointer: jet_core::PointerShape::RESET,
                context: None,
                rect: Rect::default(),
                grid,
            },
        );
        id
    }

    /// The initial grid of the first pane.
    fn first_grid(&self) -> (u16, u16) {
        self.panes.values().next().map_or((80, 24), |p| p.grid)
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The pane with the keyboard.
    fn focused_id(&self) -> Option<PaneId> {
        self.tab().map(|t| t.focused)
    }

    fn focused_slot(&self) -> Option<&PaneSlot> {
        self.panes.get(&self.focused_id()?)
    }

    fn pump(&mut self) {
        self.pump_remotes();
        let focused = self.focused_id();
        let mut dirty = false;
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            while let Some(event) = self.panes.get(&id).and_then(|p| p.pane.try_event()) {
                match event {
                    PaneEvent::Title(title) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.title.clone_from(&title);
                        }
                        self.update_title();
                        self.emit(What::Title { title }, Some(id));
                        dirty = true;
                    }
                    PaneEvent::Cwd(cwd) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.cwd = Some(cwd);
                        }
                    }
                    PaneEvent::Location(location) => {
                        let what = location.as_ref().map(|l| What::Cwd {
                            path: l.path.clone(),
                            host: l.host.clone(),
                        });
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.location = location;
                            slot.location_at = Instant::now();
                        }
                        self.refresh_host(id);
                        if let Some(what) = what {
                            self.emit(what, Some(id));
                        }
                        dirty = true;
                    }
                    PaneEvent::UserVar(name, value) => {
                        self.set_var(id, name, value);
                        dirty = true;
                    }
                    PaneEvent::Progress(progress) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.progress = progress;
                        }
                        self.emit(progress_event(progress), Some(id));
                        dirty = true;
                    }
                    PaneEvent::Bell => {
                        self.bell();
                        self.emit(What::Bell, Some(id));
                    }
                    PaneEvent::Notify(note) => {
                        // Not for the pane you're looking at, unless asked:
                        // you can see it.
                        let watching = self.focused && focused == Some(id);
                        let show = match note.occasion {
                            Occasion::Always => true,
                            Occasion::Unwatched => !watching,
                            Occasion::Hidden => !self.focused,
                        };
                        if self.settings.config.terminal.notifications && show {
                            self.host.notify(&note.title, &note.body);
                        }
                        let (title, body) = (note.title, note.body);
                        self.emit(What::Notification { title, body }, Some(id));
                    }
                    PaneEvent::CommandFinished(status) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.exit = status;
                        }
                        self.emit(What::CommandFinished { status }, Some(id));
                        dirty = true;
                    }
                    PaneEvent::Pointer(shape) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            // Shown from the next mouse move.
                            slot.pointer = shape;
                        }
                    }
                    PaneEvent::Context(context) => {
                        if let Some(slot) = self.panes.get_mut(&id) {
                            slot.context = context;
                        }
                        self.refresh_host(id);
                        dirty = true;
                    }
                    PaneEvent::Copy(text) => {
                        if let Err(err) = self.host.set_clipboard(&text) {
                            eprintln!("jet: copying to the clipboard: {err}");
                        }
                    }
                    PaneEvent::Paste => self.paste(id),
                    PaneEvent::Exited(_) | PaneEvent::Failed(_) => {
                        if let PaneEvent::Failed(err) = &event {
                            eprintln!("jet: pane failed: {err}");
                        }
                        self.close_pane(id);
                        break;
                    }
                }
            }
            if !self.panes.contains_key(&id) {
                continue;
            }
            // Paced, with a frame already out this vblank: leave the panes
            // parsing, and take what they have at the next one.
            if self.paced && self.drew_since_vblank {
                self.frame_pending = true;
                continue;
            }
            dirty |= self.latch(id, focused);
        }
        if dirty {
            self.sync_blink();
            self.render();
        }
    }

    /// Take the newest snapshot `id` has parsed, if any; true when it did.
    fn latch(&mut self, id: PaneId, focused: Option<PaneId>) -> bool {
        let Some(slot) = self.panes.get_mut(&id) else {
            return false;
        };
        let Some(snapshot) = slot.pane.take_snapshot() else {
            return false;
        };
        let at = |s: &Snapshot| s.cursor.map(|c| (c.x, c.y));
        // A moving cursor (typing, output) shows solid, then blinks.
        let moved = slot.snapshot.as_ref().map(at) != Some(at(&snapshot));
        slot.snapshot = Some(snapshot);
        if moved && Some(id) == focused {
            self.reset_blink();
        }
        // The text under an underlined link may have moved.
        if self.link.as_ref().is_some_and(|t| t.pane == id) {
            self.link = self.link_at(self.pointer.0, self.pointer.1);
        }
        true
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.gpu.device, &self.config);
        self.relayout(false);
        // At once, paced or not: a frame at the old size would stretch.
        self.draw();
    }

    /// Who draws the title bar. The quake window has none.
    fn decorations(&self) -> Decorations {
        if self.host.quake() {
            Decorations::System
        } else {
            self.settings.config.window.decorations
        }
    }

    /// Where the title bar, tab bar and panes go in a `width` by `height`
    /// client area.
    fn chrome_for(&self, width: i32, height: i32) -> Chrome {
        let bar = &self.settings.config.tab_bar;
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let scale = self.dpi as f32 / 96.0;
        let caption = self.decorations() == Decorations::Custom;
        let row = m.height as i32 + pad;
        Chrome::new(chrome::Input {
            width,
            height,
            caption,
            tabs_shown: match bar.show {
                TabBarShow::Auto => self.tabs.len() > 1,
                TabBarShow::Always => true,
                TabBarShow::Never => false,
            },
            position: bar.position,
            // Windows' own caption is 32 DIPs; jet's isn't thinner.
            row: if caption {
                row.max((CAPTION_DIPS * scale).round() as i32)
            } else {
                row
            },
            side: i32::from(bar.width) * m.width as i32 + 2 * pad,
            button: if SYSTEM_BUTTONS {
                0
            } else {
                (BUTTON_DIPS * scale).round() as i32
            },
            left: if SYSTEM_BUTTONS {
                (TRAFFIC_LIGHTS_DIPS * scale).round() as i32
            } else {
                0
            },
            gap: (DRAG_GAP_DIPS * scale).round() as i32,
            status: self.status_at(),
            status_size: self.status_size(),
        })
    }

    fn chrome(&self) -> Chrome {
        self.chrome_for(self.config.width as i32, self.config.height as i32)
    }

    fn vertical_tabs(&self) -> bool {
        matches!(
            self.settings.config.tab_bar.position,
            TabBarPosition::Left | TabBarPosition::Right
        )
    }

    /// Place every tab's panes in the window and resize the ones whose grid
    /// changed (all of them with `force`).
    fn relayout(&mut self, force: bool) {
        let area = self.chrome().panes;
        let m = self.renderer.cell_metrics();
        let mut rects = Vec::new();
        for tab in &self.tabs {
            if let Some(remote) = tab.remote {
                rects.extend(self.remote_rects(remote, area));
                continue;
            }
            tab.root
                .layout(area, self.gap(), &mut rects, &mut Vec::new());
            // The rest keep their places, and sizes, for when it unzooms.
            if tab.zoomed {
                rects.push((tab.focused, area));
            }
        }
        for (id, rect) in rects {
            let Some(slot) = self.panes.get_mut(&id) else {
                continue;
            };
            slot.rect = rect;
            let grid = self
                .renderer
                .grid_size(rect.w.max(1) as u32, rect.h.max(1) as u32);
            if force || grid != slot.grid {
                slot.grid = grid;
                slot.pane.resize(Size {
                    cols: grid.0,
                    rows: grid.1,
                    cell_width: u16::try_from(m.width).unwrap_or(u16::MAX),
                    cell_height: u16::try_from(m.height).unwrap_or(u16::MAX),
                });
            }
        }
        self.refresh_banner();
        self.sync_remotes();
    }

    /// The active tab's dividers, in layout order.
    fn dividers(&self) -> Vec<Divider> {
        let mut dividers = Vec::new();
        if let Some(tab) = self.tab().filter(|t| !t.zoomed) {
            let area = self.chrome().panes;
            if let Some(remote) = tab.remote {
                return self.remote_dividers(remote, area);
            }
            tab.root
                .layout(area, self.gap(), &mut Vec::new(), &mut dividers);
        }
        dividers
    }

    /// The divider under client `(x, y)`, within a few pixels: one pixel is
    /// too fine to aim for.
    fn divider_at(&self, x: i32, y: i32) -> Option<(usize, Divider)> {
        let slop = self.gap() * DIVIDER_SLOP;
        self.dividers().into_iter().enumerate().find(|(_, d)| {
            let r = d.rect;
            let grown = if d.horizontal {
                Rect {
                    x: r.x - slop,
                    w: r.w + 2 * slop,
                    ..r
                }
            } else {
                Rect {
                    y: r.y - slop,
                    h: r.h + 2 * slop,
                    ..r
                }
            };
            grown.contains(x, y)
        })
    }

    /// Follow the mouse with the dragged divider, keeping a couple of cells
    /// on each side.
    fn drag_to(&mut self, x: i32, y: i32) {
        let Some((index, divider)) = self.drag else {
            return;
        };
        let m = self.renderer.cell_metrics();
        let pad = 2 * self.renderer.padding() as i32;
        let min = if divider.horizontal {
            2 * m.width as i32 + pad
        } else {
            m.height as i32 + pad
        };
        let ratio = divider.ratio_at(x, y, min);
        if self.tab().is_some_and(|t| t.remote.is_some()) {
            let permille = (ratio * 1000.0).round() as u16;
            self.remote_act(jetmux_proto::Action::SetRatio {
                split: u16::try_from(index).unwrap_or(u16::MAX),
                permille,
            });
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.root.set_ratio(index, ratio)
        {
            self.relayout(false);
            self.render();
        }
    }

    /// Pixels between splits.
    fn gap(&self) -> i32 {
        (self.dpi as f32 / 96.0).round().max(1.0) as i32
    }

    /// Draw now, or when paced and a frame already went out since the last
    /// vblank, at the next one. The first frame after a quiet spell (a
    /// keystroke's echo) never waits.
    fn render(&mut self) {
        if self.paced {
            if self.drew_since_vblank {
                self.frame_pending = true;
                return;
            }
            self.drew_since_vblank = true;
            self.host.want_vblank(true);
        }
        self.draw();
    }

    /// The monitor's vblank, when paced: draw what's owed with the newest
    /// snapshots, or stop asking for vblanks.
    #[cfg(windows)]
    fn vblank(&mut self) {
        self.drew_since_vblank = false;
        if !std::mem::take(&mut self.frame_pending) {
            self.host.want_vblank(false);
            return;
        }
        self.pump();
        if !self.drew_since_vblank {
            self.render();
        }
    }

    fn draw(&mut self) {
        if self.config.width == 0 {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let pad = self.renderer.padding();
        let focused_id = tab.focused;
        let ids = tab.shown();
        let split = ids.len() > 1;
        // A backbuffer taken and not presented stalls the next one (DX12
        // waits out a whole second), so first make sure there's something
        // to draw: until a pane's first snapshot, there isn't.
        let mut latched = false;
        let drawable = |app: &Self| {
            ids.iter()
                .any(|id| app.panes.get(id).is_some_and(|s| s.snapshot.is_some()))
        };
        if !drawable(self) {
            for id in &ids {
                latched |= self.latch(*id, Some(focused_id));
            }
            if !drawable(self) {
                return;
            }
        }
        // Wait for a backbuffer before taking snapshots: on a vsync'd
        // swapchain that wait is up to a frame, and output parsed during it
        // would otherwise show a frame late.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                return;
            }
            _ => return,
        };
        for id in &ids {
            latched |= self.latch(*id, Some(focused_id));
        }
        if latched {
            self.sync_blink();
        }
        // Cursors as shown: focused and blinking hides in the off phase,
        // anything without the keyboard is a hollow block. The banner
        // covers the focused pane's top rows. Lines are shared, so these
        // copies are cheap and unchanged rows stay cached.
        let mut shown: Vec<(PaneId, Rect, Snapshot)> = Vec::new();
        for id in &ids {
            let Some(slot) = self.panes.get(id) else {
                continue;
            };
            let Some(snapshot) = &slot.snapshot else {
                continue;
            };
            let has_keys = self.focused && *id == focused_id;
            let mut snapshot = snapshot.clone();
            snapshot.cursor = snapshot.cursor.and_then(|mut c| {
                if !has_keys {
                    if c.shape == CursorShape::Block {
                        c.shape = CursorShape::BlockHollow;
                    }
                } else if c.blinking && !self.blink_on {
                    return None;
                }
                Some(c)
            });
            if let Some(link) = self.link.as_ref().filter(|t| t.pane == *id) {
                for (row, cols) in &link.cells {
                    if let Some(line) = snapshot.lines.get_mut(usize::from(*row)) {
                        let mut underlined = (**line).clone();
                        for cell in underlined
                            .cells
                            .get_mut(usize::from(cols.start)..usize::from(cols.end))
                            .unwrap_or_default()
                        {
                            cell.underline = Underline::Single;
                        }
                        *line = Arc::new(underlined);
                    }
                }
            }
            if has_keys
                && !self.preedit.is_empty()
                && let Some(cursor) = slot.snapshot.as_ref().and_then(|s| s.cursor)
                && let Some(line) = snapshot.lines.get_mut(usize::from(cursor.y))
            {
                let (x, y) = (usize::from(cursor.x), cursor.y);
                let (with, used) = line.with_preedit(x, &self.preedit);
                *line = Arc::new(with);
                let before = self
                    .preedit_cursor
                    .and_then(|at| self.preedit.get(..at))
                    .map_or(used, Line::text_width)
                    .min(used);
                let x = u16::try_from(x + before).unwrap_or(u16::MAX);
                snapshot.cursor = Some(jet_core::Cursor {
                    x,
                    y,
                    shape: CursorShape::Bar,
                    blinking: false,
                });
            }
            overlay::mark_matches(&mut snapshot);
            if *id == focused_id {
                self.draw_overlay(&mut snapshot, has_keys);
            }
            if *id == focused_id && !self.banner.is_empty() {
                let covered = self.banner.len();
                snapshot.cursor = snapshot.cursor.filter(|c| usize::from(c.y) >= covered);
                for (line, banner) in snapshot.lines.iter_mut().zip(&self.banner) {
                    *line = Arc::clone(banner);
                }
            }
            shown.push((*id, slot.rect, snapshot));
        }
        self.report_ime_area(focused_id);
        let colors = self
            .panes
            .get(&focused_id)
            .and_then(|s| s.snapshot.as_ref())
            .map(|s| s.colors)
            .or_else(|| shown.first().map(|(_, _, s)| s.colors));
        let Some(colors) = colors else {
            return;
        };
        self.sync_progress();
        self.refresh_status();
        let chrome = self.chrome();
        let bar = self.tab_bar_snapshot(colors, chrome.tabs);
        let title_rect = chrome.title;
        let status = self.status_snapshot(colors, chrome.status);
        // The title bar shows the title unless the tabs are in it.
        let tabs_in_caption = chrome.tabs.h > 0 && chrome.tabs.y == chrome.caption.y;
        let title = (chrome.caption.h > 0 && !tabs_in_caption)
            .then(|| self.title_snapshot(colors, title_rect));
        let opacity = self.settings.config.window.opacity;
        let fade = 1.0 - self.settings.config.window.unfocused_split_opacity;

        let mut grids: Vec<Grid<'_>> = Vec::new();
        let m = self.renderer.cell_metrics();
        let cell_h = m.height;
        let bar_grid = |id, snapshot, r, vertical| bar_grid(id, snapshot, r, vertical, pad, cell_h);
        let mut images = Vec::new();
        if let Some(bar) = &bar {
            let grid = bar_grid(TAB_BAR, bar, chrome.tabs, self.vertical_tabs());
            images = self.icon_images(grid.origin);
            grids.push(grid);
        }
        if let Some(title) = &title {
            grids.push(bar_grid(CAPTION, title, title_rect, false));
        }
        if let Some(status) = &status {
            let vertical = self.status_at() == Some(StatusAt::TabBar) && self.vertical_tabs();
            grids.push(bar_grid(STATUS, status, chrome.status, vertical));
        }
        let mut overlays = Vec::new();
        if chrome.buttons[0].w > 0 {
            self.button_quads(&chrome, colors, &mut overlays);
        }
        self.tab_color_lines(colors, &mut overlays);
        self.close_quads(colors, &mut overlays);
        for (id, rect, snapshot) in &shown {
            grids.push(Grid {
                id: *id,
                snapshot,
                area: [rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32],
                origin: [rect.x.max(0) as u32 + pad, rect.y.max(0) as u32 + pad],
            });
            // Panes without the keyboard fade toward their background.
            if split && *id != focused_id {
                let [r, g, b, _] = jet_render::rgba(snapshot.colors.background);
                overlays.push(Quad {
                    pos: [rect.x as f32, rect.y as f32],
                    size: [rect.w as f32, rect.h as f32],
                    color: [r, g, b, fade * opacity],
                });
            }
        }
        if split {
            let color = mix(colors.background, colors.foreground, 0.25);
            for Divider { rect: d, .. } in self.dividers() {
                overlays.push(Quad {
                    pos: [d.x as f32, d.y as f32],
                    size: [d.w as f32, d.h as f32],
                    color: jet_render::rgba(color),
                });
            }
        }

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let clear = jet_render::rgba(colors.background);
        let (w, h) = (self.config.width, self.config.height);
        self.renderer
            .render_grids(&grids, &images, &overlays, clear, &view, w, h);
        self.gpu.queue.present(frame);
    }

    /// The tab bar's and title bar's colors: the config's, else steps from
    /// the terminal's.
    fn bar_colors(&self, colors: jet_core::Colors) -> BarColors {
        let bar = &self.settings.config.tab_bar;
        let pick = |c: Option<jet_config::Color>, or: Rgb| c.map_or(or, |c| c.0);
        // A step toward the text color reads as a bar on dark and light themes.
        let background = pick(
            bar.background,
            mix(colors.background, colors.foreground, 0.08),
        );
        BarColors {
            background,
            foreground: pick(bar.foreground, mix(colors.foreground, background, 0.45)),
            active_background: pick(bar.active_background, colors.background),
            active_foreground: pick(bar.active_foreground, colors.foreground),
        }
    }

    /// Text cells across and down `rect`, inside the padding.
    fn cells_in(&self, rect: Rect) -> (usize, usize) {
        let m = self.renderer.cell_metrics();
        let pad = 2 * self.renderer.padding() as i32;
        let fit = |px: i32, cell: u32| ((px - pad).max(0) / cell.max(1) as i32) as usize;
        (fit(rect.w, m.width), fit(rect.h, m.height).max(1))
    }

    /// Cells per tab in a row of `cols`.
    fn tab_width(&self, cols: usize) -> usize {
        (cols / self.tabs.len().max(1)).clamp(4, 32)
    }

    /// The focused pane's title as the shell last set it, and its icon:
    /// text goes in front, an image file gets two blank cells there to be
    /// drawn into.
    fn tab_label(&self, tab: &Tab) -> (String, Option<Icon>) {
        let slot = self.panes.get(&tab.focused);
        let title = tab
            .title
            .as_deref()
            .or(slot.map(|p| p.title.as_str()))
            .filter(|t| !t.is_empty())
            .unwrap_or("shell");
        let zoom = if tab.zoomed { "⊞ " } else { "" };
        let title = format!("{zoom}{title}");
        match tab.icon.as_ref().or(slot.and_then(|p| p.icon.as_ref())) {
            Some(Icon::Text(text)) => (format!("{text} {title}"), None),
            Some(image) => (format!("   {title}"), Some(image.clone())),
            None => (title.to_owned(), None),
        }
    }

    /// The tab bar as a little screen: a row of equal-width tabs, or a
    /// column of one tab per row. The active tab takes the terminal's own
    /// colors, so it joins the panes.
    fn tab_bar_snapshot(&mut self, colors: jet_core::Colors, rect: Rect) -> Option<Snapshot> {
        if rect.h == 0 || rect.w == 0 {
            self.tab_bar.clear();
            return None;
        }
        let c = self.bar_colors(colors);
        let config = &self.settings.config;
        // Cell backgrounds are solid; a see-through window keeps the bar
        // see-through too (bar background aside, unless configured), and
        // tells tabs apart by their text.
        let solid = config.window.opacity >= 1.0;
        let active_bg =
            (solid || config.tab_bar.active_background.is_some()).then_some(c.active_background);
        let hover_bg = mix(c.background, c.active_foreground, 0.12);
        let style = |active: bool, hovered: bool, color: Option<(Rgb, TabColorStyle)>| {
            let mut cell = Cell::default();
            if active {
                (cell.fg, cell.bg) = (Some(c.active_foreground), active_bg);
                cell.flags = CellFlags::BOLD;
            } else {
                cell.fg = Some(c.foreground);
            }
            if hovered && !active {
                (cell.fg, cell.bg) = (Some(c.active_foreground), Some(hover_bg));
            }
            // The profile's color, at full strength on the active tab.
            let strength = if active {
                1.0
            } else if hovered {
                0.75
            } else {
                0.55
            };
            match color {
                Some((color, TabColorStyle::Fill)) => {
                    let bg = mix(c.background, color, strength);
                    (cell.fg, cell.bg) = (Some(readable_on(bg)), Some(bg));
                }
                Some((color, TabColorStyle::Text)) => {
                    cell.fg = Some(mix(c.background, color, strength.max(0.75)));
                }
                _ => {}
            }
            cell
        };
        let tab_colors: Vec<_> = self.tabs.iter().map(|t| self.tab_color(t)).collect();
        // A tab under the mouse gets a close button in its last cells.
        let hover = self.tab_hover;
        let push_tab = |line: &mut Line, i: usize, text: &str, width: usize| {
            let hot = hover.filter(|(h, _)| *h == i);
            let color = tab_colors.get(i).copied().flatten();
            let tab = style(i == self.active, hot.is_some(), color);
            // A dot takes the place of the title's leading space.
            let (text, width) = match (color, text.strip_prefix(' ')) {
                (Some((color, TabColorStyle::Dot)), Some(rest)) if width > 1 => {
                    let mut dot = tab;
                    dot.fg = Some(color);
                    line.push_text("●", 1, dot);
                    (rest, width - 1)
                }
                _ => (text, width),
            };
            // Its last two cells stay blank for the close button, drawn
            // over them.
            if hot.is_some() && width > 3 {
                line.push_text(text, width - 2, tab);
                line.push_text("", 2, tab);
            } else {
                line.push_text(text, width, tab);
            }
        };
        // Where image icons go: cell column and row in this snapshot.
        let mut spots = Vec::new();
        let mut spot = |col: usize, row: usize, limit: usize, file: Option<Icon>| {
            if let Some(file) = file.filter(|_| col + 2 <= limit) {
                spots.push((col, row, file));
            }
        };
        let (cols, rows) = self.cells_in(rect);
        let mut lines = Vec::new();
        if self.vertical_tabs() {
            for (i, tab) in self.tabs.iter().enumerate().take(rows) {
                let mut line = Line::default();
                let (title, file) = self.tab_label(tab);
                let prefix = format!(" {} ", i + 1);
                spot(prefix.len(), i, cols.saturating_sub(2), file);
                push_tab(&mut line, i, &format!("{prefix}{title}"), cols);
                lines.push(line);
            }
            lines.resize_with(rows, || Line::from_text("", cols, Cell::default()));
        } else {
            let width = self.tab_width(cols);
            let mut line = Line::default();
            for (i, tab) in self.tabs.iter().enumerate() {
                let (title, file) = self.tab_label(tab);
                let prefix = format!(" {} ", i + 1);
                let start = i * width;
                spot(start + prefix.len(), 0, start + width - 2, file);
                push_tab(&mut line, i, &format!("{prefix}{title} "), width);
            }
            line.push_text(
                "",
                cols.saturating_sub(width * self.tabs.len()),
                Cell::default(),
            );
            lines.push(line);
        }
        self.icon_spots = spots;
        let lines = reuse(&mut self.tab_bar, lines);
        Some(Snapshot {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: u16::try_from(lines.len()).unwrap_or(u16::MAX),
            lines,
            cursor: None,
            colors: jet_core::Colors {
                background: c.background,
                ..colors
            },
            images: Vec::new(),
            search: None,
            bracketed_paste: false,
        })
    }

    /// Serve `jet-cli` on this process's pipe; jet works on without it.
    fn listen(&mut self) {
        let wake = self.host.waker(Wake::Cli);
        match cli_transport::listen(&self.socket, move || wake()) {
            Ok(rx) => self.cli = Some(rx),
            Err(err) => eprintln!("jet: serving {}: {err}", self.socket),
        }
    }

    fn serve_cli(&mut self) {
        loop {
            let Some(Ok(incoming)) = self.cli.as_ref().map(Receiver::try_recv) else {
                return;
            };
            let body = match self.answer(incoming.request, &incoming.out) {
                Ok(answer) => Body::Ok(answer),
                Err(failure) => Body::Error(failure),
            };
            let _ = incoming.out.send(Message::Reply(body));
        }
    }

    fn answer(&mut self, request: Request, out: &Sender<Message>) -> Result<Answer, Failure> {
        let done = Ok(Answer::Done(Done {}));
        match request {
            Request::Hello => Ok(Answer::Hello(Hello {
                version: jet_cli::VERSION,
                pid: std::process::id(),
                quake: self.host.quake(),
            })),
            Request::List => Ok(Answer::List(self.listing())),
            Request::Spawn(open) => {
                let o = self.what_to_open(open)?;
                let pane = self
                    .open_tab(o.cwd, o.profile.as_deref(), o.command)
                    .map_err(|e| Failure::new("spawn_failed", e.to_string()))?;
                Ok(Answer::Opened(self.opened(pane)))
            }
            Request::Split(split) => {
                let target = self.pane_of(split.pane)?;
                self.show_pane(target);
                let side = match split.direction {
                    jet_cli::Direction::Right => Direction::Right,
                    jet_cli::Direction::Down => Direction::Down,
                    jet_cli::Direction::Left => Direction::Left,
                    jet_cli::Direction::Up => Direction::Up,
                };
                let o = self.what_to_open(split.open)?;
                let pane = self
                    .open_split(target, side, o.cwd, o.profile.as_deref(), o.command)
                    .map_err(|e| Failure::new("spawn_failed", e.to_string()))?;
                if let Some(ratio) = split.ratio
                    && let Some(tab) = self.tabs.get_mut(self.active)
                {
                    tab.root.share(pane, ratio);
                    self.relayout(false);
                    self.render();
                }
                Ok(Answer::Opened(self.opened(pane)))
            }
            Request::SendText(send) => {
                let id = self.pane_of(send.pane)?;
                if let Some(slot) = self.panes.get(&id) {
                    if send.paste {
                        slot.pane.paste(send.text);
                    } else {
                        slot.pane.input(send.text.into_bytes());
                    }
                }
                done
            }
            Request::GetText(target) => {
                let id = self.pane_in(&target)?;
                let lines = self
                    .panes
                    .get(&id)
                    .and_then(|s| s.snapshot.as_ref())
                    .map(|s| s.lines.iter().map(|l| l.text()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let end = lines
                    .iter()
                    .rposition(|l| !l.is_empty())
                    .map_or(0, |i| i + 1);
                let text = lines.get(..end).unwrap_or_default().join("\n");
                Ok(Answer::Text(Text { text }))
            }
            Request::Activate(target) => {
                let id = self.pane_in(&target)?;
                self.show_pane(id);
                self.host.request(WindowRequest::Raise);
                done
            }
            Request::Close(target) => {
                match (target.tab, target.pane) {
                    (Some(_), _) => {
                        let index = self.tab_index(&target)?;
                        self.close_tab(index);
                    }
                    _ => {
                        let id = self.pane_of(target.pane)?;
                        self.close_pane(id);
                    }
                }
                done
            }
            Request::SetTitle(label) => {
                let index = self.tab_index(&label.target)?;
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.title = label.value;
                }
                self.render();
                done
            }
            Request::SetColor(label) => {
                let index = self.tab_index(&label.target)?;
                let color = match label.value.as_deref() {
                    None => None,
                    Some(text) => Some(jet_config::parse_color(text).ok_or_else(|| {
                        Failure::new("bad_color", format!("{text:?} isn't a #rrggbb color"))
                    })?),
                };
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.color = color;
                }
                self.render();
                done
            }
            Request::SetIcon(label) => {
                let index = self.tab_index(&label.target)?;
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.icon = label.value.map(Icon::from);
                }
                self.render();
                done
            }
            Request::SetVar(var) => {
                let id = self.pane_of(var.pane)?;
                self.set_var(id, var.name, var.value);
                self.render();
                done
            }
            Request::Resize(resize) => {
                let id = self.pane_of(resize.pane)?;
                let index = self.tab_index(&jet_cli::Target {
                    pane: Some(id),
                    tab: None,
                })?;
                let split = self
                    .tabs
                    .get_mut(index)
                    .is_some_and(|tab| tab.root.share(id, resize.ratio));
                if !split {
                    return Err(Failure::new(
                        "not_split",
                        format!("pane {id} has the tab to itself"),
                    ));
                }
                self.relayout(false);
                self.render();
                done
            }
            Request::ReloadConfig => {
                self.reload_config(true);
                match &self.settings.error {
                    Some(err) => Err(Failure::new("config_error", err.clone())),
                    None => done,
                }
            }
            Request::Events(filter) => {
                self.subscribers.push((filter, out.clone()));
                done
            }
            Request::ToggleQuake => {
                if !self.host.quake() {
                    return Err(Failure::new("not_quake", "this isn't jet quake's window"));
                }
                (self.host.waker(Wake::Quake))();
                done
            }
        }
    }

    /// A request's cwd, profile and command, checked.
    fn what_to_open(&self, open: jet_cli::Open) -> Result<Opening, Failure> {
        if let Some(name) = &open.profile
            && !self.settings.config.profiles.contains_key(name)
        {
            return Err(Failure::new(
                "no_such_profile",
                format!("no profile {name:?} in the config"),
            ));
        }
        let cwd = open.cwd.or_else(|| self.inherited_cwd());
        let command = (!open.command.is_empty()).then_some(jet_config::Command::Argv(open.command));
        Ok(Opening {
            cwd,
            profile: open.profile,
            command,
        })
    }

    fn opened(&self, pane: PaneId) -> Opened {
        let tab = self.tab_of(pane).unwrap_or_default();
        Opened { pane, tab }
    }

    fn tab_of(&self, pane: PaneId) -> Option<u64> {
        self.tabs
            .iter()
            .find(|t| t.root.panes().contains(&pane))
            .map(|t| t.id)
    }

    /// `pane` if it exists, else the focused one.
    fn pane_of(&self, pane: Option<u64>) -> Result<PaneId, Failure> {
        match pane {
            Some(id) if self.panes.contains_key(&id) => Ok(id),
            Some(id) => Err(Failure::new(
                "no_such_pane",
                format!("pane {id} doesn't exist"),
            )),
            None => self
                .focused_id()
                .ok_or_else(|| Failure::new("no_such_pane", "no pane has the focus")),
        }
    }

    /// The pane a target means: a tab's focused one, else as `pane_of`.
    fn pane_in(&self, target: &jet_cli::Target) -> Result<PaneId, Failure> {
        match (target.tab, target.pane) {
            (Some(_), None) => {
                let index = self.tab_index(target)?;
                self.tabs
                    .get(index)
                    .map(|t| t.focused)
                    .ok_or_else(|| Failure::new("no_such_tab", "tab vanished"))
            }
            (_, pane) => self.pane_of(pane),
        }
    }

    /// The index of the tab a target means: its tab, its pane's, else the
    /// active one.
    fn tab_index(&self, target: &jet_cli::Target) -> Result<usize, Failure> {
        let found = match (target.tab, target.pane) {
            (Some(id), _) => self.tabs.iter().position(|t| t.id == id),
            (None, Some(pane)) => self
                .tabs
                .iter()
                .position(|t| t.root.panes().contains(&pane)),
            (None, None) => Some(self.active),
        };
        found.ok_or_else(|| match target.tab {
            Some(id) => Failure::new("no_such_tab", format!("tab {id} doesn't exist")),
            None => Failure::new("no_such_pane", "that pane doesn't exist"),
        })
    }

    /// Bring `pane`'s tab forward and give it the keyboard.
    fn show_pane(&mut self, pane: PaneId) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|t| t.root.panes().contains(&pane))
        {
            if index != self.active {
                self.activate_tab(index);
            }
            self.focus_pane(pane);
        }
    }

    fn listing(&self) -> Listing {
        let tabs = self.tabs.iter().enumerate().map(|(i, tab)| {
            let panes = tab.root.panes().into_iter().filter_map(|id| {
                let slot = self.panes.get(&id)?;
                let cwd = slot
                    .location
                    .as_ref()
                    .map(|l| PathBuf::from(&l.path))
                    .or_else(|| slot.cwd.clone())
                    .or_else(|| slot.pane.pid().and_then(jet_pty::process_cwd))
                    .map(|c| c.to_string_lossy().into_owned());
                Some(PaneInfo {
                    id,
                    title: slot.title.clone(),
                    focused: id == tab.focused,
                    cols: slot.grid.0,
                    rows: slot.grid.1,
                    cwd,
                    host: slot.location.as_ref().and_then(|l| l.host.clone()),
                    pid: slot.pane.pid(),
                    profile: slot.profile.clone(),
                })
            });
            TabInfo {
                id: tab.id,
                index: i + 1,
                title: self.tab_label(tab).0.trim().to_owned(),
                active: i == self.active,
                panes: panes.collect(),
            }
        });
        Listing {
            pid: std::process::id(),
            quake: self.host.quake(),
            tabs: tabs.collect(),
        }
    }

    fn set_var(&mut self, pane: PaneId, name: String, value: String) {
        if let Some(slot) = self.panes.get_mut(&pane) {
            if value.is_empty() {
                slot.vars.remove(&name);
            } else {
                slot.vars.insert(name.clone(), value.clone());
            }
        }
        self.emit(What::Var { name, value }, Some(pane));
    }

    /// Tell subscribers; those gone stop being told.
    fn emit(&mut self, what: What, pane: Option<PaneId>) {
        if self.subscribers.is_empty() {
            return;
        }
        let event = Event {
            what,
            window: std::process::id(),
            tab: pane.and_then(|p| self.tab_of(p)),
            pane,
        };
        self.subscribers.retain(|(filter, out)| {
            !filter.wants(&event) || out.send(Message::Event(event.clone())).is_ok()
        });
    }

    /// Where the status goes, if it has segments. The title bar needs to
    /// be jet's; without it they go to the bottom.
    fn status_at(&self) -> Option<StatusAt> {
        let status = &self.settings.config.status;
        if status.segments.is_empty() {
            return None;
        }
        Some(match status.position {
            StatusPosition::Titlebar if self.decorations() == Decorations::Custom => {
                StatusAt::Caption
            }
            StatusPosition::Titlebar | StatusPosition::Bottom => StatusAt::Bottom,
            StatusPosition::TabBar => StatusAt::TabBar,
            StatusPosition::Top => StatusAt::Top,
        })
    }

    /// Pixels the status needs across (or down, at a side bar's foot).
    fn status_size(&self) -> i32 {
        let m = self.renderer.cell_metrics();
        let pad = 2 * self.renderer.padding() as i32;
        if self.status_at() == Some(StatusAt::TabBar) && self.vertical_tabs() {
            return self.status.len() as i32 * m.height as i32 + pad;
        }
        let style = self.settings.config.status.style;
        let cols = status::row_width(&self.status, style) as i32;
        if cols == 0 {
            0
        } else {
            cols * m.width as i32 + pad
        }
    }

    /// Fill the segments in from the focused pane; whether they changed.
    fn refresh_status(&mut self) -> bool {
        let segments = &self.settings.config.status.segments;
        if segments.is_empty() {
            return !std::mem::take(&mut self.status).is_empty();
        }
        let colors = self
            .focused_slot()
            .and_then(|s| s.snapshot.as_ref())
            .map(|s| s.colors);
        let Some(colors) = colors else {
            return false;
        };
        let c = self.bar_colors(colors);
        let (time, date) = self.host.local_time();
        let slot = self.focused_slot();
        let front = slot.and_then(|s| s.front.as_ref());
        let ssh = front.and_then(|f| f.ssh.as_ref());
        // Without a report, where the program in front is, unless that's
        // ssh: its local directory says nothing about the remote one.
        let polled = slot
            .and_then(|s| s.front_cwd.as_ref())
            .filter(|_| ssh.is_none())
            .map(|dir| Location {
                host: None,
                path: dir.to_string_lossy().into_owned(),
            });
        let session = jet_config::status::Session {
            title: slot.map_or("", |s| s.title.as_str()),
            location: slot.and_then(|s| s.location.as_ref()).or(polled.as_ref()),
            ssh,
            process: front.map(|f| f.process.name.as_str()),
            profile: slot.and_then(|s| s.profile.as_deref()),
            tab: self.active + 1,
            tabs: self.tabs.len(),
            time: &time,
            date: &date,
            progress: slot.map(|s| s.progress),
            exit: slot.and_then(|s| s.exit),
            vars: slot.map(|s| &s.vars),
            context: slot.and_then(|s| s.context.as_ref()),
            local_user: &self.local_user,
            local_host: &self.local_host,
        };
        let shown =
            jet_config::status::shown(segments, &session, c.active_foreground, c.background);
        let changed = shown != self.status;
        self.status = shown;
        changed
    }

    /// Look up what the focused pane is running, for the status and
    /// `[hosts]`: `ssh` and where to. Once a second, so only while there's
    /// a status or a host rule.
    fn poll_session(&mut self) {
        let config = &self.settings.config;
        if config.status.segments.is_empty() && config.hosts.is_empty() {
            return;
        }
        let since = std::mem::replace(&mut self.polled, Instant::now());
        let Some(id) = self.focused_id() else {
            return;
        };
        let Some(slot) = self.panes.get_mut(&id) else {
            return;
        };
        let pid = slot.pane.pid();
        let process = pid.and_then(jet_pty::foreground);
        if process.as_ref().map(|p| p.pid) != slot.front.as_ref().map(|f| f.process.pid) {
            // Something else is in front: a place reported before it could
            // have started (the shell's, before ssh; ssh's, after it exits)
            // is stale.
            if slot.location_at <= since {
                slot.location = None;
            }
            slot.front = process.map(|process| {
                let argv = jet_config::Command::Line(process.command_line.clone()).argv();
                Front {
                    ssh: jet_core::ssh_target(&argv),
                    process,
                }
            });
        }
        slot.front_cwd = pid.and_then(jet_pty::process_cwd);
        let restyled = self.refresh_host(id);
        if self.refresh_status() || restyled {
            self.render();
        }
    }

    /// Give pane `id` the look of the `[hosts]` rule its host matches, if
    /// that changed; whether it did.
    fn refresh_host(&mut self, id: PaneId) -> bool {
        let config = &self.settings.config;
        let Some(slot) = self.panes.get(&id) else {
            return false;
        };
        let rule = if config.hosts.is_empty() {
            None
        } else {
            let ssh = slot.front.as_ref().and_then(|f| f.ssh.as_ref());
            let session = jet_config::status::Session {
                location: slot.location.as_ref(),
                ssh,
                context: slot.context.as_ref(),
                local_host: &self.local_host,
                ..Default::default()
            };
            let host = session.value(&jet_config::status::Field::Host);
            host.and_then(|h| config.host_rule(&h)).map(str::to_owned)
        };
        if rule == slot.host_rule {
            return false;
        }
        slot.pane
            .configure(config.pane_vt_settings(slot.profile.as_deref(), rule.as_deref()));
        if let Some(slot) = self.panes.get_mut(&id) {
            slot.host_rule = rule;
        }
        true
    }

    /// The status as a little screen: a row, or a column at the foot of a
    /// side tab bar.
    fn status_snapshot(&mut self, colors: jet_core::Colors, rect: Rect) -> Option<Snapshot> {
        if rect.w == 0 || rect.h == 0 || self.status.is_empty() {
            return None;
        }
        let c = self.bar_colors(colors);
        let (cols, _) = self.cells_in(rect);
        let vertical = self.status_at() == Some(StatusAt::TabBar) && self.vertical_tabs();
        let lines = if vertical {
            status::column(&self.status, cols)
        } else {
            vec![status::row(
                &self.status,
                self.settings.config.status.style,
                cols,
            )]
        };
        let lines = reuse(&mut self.status_lines, lines);
        Some(Snapshot {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: u16::try_from(lines.len()).unwrap_or(u16::MAX),
            lines,
            cursor: None,
            colors: jet_core::Colors {
                background: c.background,
                ..colors
            },
            images: Vec::new(),
            search: None,
            bracketed_paste: false,
        })
    }

    /// jet's title bar when the tabs are elsewhere: the focused pane's title.
    fn title_snapshot(&mut self, colors: jet_core::Colors, rect: Rect) -> Snapshot {
        let c = self.bar_colors(colors);
        let (cols, _) = self.cells_in(rect);
        let title = self
            .focused_slot()
            .map(|p| p.title.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or("jet");
        let mut cell = Cell::default();
        cell.fg = Some(c.active_foreground);
        let line = Line::from_text(&format!(" {title}"), cols, cell);
        Snapshot {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: 1,
            lines: reuse(&mut self.caption_line, vec![line]),
            cursor: None,
            colors: jet_core::Colors {
                background: c.background,
                ..colors
            },
            images: Vec::new(),
            search: None,
            bracketed_paste: false,
        }
    }

    /// Minimize, maximize (or restore) and close, drawn as rects: crisp at
    /// any scale, with no icon font needed.
    fn button_quads(&self, chrome: &Chrome, colors: jet_core::Colors, out: &mut Vec<Quad>) {
        let c = self.bar_colors(colors);
        let scale = self.dpi as f32 / 96.0;
        let stroke = scale.round().max(1.0);
        let size = (10.0 * scale).round();
        let zoomed = self.host.maximized();
        let [r, g, b, _] = jet_render::rgba(c.background);
        let base = [r, g, b, self.settings.config.window.opacity];
        let rect = |out: &mut Vec<Quad>, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]| {
            out.push(Quad {
                pos: [x, y],
                size: [w, h],
                color,
            });
        };
        for (button, at) in BUTTONS.iter().zip(chrome.buttons) {
            let hot = self.hover == Some(*button);
            let close = *button == Button::Close;
            let (bg, fg) = match (hot, close) {
                (true, true) => (jet_render::rgba(CLOSE_HOVER), [1.0; 4]),
                (true, false) => (
                    jet_render::rgba(mix(c.background, c.active_foreground, 0.15)),
                    jet_render::rgba(c.active_foreground),
                ),
                _ => (base, jet_render::rgba(c.active_foreground)),
            };
            let (x, y, w, h) = (at.x as f32, at.y as f32, at.w as f32, at.h as f32);
            rect(out, x, y, w, h, bg);
            let x0 = (x + (w - size) / 2.0).round();
            let y0 = (y + (h - size) / 2.0).round();
            let outline = |out: &mut Vec<Quad>, x: f32, y: f32, s: f32| {
                rect(out, x, y, s, stroke, fg);
                rect(out, x, y + s - stroke, s, stroke, fg);
                rect(out, x, y, stroke, s, fg);
                rect(out, x + s - stroke, y, stroke, s, fg);
            };
            match button {
                Button::Minimize => rect(out, x0, y0 + (size / 2.0).round(), size, stroke, fg),
                Button::Maximize if zoomed => {
                    // Restore: a square with another behind it.
                    let gap = 2.0 * stroke;
                    outline(out, x0, y0 + gap, size - gap);
                    rect(out, x0 + gap, y0, size - gap, stroke, fg);
                    rect(out, x0 + size - stroke, y0, stroke, size - gap, fg);
                }
                Button::Maximize => outline(out, x0, y0, size),
                Button::Close => x_mark(out, [x0, y0], size, stroke, fg),
            }
        }
    }

    /// The tab under client `(x, y)`, if it's on the tab bar.
    fn tab_at(&self, x: i32, y: i32) -> Option<usize> {
        self.tab_hit(x, y).map(|(i, _)| i)
    }

    /// The tab under client `(x, y)`, and whether that's where its close
    /// button goes.
    fn tab_hit(&self, x: i32, y: i32) -> Option<(usize, bool)> {
        let r = self.chrome().tabs;
        if !r.contains(x, y) {
            return None;
        }
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let (cols, _) = self.cells_in(r);
        let col = (x - r.x - pad).div_euclid(m.width.max(1) as i32);
        let (i, width, at) = if self.vertical_tabs() {
            let row = (y - r.y - pad).div_euclid(m.height.max(1) as i32);
            (row, cols as i32, col)
        } else {
            let width = self.tab_width(cols) as i32;
            (col.div_euclid(width), width, col.rem_euclid(width))
        };
        let i = usize::try_from(i).ok().filter(|i| *i < self.tabs.len())?;
        let on_close = width > 3 && at >= width - 2;
        Some((i, on_close))
    }

    /// Tab `i`'s cells in the tab bar.
    fn tab_cells(&self, i: usize) -> Option<Rect> {
        let r = self.chrome().tabs;
        if r.h == 0 {
            return None;
        }
        let m = self.renderer.cell_metrics();
        let (cw, ch) = (m.width as i32, m.height as i32);
        let pad = self.renderer.padding() as i32;
        let (cols, _) = self.cells_in(r);
        let i = i as i32;
        Some(if self.vertical_tabs() {
            Rect {
                x: r.x + pad,
                y: r.y + pad + i * ch,
                w: cols as i32 * cw,
                h: ch,
            }
        } else {
            let width = self.tab_width(cols) as i32 * cw;
            Rect {
                x: r.x + pad + i * width,
                y: r.y + (r.h - ch).max(0) / 2,
                w: width,
                h: ch,
            }
        })
    }

    /// The close button's slot on tab `i`: its last two cells.
    fn close_area(&self, i: usize) -> Option<Rect> {
        let tab = self.tab_cells(i)?;
        let cw = self.renderer.cell_metrics().width as i32;
        Some(Rect {
            x: tab.x + tab.w - 2 * cw,
            w: 2 * cw,
            ..tab
        })
    }

    /// A tab's color, if it has one, and how it shows: its own, else its
    /// focused pane's host rule's, else that pane's profile's.
    fn tab_color(&self, tab: &Tab) -> Option<(Rgb, TabColorStyle)> {
        let config = &self.settings.config;
        let slot = self.panes.get(&tab.focused);
        let profile = slot
            .and_then(|s| s.profile.as_deref())
            .and_then(|name| config.profiles.get(name));
        let host = slot
            .and_then(|s| s.host_rule.as_deref())
            .and_then(|name| config.hosts.get(name));
        let color = tab
            .color
            .or_else(|| host.and_then(|h| h.tab_color).map(|c| c.0))
            .or_else(|| profile.and_then(|p| p.tab_color).map(|c| c.0))?;
        let style = host
            .and_then(|h| h.color_style)
            .or_else(|| profile.and_then(|p| p.color_style))
            .unwrap_or(config.tab_bar.color_style);
        Some((color, style))
    }

    /// With a line style for `tab-color`, each colored tab's line, dimmer
    /// off the active tab.
    fn tab_color_lines(&self, colors: jet_core::Colors, out: &mut Vec<Quad>) {
        let thick = (2.0 * self.dpi as f32 / 96.0).round().max(1.0) as i32;
        let background = self.bar_colors(colors).background;
        for (i, tab) in self.tabs.iter().enumerate() {
            let (Some((color, style)), Some(r)) = (self.tab_color(tab), self.tab_cells(i)) else {
                continue;
            };
            let line = match style {
                TabColorStyle::Top => Rect { h: thick, ..r },
                TabColorStyle::Bottom => Rect {
                    y: r.y + r.h - thick,
                    h: thick,
                    ..r
                },
                TabColorStyle::Left => Rect { w: thick, ..r },
                TabColorStyle::Right => Rect {
                    x: r.x + r.w - thick,
                    w: thick,
                    ..r
                },
                TabColorStyle::Fill | TabColorStyle::Dot | TabColorStyle::Text => continue,
            };
            let color = if i == self.active {
                color
            } else {
                mix(background, color, 0.55)
            };
            out.push(Quad {
                pos: [line.x as f32, line.y as f32],
                size: [line.w as f32, line.h as f32],
                color: jet_render::rgba(color),
            });
        }
    }

    /// The hovered tab's close button: an X, on a red square when the
    /// mouse is on it.
    fn close_quads(&self, colors: jet_core::Colors, out: &mut Vec<Quad>) {
        let Some((i, on)) = self.tab_hover else {
            return;
        };
        let Some(a) = self.close_area(i) else {
            return;
        };
        let scale = self.dpi as f32 / 96.0;
        let stroke = scale.round().max(1.0);
        let side = (a.w.min(a.h) as f32 * 0.85).round();
        let (cx, cy) = (a.x as f32 + a.w as f32 / 2.0, a.y as f32 + a.h as f32 / 2.0);
        let fg = if on {
            out.push(Quad {
                pos: [(cx - side / 2.0).round(), (cy - side / 2.0).round()],
                size: [side, side],
                color: jet_render::rgba(CLOSE_HOVER),
            });
            [1.0; 4]
        } else {
            jet_render::rgba(self.bar_colors(colors).active_foreground)
        };
        let size = (side * 0.45).round();
        let at = [(cx - size / 2.0).round(), (cy - size / 2.0).round()];
        x_mark(out, at, size, stroke, fg);
    }

    /// The tab bar's image icons at `origin` (its first cell), loading
    /// each the first time.
    fn icon_images(&mut self, origin: [u32; 2]) -> Vec<Image> {
        let m = self.renderer.cell_metrics();
        let (cw, ch) = (m.width as f32, m.height as f32);
        // Two cells wide, a cell tall, with a little air.
        let px = ((2.0 * cw).min(ch) * 0.9).round() as u32;
        let mut images = Vec::new();
        for (col, row, image) in std::mem::take(&mut self.icon_spots) {
            let id = match self.icons.get(&image) {
                Some(id) => *id,
                None => {
                    let id = icon::bitmap(&image, px)
                        .inspect_err(|err| eprintln!("jet: {err}"))
                        .ok()
                        .and_then(|b| self.renderer.add_image(b.width, b.height, &b.rgba));
                    self.icons.insert(image, id);
                    id
                }
            };
            let Some(id) = id else {
                continue;
            };
            let x = origin[0] as f32 + col as f32 * cw + (2.0 * cw - px as f32) / 2.0;
            let y = origin[1] as f32 + row as f32 * ch + (ch - px as f32) / 2.0;
            images.push(Image {
                id,
                pos: [x.round(), y.round()],
                size: [px as f32, px as f32],
            });
        }
        images
    }

    /// Track the tab under the mouse, for its highlight and close button.
    fn hover_tab(&mut self, x: i32, y: i32) {
        let hit = self.tab_hit(x, y);
        if hit == self.tab_hover {
            return;
        }
        if hit.is_some() && self.tab_hover.is_none() {
            self.host.track_leave();
        }
        self.tab_hover = hit;
        self.render();
    }

    fn set_hover(&mut self, button: Option<Button>) {
        if button != self.hover {
            self.hover = button;
            self.render();
        }
    }

    /// The focused pane's progress on the taskbar, else any pane's.
    fn sync_progress(&mut self) {
        let wanted = if self.settings.config.window.taskbar_progress {
            let focused = self.focused_slot().map_or(Progress::Hidden, |p| p.progress);
            if focused == Progress::Hidden {
                self.panes
                    .values()
                    .map(|p| p.progress)
                    .find(|p| *p != Progress::Hidden)
                    .unwrap_or(Progress::Hidden)
            } else {
                focused
            }
        } else {
            Progress::Hidden
        };
        self.host.progress(wanted);
    }

    fn update_title(&mut self) {
        let title = self
            .focused_slot()
            .map(|p| p.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "jet".to_owned());
        self.host.set_title(&title);
    }

    /// New fonts or padding: same window, so the grids change instead.
    fn rebuild_renderer(&mut self) {
        let size = (self.settings.config.font.size + self.font_delta).max(MIN_FONT_PT);
        match load_fonts(self.dpi, &self.settings.config, size) {
            Ok((fonts, problem)) => {
                let config = &self.settings.config;
                self.renderer = make_renderer(&self.gpu, self.view_format, self.dpi, config, fonts);
                self.tab_bar.clear();
                self.icons.clear();
                self.set_font_problem(problem);
            }
            Err(err) => eprintln!("jet: rebuilding fonts for {} dpi: {err}", self.dpi),
        }
    }

    fn set_font_problem(&mut self, problem: Option<String>) {
        if problem != self.font_problem {
            self.font_problem = problem;
            self.banner_dismissed = false;
        }
    }

    /// Lay out the banner for the current problems and the focused pane.
    fn refresh_banner(&mut self) {
        let problems: Vec<&str> = [
            &self.settings.error,
            &self.font_problem,
            &self.hotkey_problem,
            &self.remote_problem,
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
        let grid = self.focused_slot().map(|p| p.grid);
        self.banner = match grid {
            Some(grid) if !self.banner_dismissed && !problems.is_empty() => banner(&problems, grid),
            _ => Vec::new(),
        };
    }

    fn reload_config(&mut self, force: bool) {
        let error = self.settings.error.clone();
        let new = self.settings.reload(force);
        if self.settings.error != error {
            self.banner_dismissed = false;
            self.refresh_banner();
            self.render();
        }
        if let Some(new) = new {
            self.apply_config(new);
            self.emit(What::ConfigReloaded, None);
        }
    }

    /// Put `new` in effect.
    fn apply_config(&mut self, new: Config) {
        let old = std::mem::replace(&mut self.settings.config, new);
        let new = &self.settings.config;
        self.bindings = new.bindings().unwrap_or_default();
        for slot in self.panes.values() {
            let (profile, host) = (slot.profile.as_deref(), slot.host_rule.as_deref());
            let settings = new.pane_vt_settings(profile, host);
            if settings != old.pane_vt_settings(profile, host) {
                slot.pane.configure(settings);
            }
        }

        let quake_changed = new.quake != old.quake;
        let frame_changed = new.window != old.window
            || new.vt_settings() != old.vt_settings()
            || new.colors != old.colors;
        let bar_changed = new.tab_bar != old.tab_bar || new.status != old.status;
        let fonts_changed = new.font != old.font || new.window.padding != old.window.padding;
        if new.quake.autostart != old.quake.autostart && !self.host.quake() {
            self.host.autostart(new.quake.autostart);
        }
        if quake_changed {
            self.reconfigure_quake();
        }
        if frame_changed {
            self.renderer
                .set_opacity(self.settings.config.window.opacity);
            self.renderer
                .set_selection(selection(&self.settings.config));
            self.host.style_frame(&self.settings.config);
            self.host.request(WindowRequest::Reframe);
        }

        if fonts_changed {
            self.rebuild_renderer();
        }
        if bar_changed {
            self.tab_bar.clear();
            self.status_lines.clear();
        }
        if fonts_changed || bar_changed {
            self.relayout(true);
        }
        // The rules may have changed: each pane's match.
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            self.refresh_host(id);
        }
        self.render();
    }

    /// The system went light or dark: load the config's theme for it.
    pub fn set_appearance(&mut self, appearance: jet_config::Appearance) {
        if self.settings.dirs.appearance != appearance {
            self.settings.dirs.appearance = appearance;
            self.reload_config(true);
        }
    }

    fn perform(&mut self, action: Action) {
        let focused = self.focused_id();
        match action {
            Action::Copy => {
                if let Some(slot) = self.focused_slot() {
                    slot.pane.copy();
                }
            }
            Action::Paste if self.overlay.is_some() => {
                let text = self.host.clipboard().unwrap_or_default();
                let outcome = match &mut self.overlay {
                    Some(Overlay::Find(find)) => find.paste(&text),
                    Some(Overlay::Palette(palette)) => palette.paste(&text),
                    Some(Overlay::Rename(rename)) => rename.paste(&text),
                    None => Outcome::Nothing,
                };
                self.overlay_did(outcome);
            }
            Action::Paste => {
                if let Some(id) = focused {
                    self.paste(id);
                }
            }
            Action::NewTab(profile) => self.new_tab(profile.as_deref()),
            Action::CloseSurface => {
                if let Some(id) = focused
                    && !self.ask_close(Closing::Pane(id))
                {
                    self.close_pane(id);
                }
            }
            Action::CloseTab => self.ask_close_tab(self.active),
            Action::NextTab | Action::PreviousTab => {
                let n = self.tabs.len().max(1);
                let step = if action == Action::NextTab { 1 } else { n - 1 };
                self.activate_tab((self.active + step) % n);
            }
            Action::GotoTab(n) => {
                if n <= self.tabs.len() {
                    self.activate_tab(n - 1);
                }
            }
            Action::LastTab => self.activate_tab(self.tabs.len().saturating_sub(1)),
            Action::NewSplit(side, profile) => self.new_split(side, profile.as_deref()),
            Action::GotoSplit(target) => self.goto_split(target),
            Action::FontSize(points) => self.change_font_size(self.font_delta + points),
            Action::ResetFontSize => self.change_font_size(0.0),
            Action::CopyCommandOutput => {
                if let Some(slot) = self.focused_slot() {
                    slot.pane.copy_command_output();
                }
            }
            Action::JumpToPrompt(n) => {
                if let Some(slot) = self.focused_slot() {
                    slot.pane.jump_to_prompt(n);
                }
            }
            Action::CopyMode => {
                if let Some(slot) = self.focused_slot() {
                    slot.pane.copy_mode();
                }
            }
            Action::QuickSelect => {
                if let Some(slot) = self.focused_slot() {
                    slot.pane.quick_select();
                }
            }
            Action::ResizeSplit(dir, percent) => {
                if self.remote_resize(dir, percent) {
                    return;
                }
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.root.nudge(tab.focused, dir, f32::from(percent) / 100.0)
                {
                    self.relayout(false);
                    self.render();
                }
            }
            Action::EqualizeSplits => {
                // jetmux has no such thing.
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.remote.is_none()
                {
                    tab.root.equalize();
                    self.relayout(false);
                    self.render();
                }
            }
            Action::RenameTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let name = tab.title.as_deref().unwrap_or_default();
                    self.overlay = Some(Overlay::Rename(Rename::new(tab.id, name)));
                    self.render();
                }
            }
            Action::SetTabColor(TabColor::Pick) => self.choose_tab_color(),
            Action::SetTabColor(color) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.color = match color {
                        TabColor::Set(rgb) => Some(rgb),
                        TabColor::Pick | TabColor::Clear => None,
                    };
                    self.render();
                }
            }
            Action::MoveTab(n) => self.move_tab(n),
            Action::StartSearch => {
                if let Some(id) = focused
                    && !matches!(&self.overlay, Some(Overlay::Find(f)) if f.pane == id)
                {
                    self.close_overlay();
                    self.overlay = Some(Overlay::Find(Find::new(id)));
                    self.render();
                }
            }
            Action::EndSearch => {
                if matches!(self.overlay, Some(Overlay::Find(_))) {
                    self.close_overlay();
                }
            }
            Action::NavigateSearch(n) => self.overlay_did(Outcome::Step(n)),
            Action::ToggleCommandPalette => {
                let open = matches!(self.overlay, Some(Overlay::Palette(_)));
                self.close_overlay();
                if !open {
                    let commands = self.palette_commands();
                    self.open_palette(Palette::new("", commands));
                }
            }
            Action::ToggleSplitZoom => {
                if self.remote_act(jetmux_proto::Action::ToggleZoom) {
                    return;
                }
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && (tab.zoomed || tab.root.panes().len() > 1)
                {
                    tab.zoomed = !tab.zoomed;
                    self.relayout(false);
                    self.render();
                }
            }
            Action::ReloadConfig => self.reload_config(true),
            Action::Ignore => {}
            Action::Detach => self.detach(),
            Action::OpenConfig => self.open_config(),
            Action::ResetQuake => self.reconfigure_quake(),
            Action::ChooseTheme => self.choose_theme(),
            Action::NewTabMenu => self.new_tab_menu(),
            Action::CopyVersion => {
                if let Err(err) = self.host.set_clipboard(&version()) {
                    eprintln!("jet: copying the version: {err}");
                }
            }
            Action::Text(bytes) => {
                if let Some(slot) = focused.and_then(|id| self.panes.get(&id)) {
                    slot.pane.input(bytes);
                }
            }
        }
    }

    /// Unzoom the active tab, before something that needs its splits.
    fn unzoom(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && std::mem::take(&mut tab.zoomed)
        {
            self.relayout(false);
        }
    }

    /// Close the find bar (ending its search) or the palette.
    fn close_overlay(&mut self) {
        if let Some(Overlay::Find(find)) = self.overlay.take()
            && let Some(slot) = self.panes.get(&find.pane)
        {
            slot.pane.search(String::new(), false);
        }
        if let Some(before) = self.previewing.take() {
            self.apply_config(before);
        }
        self.render();
    }

    fn open_palette(&mut self, palette: Palette) {
        self.close_overlay();
        self.overlay = Some(Overlay::Palette(palette));
        self.render();
    }

    /// Ask, in the palette, `question` with these answers; the first is
    /// what Enter picks.
    fn ask(&mut self, question: &str, answers: Vec<(String, Choice)>) {
        let commands = answers
            .into_iter()
            .map(|(label, choice)| PaletteCommand {
                label,
                hint: String::new(),
                choice,
            })
            .collect();
        self.open_palette(Palette::new(question, commands));
    }

    /// Do what was picked in the palette, which has closed.
    fn pick(&mut self, choice: Choice) {
        match choice {
            Choice::Run(action) => self.perform(action),
            Choice::Close(Closing::Pane(id)) => self.close_pane(id),
            Choice::Close(Closing::Tab(id)) => {
                if let Some(index) = self.tabs.iter().position(|t| t.id == id) {
                    self.close_tab(index);
                }
            }
            Choice::Close(Closing::Window) => {
                self.close_confirmed = true;
                self.host.request(WindowRequest::Close);
            }
            Choice::Paste(id, text) => {
                if let Some(slot) = self.panes.get(&id) {
                    slot.pane.paste(text);
                }
            }
            Choice::Theme(name) => self.write_theme(&name),
            Choice::Cancel => {}
        }
    }

    /// With a theme selected in the palette, show it.
    fn preview_theme(&mut self) {
        let Some(Overlay::Palette(palette)) = &self.overlay else {
            return;
        };
        let (Some(Choice::Theme(name)), Some(_)) = (palette.current(), &self.previewing) else {
            return;
        };
        match jet_config::load_with_theme(&self.settings.dirs, name) {
            Ok(loaded) => self.apply_config(loaded.config),
            Err(err) => eprintln!("jet: previewing {name}: {err}"),
        }
    }

    /// The built-in themes and the user's, to pick from; each shows while
    /// selected.
    fn choose_theme(&mut self) {
        let themes = self.settings.dirs.themes();
        let current = self.active_theme().map(str::to_owned);
        let commands = themes
            .into_iter()
            .map(|name| PaletteCommand {
                hint: if current.as_deref() == Some(name.as_str()) {
                    "current".into()
                } else {
                    String::new()
                },
                choice: Choice::Theme(name.clone()),
                label: name,
            })
            .collect();
        let mut palette = Palette::new("theme", commands);
        if let Some(name) = current {
            palette.select(&Choice::Theme(name));
        }
        self.open_palette(palette);
        self.previewing = Some(self.settings.config.clone());
    }

    /// The `[theme]` key in effect, for the system's light or dark mode,
    /// and its theme.
    fn theme_key(&self) -> &'static str {
        let appearance = self.settings.dirs.appearance;
        self.settings.config.theme.key(appearance)
    }

    fn active_theme(&self) -> Option<&str> {
        let theme = &self.settings.config.theme;
        match self.theme_key() {
            "light" => theme.light.as_deref(),
            "dark" => theme.dark.as_deref(),
            _ => theme.name.as_deref(),
        }
    }

    /// Set the `[theme]` key in effect in the config file and load it.
    fn write_theme(&mut self, name: &str) {
        let key = self.theme_key();
        let written = (|| -> Result<(), String> {
            let path = self
                .settings
                .dirs
                .main_path()
                .ok_or("no place for a config file")?;
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => return Err(format!("{}: {err}", path.display())),
            };
            let new = jet_config::with_theme(&text, key, name).ok_or_else(|| {
                format!(
                    "set [theme] {key} = \"{name}\" by hand in {}",
                    path.display()
                )
            })?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&path, new).map_err(|e| format!("{}: {e}", path.display()))
        })();
        self.reload_config(true);
        let took = self.active_theme() == Some(name);
        let problem = match written {
            Err(err) => Some(format!("theme: {err}")),
            Ok(()) if !took && self.settings.error.is_none() => Some(format!(
                "theme: another file (an include?) sets the theme over {name}"
            )),
            Ok(()) => None,
        };
        if let Some(problem) = problem {
            self.settings.error = Some(problem);
            self.banner_dismissed = false;
            self.refresh_banner();
            self.render();
        }
    }

    /// The config file in an editor: `$VISUAL` or `$EDITOR` in a new tab,
    /// else the system's. A missing one starts as the documented defaults.
    fn open_config(&mut self) {
        let Some(path) = self.settings.dirs.main_path() else {
            eprintln!("jet: no place for a config file");
            return;
        };
        if !path.exists() {
            let made = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, jet_config::starter()));
            if let Err(err) = made {
                self.settings.error = Some(format!("{}: {err}", path.display()));
                self.banner_dismissed = false;
                self.refresh_banner();
                self.render();
                return;
            }
        }
        let editor = ["VISUAL", "EDITOR"]
            .into_iter()
            .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()));
        if let Some(editor) = editor {
            let mut argv = jet_config::Command::Line(editor).argv();
            argv.push(path.to_string_lossy().into_owned());
            let command = Some(jet_config::Command::Argv(argv));
            match self.open_tab(path.parent().map(Path::to_owned), None, command) {
                Ok(_) => return,
                Err(err) => eprintln!("jet: opening the config in $EDITOR: {err}"),
            }
        }
        self.host.open_file(&path);
    }

    /// Colors to give the active tab, over its profile's or host's.
    fn choose_tab_color(&mut self) {
        const COLORS: [(&str, u32); 9] = [
            ("Red", 0xe5484d),
            ("Orange", 0xf76b15),
            ("Yellow", 0xffc53d),
            ("Green", 0x46a758),
            ("Teal", 0x12a594),
            ("Blue", 0x0090ff),
            ("Purple", 0x8e4ec6),
            ("Pink", 0xd6409f),
            ("Grey", 0x8b8d98),
        ];
        let rgb = |c: u32| Rgb {
            r: (c >> 16) as u8,
            g: (c >> 8) as u8,
            b: c as u8,
        };
        let mut answers: Vec<(String, Choice)> = COLORS
            .into_iter()
            .map(|(name, c)| {
                let action = Action::SetTabColor(TabColor::Set(rgb(c)));
                (format!("{name} (#{c:06x})"), Choice::Run(action))
            })
            .collect();
        answers.push((
            "None: what the config gives it".into(),
            Choice::Run(Action::SetTabColor(TabColor::Clear)),
        ));
        self.ask("tab color", answers);
    }

    /// The profiles to open a tab with.
    fn new_tab_menu(&mut self) {
        let mut entries = vec![("Default shell".to_owned(), Action::NewTab(None))];
        for name in self.settings.config.profiles.keys() {
            entries.push((name.clone(), Action::NewTab(Some(name.clone()))));
        }
        let commands = entries
            .into_iter()
            .map(|(label, action)| PaletteCommand {
                hint: self.hint(&action),
                label,
                choice: Choice::Run(action),
            })
            .collect();
        self.open_palette(Palette::new("new tab", commands));
    }

    fn hint(&self, action: &Action) -> String {
        keybinds::chord_for(&self.bindings, action)
            .map(keybinds::chord_name)
            .unwrap_or_default()
    }

    /// What's running in pane `id` besides its shell, if anything.
    fn busy(&self, id: PaneId) -> Option<String> {
        let shell = self.panes.get(&id)?.pane.pid()?;
        let front = jet_pty::foreground(shell)?;
        // Console hosts and WSL's helper stay up beside an idle shell.
        let helper = ["conhost.exe", "openconsole.exe", "wslhost.exe"];
        let name = front.name.to_lowercase();
        (front.pid != shell && !helper.contains(&name.as_str())).then_some(front.name)
    }

    /// When something's running in `what` and the config says to, ask
    /// before closing it. Whether it asked (the caller doesn't close).
    fn ask_close(&mut self, what: Closing) -> bool {
        if !self.settings.config.window.confirm_close {
            return false;
        }
        let (panes, label) = match what {
            Closing::Pane(id) => {
                let alone = self.tab().is_some_and(|t| t.root.panes().len() == 1);
                (vec![id], if alone { "Close tab" } else { "Close split" })
            }
            Closing::Tab(id) => match self.tabs.iter().find(|t| t.id == id) {
                Some(tab) => (tab.root.panes(), "Close tab"),
                None => return false,
            },
            Closing::Window => (self.panes.keys().copied().collect(), "Close window"),
        };
        let Some(running) = panes.into_iter().find_map(|id| self.busy(id)) else {
            return false;
        };
        let answers = vec![
            (label.to_owned(), Choice::Close(what)),
            ("Cancel".to_owned(), Choice::Cancel),
        ];
        self.ask(&format!("{running} is running"), answers);
        true
    }

    fn ask_close_tab(&mut self, index: usize) {
        let Some(id) = self.tabs.get(index).map(|t| t.id) else {
            return;
        };
        if !self.ask_close(Closing::Tab(id)) {
            self.close_tab(index);
        }
    }

    /// The window was asked to close: whether to let it, rather than ask
    /// first.
    pub fn close_requested(&mut self) -> bool {
        self.host.quake()
            || std::mem::take(&mut self.close_confirmed)
            || !self.ask_close(Closing::Window)
    }

    /// Act on what a key did to the overlay.
    fn overlay_did(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Nothing => {}
            Outcome::Edited => {
                if let Some(Overlay::Find(find)) = &self.overlay
                    && let Some(slot) = self.panes.get(&find.pane)
                {
                    slot.pane.search(find.query().to_owned(), find.regex);
                }
                self.render();
                self.preview_theme();
            }
            Outcome::Moved => {
                self.render();
                self.preview_theme();
            }
            Outcome::Close => self.close_overlay(),
            Outcome::Step(n) => {
                if let Some(Overlay::Find(find)) = &self.overlay
                    && let Some(slot) = self.panes.get(&find.pane)
                {
                    slot.pane.search_step(n);
                }
            }
            Outcome::Pick(choice) => {
                // Keep a picked theme up while it's written.
                if matches!(choice, Choice::Theme(_)) {
                    self.previewing = None;
                }
                self.close_overlay();
                self.pick(choice);
            }
            Outcome::Rename(name) => {
                if let Some(Overlay::Rename(rename)) = &self.overlay
                    && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == rename.tab)
                {
                    if tab.remote.is_some() {
                        // The daemon names it (or, empty, goes back to titles).
                        self.remote_act(jetmux_proto::Action::RenameTab(name));
                    } else {
                        tab.title = (!name.is_empty()).then_some(name);
                    }
                }
                self.close_overlay();
            }
        }
    }

    /// Draw the open overlay over the focused pane's `snapshot`, with the
    /// cursor in it when the window has the keys.
    fn draw_overlay(&self, snapshot: &mut Snapshot, typing: bool) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        let (bg, fg) = (snapshot.colors.background, snapshot.colors.foreground);
        let colors = overlay::Colors {
            background: mix(bg, fg, 0.12),
            foreground: fg,
            selected: mix(bg, fg, 0.28),
        };
        match overlay {
            Overlay::Find(find) => find.draw(snapshot, colors, typing),
            Overlay::Palette(palette) => palette.draw(snapshot, colors, typing),
            Overlay::Rename(rename) => rename.draw(snapshot, colors, typing),
        }
    }

    /// The quake window per `[quake]` again, hotkey and place.
    fn reconfigure_quake(&mut self) {
        if !self.host.quake() {
            return;
        }
        let problem = self.host.reconfigure_quake(&self.settings.config.quake);
        if problem != self.hotkey_problem {
            self.hotkey_problem = problem;
            self.banner_dismissed = false;
            self.refresh_banner();
        }
    }

    /// What the palette offers: every action worth a name, the profiles,
    /// and the tabs to switch to.
    fn palette_commands(&self) -> Vec<PaletteCommand> {
        use Direction::{Down, Left, Right, Up};
        let mut list: Vec<(String, Action)> = vec![
            ("Find in scrollback".into(), Action::StartSearch),
            ("New tab".into(), Action::NewTab(None)),
        ];
        for name in self.settings.config.profiles.keys() {
            list.push((
                format!("New tab: {name}"),
                Action::NewTab(Some(name.clone())),
            ));
        }
        for (side, name) in [(Right, "right"), (Down, "down"), (Left, "left"), (Up, "up")] {
            list.push((format!("Split {name}"), Action::NewSplit(side, None)));
        }
        for name in self.settings.config.profiles.keys() {
            let action = Action::NewSplit(Right, Some(name.clone()));
            list.push((format!("Split right: {name}"), action));
        }
        list.extend([
            ("Toggle split zoom".into(), Action::ToggleSplitZoom),
            ("Next split".into(), Action::GotoSplit(SplitTarget::Next)),
            (
                "Previous split".into(),
                Action::GotoSplit(SplitTarget::Previous),
            ),
            ("Close split".into(), Action::CloseSurface),
            ("Close tab".into(), Action::CloseTab),
            ("Next tab".into(), Action::NextTab),
            ("Previous tab".into(), Action::PreviousTab),
        ]);
        if self.tab().is_some_and(|t| t.remote.is_some()) {
            list.push(("Detach jetmux session".into(), Action::Detach));
        }
        if self.host.quake() {
            list.push(("Quake: reset position".into(), Action::ResetQuake));
        }
        list.push((format!("About: {} (copy)", version()), Action::CopyVersion));
        list.extend([
            ("Open config".into(), Action::OpenConfig),
            ("Choose theme".into(), Action::ChooseTheme),
            ("New tab with profile".into(), Action::NewTabMenu),
        ]);
        for (i, tab) in self.tabs.iter().enumerate() {
            let (label, _) = self.tab_label(tab);
            list.push((
                format!("Go to tab {}: {label}", i + 1),
                Action::GotoTab(i + 1),
            ));
        }
        list.extend([
            ("Copy".into(), Action::Copy),
            ("Paste".into(), Action::Paste),
            (
                "Copy last command's output".into(),
                Action::CopyCommandOutput,
            ),
            ("Rename tab".into(), Action::RenameTab),
            ("Set tab color".into(), Action::SetTabColor(TabColor::Pick)),
            ("Move tab left".into(), Action::MoveTab(-1)),
            ("Move tab right".into(), Action::MoveTab(1)),
            ("Equalize splits".into(), Action::EqualizeSplits),
            ("Copy mode".into(), Action::CopyMode),
            ("Quick select".into(), Action::QuickSelect),
            ("Jump to previous prompt".into(), Action::JumpToPrompt(-1)),
            ("Jump to next prompt".into(), Action::JumpToPrompt(1)),
            ("Increase font size".into(), Action::FontSize(1.0)),
            ("Decrease font size".into(), Action::FontSize(-1.0)),
            ("Reset font size".into(), Action::ResetFontSize),
            ("Reload config".into(), Action::ReloadConfig),
        ]);
        list.into_iter()
            .map(|(label, action)| PaletteCommand {
                hint: self.hint(&action),
                label,
                choice: Choice::Run(action),
            })
            .collect()
    }

    /// A key for the open overlay, if one is: it takes them all.
    fn overlay_key(&mut self, input: &KeyInput) -> bool {
        let rows = self.focused_slot().map_or(24, |p| p.grid.1);
        let outcome = match &mut self.overlay {
            Some(Overlay::Find(find)) => find.key(input),
            Some(Overlay::Palette(palette)) => palette.key(input, rows),
            Some(Overlay::Rename(rename)) => rename.key(input),
            None => return false,
        };
        self.overlay_did(outcome);
        true
    }

    /// A click while the palette is open runs the command under it, or
    /// closes the palette when outside it. Whether the click was its.
    fn palette_click(&mut self, x: i32, y: i32) -> bool {
        let Some(Overlay::Palette(palette)) = &self.overlay else {
            return false;
        };
        let hit = self.focused_slot().and_then(|slot| {
            if !slot.rect.contains(x, y) {
                return None;
            }
            let (col, row) = self
                .renderer
                .cell_at(x - slot.rect.x, y - slot.rect.y, slot.grid);
            palette.at(slot.grid, col, row)
        });
        match hit {
            Some(Some(choice)) => self.overlay_did(Outcome::Pick(choice)),
            Some(None) => {}
            None => self.close_overlay(),
        }
        self.swallow_release = true;
        true
    }

    fn change_font_size(&mut self, delta: f32) {
        self.font_delta = delta.max(MIN_FONT_PT - self.settings.config.font.size);
        self.rebuild_renderer();
        self.relayout(true);
        self.render();
    }

    /// The grid a new pane starts with, until the layout resizes it.
    fn current_grid(&self) -> (u16, u16) {
        self.focused_slot()
            .map_or_else(|| self.first_grid(), |p| p.grid)
    }

    /// Where a new tab or split starts: where the focused shell last said it
    /// was, else its process's directory (right for cmd, but PowerShell
    /// doesn't move its process when it changes location).
    fn inherited_cwd(&self) -> Option<PathBuf> {
        let slot = self.focused_slot()?;
        slot.cwd
            .clone()
            .filter(|dir| dir.is_dir())
            .or_else(|| slot.pane.pid().and_then(jet_pty::process_cwd))
    }

    /// With `[shell] follow-ssh`, when no profile was asked for and the
    /// focused pane is running `ssh` or `wsl`: that again, into the
    /// directory its shell last reported.
    fn run_again(&self, profile: Option<&str>) -> Option<jet_config::Command> {
        if profile.is_some() || !self.settings.config.shell.follow_ssh {
            return None;
        }
        let slot = self.focused_slot()?;
        let process = jet_pty::foreground(slot.pane.pid()?)?;
        let argv = jet_config::Command::Line(process.command_line).argv();
        let location = slot.location.as_ref();
        let remote = location
            .filter(|l| {
                l.host
                    .as_deref()
                    .is_some_and(|h| !h.eq_ignore_ascii_case(&self.local_host))
            })
            .map(|l| l.path.as_str());
        // WSL shares the Windows host name: any Linux path will do.
        let linux = location
            .map(|l| l.path.as_str())
            .filter(|path| path.starts_with('/'));
        status::ssh_again(&argv, remote, None)
            .or_else(|| status::wsl_again(&argv, linux))
            .map(jet_config::Command::Argv)
    }

    fn new_tab(&mut self, profile: Option<&str>) {
        if let Some((name, spec)) = self.jetmux_profile(profile) {
            self.open_jetmux(&name, &spec);
            return;
        }
        let command = self.run_again(profile);
        let profile = self.or_default(profile, &command);
        if let Err(err) = self.open_tab(self.inherited_cwd(), profile.as_deref(), command) {
            eprintln!("jet: opening a tab: {err}");
        }
    }

    /// `profile`, else `[shell] profile` unless following ssh.
    fn or_default(
        &self,
        profile: Option<&str>,
        again: &Option<jet_config::Command>,
    ) -> Option<String> {
        let default = again
            .is_none()
            .then(|| self.settings.config.shell.profile.clone());
        profile.map(str::to_owned).or(default.flatten())
    }

    fn new_split(&mut self, side: Direction, profile: Option<&str>) {
        let split = jetmux_proto::Action::Split(jetmux::wire_direction(side), Default::default());
        if self.remote_act(split) {
            return;
        }
        if let Some((name, spec)) = self.jetmux_profile(profile) {
            self.open_jetmux(&name, &spec);
            return;
        }
        let Some(target) = self.focused_id() else {
            return;
        };
        let command = self.run_again(profile);
        let profile = self.or_default(profile, &command);
        let cwd = self.inherited_cwd();
        if let Err(err) = self.open_split(target, side, cwd, profile.as_deref(), command) {
            eprintln!("jet: opening a split: {err}");
        }
    }

    /// Save the session for next time, when restoring is on, if it changed
    /// (or the window is `closing`: the last to close wins).
    fn save_session(&mut self, closing: bool) {
        self.saved_at = Instant::now();
        if !self.settings.config.window.restore || self.host.quake() {
            return;
        }
        let session = self.session();
        if closing || self.saved.as_ref() != Some(&session) {
            session.save();
            self.saved = Some(session);
        }
    }

    /// The tabs and splits to open next time.
    fn session(&self) -> Session {
        // A jetmux session keeps its own tabs.
        let local: Vec<&Tab> = self.tabs.iter().filter(|t| t.remote.is_none()).collect();
        let active = self
            .tab()
            .and_then(|front| local.iter().position(|t| t.id == front.id))
            .unwrap_or(0);
        let tabs = local
            .into_iter()
            .map(|tab| session::Tab {
                title: tab.title.clone(),
                color: tab.color.map(|c| [c.r, c.g, c.b]),
                root: self.saved(&tab.root),
                focused: tab
                    .root
                    .panes()
                    .iter()
                    .position(|id| *id == tab.focused)
                    .unwrap_or(0),
            })
            .collect();
        Session { tabs, active }
    }

    fn saved(&self, node: &Node) -> session::Node {
        match node {
            Node::Leaf(id) => {
                let slot = self.panes.get(id);
                session::Node::Pane {
                    cwd: slot.and_then(|slot| {
                        slot.cwd
                            .clone()
                            .filter(|dir| dir.is_dir())
                            .or_else(|| slot.pane.pid().and_then(jet_pty::process_cwd))
                    }),
                    profile: slot.and_then(|slot| slot.profile.clone()),
                }
            }
            Node::Split {
                horizontal,
                ratio,
                first,
                second,
            } => session::Node::Split {
                horizontal: *horizontal,
                ratio: *ratio,
                first: Box::new(self.saved(first)),
                second: Box::new(self.saved(second)),
            },
        }
    }

    /// Open `session`'s tabs; the first tab's first shell, `first`, is
    /// already running in the only tab.
    fn restore(&mut self, session: Session, first: PaneId) {
        let mut first = Some(first);
        let mut tabs = Vec::new();
        for saved in session.tabs {
            // The tab already open keeps its id.
            let id = if tabs.is_empty() {
                self.tabs.first().map(|t| t.id)
            } else {
                None
            };
            let Some(root) = self.rebuild(&saved.root, &mut first) else {
                continue;
            };
            let panes = root.panes();
            let Some(&lead) = panes.first() else {
                continue;
            };
            let mut tab = Tab::new(id.unwrap_or_else(|| self.take_tab_id()), lead);
            tab.focused = panes.get(saved.focused).copied().unwrap_or(lead);
            tab.title = saved.title;
            tab.color = saved.color.map(|[r, g, b]| Rgb { r, g, b });
            tab.root = root;
            tabs.push(tab);
        }
        if tabs.is_empty() {
            return;
        }
        self.tabs = tabs;
        self.active = session.active.min(self.tabs.len() - 1);
    }

    /// `saved`'s splits with a shell in each pane, `first` (once) being the
    /// one already started. A pane that won't start leaves its sibling.
    fn rebuild(&mut self, saved: &session::Node, first: &mut Option<PaneId>) -> Option<Node> {
        match saved {
            session::Node::Pane { cwd, profile } => {
                if let Some(id) = first.take() {
                    return Some(Node::Leaf(id));
                }
                let cwd = cwd.clone().filter(|dir| dir.is_dir());
                let profile = profile
                    .as_deref()
                    .filter(|name| self.settings.config.profiles.contains_key(*name));
                match self.spawn(self.first_grid(), cwd, profile, None) {
                    Ok(id) => Some(Node::Leaf(id)),
                    Err(err) => {
                        eprintln!("jet: restoring a shell: {err}");
                        None
                    }
                }
            }
            session::Node::Split {
                horizontal,
                ratio,
                first: a,
                second: b,
            } => match (self.rebuild(a, first), self.rebuild(b, first)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    horizontal: *horizontal,
                    ratio: ratio.clamp(0.05, 0.95),
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (a, b) => a.or(b),
            },
        }
    }

    /// Profile `name`, when it shows a jetmux session.
    fn jetmux_profile(&self, name: Option<&str>) -> Option<(String, jet_config::Jetmux)> {
        let name = name?;
        let spec = self.settings.config.profiles.get(name)?.jetmux.clone()?;
        Some((name.to_owned(), spec))
    }

    /// jetmux sessions open their own tabs, when they're ready.
    fn refuse_jetmux(&self, profile: Option<&str>) -> Result<(), BoxError> {
        match self.jetmux_profile(profile) {
            Some((name, _)) => Err(format!("{name} is a jetmux session: open it from jet").into()),
            None => Ok(()),
        }
    }

    fn take_tab_id(&mut self) -> u64 {
        self.next_tab += 1;
        self.next_tab - 1
    }

    fn open_tab(
        &mut self,
        cwd: Option<PathBuf>,
        profile: Option<&str>,
        command: Option<jet_config::Command>,
    ) -> Result<PaneId, BoxError> {
        self.refuse_jetmux(profile)?;
        let id = self.spawn(self.current_grid(), cwd, profile, command)?;
        let tab = self.take_tab_id();
        self.tabs.push(Tab::new(tab, id));
        self.activate_tab(self.tabs.len() - 1);
        // The bar may have just appeared.
        self.relayout(false);
        self.render();
        self.emit(What::PaneOpened, Some(id));
        Ok(id)
    }

    /// Split `target`, in the active tab.
    fn open_split(
        &mut self,
        target: PaneId,
        side: Direction,
        cwd: Option<PathBuf>,
        profile: Option<&str>,
        command: Option<jet_config::Command>,
    ) -> Result<PaneId, BoxError> {
        self.refuse_jetmux(profile)?;
        if self.tab().is_some_and(|t| t.remote.is_some()) {
            return Err("that's a jetmux tab: split it from jet".into());
        }
        let id = self.spawn(self.current_grid(), cwd, profile, command)?;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.root.split(target, id, side);
            tab.zoomed = false;
        }
        self.relayout(false);
        self.focus_pane(id);
        self.emit(What::PaneOpened, Some(id));
        Ok(id)
    }

    fn goto_split(&mut self, target: SplitTarget) {
        self.unzoom();
        let Some(tab) = self.tab() else {
            return;
        };
        let order = tab.root.panes();
        let at = order.iter().position(|id| *id == tab.focused).unwrap_or(0);
        let next = match target {
            SplitTarget::Next => order.get((at + 1) % order.len().max(1)).copied(),
            SplitTarget::Previous => order
                .get((at + order.len().saturating_sub(1)) % order.len().max(1))
                .copied(),
            SplitTarget::Toward(dir) => {
                let rects: Vec<(PaneId, Rect)> = order
                    .iter()
                    .filter_map(|id| Some((*id, self.panes.get(id)?.rect)))
                    .collect();
                layout::neighbor(&rects, tab.focused, dir)
            }
        };
        if let Some(id) = next {
            self.focus_pane(id);
        }
    }

    /// Give `id`, in the active tab, the keyboard.
    fn focus_pane(&mut self, id: PaneId) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let old = std::mem::replace(&mut tab.focused, id);
        if old != id && std::mem::take(&mut tab.zoomed) {
            self.relayout(false);
        }
        if old != id && self.focused {
            if let Some(slot) = self.panes.get(&old) {
                slot.pane.focus(false);
            }
            if let Some(slot) = self.panes.get(&id) {
                slot.pane.focus(true);
            }
        }
        self.after_focus_change();
    }

    /// Move the active tab `n` places right (negative, left), wrapping.
    fn move_tab(&mut self, n: isize) {
        let len = self.tabs.len();
        if len < 2 {
            return;
        }
        let to = (self.active as isize + n).rem_euclid(len as isize) as usize;
        let tab = self.tabs.remove(self.active);
        self.tabs.insert(to, tab);
        self.active = to;
        self.render();
    }

    fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let old = self.focused_id();
        // Divider indexes are per tab.
        self.drag = None;
        self.active = index;
        let new = self.focused_id();
        if old != new && self.focused {
            for (id, on) in [(old, false), (new, true)] {
                if let Some(slot) = id.and_then(|id| self.panes.get(&id)) {
                    slot.pane.focus(on);
                }
            }
        }
        self.after_focus_change();
    }

    fn after_focus_change(&mut self) {
        // The find bar belongs to its pane, the palette to the moment.
        let keep = match &self.overlay {
            Some(Overlay::Find(find)) => Some(find.pane) == self.focused_id(),
            Some(Overlay::Palette(_) | Overlay::Rename(_)) => false,
            None => true,
        };
        if !keep {
            self.close_overlay();
        }
        if let Some(id) = self.focused_id() {
            self.emit(What::Focus, Some(id));
        }
        self.blink_timer_restart();
        self.refresh_banner();
        self.update_title();
        self.sync_remotes();
        self.render();
    }

    fn blink_timer_restart(&mut self) {
        self.reset_blink();
        self.sync_blink();
    }

    /// Close a pane; its tab closes with its last pane, the window with the
    /// last tab.
    fn close_pane(&mut self, id: PaneId) {
        if self.close_remote_pane(id) {
            return;
        }
        let Some(index) = self.tabs.iter().position(|t| t.root.panes().contains(&id)) else {
            return;
        };
        self.emit(What::PaneClosed, Some(id));
        // Dropping the pane ends its shell; its tab's dividers renumber.
        self.panes.remove(&id);
        self.drag = None;
        if self.mouse_pane == Some(id) {
            self.mouse_pane = None;
        }
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let root = std::mem::replace(&mut tab.root, Node::Leaf(0));
        tab.zoomed = false;
        match root.without(id) {
            Some(root) => {
                let next = if tab.focused == id {
                    root.panes().first().copied()
                } else {
                    None
                };
                tab.root = root;
                self.relayout(false);
                if let Some(next) = next {
                    if index == self.active {
                        self.focus_pane(next);
                    } else if let Some(tab) = self.tabs.get_mut(index) {
                        tab.focused = next;
                    }
                }
                self.render();
            }
            None => self.remove_tab(index),
        }
    }

    fn close_tab(&mut self, index: usize) {
        if self.close_remote_tab(index) {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        for id in tab.root.panes() {
            self.emit(What::PaneClosed, Some(id));
            self.panes.remove(&id);
        }
        self.remove_tab(index);
    }

    /// Drop an emptied tab; the window goes with the last one.
    fn remove_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // Async: closing re-enters the window procedure.
            self.host.request(WindowRequest::Close);
            return;
        }
        if index == self.active {
            // Its panes are gone: the tab taking its place gets the focus.
            self.active = usize::MAX;
            self.activate_tab(index.min(self.tabs.len() - 1));
        } else if self.active > index {
            self.active -= 1;
        }
        self.relayout(false);
        self.render();
    }

    fn paste(&mut self, id: PaneId) {
        let Some(slot) = self.panes.get(&id) else {
            return;
        };
        let Some(text) = self.host.clipboard() else {
            eprintln!("jet: nothing to paste (clipboard empty or busy)");
            return;
        };
        let bracketed = slot.snapshot.as_ref().is_some_and(|s| s.bracketed_paste);
        let protect = self.settings.config.terminal.paste_protection;
        if !protect || bracketed || !text.contains(['\n', '\r']) {
            slot.pane.paste(text);
            return;
        }
        let lines = text.trim_end().lines().count().max(1);
        let what = if lines == 1 {
            "a line".to_owned()
        } else {
            format!("{lines} lines")
        };
        let answers = vec![
            ("Paste".to_owned(), Choice::Paste(id, text)),
            ("Cancel".to_owned(), Choice::Cancel),
        ];
        self.ask(&format!("paste {what}, which may run?"), answers);
    }

    /// The active tab's pane under client `(x, y)`.
    fn pane_at(&self, x: i32, y: i32) -> Option<PaneId> {
        let tab = self.tab()?;
        tab.shown()
            .into_iter()
            .find(|id| self.panes.get(id).is_some_and(|p| p.rect.contains(x, y)))
    }

    /// A mouse button or move at client `(x, y)`; negative past the left or
    /// top edge while captured.
    fn mouse(&mut self, action: MouseAction, x: i32, y: i32) {
        self.pointer = (x, y);
        let bit = match action {
            MouseAction::Press(b) | MouseAction::Release(b) => match b {
                MouseButton::Left => 1,
                MouseButton::Middle => 2,
                MouseButton::Right => 4,
            },
            MouseAction::Move => 0,
        };
        if let Some((px, py)) = self.tab_press {
            match action {
                MouseAction::Move => {
                    let (fx, fy) = self.host.drag_threshold();
                    if (x - px).abs() > fx || (y - py).abs() > fy {
                        self.tab_press = None;
                        self.host.request(WindowRequest::Move);
                    }
                }
                MouseAction::Release(MouseButton::Left) => {
                    self.tab_press = None;
                    self.host.capture(false);
                }
                _ => {}
            }
            return;
        }
        if let MouseAction::Release(_) = action
            && std::mem::take(&mut self.swallow_release)
        {
            return;
        }
        if let MouseAction::Press(_) = action
            && self.buttons == 0
            && self.palette_click(x, y)
        {
            return;
        }
        if self.buttons == 0 && self.drag.is_none() {
            match action {
                MouseAction::Move => self.hover_link(),
                MouseAction::Press(MouseButton::Left) => {
                    if let Some(target) = self.link_at(x, y) {
                        self.click_target(target);
                        self.swallow_release = true;
                        return;
                    }
                }
                _ => {}
            }
        }
        // The tab bar: left click switches, middle click closes.
        if self.hover.is_some() {
            self.set_hover(None);
        }
        if matches!(action, MouseAction::Move) && self.buttons == 0 && self.drag.is_none() {
            self.hover_tab(x, y);
        }
        if let MouseAction::Press(button) = action
            && self.buttons == 0
            && self.chrome().tabs.contains(x, y)
        {
            if let Some((i, on_close)) = self.tab_hit(x, y) {
                match button {
                    MouseButton::Left if on_close => self.ask_close_tab(i),
                    // The window drags by its tabs too.
                    MouseButton::Left => {
                        self.activate_tab(i);
                        self.host.capture(true);
                        self.tab_press = Some((x, y));
                        return;
                    }
                    MouseButton::Middle => self.ask_close_tab(i),
                    MouseButton::Right => self.new_tab_menu(),
                }
            } else if button == MouseButton::Right {
                self.new_tab_menu();
            }
            self.swallow_release = true;
            return;
        }
        // A divider drag, under way or starting.
        if self.drag.is_some() {
            match action {
                MouseAction::Move => self.drag_to(x, y),
                MouseAction::Release(MouseButton::Left) => {
                    self.drag = None;
                    self.host.capture(false);
                }
                _ => {}
            }
            return;
        }
        if let MouseAction::Press(MouseButton::Left) = action
            && self.buttons == 0
            && let Some(divider) = self.divider_at(x, y)
        {
            self.host.capture(true);
            self.drag = Some(divider);
            return;
        }
        // A drag stays with the pane it started in.
        let target = if self.buttons != 0 {
            self.mouse_pane
        } else {
            self.pane_at(x, y)
        };
        let Some(id) = target else {
            return;
        };
        let Some(rect) = self.panes.get(&id).map(|p| p.rect) else {
            return;
        };
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let (gx, gy) = (x - rect.x - pad, y - rect.y - pad);
        if let MouseAction::Press(_) = action
            && self.buttons == 0
            && !self.banner.is_empty()
            && Some(id) == self.focused_id()
            && (0..self.banner.len() as i32 * m.height as i32).contains(&gy)
        {
            self.banner_dismissed = true;
            self.banner.clear();
            self.swallow_release = true;
            self.render();
            return;
        }
        match action {
            MouseAction::Press(_) => {
                if self.buttons == 0 {
                    self.host.capture(true);
                    self.mouse_pane = Some(id);
                    if Some(id) != self.focused_id() {
                        self.focus_pane(id);
                    }
                }
                self.buttons |= bit;
            }
            MouseAction::Release(_) => {
                self.buttons &= !bit;
                if self.buttons == 0 {
                    self.stop_autoscroll();
                    // Windows: its WM_CAPTURECHANGED arrives while App is
                    // borrowed and gets default handling, as wanted here.
                    self.host.capture(false);
                }
            }
            MouseAction::Move => {
                // Half cells: selection snaps at cell midpoints.
                let w = (m.width / 2).max(1) as i32;
                let key = (id, gx.div_euclid(w), gy.div_euclid(m.height.max(1) as i32));
                if self.last_move == Some(key) {
                    return;
                }
                self.last_move = Some(key);
                let rows = self.panes.get(&id).map_or(0, |p| i32::from(p.grid.1));
                if self.buttons & 1 != 0 && (gy < 0 || gy >= rows * m.height as i32) {
                    self.start_autoscroll();
                } else {
                    self.stop_autoscroll();
                }
            }
        }
        if let Some(slot) = self.panes.get(&id) {
            slot.pane.mouse(MouseInput {
                action,
                x: f64::from(gx),
                y: f64::from(gy),
                mods: self.host.mods(),
            });
        }
    }

    /// What the mouse should look like at client `(x, y)`: resize arrows
    /// over dividers, an arrow over the bars, else an I-beam.
    fn pointer_at(&self, x: i32, y: i32) -> Pointer {
        if self.link_at(x, y).is_some() {
            return Pointer::Link;
        }
        match self.divider_at(x, y) {
            Some((_, d)) if d.horizontal => Pointer::ResizeColumns,
            Some(_) => Pointer::ResizeRows,
            None => {
                let chrome = self.chrome();
                if chrome.tabs.contains(x, y) || chrome.caption.contains(x, y) {
                    return Pointer::Arrow;
                }
                let asked = self.pane_at(x, y).and_then(|id| self.panes.get(&id));
                match asked.map(|slot| slot.pointer) {
                    Some(shape) if shape != jet_core::PointerShape::RESET => Pointer::Shape(shape),
                    _ => Pointer::Text,
                }
            }
        }
    }

    /// The mouse left the window: drop hover highlights.
    fn mouse_left(&mut self) {
        let tab = self.tab_hover.take().is_some();
        let button = self.hover.take().is_some();
        let link = self.link.take().is_some();
        if tab || button || link {
            self.render();
        }
    }

    /// What a click at client `(x, y)` would open, if the link modifier
    /// is held: a `[plumb]` rule's match, else a URL.
    fn link_at(&self, x: i32, y: i32) -> Option<Target> {
        if !self.host.mods().contains(LINK_MODS) {
            return None;
        }
        let pane = self.pane_at(x, y)?;
        let slot = self.panes.get(&pane)?;
        let (col, row) = self
            .renderer
            .cell_at(x - slot.rect.x, y - slot.rect.y, slot.grid);
        let snapshot = slot.snapshot.as_ref()?;
        let rules = &self.settings.config.plumb;
        if !rules.is_empty()
            && let Some(line) = snapshot.line_text(col, row)
        {
            // Looking up what's in front costs a process walk: only when a
            // rule cares.
            let picky = rules.values().any(|r| r.on != plumb::On::Any);
            let remote = picky && self.ssh_in(slot).is_some();
            if let Some(hit) = plumb::hit(rules, remote, &line.text, line.at) {
                return Some(Target {
                    pane,
                    cells: line.cells(hit.range.clone()),
                    what: Clicked::Plumb(hit),
                });
            }
        }
        let link = snapshot.url_at(col, row)?;
        Some(Target {
            pane,
            cells: link.cells,
            what: Clicked::Url(link.url),
        })
    }

    /// The `ssh` command line running in front in `slot`, if any.
    fn ssh_in(&self, slot: &PaneSlot) -> Option<Vec<String>> {
        let process = jet_pty::foreground(slot.pane.pid()?)?;
        let argv = jet_config::Command::Line(process.command_line).argv();
        jet_core::ssh_target(&argv).map(|_| argv)
    }

    /// Where `slot`'s shell last said it was, when that's another host.
    fn remote_dir<'s>(&self, slot: &'s PaneSlot) -> Option<&'s str> {
        slot.location
            .as_ref()
            .filter(|l| {
                l.host
                    .as_deref()
                    .is_some_and(|h| !h.eq_ignore_ascii_case(&self.local_host))
            })
            .map(|l| l.path.as_str())
    }

    fn click_target(&mut self, target: Target) {
        match target.what {
            Clicked::Url(url) => self.host.open_url(&url),
            Clicked::Plumb(hit) => {
                if let Err(err) = self.plumb(target.pane, &hit) {
                    eprintln!("jet: plumb.{}: {err}", hit.rule);
                }
            }
        }
    }

    /// Do what `hit`'s rule says with the text it matched in `pane`.
    fn plumb(&mut self, pane: PaneId, hit: &plumb::Hit) -> Result<(), BoxError> {
        let config = &self.settings.config;
        let rule = config.plumb.get(&hit.rule).ok_or("the rule is gone")?;
        let action = rule.action().ok_or("no action")?;
        let slot = self.panes.get(&pane).ok_or("the pane is gone")?;
        let ssh = self.ssh_in(slot);
        let remote_dir = ssh.as_ref().and_then(|_| self.remote_dir(slot));
        let local_dir = slot
            .cwd
            .clone()
            .filter(|d| d.is_dir())
            .or_else(|| slot.pane.pid().and_then(jet_pty::process_cwd));
        let cwd = match (&ssh, remote_dir, &local_dir) {
            (Some(_), dir, _) => dir.unwrap_or_default().to_owned(),
            (None, _, dir) => dir
                .as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
        };
        let host = slot
            .context
            .as_ref()
            .and_then(jet_core::Context::host)
            .map(str::to_owned)
            .or_else(|| slot.location.as_ref().and_then(|l| l.host.clone()))
            .unwrap_or_else(|| self.local_host.clone());
        let var = |name: &str| match name {
            "cwd" => Some(cwd.clone()),
            "host" => Some(host.clone()),
            _ => hit.vars.get(name).cloned(),
        };
        // Filled a word at a time: matched text never becomes more words.
        let fill = |words: Vec<String>| -> Result<Vec<String>, BoxError> {
            let filled = words.iter().map(|w| plumb::fill(w, var));
            Ok(filled.collect::<Result<_, _>>()?)
        };
        match action {
            plumb::Action::Open(url) => {
                let url = plumb::fill(url, var)?;
                // Only what can't run a program hidden behind the text.
                let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
                if !matches!(scheme.as_deref(), Some("http" | "https" | "mailto")) {
                    return Err(format!("{url:?} isn't an http(s) or mailto URL").into());
                }
                self.host.open_url(&url);
            }
            plumb::Action::Run(command) => {
                run_detached(&fill(command.argv())?, local_dir.as_deref())?;
            }
            plumb::Action::View(command, place) => {
                let words = fill(command.argv())?;
                let argv = match &ssh {
                    Some(ssh) => {
                        let line = words
                            .iter()
                            .map(|w| status::sh_quote(w))
                            .collect::<Vec<_>>();
                        status::ssh_again(ssh, remote_dir, Some(&line.join(" ")))
                            .ok_or("not an ssh command line")?
                    }
                    None => words,
                };
                let command = Some(jet_config::Command::Argv(argv));
                let side = match place {
                    plumb::Place::Right => Direction::Right,
                    plumb::Place::Down => Direction::Down,
                    plumb::Place::Left => Direction::Left,
                    plumb::Place::Up => Direction::Up,
                    plumb::Place::Tab => {
                        self.open_tab(local_dir, None, command)?;
                        return Ok(());
                    }
                };
                self.open_split(pane, side, local_dir, None, command)?;
            }
        }
        Ok(())
    }

    /// Underline the URL under the mouse, or stop: the mouse moved or the
    /// modifiers changed.
    fn hover_link(&mut self) {
        let link = self.link_at(self.pointer.0, self.pointer.1);
        if link != self.link {
            self.link = link;
            self.render();
        }
    }

    /// Tell the input method where the focused pane's cursor is, when that
    /// moved.
    fn report_ime_area(&mut self, focused: PaneId) {
        let Some(slot) = self.panes.get(&focused) else {
            return;
        };
        let Some(cursor) = slot.snapshot.as_ref().and_then(|s| s.cursor) else {
            return;
        };
        let m = self.renderer.cell_metrics();
        let pad = self.renderer.padding() as i32;
        let (w, h) = (m.width as i32, m.height as i32);
        let area = Rect {
            x: slot.rect.x + pad + i32::from(cursor.x) * w,
            y: slot.rect.y + pad + i32::from(cursor.y) * h,
            w,
            h,
        };
        if self.ime_area != Some(area) {
            self.ime_area = Some(area);
            self.host.ime_area(area);
        }
    }

    /// Wheel `notches` (positive away from the user, as fractions from
    /// touchpads) over client `(x, y)`.
    fn wheel(&mut self, notches: f64, x: i32, y: i32) {
        self.wheel_rest += notches;
        let whole = self.wheel_rest.trunc();
        if whole == 0.0 {
            return;
        }
        self.wheel_rest -= whole;
        let Some(slot) = self.pane_at(x, y).and_then(|id| self.panes.get(&id)) else {
            return;
        };
        let (x, y) = (x - slot.rect.x, y - slot.rect.y);
        let cell = self.renderer.cell_at(x, y, slot.grid);
        // Away from the user scrolls up, toward older output.
        slot.pane.wheel(-(whole as i32), cell, self.host.mods());
    }

    /// Run `key`'s binding with `mods`, if it has one.
    fn run_binding(&mut self, key: Key, mods: Mods) -> bool {
        if let Some(bound) = keybinds::find(&self.bindings, key, mods)
            && let Some(action) = bound
        {
            self.perform(action);
            return true;
        }
        false
    }

    /// A key for the focused pane to encode.
    fn send_key(&mut self, input: KeyInput) {
        if self.overlay_key(&input) {
            return;
        }
        if let Some(slot) = self.focused_slot() {
            slot.pane.key(input);
        }
    }

    fn lost_capture(&mut self) {
        self.drag = None;
        self.tab_press = None;
        if self.buttons & 1 != 0 {
            self.stop_autoscroll();
            if let Some(slot) = self.mouse_pane.and_then(|id| self.panes.get(&id)) {
                slot.pane.mouse(MouseInput {
                    action: MouseAction::Release(MouseButton::Left),
                    x: -1.0,
                    y: -1.0,
                    mods: Mods::empty(),
                });
            }
        }
        self.buttons = 0;
    }

    fn start_autoscroll(&mut self) {
        if !self.autoscroll_timer {
            self.autoscroll_timer = true;
            self.host.set_timer(Timer::Autoscroll, AUTOSCROLL_EVERY);
        }
    }

    fn stop_autoscroll(&mut self) {
        if std::mem::take(&mut self.autoscroll_timer) {
            self.host.kill_timer(Timer::Autoscroll);
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        if let Some(slot) = self.focused_slot() {
            slot.pane.focus(focused);
        }
        self.reset_blink();
        self.sync_blink();
        self.render();
    }

    fn timer(&mut self, timer: Timer) {
        match timer {
            Timer::Blink => {
                self.blink_on = !self.blink_on;
                self.render();
            }
            Timer::Autoscroll => {
                if let Some(slot) = self.mouse_pane.and_then(|id| self.panes.get(&id)) {
                    slot.pane.autoscroll_tick();
                }
            }
            Timer::Config => {
                self.reload_config(false);
                self.poll_session();
                if self.saved_at.elapsed() >= SESSION_EVERY {
                    self.save_session(false);
                }
            }
            Timer::Flash => {
                self.host.kill_timer(Timer::Flash);
                self.renderer.set_flash(None);
                self.render();
            }
        }
    }

    /// Show the cursor and restart the blink countdown.
    fn reset_blink(&mut self) {
        self.blink_on = true;
        if self.blink_timer {
            self.blink_timer = false;
            self.sync_blink();
        }
    }

    /// Run the blink timer only while a focused cursor asks to blink, so an
    /// idle window costs nothing.
    fn sync_blink(&mut self) {
        let interval = self.host.blink_interval();
        let want = self.focused
            && interval.is_some()
            && self
                .focused_slot()
                .and_then(|p| p.snapshot.as_ref())
                .and_then(|s| s.cursor)
                .is_some_and(|c| c.blinking);
        if want && !self.blink_timer {
            let every = interval.unwrap_or_default().max(Duration::from_millis(100));
            self.host.set_timer(Timer::Blink, every);
        } else if !want && self.blink_timer {
            self.host.kill_timer(Timer::Blink);
            self.blink_on = true;
        }
        self.blink_timer = want;
    }

    /// In front: a brief white wash. Behind: flash the taskbar button until
    /// the window is brought back.
    fn bell(&mut self) {
        let now = Instant::now();
        if self.last_bell.is_some_and(|t| now - t < BELL_GAP) {
            return;
        }
        self.last_bell = Some(now);
        let bell = &self.settings.config.terminal.bell;
        let (flash, attention) = (bell.contains(&Bell::Flash), bell.contains(&Bell::Attention));
        if bell.contains(&Bell::Sound) {
            self.host.beep();
        }
        if self.focused && flash {
            self.renderer.set_flash(Some(FLASH));
            self.render();
            self.host.set_timer(Timer::Flash, FLASH_FOR);
        } else if !self.focused && attention {
            self.host.request(WindowRequest::Attention);
        }
    }
}

/// A renderer for `config`'s fonts at `size_pt` and `dpi`, and why the
/// configured font couldn't be used, if it couldn't.
/// The first pane's id; later ones count up from it.
const FIRST_PANE: PaneId = 1;

/// The first shell, started before the window: on macOS the event loop and
/// window take ~150 ms, which the shell's own startup can use. Its wakes go
/// nowhere until [`App::new`] takes it.
/// What a window opens with, got ready before it.
#[derive(Default)]
pub struct Start {
    early: Option<EarlyShell>,
    session: Option<Session>,
    first: First,
}

/// What the first tab runs: from the command line, a restored session, or
/// the config's default profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct First {
    pub cwd: Option<PathBuf>,
    pub profile: Option<String>,
    /// Over the profile's command.
    pub command: Option<Vec<String>>,
}

/// What `jet quake` started in the background is run with: it stays
/// hidden until the hotkey, and leaves a running one alone.
pub const BACKGROUND: &str = "--background";

pub const USAGE: &str = "usage: jet [DIR] [--profile NAME] [-e COMMAND [ARGS...]]
       jet quake
       jet config check
       jet --version";

impl First {
    /// `jet [DIR] [--profile NAME] [-e COMMAND...]`: everything after `-e`
    /// is the command.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut first = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-e" => {
                    let argv: Vec<String> = args.by_ref().cloned().collect();
                    if argv.is_empty() {
                        return Err("-e needs a command".into());
                    }
                    first.command = Some(argv);
                }
                "--profile" => {
                    let name = args.next().ok_or("--profile needs a name")?;
                    first.profile = Some(name.clone());
                }
                flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
                dir => {
                    if first.cwd.is_some() {
                        return Err(format!("one directory only, not {dir} too"));
                    }
                    let path = std::path::absolute(dir).map_err(|e| format!("{dir}: {e}"))?;
                    if !path.is_dir() {
                        return Err(format!("{dir} isn't a directory"));
                    }
                    first.cwd = Some(path);
                }
            }
        }
        Ok(first)
    }

    /// Its command, else its profile's.
    fn launch<'a>(&'a self, config: &'a Config) -> (Option<PathBuf>, Option<jet_config::Command>) {
        let (cwd, profiled) = launch(config, self.cwd.clone(), self.profile.as_deref());
        let asked = self.command.clone().map(jet_config::Command::Argv);
        (cwd, asked.or_else(|| profiled.cloned()))
    }
}

impl Start {
    /// The saved session, when it's to be restored, and its first shell
    /// started early. A shell that fails here is retried, and reported, by
    /// the window's own spawn.
    /// `asked`: what the command line said, which skips restoring.
    pub fn new(config: &Config, quake: bool, asked: First) -> Self {
        let restore = config.window.restore && !quake && asked == First::default();
        let session = restore.then(Session::load).flatten();
        let first = match &session {
            Some(session) => {
                let (cwd, profile) = session.first();
                First {
                    cwd,
                    profile,
                    command: None,
                }
            }
            None => {
                let quake = quake.then(|| config.quake.profile.clone()).flatten();
                let default = quake
                    .or_else(|| config.window.profile.clone())
                    .or_else(|| config.shell.profile.clone());
                // `-e` without --profile runs in the plain shell's place.
                let default = default.filter(|_| asked.command.is_none());
                First {
                    profile: asked.profile.or(default),
                    ..asked
                }
            }
        };
        let early = EarlyShell::start(config, &first).ok();
        Self {
            early,
            session,
            first,
        }
    }
}

struct EarlyShell {
    pane: Pane,
    /// The restored session's first, else the default shell.
    profile: Option<String>,
    wake: Arc<OnceLock<Arc<dyn Fn() + Send + Sync>>>,
}

impl EarlyShell {
    fn start(config: &Config, first: &First) -> Result<Self, BoxError> {
        let wake: Arc<OnceLock<Arc<dyn Fn() + Send + Sync>>> = Arc::default();
        let relay = Arc::clone(&wake);
        let relay: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(wake) = relay.get() {
                wake();
            }
        });
        // The cell size waits on the fonts; App::new sends the real one.
        let size = Size {
            cols: config.window.columns,
            rows: config.window.rows,
            cell_width: 8,
            cell_height: 16,
        };
        let socket = cli_transport::address_for(std::process::id());
        let env = pane_env(&socket, FIRST_PANE);
        let (cwd, command) = first.launch(config);
        let vt = config.profile_vt_settings(first.profile.as_deref());
        let pane = spawn_shell(size, relay, config, vt, cwd, command.as_ref(), &env)?;
        Ok(Self {
            pane,
            profile: first.profile.clone(),
            wake,
        })
    }
}

/// Where a shell from profile `name` starts (its own directory over `cwd`)
/// and what it runs.
fn launch<'a>(
    config: &'a Config,
    cwd: Option<PathBuf>,
    name: Option<&str>,
) -> (Option<PathBuf>, Option<&'a jet_config::Command>) {
    let profile = name.and_then(|name| config.profiles.get(name));
    let cwd = profile.and_then(|p| p.cwd.clone()).or(cwd);
    (cwd, profile.and_then(|p| p.command.as_ref()))
}

/// A PTY of `grid` cells of `m`'s size.
fn pty_size(grid: (u16, u16), m: jet_font::CellMetrics) -> Result<Size, BoxError> {
    Ok(Size {
        cols: grid.0,
        rows: grid.1,
        cell_width: u16::try_from(m.width)?,
        cell_height: u16::try_from(m.height)?,
    })
}

/// What tells pane `id`'s programs how to reach this window.
fn pane_env(socket: &str, id: PaneId) -> [(&'static str, String); 2] {
    [
        (jet_cli::SOCKET_VAR, socket.to_owned()),
        (jet_cli::PANE_VAR, id.to_string()),
    ]
}

/// The configured fonts at `size_pt`, and what went wrong finding them.
fn load_fonts(
    dpi: u32,
    config: &Config,
    size_pt: f32,
) -> Result<(Fonts, Option<String>), BoxError> {
    let font = &config.font;
    let size_px = size_pt * dpi as f32 / 72.0;
    let mut problem = None;
    let faces = font.family.as_deref().and_then(|name| {
        let faces = jet_font::fallback::system_family(name);
        if faces.is_none() {
            problem = Some(format!(
                "font {name:?} isn't installed, using JetBrains Mono"
            ));
        }
        faces
    });
    let features: Vec<&str> = font.features.iter().map(String::as_str).collect();
    let configured = Fonts::with_faces(
        faces.unwrap_or_else(jet_font::bundled),
        size_px,
        &features,
        jet_font::fallback::system(),
    );
    let fonts = match configured {
        Ok(fonts) => fonts,
        Err(err) => {
            problem = Some(format!("font: {err}; using the default font"));
            Fonts::new(size_px, &[], jet_font::fallback::system())?
        }
    };
    if let Some(problem) = &problem {
        eprintln!("jet: {problem}");
    }
    Ok((fonts, problem))
}

/// `jet 0.1.0 (x86_64-windows, libghostty-vt 1.2.3)`.
fn version() -> String {
    let vt = libghostty_vt::build_info::version_string().unwrap_or_default();
    format!(
        "jet {} ({}-{}, libghostty-vt {vt})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

fn make_renderer(
    gpu: &Gpu,
    format: wgpu::TextureFormat,
    dpi: u32,
    config: &Config,
    fonts: Fonts,
) -> Renderer {
    let scale = dpi as f32 / 96.0;
    let padding = (config.window.padding as f32 * scale).round() as u32;
    let mut renderer = Renderer::new(&gpu.device, &gpu.queue, format, fonts, padding);
    renderer.set_opacity(config.window.opacity);
    renderer.set_selection(selection(config));
    renderer
}

fn selection(config: &Config) -> jet_render::Selection {
    let colors = &config.colors;
    jet_render::Selection {
        background: colors.selection_background.map(|c| c.0),
        foreground: colors.selection_foreground.map(|c| c.0),
    }
}

const BANNER_BG: Rgb = Rgb {
    r: 0x5c,
    g: 0x1a,
    b: 0x22,
};
const BANNER_FG: Rgb = Rgb {
    r: 0xff,
    g: 0xdc,
    b: 0xdc,
};

/// Rows for `problems`: each problem's lines, then a hint. At most half
/// the grid, so the shell stays usable.
fn banner(problems: &[&str], (cols, rows): (u16, u16)) -> Vec<Arc<Line>> {
    // Struct update syntax can't reach Cell's private text fields.
    let mut style = Cell::default();
    (style.fg, style.bg) = (Some(BANNER_FG), Some(BANNER_BG));
    let (mut bold, mut faint) = (style, style);
    bold.flags = CellFlags::BOLD;
    faint.flags = CellFlags::FAINT;
    let mut lines = Vec::new();
    for problem in problems {
        for (i, text) in problem.lines().enumerate() {
            let (prefix, style) = if i == 0 {
                (" ⚠ ", bold)
            } else {
                ("   ", style)
            };
            lines.push((format!("{prefix}{text}"), style));
        }
    }
    let max = usize::from(rows / 2).max(1);
    if lines.len() >= max {
        lines.truncate(max.saturating_sub(1));
    }
    let hint = "   save the config to retry · click here to dismiss";
    lines.push((hint.to_owned(), faint));
    lines
        .into_iter()
        .map(|(text, style)| Arc::new(Line::from_text(&text, usize::from(cols), style)))
        .collect()
}

/// The shell from `-e <program> [args...]`, else the config's, else the
/// platform's: the first of pwsh, Windows PowerShell and cmd that starts,
/// or the login shell (`$SHELL`, else the account's), run as one so its
/// profile sets `PATH` even for an app opened from Finder.
/// Start `argv` in the background, in `cwd`, with nothing attached (and
/// no console window on Windows).
fn run_detached(argv: &[String], cwd: Option<&std::path::Path>) -> Result<(), BoxError> {
    let (program, args) = argv.split_first().ok_or("nothing to run")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    std::thread::Builder::new()
        .name("jet-plumb".into())
        .spawn(move || child.wait())?;
    Ok(())
}

/// Black or white text, whichever reads on `bg`.
fn readable_on(bg: Rgb) -> Rgb {
    let luma = 0.299 * f32::from(bg.r) + 0.587 * f32::from(bg.g) + 0.114 * f32::from(bg.b);
    if luma > 150.0 {
        Rgb {
            r: 16,
            g: 16,
            b: 16,
        }
    } else {
        Rgb {
            r: 255,
            g: 255,
            b: 255,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_shell(
    size: Size,
    wake: Arc<dyn Fn() + Send + Sync>,
    config: &Config,
    settings: jet_core::VtSettings,
    cwd: Option<PathBuf>,
    profile: Option<&jet_config::Command>,
    env: &[(&str, String)],
) -> Result<Pane, BoxError> {
    let waker = || {
        let wake = Arc::clone(&wake);
        move || wake()
    };
    let options = |command| {
        let command = Command {
            cwd: cwd.clone(),
            env: env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
            ..command
        };
        PaneOptions {
            command: if config.shell.integration {
                integration::inject(command)
            } else {
                command
            },
            size,
            scrollback: config.terminal.scrollback,
            settings: settings.clone(),
        }
    };
    let flag: Vec<String> = std::env::args()
        .skip_while(|arg| arg != "-e")
        .skip(1)
        .collect();
    let configured = config.shell.command.as_ref().map(jet_config::Command::argv);
    let argv = profile
        .map(jet_config::Command::argv)
        .or(Some(flag).filter(|a| !a.is_empty()))
        .or(configured);
    if let Some((program, args)) = argv.as_ref().and_then(|a| a.split_first()) {
        let command = args.iter().fold(Command::new(program), Command::arg);
        return Ok(Pane::spawn(options(command), waker())?);
    }
    #[cfg(windows)]
    let shells = [
        Command::new("pwsh.exe").arg("-NoLogo"),
        Command::new("powershell.exe").arg("-NoLogo"),
        Command::new("cmd.exe"),
    ];
    #[cfg(not(windows))]
    let shells = [Command::default(), Command::new("/bin/sh")];

    let mut last_err = None;
    for shell in shells {
        let name = shell
            .program
            .clone()
            .unwrap_or_else(|| "the login shell".into());
        match Pane::spawn(options(shell), waker()) {
            Ok(pane) => {
                eprintln!("jet: running {}", name.to_string_lossy());
                return Ok(pane);
            }
            Err(err) => {
                eprintln!("jet: {} unavailable: {err}", name.to_string_lossy());
                last_err = Some(err);
            }
        }
    }
    Err(last_err.map_or_else(|| "no shell to run".into(), Into::into))
}

/// The tab bar's (and jet's title bar's) colors.
struct BarColors {
    background: Rgb,
    foreground: Rgb,
    active_background: Rgb,
    active_foreground: Rgb,
}

/// `lines` as shared rows, reusing `cache`'s where unchanged so their
/// rendered rows stay cached.
fn reuse(cache: &mut Vec<Arc<Line>>, lines: Vec<Line>) -> Vec<Arc<Line>> {
    let out: Vec<Arc<Line>> = lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| match cache.get(i) {
            Some(old) if **old == line => Arc::clone(old),
            _ => Arc::new(line),
        })
        .collect();
    cache.clone_from(&out);
    out
}

/// A bar's grid over `r`: a row's text sits in the middle, a column's
/// starts a pad down.
fn bar_grid(
    id: u64,
    snapshot: &Snapshot,
    r: Rect,
    vertical: bool,
    pad: u32,
    cell_h: u32,
) -> Grid<'_> {
    let inset = if vertical {
        pad
    } else {
        (r.h as u32).saturating_sub(cell_h) / 2
    };
    Grid {
        id,
        snapshot,
        area: [r.x as f32, r.y as f32, r.w as f32, r.h as f32],
        origin: [r.x.max(0) as u32 + pad, r.y.max(0) as u32 + inset],
    }
}

/// An X `size` square at `at`: both diagonals, a stroke at a time, so it
/// stays crisp at any scale.
fn x_mark(out: &mut Vec<Quad>, at: [f32; 2], size: f32, stroke: f32, color: [f32; 4]) {
    let steps = (size / stroke) as usize;
    for k in 0..steps {
        let d = k as f32 * stroke;
        for x in [at[0] + d, at[0] + size - stroke - d] {
            out.push(Quad {
                pos: [x, at[1] + d],
                size: [stroke, stroke],
                color,
            });
        }
    }
}

fn progress_event(progress: Progress) -> What {
    let (state, percent) = match progress {
        Progress::Hidden => ("hidden", None),
        Progress::Normal(p) => ("normal", Some(p)),
        Progress::Error(p) => ("error", Some(p)),
        Progress::Indeterminate => ("indeterminate", None),
        Progress::Paused(p) => ("paused", Some(p)),
    };
    What::Progress {
        state: state.to_owned(),
        percent,
    }
}

impl<H: Host> Drop for App<H> {
    /// A closed window's socket would otherwise outlive it on Unix.
    fn drop(&mut self) {
        // Its shells are still running: closing the window is what ends
        // them, after this.
        self.save_session(true);
        if self.cli.is_some() {
            cli_transport::forget(&self.socket);
        }
    }
}

/// The kernel's name for this computer.
#[cfg(not(windows))]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).into_owned()
}

/// This computer's user and host names, for the status bar.
fn local_names() -> (String, String) {
    let var = |name| std::env::var(name).unwrap_or_default();
    #[cfg(windows)]
    let (user, host) = (var("USERNAME"), var("COMPUTERNAME"));
    #[cfg(not(windows))]
    let (user, host) = (var("USER"), hostname());
    let host = host
        .trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    (user, host)
}

#[cfg(test)]
mod tests {
    use crate::app::First;

    fn parse(args: &[&str]) -> Result<First, String> {
        First::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_command_line_picks_the_first_tab() {
        let dir = std::env::temp_dir();
        let dir_arg = dir.to_string_lossy().into_owned();
        let first = parse(&[&dir_arg, "--profile", "bash", "-e", "htop", "-d", "5"]);
        assert_eq!(
            first,
            Ok(First {
                cwd: Some(std::path::absolute(&dir).unwrap_or(dir.clone())),
                profile: Some("bash".into()),
                command: Some(vec!["htop".into(), "-d".into(), "5".into()]),
            })
        );
        assert_eq!(parse(&[]), Ok(First::default()));
        assert!(parse(&["-e"]).is_err());
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["/no/such/dir/here"]).is_err());
    }
}
