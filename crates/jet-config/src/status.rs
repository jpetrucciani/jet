//! `[status]`: powerline-style segments filled in from the focused pane's
//! session, in the title bar, the tab bar, or a row of their own.

use std::fmt;

use std::collections::HashMap;

use jet_core::{Context, ContextKind, Location, Progress, Rgb, Ssh};
use serde::{Deserialize, Serialize};

use crate::Color;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Status {
    pub position: StatusPosition,
    pub style: StatusStyle,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPosition {
    /// Beside the window buttons, when jet draws the title bar; else
    /// `bottom`.
    #[default]
    Titlebar,
    /// The end of the tab bar: its right end, or the bottom of a side bar.
    TabBar,
    /// A row of its own above or below the panes, with left and right
    /// segments.
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusStyle {
    /// Arrow separators.
    #[default]
    Powerline,
    /// Half circles.
    Round,
    /// Blocks of color, no separators.
    Plain,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Segment {
    pub text: Template,
    /// Which end of a `top` or `bottom` row; elsewhere they all sit right.
    pub align: Align,
    /// Unset: the terminal's background, on a palette color.
    pub foreground: Option<Color>,
    pub background: Option<Color>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    Left,
    #[default]
    Right,
}

/// What `{name}` in a segment stands for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    /// The title the shell set.
    Title,
    /// Where the shell is, as it reported it (remote paths too).
    Cwd,
    /// The last part of that.
    Dir,
    /// The host the innermost context (OSC 3008) or the shell (OSC 7)
    /// reported, else the `ssh` destination, else this computer.
    Host,
    /// The user the innermost context runs as, else the `ssh` login
    /// user, else the local one.
    User,
    /// `user@host` of an `ssh` running in front; empty otherwise.
    Ssh,
    /// The program in front, without `.exe`.
    Process,
    /// The pane's profile name, `default` for the shell.
    Profile,
    /// The tab's number, and how many there are.
    Tab,
    Tabs,
    /// `HH:MM` and `YYYY-MM-DD`, local.
    Time,
    Date,
    /// `42%` while a progress report is showing.
    Progress,
    /// The last command's exit status when it failed (shells that send
    /// OSC 133;D); empty after a success.
    Exit,
    /// What the innermost OSC 3008 context changed: the user for `sudo`
    /// or `run0`, the container, VM or remote host; empty otherwise.
    Context,
    /// A user variable: `{var.k8s}`, set with OSC 1337 SetUserVar.
    Var(String),
}

const FIELDS: &[(&str, Field)] = &[
    ("title", Field::Title),
    ("cwd", Field::Cwd),
    ("dir", Field::Dir),
    ("host", Field::Host),
    ("user", Field::User),
    ("ssh", Field::Ssh),
    ("process", Field::Process),
    ("profile", Field::Profile),
    ("tab", Field::Tab),
    ("tabs", Field::Tabs),
    ("time", Field::Time),
    ("date", Field::Date),
    ("progress", Field::Progress),
    ("exit", Field::Exit),
    ("context", Field::Context),
];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    Field(Field),
}

/// Segment text: literal text with `{field}`s, `{{` and `}}` for braces.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

impl Template {
    /// The text with fields filled in; `None` when any field is empty, so
    /// a segment about SSH shows only while there is one.
    pub fn render(&self, mut value: impl FnMut(&Field) -> Option<String>) -> Option<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Field(field) => out.push_str(&value(field).filter(|v| !v.is_empty())?),
            }
        }
        Some(out)
    }
}

impl TryFrom<String> for Template {
    type Error = String;

    fn try_from(source: String) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    text.push('}');
                }
                '{' => {
                    let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                    let field = match name.strip_prefix("var.") {
                        Some(var) if !var.is_empty() => Field::Var(var.to_owned()),
                        _ => FIELDS
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map(|(_, f)| f.clone())
                            .ok_or_else(|| {
                                let known: Vec<&str> = FIELDS.iter().map(|(n, _)| *n).collect();
                                format!(
                                    "unknown field {{{name}}}; known: {}, var.NAME",
                                    known.join(", ")
                                )
                            })?,
                    };
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Field(field));
                }
                '}' => return Err("a lone `}`: write `}}` for a brace".into()),
                c => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { source, parts })
    }
}

impl From<Template> for String {
    fn from(t: Template) -> Self {
        t.source
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// What the fields read, for one pane.
#[derive(Debug, Default)]
pub struct Session<'a> {
    pub title: &'a str,
    pub location: Option<&'a Location>,
    pub ssh: Option<&'a Ssh>,
    /// The program in front.
    pub process: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub tab: usize,
    pub tabs: usize,
    pub time: &'a str,
    pub date: &'a str,
    pub progress: Option<Progress>,
    /// The last command's exit status, from OSC 133;D.
    pub exit: Option<i32>,
    pub vars: Option<&'a HashMap<String, String>>,
    /// The innermost context, from OSC 3008.
    pub context: Option<&'a Context>,
    pub local_user: &'a str,
    pub local_host: &'a str,
}

impl Session<'_> {
    pub fn value(&self, field: &Field) -> Option<String> {
        let owned = |s: &str| Some(s.to_owned());
        match field {
            Field::Title => owned(self.title),
            Field::Cwd => self.location.map(|l| l.path.clone()),
            Field::Dir => {
                let path = &self.location?.path;
                let trimmed = path.trim_end_matches(['/', '\\']);
                let last = trimmed.rsplit(['/', '\\']).next().unwrap_or_default();
                // A root is its own name.
                owned(if last.is_empty() || last.ends_with(':') {
                    path
                } else {
                    last
                })
            }
            Field::Host => self
                .context
                .and_then(Context::host)
                .map(str::to_owned)
                .or_else(|| self.location.and_then(|l| l.host.clone()))
                .or_else(|| self.ssh.map(|s| s.host.clone()))
                .or_else(|| owned(self.local_host)),
            Field::User => self
                .context
                .and_then(Context::run_as)
                .map(str::to_owned)
                .or_else(|| self.ssh.and_then(|s| s.user.clone()))
                .or_else(|| owned(self.local_user)),
            Field::Ssh => self.ssh.map(|s| match &s.user {
                Some(user) => format!("{user}@{}", s.host),
                None => s.host.clone(),
            }),
            Field::Process => self.process.map(|p| {
                let lower = p.to_ascii_lowercase();
                match lower.strip_suffix(".exe") {
                    Some(_) => p.get(..p.len() - 4).unwrap_or(p).to_owned(),
                    None => p.to_owned(),
                }
            }),
            Field::Context => {
                let c = self.context?;
                match c.kind? {
                    ContextKind::Elevate | ContextKind::Chpriv => c.target_user.clone(),
                    ContextKind::Container => c.container.clone(),
                    ContextKind::Vm => c.vm.clone(),
                    ContextKind::Remote => c.target_host.clone(),
                    _ => None,
                }
            }
            Field::Profile => owned(self.profile.unwrap_or("default")),
            Field::Tab => Some(self.tab.to_string()),
            Field::Tabs => Some(self.tabs.to_string()),
            Field::Time => owned(self.time),
            Field::Date => owned(self.date),
            Field::Progress => match self.progress? {
                Progress::Normal(p) | Progress::Error(p) | Progress::Paused(p) => {
                    Some(format!("{p}%"))
                }
                Progress::Indeterminate => owned("…"),
                Progress::Hidden => None,
            },
            Field::Exit => self.exit.filter(|&s| s != 0).map(|s| s.to_string()),
            Field::Var(name) => self.vars?.get(name).cloned(),
        }
    }
}

/// A segment with something to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub text: String,
    pub fg: Rgb,
    pub bg: Rgb,
    pub align: Align,
}

/// The segments whose fields all have values, colored: unset colors
/// alternate between two steps from `bg` toward `fg`.
pub fn shown(segments: &[Segment], session: &Session, fg: Rgb, bg: Rgb) -> Vec<Shown> {
    let steps = [mix(bg, fg, 0.28), mix(bg, fg, 0.16)];
    let mut out = Vec::new();
    for segment in segments {
        let Some(text) = segment.text.render(|f| session.value(f)) else {
            continue;
        };
        let step = steps[out.len() % 2];
        out.push(Shown {
            text,
            fg: segment.foreground.map_or(fg, |c| c.0),
            bg: segment.background.map_or(step, |c| c.0),
            align: segment.align,
        });
    }
    out
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb {
        r: f(a.r, b.r),
        g: f(a.g, b.g),
        b: f(a.b, b.b),
    }
}

#[cfg(test)]
mod tests {
    use jet_core::{Context, ContextKind, Location, Ssh};

    use crate::status::{Field, Session, Template};

    fn template(s: &str) -> Result<Template, String> {
        Template::try_from(s.to_owned())
    }

    #[test]
    fn fields_fill_in_and_empty_ones_hide() -> Result<(), String> {
        let t = template("{user}@{host} {{x}}")?;
        let value = |f: &Field| match f {
            Field::User => Some("me".to_owned()),
            Field::Host => Some("box".to_owned()),
            _ => None,
        };
        assert_eq!(t.render(value).as_deref(), Some("me@box {x}"));
        let t = template(" {var.k8s}")?;
        assert_eq!(t.render(value), None);
        let var = |f: &Field| matches!(f, Field::Var(v) if v == "k8s").then(|| "prod".to_owned());
        assert_eq!(t.render(var).as_deref(), Some(" prod"));
        Ok(())
    }

    #[test]
    fn mistakes_are_errors() {
        assert!(template("{hots}").is_err_and(|e| e.contains("unknown field {hots}")));
        assert!(template("a } b").is_err());
        assert!(template("{var.}").is_err());
    }

    #[test]
    fn host_prefers_what_the_shell_reports() {
        let location = Location {
            host: Some("reported".into()),
            path: "/home/me/src/".into(),
        };
        let ssh = Ssh {
            user: None,
            host: "box".into(),
        };
        let mut s = Session {
            local_host: "desk",
            local_user: "jo",
            ..Session::default()
        };
        assert_eq!(s.value(&Field::Host).as_deref(), Some("desk"));
        assert_eq!(s.value(&Field::Ssh), None);
        s.ssh = Some(&ssh);
        assert_eq!(s.value(&Field::Host).as_deref(), Some("box"));
        assert_eq!(s.value(&Field::User).as_deref(), Some("jo"));
        s.location = Some(&location);
        assert_eq!(s.value(&Field::Host).as_deref(), Some("reported"));
        assert_eq!(s.value(&Field::Dir).as_deref(), Some("src"));
        s.process = Some("SSH.EXE");
        assert_eq!(s.value(&Field::Process).as_deref(), Some("SSH"));
        // An OSC 3008 context beats them all: sudo on a remote box.
        let context = Context {
            kind: Some(ContextKind::Elevate),
            user: Some("me".into()),
            hostname: Some("box2".into()),
            target_user: Some("root".into()),
            ..Context::default()
        };
        assert_eq!(s.value(&Field::Context), None);
        s.context = Some(&context);
        let fields = [Field::Host, Field::User, Field::Context].map(|f| s.value(&f));
        assert_eq!(
            fields.each_ref().map(Option::as_deref),
            [Some("box2"), Some("root"), Some("root")]
        );
    }
}
