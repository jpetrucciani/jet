//! The daemon end to end, through the binary and its socket.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs jetmux against a server of its own; stops it when done.
struct Server {
    dir: PathBuf,
}

impl Server {
    fn new(name: &str) -> io::Result<Self> {
        // Short: a socket path must fit in sun_path (104 bytes on macOS),
        // which macOS's own temp dir leaves no room for.
        let dir = PathBuf::from(format!("/tmp/jetmux-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn run(&self, args: &[&str]) -> io::Result<Output> {
        jetmux(&self.dir, args)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = jetmux(&self.dir, &["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn jetmux(dir: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_jetmux"))
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("JETMUX")
        .env_remove("JETMUX_SESSION")
        .env_remove("JETMUX_PANE")
        .args(["-L", "test"])
        .args(args)
        .output()
}

/// macOS gave accepted sockets the listener's O_NONBLOCK, so every
/// request failed there while Linux was fine.
#[test]
fn a_detached_session_answers() -> io::Result<()> {
    let server = Server::new("daemon")?;
    let new = server.run(&["new", "-d", "-s", "demo", "--", "sh", "-c", "sleep 30"])?;
    assert!(
        new.status.success(),
        "new: {}",
        String::from_utf8_lossy(&new.stderr)
    );
    let ls = server.run(&["ls"])?;
    let listed = String::from_utf8_lossy(&ls.stdout);
    assert!(listed.starts_with("demo:"), "ls: {listed}");
    Ok(())
}
