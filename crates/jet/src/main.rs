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
#[cfg(any(windows, test))]
mod keymap;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod layout;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod overlay;
#[cfg(windows)]
mod quake;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod status;
#[cfg(windows)]
mod taskbar;

use std::process::ExitCode;

use libghostty_vt::build_info;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    if std::env::args().any(|arg| arg == "--version") {
        return match print_build_info() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("jet: failed to query libghostty-vt build info: {err}");
                ExitCode::FAILURE
            }
        };
    }
    let quake = args.first().is_some_and(|a| a == "quake");
    #[cfg(windows)]
    {
        app::win32::run(quake)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        app::winit::run(quake)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = quake;
        eprintln!("jet: no window backend for this platform");
        ExitCode::FAILURE
    }
}

/// Print where the config comes from, what was ignored and the result.
fn config_check() -> ExitCode {
    let dirs = jet_config::Dirs::from_env();
    match jet_config::load(&dirs) {
        Ok(loaded) => {
            if loaded.sources.is_empty() {
                println!("no config file; tried:");
                for path in dirs.candidates() {
                    println!("  {}", path.display());
                }
            }
            for source in &loaded.sources {
                println!("read {}", source.display());
            }
            for warning in &loaded.warnings {
                println!("warning: {warning}");
            }
            match loaded.config.to_toml() {
                Ok(text) => {
                    println!("\n{text}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("jet: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(err) => {
            eprintln!("jet: {err}");
            ExitCode::FAILURE
        }
    }
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

fn print_build_info() -> Result<(), libghostty_vt::Error> {
    println!(
        "jet {} ({}-{})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("libghostty-vt {}", build_info::version_string()?);
    println!("  optimize: {:?}", build_info::optimize_mode()?);
    println!("  simd: {}", build_info::supports_simd()?);
    println!(
        "  kitty graphics: {}",
        build_info::supports_kitty_graphics()?
    );
    println!(
        "  tmux control mode: {}",
        build_info::supports_tmux_control_mode()?
    );
    Ok(())
}
