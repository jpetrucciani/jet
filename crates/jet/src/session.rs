//! The tabs and splits a window had when it closed, with each shell's
//! directory and profile, saved so the next start can open them again
//! (`[window] restore`). Shells come back, not what ran in them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct Tab {
    /// Only one the user gave it.
    pub title: Option<String>,
    /// From `set_tab_color`, as `[r, g, b]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    pub root: Node,
    /// Which pane had the keyboard, in [`Node::leaves`] order.
    pub focused: usize,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum Node {
    Pane {
        cwd: Option<PathBuf>,
        profile: Option<String>,
    },
    Split {
        horizontal: bool,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Session {
    /// The saved session, if there is one that reads.
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(path()?).ok()?;
        match serde_json::from_str::<Self>(&text) {
            Ok(session) if !session.tabs.is_empty() => Some(session),
            Ok(_) => None,
            Err(err) => {
                eprintln!("jet: ignoring the saved session: {err}");
                None
            }
        }
    }

    /// Save it for the next start; with no tabs, forget the last one.
    pub fn save(&self) {
        let Some(path) = path() else {
            return;
        };
        let saved = if self.tabs.is_empty() {
            match std::fs::remove_file(&path) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        } else {
            // Written aside and renamed over, so a crash midway leaves the
            // old one whole.
            let part = path.with_extension("json.part");
            serde_json::to_string(self)
                .map_err(std::io::Error::other)
                .and_then(|json| {
                    path.parent().map_or(Ok(()), std::fs::create_dir_all)?;
                    std::fs::write(&part, json)?;
                    std::fs::rename(&part, &path)
                })
        };
        if let Err(err) = saved {
            eprintln!("jet: saving the session to {}: {err}", path.display());
        }
    }

    /// Where its first shell starts and with which profile.
    pub fn first(&self) -> (Option<PathBuf>, Option<String>) {
        let mut node = self.tabs.first().map(|t| &t.root);
        while let Some(Node::Split { first, .. }) = node {
            node = Some(first);
        }
        match node {
            Some(Node::Pane { cwd, profile }) => {
                (cwd.clone().filter(|dir| dir.is_dir()), profile.clone())
            }
            _ => (None, None),
        }
    }
}

/// Per user: `%LOCALAPPDATA%\jet`, `~/Library/Application Support/jet`, or
/// `$XDG_STATE_HOME/jet` (`~/.local/state/jet`).
fn path() -> Option<PathBuf> {
    let var = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let dir = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        var("XDG_STATE_HOME").or_else(|| var("HOME").map(|h| h.join(".local/state")))
    };
    Some(dir?.join("jet").join("session.json"))
}

#[cfg(test)]
mod tests {
    use crate::session::{Node, Session, Tab};

    #[test]
    fn round_trips_and_starts_at_the_first_pane() -> Result<(), serde_json::Error> {
        let here = std::env::current_dir().ok();
        let session = Session {
            tabs: vec![Tab {
                color: Some([1, 2, 3]),
                title: Some("logs".into()),
                root: Node::Split {
                    horizontal: true,
                    ratio: 0.3,
                    first: Box::new(Node::Pane {
                        cwd: here.clone(),
                        profile: Some("wsl".into()),
                    }),
                    second: Box::new(Node::Pane {
                        cwd: Some("/no/such/dir".into()),
                        profile: None,
                    }),
                },
                focused: 1,
            }],
            active: 0,
        };
        let json = serde_json::to_string(&session)?;
        assert_eq!(serde_json::from_str::<Session>(&json)?, session);
        assert_eq!(session.first(), (here, Some("wsl".into())));
        Ok(())
    }
}
