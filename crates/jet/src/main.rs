// A GUI app on Windows: no console window. Output still reaches redirected
// handles (jet-devhost captures it into its log).
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod app;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod chrome;
#[cfg(windows)]
mod dwm;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod icon;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod integration;
#[cfg(any(windows, test))]
mod keymap;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod layout;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod overlay;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod pane;
#[cfg(windows)]
mod quake;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod remote;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod session;
mod sound;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod status;
#[cfg(windows)]
mod taskbar;

use std::fmt::Write as _;
use std::process::ExitCode;

use libghostty_vt::build_info;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        attach_console();
        print(&format!("{}\n", app::USAGE));
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|a| a == "config") {
        attach_console();
        return match args.get(1).map(String::as_str) {
            Some("check") => config_check(),
            _ => {
                eprintln!("usage: jet config check");
                ExitCode::FAILURE
            }
        };
    }
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        // A GUI program on Windows: print to the shell that ran it.
        attach_console();
        return match build_info() {
            Ok(text) => {
                print(&text);
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("jet: failed to query libghostty-vt build info: {err}");
                ExitCode::FAILURE
            }
        };
    }
    let quake = args.first().is_some_and(|a| a == "quake");
    let first = if quake {
        app::First::default()
    } else {
        match app::First::parse(&args) {
            Ok(first) => first,
            Err(err) => {
                attach_console();
                eprintln!("jet: {err}\n{}", app::USAGE);
                return ExitCode::FAILURE;
            }
        }
    };
    // jet quake [NAME] [--background], and the flags below.
    let flags = if quake { &args[1..] } else { &[] };
    let flag = |name: &str| flags.iter().any(|a| a == name);
    let background = flag(app::BACKGROUND);
    #[cfg(windows)]
    {
        let name = flags.iter().find(|a| !a.starts_with("--")).cloned();
        if quake && flag(quake::UNINSTALL) {
            quake::uninstall();
            return ExitCode::SUCCESS;
        }
        if quake && flag(quake::AUTOSTART) {
            // A config that doesn't load: the default, on.
            let config = jet_config::load(&jet_config::Dirs::from_env());
            let on = config.map_or(true, |l| l.config.quake.autostart);
            return match quake::autostart(on) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            };
        }
        if quake && flag(quake::WINDOW) {
            return app::win32::run(Some(name), background, first);
        }
        if quake {
            return match quake::coordinate(background, name) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("jet: {err}");
                    ExitCode::FAILURE
                }
            };
        }
        app::win32::run(None, background, first)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        app::winit::run(quake, background, first)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (quake, background, first);
        eprintln!("jet: no window backend for this platform");
        ExitCode::FAILURE
    }
}

/// Print where the config comes from, what was ignored and the result.
fn config_check() -> ExitCode {
    let dirs = jet_config::Dirs::from_env();
    let loaded = match jet_config::load(&dirs) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("jet: {err}");
            return ExitCode::FAILURE;
        }
    };
    let config = match loaded.config.to_toml() {
        Ok(text) => text,
        Err(err) => {
            eprintln!("jet: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Writing to a String can't fail.
    let mut out = String::new();
    if loaded.sources.is_empty() {
        out.push_str("no config file; tried:\n");
        for path in dirs.candidates() {
            let _ = writeln!(out, "  {}", path.display());
        }
    }
    for source in &loaded.sources {
        let _ = writeln!(out, "read {}", source.display());
    }
    for path in &loaded.absent {
        let _ = writeln!(out, "skipped {} (not there)", path.display());
    }
    for warning in &loaded.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    let _ = writeln!(out, "\n{config}");
    print(&out);
    ExitCode::SUCCESS
}

/// Print for the commands that print and exit. A reader that has gone
/// (`jet --version | head -1`) isn't an error, where println! panics.
fn print(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// A GUI-subsystem exe gets no console: borrow the one it was started
/// from, so `jet config check` prints in the shell.
fn attach_console() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

fn build_info() -> Result<String, libghostty_vt::Error> {
    Ok(format!(
        "jet {} ({}-{})\n\
         libghostty-vt {}\n  \
         optimize: {:?}\n  \
         simd: {}\n  \
         kitty graphics: {}\n  \
         tmux control mode: {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS,
        build_info::version_string()?,
        build_info::optimize_mode()?,
        build_info::supports_simd()?,
        build_info::supports_kitty_graphics()?,
        build_info::supports_tmux_control_mode()?,
    ))
}
