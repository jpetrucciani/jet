//! jet-devhost: runs in the logged-in desktop session of the Windows dev box so
//! builds launched over the network actually show up on the desktop.
//!
//! No auth: bind it to the tailnet address only (`JET_DEVHOST_ADDR`).

#[cfg(windows)]
mod desktop;

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const DEFAULT_ADDR: &str = "127.0.0.1:7878";
const LOG_FILE: &str = "run.log";
/// devhost's own log, so a closed console still leaves a trail.
const DEVHOST_LOG: &str = "devhost.log";

struct AppState {
    root: PathBuf,
    child: Mutex<Option<Child>>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match serve().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jet-devhost: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), String> {
    #[cfg(windows)]
    desktop::init();
    let addr: SocketAddr = std::env::var("JET_DEVHOST_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_owned())
        .parse()
        .map_err(|err| format!("invalid JET_DEVHOST_ADDR: {err}"))?;
    let root = data_dir();
    tokio::fs::create_dir_all(root.join("builds"))
        .await
        .map_err(|err| format!("creating {}: {err}", root.display()))?;
    let log_path = root.join(DEVHOST_LOG);
    std::panic::set_hook(Box::new(move |info| {
        log(&log_path, &format!("panic: {info}"))
    }));
    let result = listen(addr, root.clone()).await;
    match &result {
        Ok(()) => log(&root.join(DEVHOST_LOG), "shut down cleanly (ctrl-c)"),
        Err(err) => log(&root.join(DEVHOST_LOG), &format!("exiting: {err}")),
    }
    result
}

async fn listen(addr: SocketAddr, root: PathBuf) -> Result<(), String> {
    let state = Arc::new(AppState {
        root,
        child: Mutex::new(None),
    });
    let app = Router::new()
        .route("/build/{version}/{file}", put(put_build))
        .route("/run", post(run))
        .route("/logs", get(logs))
        .route("/screenshot", get(screenshot))
        .route("/input", post(input))
        .route("/self", put(update_self))
        .layer(DefaultBodyLimit::disable())
        .with_state(state.clone());

    let listener = bind(addr).await?;
    log(
        &state.root.join(DEVHOST_LOG),
        &format!("listening on {addr}, data in {}", state.root.display()),
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // An error here means no signal handler; shutting down is the safe outcome.
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|err| format!("server: {err}"))?;

    if let Some(mut child) = state.child.lock().await.take() {
        // Best effort on shutdown: the child may already be gone.
        let _ = child.kill().await;
    }
    Ok(())
}

/// Print and append to the devhost log. Logging must never take the server
/// down, so write failures are ignored.
fn log(path: &Path, message: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = format!("[{secs}] {message}");
    eprintln!("jet-devhost: {line}");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{line}");
    }
}

fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("JET_DEVHOST_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("jet-devhost")
}

/// `PUT /build/{version}/{file}`: store one file of a build (exe or DLL).
async fn put_build(
    State(state): State<Arc<AppState>>,
    UrlPath((version, file)): UrlPath<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let dir = state.root.join("builds").join(safe_name(&version)?);
    let path = dir.join(safe_name(&file)?);
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(&path, &body).await?;
    // Lets the same flow run on a Linux box for testing; Windows has no exec bit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).await?;
    }
    log(
        &state.root.join(DEVHOST_LOG),
        &format!("stored {} ({} bytes)", path.display(), body.len()),
    );
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
struct RunRequest {
    version: String,
    #[serde(default = "default_exe")]
    exe: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn default_exe() -> String {
    "jet.exe".to_owned()
}

#[derive(Serialize)]
struct RunResponse {
    pid: Option<u32>,
}

/// `POST /run`: kill the previous instance, then launch a stored build with
/// stdout and stderr going to the log file.
async fn run(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, AppError> {
    let dir = state.root.join("builds").join(safe_name(&req.version)?);
    let exe = dir.join(safe_name(&req.exe)?);
    if !exe.is_file() {
        return Err(AppError::NotFound(exe.display().to_string()));
    }

    let mut slot = state.child.lock().await;
    if let Some(mut old) = slot.take() {
        // Already exited is fine; we only care that it's gone.
        let _ = old.kill().await;
    }

    let output = std::fs::File::create(state.root.join(LOG_FILE))?;
    let child = Command::new(&exe)
        .args(&req.args)
        .envs(&req.env)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id();
    log(
        &state.root.join(DEVHOST_LOG),
        &format!("started {} pid={pid:?}", exe.display()),
    );
    *slot = Some(child);
    Ok(Json(RunResponse { pid }))
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
}

/// `GET /logs?lines=N`: tail of the current run's log.
async fn logs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LogsQuery>,
) -> Result<String, AppError> {
    let text = match tokio::fs::read(state.root.join(LOG_FILE)).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    Ok(tail(&text, query.lines.unwrap_or(200)).to_owned())
}

fn tail(text: &str, lines: usize) -> &str {
    if lines == 0 {
        return "";
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    match trimmed.rmatch_indices('\n').nth(lines - 1) {
        Some((idx, _)) => &text[idx + 1..],
        None => text,
    }
}

/// Binding retries briefly so a self-update's new process can wait for the
/// old one to let go of the port.
async fn bind(addr: SocketAddr) -> Result<tokio::net::TcpListener, String> {
    let mut tries = 0;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && tries < 50 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => return Err(format!("binding {addr}: {err}")),
        }
    }
}

#[derive(Deserialize)]
struct ScreenshotQuery {
    /// Only the window of the build `/run` started.
    #[serde(default)]
    window: bool,
}

/// `GET /screenshot[?window=true]`: PNG of the desktop or the running build.
async fn screenshot(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ScreenshotQuery>,
) -> Result<Response, AppError> {
    let pid = if query.window {
        Some(running_pid(&state).await?)
    } else {
        None
    };
    let png = desktop_call(move || {
        #[cfg(windows)]
        {
            let what = match pid {
                Some(pid) => desktop::Capture::Window(
                    desktop::window_of(pid)
                        .ok_or_else(|| AppError::NotFound(format!("a window for pid {pid}")))?,
                ),
                None => desktop::Capture::Desktop,
            };
            Ok(desktop::screenshot(what)?)
        }
        #[cfg(not(windows))]
        {
            let _ = pid;
            Err::<Vec<u8>, _>(AppError::Unsupported)
        }
    })
    .await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[derive(Deserialize)]
struct InputRequest {
    /// Typed as-is; `\n` presses Enter.
    #[serde(default)]
    text: String,
    /// Chords pressed after the text, e.g. `["ctrl+c", "up"]`.
    #[serde(default)]
    keys: Vec<String>,
}

/// `POST /input`: focus the running build's window, then type into it.
async fn input(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InputRequest>,
) -> Result<StatusCode, AppError> {
    let pid = running_pid(&state).await?;
    desktop_call(move || {
        #[cfg(windows)]
        {
            let hwnd = desktop::window_of(pid)
                .ok_or_else(|| AppError::NotFound(format!("a window for pid {pid}")))?;
            desktop::focus(hwnd)?;
            desktop::type_text(&req.text)?;
            for chord in &req.keys {
                desktop::press_keys(chord)?;
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = (pid, req.text, req.keys);
            Err(AppError::Unsupported)
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /self`: replace the devhost exe with the body and restart into it.
/// A running exe can be renamed but not overwritten on Windows, so the old
/// one moves aside first.
async fn update_self(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let exe = std::env::current_exe()?;
    let aside = exe.with_extension("old.exe");
    let fresh = exe.with_extension("new.exe");
    tokio::fs::write(&fresh, &body).await?;
    // Left over from the previous update; may not exist.
    let _ = tokio::fs::remove_file(&aside).await;
    tokio::fs::rename(&exe, &aside).await?;
    if let Err(err) = tokio::fs::rename(&fresh, &exe).await {
        // Put the working exe back so the next start still finds it.
        let _ = tokio::fs::rename(&aside, &exe).await;
        return Err(err.into());
    }
    std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    log(
        &state.root.join(DEVHOST_LOG),
        &format!("updated to {} bytes, restarting", body.len()),
    );
    tokio::spawn(async {
        // Let the response go out; the new process waits for the port.
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    Ok(StatusCode::ACCEPTED)
}

async fn running_pid(state: &AppState) -> Result<u32, AppError> {
    state
        .child
        .lock()
        .await
        .as_ref()
        .and_then(Child::id)
        .ok_or_else(|| AppError::NotFound("a running build".to_owned()))
}

/// Desktop calls block, so they run off the async workers.
async fn desktop_call<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|err| AppError::Io(std::io::Error::other(err)))?
}

/// A single path component: no separators, no traversal, no hidden files.
fn safe_name(name: &str) -> Result<&Path, AppError> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(Path::new(name))
    } else {
        Err(AppError::BadName(name.to_owned()))
    }
}

enum AppError {
    BadName(String),
    NotFound(String),
    #[cfg_attr(windows, expect(dead_code, reason = "only off Windows"))]
    Unsupported,
    Io(std::io::Error),
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::BadName(name) => (StatusCode::BAD_REQUEST, format!("invalid name: {name:?}\n")),
            Self::NotFound(path) => (StatusCode::NOT_FOUND, format!("not found: {path}\n")),
            Self::Unsupported => (StatusCode::NOT_IMPLEMENTED, "only on Windows\n".to_owned()),
            Self::Io(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err}\n")),
        }
        .into_response()
    }
}

#[cfg(test)]
mod tests {
    use crate::{safe_name, tail};

    #[test]
    fn safe_name_rejects_traversal() {
        for bad in ["", "..", ".", ".hidden", "a/b", "a\\b", "C:", "../jet.exe"] {
            assert!(safe_name(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["jet.exe", "20261113-180000", "ghostty-vt.dll", "v0_1"] {
            assert!(safe_name(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a\nb\n", 5), "a\nb\n");
        assert_eq!(tail("a\nb\n", 0), "");
    }
}
