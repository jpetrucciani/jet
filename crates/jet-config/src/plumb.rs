//! `[plumb.NAME]`: text that does something when Ctrl+clicked, after Plan
//! 9's plumber. A rule matches a regex in the line under the mouse, and
//! opens a URL here, runs a program here, or opens a split running a
//! command wherever the pane is (over its `ssh`).

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::Command;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Plumb {
    /// A regex; `(?<name>...)` groups become `{name}`.
    #[serde(rename = "match")]
    pub pattern: Pattern,
    /// Which panes it's for.
    pub on: On,
    /// Open this URL with the system's handler (http, https, mailto).
    pub open: Option<String>,
    /// Run this program here, in the background.
    pub run: Option<Command>,
    /// Open a split (or tab) running this, where the pane is: on the
    /// `ssh` host in the remote shell's directory, else here.
    pub view: Option<Command>,
    /// Where `view` opens.
    #[serde(rename = "in")]
    pub place: Place,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum On {
    #[default]
    Any,
    /// Panes with no `ssh` in front.
    Local,
    /// Panes running `ssh`.
    Remote,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Place {
    #[default]
    Right,
    Down,
    Left,
    Up,
    Tab,
}

/// A compiled regex, compared by its source; `None` until set.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern(Option<Regex>);

impl Pattern {
    pub fn regex(&self) -> Option<&Regex> {
        self.0.as_ref()
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        String::from(self.clone()) == String::from(other.clone())
    }
}

impl TryFrom<String> for Pattern {
    type Error = String;

    fn try_from(source: String) -> Result<Self, String> {
        Regex::new(&source)
            .map(|regex| Self(Some(regex)))
            .map_err(|e| e.to_string())
    }
}

impl From<Pattern> for String {
    fn from(pattern: Pattern) -> Self {
        pattern.0.map(|r| r.as_str().to_owned()).unwrap_or_default()
    }
}

/// What a rule does with a match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action<'a> {
    Open(&'a str),
    Run(&'a Command),
    View(&'a Command, Place),
}

impl Plumb {
    pub fn action(&self) -> Option<Action<'_>> {
        match (&self.open, &self.run, &self.view) {
            (Some(url), None, None) => Some(Action::Open(url)),
            (None, Some(run), None) => Some(Action::Run(run)),
            (None, None, Some(view)) => Some(Action::View(view, self.place)),
            _ => None,
        }
    }

    /// Mistakes worth refusing the config over: no regex, not exactly one
    /// action, `{names}` that aren't groups or built-ins.
    pub fn check(&self, name: &str) -> Result<(), String> {
        let at = |what: String| format!("plumb.{name}: {what}");
        let Some(regex) = &self.pattern.0 else {
            return Err(at("needs a match regex".into()));
        };
        let action = self
            .action()
            .ok_or_else(|| at("needs exactly one of open, run or view".into()))?;
        let words = match action {
            Action::Open(url) => vec![url.to_owned()],
            Action::Run(command) | Action::View(command, _) => command.argv(),
        };
        let groups: Vec<&str> = regex.capture_names().flatten().collect();
        for word in &words {
            fill(word, |var| {
                let known = BUILT_IN.contains(&var)
                    || groups.contains(&var)
                    || var.parse::<usize>().is_ok_and(|n| n < regex.captures_len());
                known.then(String::new)
            })
            .map_err(|var| {
                at(format!(
                    "{{{var}}} isn't a group in match, or one of {BUILT_IN:?}"
                ))
            })?;
        }
        Ok(())
    }
}

/// `{0}` is the whole match; these come from the pane.
const BUILT_IN: [&str; 2] = ["cwd", "host"];

/// A rule matched under the mouse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: String,
    /// Byte range of the match in the line.
    pub range: Range<usize>,
    /// `0`, `1`... and named groups.
    pub vars: HashMap<String, String>,
}

/// The rule whose match covers byte `at` of `text`, among those `on` the
/// pane: the longest match, then the first by name.
pub fn hit(rules: &BTreeMap<String, Plumb>, remote: bool, text: &str, at: usize) -> Option<Hit> {
    let mut best: Option<Hit> = None;
    for (name, rule) in rules {
        let fits = match rule.on {
            On::Any => true,
            On::Local => !remote,
            On::Remote => remote,
        };
        if !fits {
            continue;
        }
        let Some(regex) = &rule.pattern.0 else {
            continue;
        };
        let Some(caps) = regex
            .captures_iter(text)
            .find(|c| c.get(0).is_some_and(|m| m.start() <= at && at < m.end()))
        else {
            continue;
        };
        let Some(whole) = caps.get(0) else {
            continue;
        };
        if best.as_ref().is_some_and(|b| b.range.len() >= whole.len()) {
            continue;
        }
        let mut vars = HashMap::new();
        for (i, group) in caps.iter().enumerate() {
            vars.insert(i.to_string(), group.map_or("", |g| g.as_str()).to_owned());
        }
        for group in regex.capture_names().flatten() {
            let value = caps.name(group).map_or("", |g| g.as_str());
            vars.insert(group.to_owned(), value.to_owned());
        }
        best = Some(Hit {
            rule: name.clone(),
            range: whole.range(),
            vars,
        });
    }
    best
}

/// `word` with each `{var}` replaced (`{{` and `}}` for braces); the
/// first unknown name is the error.
pub fn fill(word: &str, mut var: impl FnMut(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                out.push_str(&var(&name).ok_or(name)?);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::Command;
    use crate::plumb::{On, Plumb, fill, hit};

    fn rule(pattern: &str, on: On) -> Result<Plumb, String> {
        Ok(Plumb {
            pattern: pattern.to_owned().try_into()?,
            on,
            run: Some(Command::Line("code --goto {file}:{line}".into())),
            ..Plumb::default()
        })
    }

    #[test]
    fn longest_match_under_the_mouse_wins() -> Result<(), String> {
        let mut rules = BTreeMap::new();
        rules.insert("word".into(), rule(r"\w+", On::Any)?);
        rules.insert(
            "file".into(),
            rule(r"(?<file>[\w/.]+\.rs):(?<line>\d+)", On::Any)?,
        );
        rules.insert("remote".into(), rule(r".+", On::Remote)?);
        let text = "error at src/main.rs:42 here";
        let at = text.find("main").unwrap_or_default();
        let found = hit(&rules, false, text, at).ok_or("no hit")?;
        assert_eq!(found.rule, "file");
        assert_eq!(&text[found.range.clone()], "src/main.rs:42");
        assert_eq!(found.vars.get("line").map(String::as_str), Some("42"));
        assert_eq!(
            hit(&rules, true, text, at).map(|h| h.rule).as_deref(),
            Some("remote")
        );
        assert_eq!(
            hit(&rules, false, text, 2).map(|h| h.rule).as_deref(),
            Some("word")
        );
        assert_eq!(hit(&rules, false, text, 5), None, "the space between");
        Ok(())
    }

    #[test]
    fn templates_check_their_names() -> Result<(), String> {
        let good = rule(r"(?<file>\S+):(?<line>\d+)", On::Any)?;
        assert_eq!(good.check("x"), Ok(()));
        let bad = rule(r"(?<path>\S+)", On::Any)?;
        assert!(bad.check("x").is_err_and(|e| e.contains("{file}")));
        let none = Plumb::default();
        assert!(none.check("x").is_err_and(|e| e.contains("match")));
        let filled = fill("{{{0}}} in {cwd}", |v| Some(v.to_uppercase()));
        assert_eq!(filled.as_deref(), Ok("{0} in CWD"));
        Ok(())
    }
}
