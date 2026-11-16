//! `[status]`: powerline-style segments filled in from the focused pane's
//! session, in the title bar, the tab bar, or a row of their own.

use std::fmt;

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
    /// The host the shell reported (OSC 7), else the `ssh` destination,
    /// else this computer.
    Host,
    /// The `ssh` login user, else the local one.
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

#[cfg(test)]
mod tests {
    use crate::status::{Field, Template};

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
}
