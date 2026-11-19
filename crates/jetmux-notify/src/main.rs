//! `jetmux-notify`: turns daemon events into alerts. Apps' own notifications
//! (OSC 9 / OSC 777) and commands that ran longer than `long_command`
//! seconds go to the desktop, a webhook (ntfy style) and web push, as the
//! `[notify]` part of `config.toml` says.

use std::time::Duration;

use jetmux_client::RpcClient;
use jetmux_notify::push::{self, Store};
use jetmux_proto::rpc::{Call, Event};
use serde::{Deserialize, Serialize};

const USAGE: &str = "\
usage: jetmux-notify [-L server] [test]

Follows the daemon's events and sends alerts as [notify] in config.toml says:

  [notify]
  long_command = 10      # alert when a command runs this many seconds (0: off)
  bell = false           # alert on bells too
  desktop = true         # notify-send, or osascript on macOS
  webhook = \"https://ntfy.sh/my-topic\"   # POST the message, title in a header
  push = true            # web push to browsers registered in jetmux-web
  subject = \"mailto:me@example.com\"      # contact the push services see

`test` sends one alert through everything configured and exits.";

/// How long to wait before reconnecting to a daemon that went away.
const RECONNECT: Duration = Duration::from_secs(1);

type BoxError = Box<dyn std::error::Error>;

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    long_command: u64,
    bell: bool,
    desktop: bool,
    webhook: Option<String>,
    push: bool,
    subject: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            long_command: 10,
            bell: false,
            desktop: true,
            webhook: None,
            push: true,
            subject: "mailto:jetmux@localhost".into(),
        }
    }
}

/// Only the `[notify]` table; the rest of the file is other binaries'.
#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    notify: Config,
}

fn load_config() -> Config {
    let dirs = jet_config::Dirs::from_env();
    let loaded = jet_config::load_for_jetmux(&dirs, &jetmux_client::config_path())
        .map_err(|e| e.to_string())
        .and_then(|loaded| {
            toml::Value::Table(loaded.config.jetmux.unwrap_or_default())
                .try_into::<File>()
                .map_err(|e| e.to_string())
        });
    match loaded {
        Ok(file) => file.notify,
        Err(e) => {
            eprintln!("jetmux-notify: config: {e}");
            Config::default()
        }
    }
}

/// What gets shown, and what a push notification carries to the web UI.
#[derive(Debug, Serialize)]
struct Alert {
    title: String,
    body: String,
    session: String,
    pane: Option<u32>,
}

fn alert_for(event: &Event, config: &Config) -> Option<Alert> {
    match event {
        Event::Notify {
            session,
            pane,
            title,
            body,
        } => Some(Alert {
            title: title.clone().unwrap_or_else(|| session.clone()),
            body: body.clone(),
            session: session.clone(),
            pane: Some(*pane),
        }),
        Event::BlockDone {
            session,
            pane,
            block,
        } => {
            let ms = block.duration_ms?;
            if config.long_command == 0 || ms < config.long_command * 1000 {
                return None;
            }
            let status = match block.exit {
                Some(0) | None => "finished".to_owned(),
                Some(code) => format!("failed (exit {code})"),
            };
            Some(Alert {
                title: format!("{} {status}", block.command),
                body: format!("after {:.0}s in {session}", ms as f64 / 1000.0),
                session: session.clone(),
                pane: Some(*pane),
            })
        }
        Event::Bell { session, pane } if config.bell => Some(Alert {
            title: session.clone(),
            body: "bell".into(),
            session: session.clone(),
            pane: Some(*pane),
        }),
        _ => None,
    }
}

async fn deliver(alert: &Alert, config: &Config) {
    if config.desktop
        && let Err(e) = desktop(alert)
    {
        eprintln!("jetmux-notify: desktop: {e}");
    }
    if let Some(url) = &config.webhook {
        let headers = [("title", alert.title.replace(['\r', '\n'], " "))];
        match push::post(
            &push::client(),
            url,
            &headers,
            alert.body.clone().into_bytes(),
        )
        .await
        {
            Ok((200..=299, _)) => {}
            Ok((status, _)) => eprintln!("jetmux-notify: webhook: {status}"),
            Err(e) => eprintln!("jetmux-notify: webhook: {e}"),
        }
    }
    if config.push {
        let result = match (Store::load_or_create(), serde_json::to_vec(alert)) {
            (Ok(mut store), Ok(payload)) if !store.subscriptions.is_empty() => {
                store.send_all(&payload, &config.subject).await
            }
            (Err(e), _) => Err(e),
            _ => Ok(()),
        };
        if let Err(e) = result {
            eprintln!("jetmux-notify: push: {e}");
        }
    }
}

fn desktop(alert: &Alert) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let mut c = std::process::Command::new("osascript");
        c.arg("-e").arg(format!(
            "display notification {} with title {}",
            quote(&alert.body),
            quote(&alert.title)
        ));
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.args(["--app-name=jetmux", &alert.title, &alert.body]);
        c
    };
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
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
            "jetmux-notify {}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let mut args = std::env::args().skip(1);
    let mut server = "default".to_owned();
    let mut test = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-L" => match args.next() {
                Some(s) => server = s,
                None => return usage(),
            },
            "test" => test = true,
            _ => return usage(),
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return eprintln!("jetmux-notify: {e}"),
    };
    if test {
        let alert = Alert {
            title: "jetmux".into(),
            body: "a test alert from jetmux-notify".into(),
            session: String::new(),
            pane: None,
        };
        return runtime.block_on(deliver(&alert, &load_config()));
    }
    loop {
        if let Err(e) = follow(&server, &runtime) {
            eprintln!("jetmux-notify: {e}; reconnecting");
        }
        std::thread::sleep(RECONNECT);
    }
}

fn usage() {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// Alert on the daemon's events until it goes away.
fn follow(server: &str, runtime: &tokio::runtime::Runtime) -> Result<(), BoxError> {
    let mut client = RpcClient::connect(server)?;
    client.call(Call::EventsSubscribe { session: None })?;
    loop {
        let event = client.next_event()?;
        // Read each time, so edits apply without a restart.
        let config = load_config();
        if let Some(alert) = alert_for(&event, &config) {
            runtime.block_on(deliver(&alert, &config));
        }
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::rpc::{BlockInfo, BlockState, Event};

    use crate::{Config, alert_for};

    #[test]
    fn long_commands_and_app_notifications_alert() {
        let config = Config::default();
        let block = |ms| Event::BlockDone {
            session: "dev".into(),
            pane: 1,
            block: BlockInfo {
                id: 3,
                command: "make".into(),
                state: BlockState::Done,
                exit: Some(2),
                duration_ms: Some(ms),
            },
        };
        assert!(alert_for(&block(900), &config).is_none());
        let alert = alert_for(&block(12_000), &config).unwrap();
        assert_eq!(alert.title, "make failed (exit 2)");
        assert_eq!(alert.body, "after 12s in dev");
        let notify = Event::Notify {
            session: "dev".into(),
            pane: 1,
            title: None,
            body: "done".into(),
        };
        assert_eq!(alert_for(&notify, &config).unwrap().title, "dev");
        let bell = Event::Bell {
            session: "dev".into(),
            pane: 1,
        };
        assert!(alert_for(&bell, &config).is_none());
    }
}
