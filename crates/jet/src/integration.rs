//! Shell integration jet sets up itself, the way Ghostty does: bash, zsh
//! and PowerShell started in a pane mark their prompts (OSC 133) and report
//! their directory (OSC 7, OSC 9;9) without the user sourcing anything.
//!
//! - zsh: `ZDOTDIR` points at jet's `.zshenv`, which puts the user's back,
//!   runs theirs and loads the hooks.
//! - bash 4 and later: `--posix` with `ENV` naming jet's script, which
//!   leaves POSIX mode, runs the startup files bash would have, and hooks
//!   the prompt.
//! - PowerShell: `-NoExit -EncodedCommand` wraps the prompt after the
//!   profile ran (inline, so execution policy doesn't apply).
//!
//! A command that runs something (`bash script.sh`, `pwsh -File x`) is
//! left alone, as are other shells.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jet_pty::Command;

const BASH: &str = include_str!("shell/jet.bash");
const ZSH: &str = include_str!("shell/jet.zsh");
const ZSHENV: &str = include_str!("shell/.zshenv");
const POWERSHELL: &str = include_str!("shell/jet.ps1");

/// `command` set up to load jet's integration, when it starts a shell jet
/// knows how to.
pub fn inject(mut command: Command) -> Command {
    let program = match &command.program {
        Some(program) => program.clone(),
        // The login shell: what portable-pty would start.
        None if cfg!(windows) => return command,
        None => match std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            Some(shell) => shell,
            None => return command,
        },
    };
    let login = command.program.is_none();
    match shell_name(&program).as_str() {
        "zsh" => {
            let Some(dir) = scripts() else {
                return command;
            };
            if let Some(user) = std::env::var_os("ZDOTDIR") {
                command.env.push(("JET_ZSH_ZDOTDIR".into(), user));
            }
            command.env.push(("ZDOTDIR".into(), dir.join("zsh").into()));
        }
        "bash" => {
            let only_flags = |a: &OsString| {
                ["-l", "--login", "-i"]
                    .map(OsStr::new)
                    .contains(&a.as_os_str())
            };
            // macOS's own bash is 3.2, which ignores ENV in POSIX mode and
            // would stay in it (Homebrew's bash is fine).
            let old_mac_bash = cfg!(target_os = "macos") && program == "/bin/bash";
            if old_mac_bash || !command.args.iter().all(only_flags) {
                return command;
            }
            let Some(dir) = scripts() else {
                return command;
            };
            let login = login || command.args.iter().any(|a| a == "-l" || a == "--login");
            if let Some(user) = std::env::var_os("ENV") {
                command.env.push(("JET_BASH_ENV".into(), user));
            }
            let mode = if login { "login" } else { "" };
            command.env.push(("JET_BASH_INJECT".into(), mode.into()));
            command
                .env
                .push(("ENV".into(), shell_path(&dir.join("jet.bash")).into()));
            command.program = Some(program);
            command.args = vec!["--posix".into(), "-i".into()];
        }
        "pwsh" | "powershell" => {
            let plain = |a: &OsString| {
                let a = a.to_string_lossy().to_ascii_lowercase();
                ["-nologo", "-noprofile", "-noexit", "-login", "-l"].contains(&a.as_str())
            };
            if !command.args.iter().all(plain) {
                return command;
            }
            command.args.push("-NoExit".into());
            command.args.push("-EncodedCommand".into());
            command.args.push(encode_command(POWERSHELL).into());
        }
        _ => {}
    }
    command
}

/// `bash`, `zsh`, `pwsh`... from a program path, lowercase and without
/// `.exe`.
fn shell_name(program: &OsStr) -> String {
    let name = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_owned()
}

/// A path as bash sees it: forward slashes, which MSYS bash (Git Bash)
/// takes with a drive letter too.
fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The scripts, written once per run to a per-user cache directory (not a
/// shared temp dir, where another user could plant them). `None` if they
/// can't be.
fn scripts() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = cache_dir()?.join("jet").join("shell-integration");
        let files = [
            ("jet.bash", BASH),
            ("zsh/.zshenv", ZSHENV),
            ("zsh/jet.zsh", ZSH),
        ];
        for (name, text) in files {
            let path = dir.join(name);
            if std::fs::read_to_string(&path).ok().as_deref() == Some(text) {
                continue;
            }
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, text));
            if let Err(err) = written {
                eprintln!(
                    "jet: writing shell integration to {}: {err}",
                    path.display()
                );
                return None;
            }
        }
        Some(dir)
    })
    .clone()
}

fn cache_dir() -> Option<PathBuf> {
    let var = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Caches"))
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|h| h.join(".cache")))
    }
}

/// `script` for `-EncodedCommand`: base64 of its UTF-16LE.
fn encode_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                let six = (n >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(ALPHABET[six as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use jet_pty::Command;

    use crate::integration::{encode_command, inject, shell_name};

    #[test]
    fn encodes_like_powershell_expects() {
        // [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('ls'))
        assert_eq!(encode_command("ls"), "bABzAA==");
        assert_eq!(encode_command("dir"), "ZABpAHIA");
    }

    #[test]
    fn names_shells_from_paths() {
        assert_eq!(shell_name("/usr/bin/BASH.EXE".as_ref()), "bash");
        assert_eq!(shell_name("/bin/zsh".as_ref()), "zsh");
        assert_eq!(shell_name("pwsh.exe".as_ref()), "pwsh");
    }

    #[test]
    fn commands_that_run_something_are_left_alone() {
        let script = Command::new("bash").arg("build.sh");
        assert_eq!(inject(script.clone()).args, script.args);
        let pwsh = Command::new("pwsh").arg("-File").arg("x.ps1");
        assert_eq!(inject(pwsh.clone()).args, pwsh.args);
        let fish = Command::new("fish");
        assert!(inject(fish).env.is_empty());
    }
}
