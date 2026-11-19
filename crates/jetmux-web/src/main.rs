//! `jetmux-web`: the browser and phone client. It listens on localhost only;
//! `tailscale serve` publishes it on the tailnet with HTTPS and identity
//! headers, which [`auth`] turns into roles. The UI is built into the
//! binary.

mod auth;
mod bridge;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use jetmux_client::RpcClient;
use jetmux_notify::push::{Store, Subscription};
use jetmux_proto::Size;
use jetmux_proto::rpc::{BlockInfo, Call, Captured, PaneInfo, SessionInfo};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::{Access, Policy, Role, Signer};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const USAGE: &str = "\
usage: jetmux-web [-L server] [--listen addr]
       jetmux-web [-L server] share session [--ttl secs]

Serves the web UI on localhost (default 127.0.0.1:7681). Publish it on your
tailnet with `tailscale serve --bg 7681`. Roles come from config.toml:

  [web]
  listen = \"127.0.0.1:7681\"
  url = \"https://jetmux.tailnet.ts.net\"   # for share links
  local = \"owner\"      # requests from this machine with no tailnet identity
  keybar = [[\"Esc\", \"Tab\", \"C-c\", \"|\", \"~\"]]
  actions = [{ name = \"git status\", send = \"git status\\r\" }]

  [web.users]
  \"you@github\" = \"owner\"
  \"*\" = \"viewer\"       # anyone else on the tailnet

`share` prints a read-only link to one session (default ttl: one hour).";

const DEFAULT_LISTEN: &str = "127.0.0.1:7681";
const DEFAULT_SHARE_TTL: u64 = 60 * 60;

type ApiError = (StatusCode, String);
type ApiResult<T> = Result<Json<T>, ApiError>;

/// A named bit of text to type, from `[web] actions` or a profile's.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Snippet {
    pub name: String,
    pub send: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct WebConfig {
    listen: String,
    url: Option<String>,
    local: Option<Role>,
    users: BTreeMap<String, Role>,
    keybar: Vec<Vec<String>>,
    actions: Vec<Snippet>,
    /// The `jetmux` binary that launches profiles; default: next to this one.
    jetmux: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.into(),
            url: None,
            local: None,
            users: BTreeMap::new(),
            keybar: Vec::new(),
            actions: Vec::new(),
            jetmux: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WebProfile {
    keybar: Vec<Vec<String>>,
    actions: Vec<Snippet>,
}

/// The parts of `config.toml` this binary reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    web: WebConfig,
    profile: BTreeMap<String, WebProfile>,
    /// From the sections shared with jet: its theme, among others.
    #[serde(skip)]
    settings: jet_core::VtSettings,
}

/// Read each time it's needed, so edits apply without a restart.
fn load_config() -> File {
    let dirs = jet_config::Dirs::from_env();
    let loaded = jet_config::load_for_jetmux(&dirs, &jetmux_client::config_path())
        .map_err(|e| e.to_string())
        .and_then(|loaded| {
            let settings = loaded.config.vt_settings();
            toml::Value::Table(loaded.config.jetmux.unwrap_or_default())
                .try_into()
                .map(|file| File { settings, ..file })
                .map_err(|e: toml::de::Error| e.to_string())
        });
    loaded.unwrap_or_else(|e| {
        eprintln!("jetmux-web: config: {e}");
        File::default()
    })
}

struct App {
    server: String,
    signer: Signer,
}

impl App {
    fn access(&self, headers: &HeaderMap, share: Option<&str>) -> Result<Access, ApiError> {
        let config = load_config();
        let policy = Policy {
            users: &config.web.users,
            local: config.web.local,
        };
        auth::access(&policy, &self.signer, headers, share)
            .ok_or((StatusCode::FORBIDDEN, "not allowed".into()))
    }

    /// A socket API call, off the async threads.
    async fn call<T: DeserializeOwned + Send + 'static>(&self, call: Call) -> Result<T, ApiError> {
        let server = self.server.clone();
        tokio::task::spawn_blocking(move || -> Result<T, String> {
            RpcClient::connect(&server)
                .and_then(|mut c| c.call_as(call))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
    }
}

type Shared = State<Arc<App>>;

#[derive(Debug, Default, Deserialize)]
struct ShareQuery {
    share: Option<String>,
}

fn require(access: &Access, role: Role) -> Result<(), ApiError> {
    if access.role >= role {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, format!("needs the {role:?} role")))
    }
}

fn require_session(access: &Access, session: &str) -> Result<(), ApiError> {
    if access.sees(session) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "not shared with you".into()))
    }
}

fn csrf(headers: &HeaderMap) -> Result<(), ApiError> {
    if auth::csrf_ok(headers) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "missing X-Jetmux header".into()))
    }
}

/// Who the browser is, for the UI to show and gate on.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct Me {
    user: Option<String>,
    role: Role,
    /// Set for share links: the only session there is.
    session: Option<String>,
    /// VAPID public key for `pushManager.subscribe`.
    push_key: Option<String>,
}

async fn me(State(app): Shared, headers: HeaderMap, Query(q): Query<ShareQuery>) -> ApiResult<Me> {
    let access = app.access(&headers, q.share.as_deref())?;
    let push_key = access
        .session
        .is_none()
        .then(|| Store::load_or_create().and_then(|s| s.public_key()).ok())
        .flatten();
    Ok(Json(Me {
        user: access.user,
        role: access.role,
        session: access.session,
        push_key,
    }))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct ProfileInfo {
    name: String,
    keybar: Vec<Vec<String>>,
    actions: Vec<Snippet>,
}

/// Key bar rows, snippets, profiles and colors, for the UI.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct UiConfig {
    keybar: Vec<Vec<String>>,
    actions: Vec<Snippet>,
    profiles: Vec<ProfileInfo>,
    theme: WebTheme,
}

/// jet's theme, every color worked out, as CSS `#rrggbb`.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct WebTheme {
    foreground: String,
    background: String,
    cursor: Option<String>,
    /// All 256 entries.
    palette: Vec<String>,
}

impl WebTheme {
    fn new(settings: &jet_core::VtSettings) -> Result<Self, jet_core::Error> {
        let colors = jet_core::FrameColors::new(settings)?;
        let css = |c: jet_core::Rgb| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
        Ok(Self {
            foreground: css(colors.colors.foreground),
            background: css(colors.colors.background),
            cursor: colors.colors.cursor.map(css),
            palette: colors.palette.iter().copied().map(css).collect(),
        })
    }
}

async fn ui_config(
    State(app): Shared,
    headers: HeaderMap,
    Query(q): Query<ShareQuery>,
) -> ApiResult<UiConfig> {
    app.access(&headers, q.share.as_deref())?;
    let config = load_config();
    let theme = WebTheme::new(&config.settings)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(UiConfig {
        keybar: config.web.keybar,
        actions: config.web.actions,
        profiles: config
            .profile
            .into_iter()
            .map(|(name, p)| ProfileInfo {
                name,
                keybar: p.keybar,
                actions: p.actions,
            })
            .collect(),
        theme,
    }))
}

async fn sessions(
    State(app): Shared,
    headers: HeaderMap,
    Query(q): Query<ShareQuery>,
) -> ApiResult<Vec<SessionInfo>> {
    let access = app.access(&headers, q.share.as_deref())?;
    let sessions: Vec<SessionInfo> = app.call(Call::SessionList {}).await?;
    Ok(Json(
        sessions
            .into_iter()
            .filter(|s| access.sees(&s.name))
            .collect(),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct PaneQuery {
    share: Option<String>,
    /// A pane id; the session's focused pane when unset.
    pane: Option<u32>,
}

fn pane_target(session: &str, pane: Option<u32>) -> Option<String> {
    Some(match pane {
        Some(id) => format!("{session}:%{id}"),
        None => session.to_owned(),
    })
}

async fn panes(
    State(app): Shared,
    headers: HeaderMap,
    Path(session): Path<String>,
    Query(q): Query<ShareQuery>,
) -> ApiResult<Vec<PaneInfo>> {
    let access = app.access(&headers, q.share.as_deref())?;
    require_session(&access, &session)?;
    let target = Some(session);
    Ok(Json(app.call(Call::PaneList { target }).await?))
}

async fn blocks(
    State(app): Shared,
    headers: HeaderMap,
    Path(session): Path<String>,
    Query(q): Query<PaneQuery>,
) -> ApiResult<Vec<BlockInfo>> {
    let access = app.access(&headers, q.share.as_deref())?;
    require_session(&access, &session)?;
    let target = pane_target(&session, q.pane);
    Ok(Json(app.call(Call::PaneBlocks { target }).await?))
}

async fn block(
    State(app): Shared,
    headers: HeaderMap,
    Path((session, id)): Path<(String, u64)>,
    Query(q): Query<PaneQuery>,
) -> ApiResult<Captured> {
    let access = app.access(&headers, q.share.as_deref())?;
    require_session(&access, &session)?;
    let call = Call::PaneReadBlock {
        target: pane_target(&session, q.pane),
        block: Some(id),
    };
    Ok(Json(app.call(call).await?))
}

/// The `jetmux` CLI, for launching profiles.
fn jetmux_binary(config: &WebConfig) -> std::path::PathBuf {
    if let Some(path) = &config.jetmux {
        return path.into();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("jetmux")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "jetmux".into())
}

async fn launch(
    State(app): Shared,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> ApiResult<()> {
    csrf(&headers)?;
    require(&app.access(&headers, None)?, Role::Owner)?;
    let config = load_config();
    if !config.profile.contains_key(&profile) {
        return Err((StatusCode::NOT_FOUND, format!("no profile {profile}")));
    }
    let status = tokio::process::Command::new(jetmux_binary(&config.web))
        .args(["-L", &app.server, "up", &profile, "-d"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !status.status.success() {
        let err = String::from_utf8_lossy(&status.stderr).trim().to_owned();
        return Err((StatusCode::BAD_GATEWAY, err));
    }
    Ok(Json(()))
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct ShareRequest {
    session: String,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    ttl_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct ShareLink {
    url: String,
}

fn share_url(base: &str, session: &str, token: &str) -> String {
    format!("{}/?share={token}#{session}", base.trim_end_matches('/'))
}

async fn share(
    State(app): Shared,
    headers: HeaderMap,
    Json(req): Json<ShareRequest>,
) -> ApiResult<ShareLink> {
    csrf(&headers)?;
    require(&app.access(&headers, None)?, Role::Owner)?;
    let ttl = req.ttl_secs.unwrap_or(DEFAULT_SHARE_TTL);
    let token = app
        .signer
        .share(&req.session, ttl)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "signing failed".into()))?;
    let base = load_config().web.url.unwrap_or_else(|| {
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!("https://{host}")
    });
    Ok(Json(ShareLink {
        url: share_url(&base, &req.session, &token),
    }))
}

async fn push_subscribe(
    State(app): Shared,
    headers: HeaderMap,
    Json(mut sub): Json<Subscription>,
) -> ApiResult<()> {
    csrf(&headers)?;
    let access = app.access(&headers, None)?;
    sub.user = access.user;
    let mut store = Store::load_or_create().map_err(server_error)?;
    store.subscribe(sub);
    store.save().map_err(server_error)?;
    Ok(Json(()))
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
struct Endpoint {
    endpoint: String,
}

async fn push_unsubscribe(
    State(app): Shared,
    headers: HeaderMap,
    Json(req): Json<Endpoint>,
) -> ApiResult<()> {
    csrf(&headers)?;
    app.access(&headers, None)?;
    let mut store = Store::load_or_create().map_err(server_error)?;
    store.unsubscribe(&req.endpoint);
    store.save().map_err(server_error)?;
    Ok(Json(()))
}

fn server_error(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Debug, Deserialize)]
struct AttachQuery {
    share: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    /// `0` fits the session to this browser; anything else follows it.
    follow: Option<u8>,
}

async fn attach(
    State(app): Shared,
    headers: HeaderMap,
    Path(session): Path<String>,
    Query(q): Query<AttachQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !auth::same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin").into_response();
    }
    let access = match app.access(&headers, q.share.as_deref()) {
        Ok(access) => access,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = require_session(&access, &session) {
        return e.into_response();
    }
    let size = Size {
        cols: q.cols.unwrap_or(80).max(2),
        rows: q.rows.unwrap_or(24).max(2),
    };
    let follow = q.follow != Some(0);
    let server = app.server.clone();
    ws.on_upgrade(move |socket| bridge::run(socket, server, session, size, follow, access.role))
}

/// The built-in UI. Anything that isn't a file is the app's index, so
/// client-side routes work on reload.
async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (path, body) = match ASSETS.iter().find(|(name, _)| *name == path) {
        Some((name, body)) => (*name, *body),
        None => match ASSETS.iter().find(|(name, _)| *name == "index.html") {
            Some((name, body)) => (*name, *body),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    // Hashed build output never changes; the index and service worker must
    // be fetched fresh so updates land.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/config", get(ui_config))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{session}/panes", get(panes))
        .route("/api/sessions/{session}/blocks", get(blocks))
        .route("/api/sessions/{session}/blocks/{id}", get(block))
        .route("/api/sessions/{session}/attach", get(attach))
        .route("/api/profiles/{profile}/up", post(launch))
        .route("/api/share", post(share))
        .route("/api/push/subscribe", post(push_subscribe))
        .route("/api/push/unsubscribe", post(push_unsubscribe))
        .fallback(get(asset))
        .with_state(app)
}

fn main() {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        use std::io::Write as _;
        // writeln, not println: a closed pipe isn't worth a panic
        let _ = writeln!(
            std::io::stdout(),
            "jetmux-web {}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let mut args = std::env::args().skip(1);
    let mut server = "default".to_owned();
    let mut listen = None;
    let mut share_session = None;
    let mut ttl = DEFAULT_SHARE_TTL;
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("-L", Some(s)) => server = s,
            ("--listen", Some(addr)) => listen = Some(addr),
            ("share", Some(session)) => share_session = Some(session),
            ("--ttl", Some(secs)) => match secs.parse() {
                Ok(secs) => ttl = secs,
                Err(_) => return usage(),
            },
            _ => return usage(),
        }
    }
    let signer = match Signer::load_or_create() {
        Ok(signer) => signer,
        Err(e) => return eprintln!("jetmux-web: signing key: {e}"),
    };
    let config = load_config();
    if let Some(session) = share_session {
        let base = config
            .web
            .url
            .unwrap_or_else(|| format!("http://{}", config.web.listen));
        match signer.share(&session, ttl) {
            Some(token) => println!("{}", share_url(&base, &session, &token)),
            None => eprintln!("jetmux-web: signing failed"),
        }
        return;
    }
    let listen = listen.unwrap_or(config.web.listen);
    let app = Arc::new(App { server, signer });
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => return eprintln!("jetmux-web: {e}"),
    };
    if let Err(e) = runtime.block_on(serve(&listen, app)) {
        eprintln!("jetmux-web: {e}");
        std::process::exit(1);
    }
}

async fn serve(listen: &str, app: Arc<App>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("jetmux-web: listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(app)).await
}

fn usage() {
    eprintln!("{USAGE}");
    std::process::exit(2);
}
