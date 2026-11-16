//! The status segments: what their fields say about the focused pane's
//! session, and how a row or column of them is laid out in cells. Plain
//! data, so it tests on any platform.

use std::collections::HashMap;

use jet_config::status::{Align, Field, Segment, StatusStyle};
use jet_core::{Cell, Line, Location, Progress, Rgb};

/// Who and where an `ssh` in front is connected to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ssh {
    pub user: Option<String>,
    pub host: String,
}

/// The destination of an `ssh` command line, if it's one: `ssh -p 22
/// me@box`, `ssh.exe -l me box`, `"C:\...\ssh.exe" box ls`.
pub fn ssh_target(argv: &[String]) -> Option<Ssh> {
    let (program, args) = argv.split_first()?;
    let name = program.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if name != "ssh" && name != "ssh.exe" {
        return None;
    }
    /// Options that take a value, from ssh(1).
    const VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut user = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flags) = arg.strip_prefix('-') else {
            let (u, host) = match arg.rsplit_once('@') {
                Some((u, host)) => (Some(u.to_owned()), host),
                None => (user, arg.as_str()),
            };
            // ssh://user@host:port
            let host = host.strip_prefix("ssh://").unwrap_or(host);
            return Some(Ssh {
                user: u,
                host: host.to_owned(),
            });
        };
        // Flags group (`-tt`); a valued one takes the rest or the next arg.
        for (i, flag) in flags.char_indices() {
            if VALUED.contains(flag) {
                let rest = flags.get(i + 1..).unwrap_or_default();
                let value = if rest.is_empty() {
                    args.next().cloned()
                } else {
                    Some(rest.to_owned())
                };
                if flag == 'l' {
                    user = value;
                }
                break;
            }
        }
    }
    None
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
                .location
                .and_then(|l| l.host.clone())
                .or_else(|| self.ssh.map(|s| s.host.clone()))
                .or_else(|| owned(self.local_host)),
            Field::User => self
                .ssh
                .and_then(|s| s.user.clone())
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

/// Separators in front of and behind a segment, pointing outward from
/// the group's anchored side.
fn caps(style: StatusStyle, align: Align) -> (Option<char>, Option<char>) {
    match (style, align) {
        (StatusStyle::Powerline, Align::Left) => (None, Some('\u{E0B0}')),
        (StatusStyle::Powerline, Align::Right) => (Some('\u{E0B2}'), None),
        (StatusStyle::Round, _) => (Some('\u{E0B6}'), Some('\u{E0B4}')),
        (StatusStyle::Plain, _) => (None, None),
    }
}

fn styled(fg: Rgb, bg: Option<Rgb>) -> Cell {
    let mut cell = Cell::default();
    (cell.fg, cell.bg) = (Some(fg), bg);
    cell
}

/// One group, left to right, as `(text, cells, style)` runs.
fn group(shown: &[&Shown], style: StatusStyle) -> Vec<(String, usize, Cell)> {
    let mut runs = Vec::new();
    for (i, s) in shown.iter().enumerate() {
        let align = s.align;
        let (before, after) = caps(style, align);
        let prev = i.checked_sub(1).and_then(|p| shown.get(p)).map(|p| p.bg);
        let next = shown.get(i + 1).map(|n| n.bg);
        match style {
            // Arrows run into the neighbor's color; round pills and plain
            // blocks keep a gap.
            StatusStyle::Powerline => {}
            _ if i > 0 => runs.push((String::new(), 1, Cell::default())),
            _ => {}
        }
        if let Some(c) = before {
            let behind = if style == StatusStyle::Powerline {
                prev
            } else {
                None
            };
            runs.push((c.to_string(), 1, styled(s.bg, behind)));
        }
        let text = format!(" {} ", s.text);
        runs.push((
            text.clone(),
            Line::text_width(&text),
            styled(s.fg, Some(s.bg)),
        ));
        if let Some(c) = after {
            let behind = if style == StatusStyle::Powerline {
                next
            } else {
                None
            };
            runs.push((c.to_string(), 1, styled(s.bg, behind)));
        }
    }
    runs
}

fn width(runs: &[(String, usize, Cell)]) -> usize {
    runs.iter().map(|(_, w, _)| w).sum()
}

/// Cells a row of `shown` needs, with no room between its ends.
pub fn row_width(shown: &[Shown], style: StatusStyle) -> usize {
    let (left, right) = split(shown);
    let gap = usize::from(!left.is_empty() && !right.is_empty());
    width(&group(&left, style)) + gap + width(&group(&right, style))
}

fn split(shown: &[Shown]) -> (Vec<&Shown>, Vec<&Shown>) {
    shown.iter().partition(|s| s.align == Align::Left)
}

/// A row `cols` wide: left segments, room, right segments. What doesn't
/// fit is cut from the right.
pub fn row(shown: &[Shown], style: StatusStyle, cols: usize) -> Line {
    let (left, right) = split(shown);
    let (left, right) = (group(&left, style), group(&right, style));
    let room = cols.saturating_sub(width(&left) + width(&right));
    let blank = (String::new(), room, Cell::default());
    let mut line = Line::default();
    let mut used = 0;
    for (text, w, cell) in left.into_iter().chain([blank]).chain(right) {
        let w = w.min(cols - used);
        line.push_text(&text, w, cell);
        used += w;
    }
    line
}

/// One segment per row, `cols` wide, for a side tab bar.
pub fn column(shown: &[Shown], cols: usize) -> Vec<Line> {
    let rows = shown.iter().map(|s| {
        let text = format!(" {}", s.text);
        Line::from_text(&text, cols, styled(s.fg, Some(s.bg)))
    });
    rows.collect()
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
    use jet_config::Color;
    use jet_config::status::{Align, Field, Segment, StatusStyle, Template};
    use jet_core::{Location, Rgb};

    use crate::status::{Session, Ssh, column, row, row_width, shown, ssh_target};

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn ssh_destinations() {
        let ssh = |line: &str| ssh_target(&argv(line));
        let me_box = Some(Ssh {
            user: Some("me".into()),
            host: "box".into(),
        });
        assert_eq!(ssh("ssh me@box"), me_box);
        assert_eq!(ssh("ssh.exe -p 2222 -A me@box ls"), me_box);
        assert_eq!(ssh("C:\\OpenSSH\\SSH.EXE -l me -tt box"), me_box);
        assert_eq!(ssh("ssh -lme box"), me_box);
        assert_eq!(ssh("ssh -i key box").map(|s| s.user), Some(None));
        assert_eq!(ssh("sshd -D"), None);
        assert_eq!(ssh("ssh -V"), None);
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
    }

    #[test]
    fn powerline_rows_fill_with_room_in_the_middle() {
        let gray = |v: u8| Rgb { r: v, g: v, b: v };
        let seg = |text: &str, align, bg: u8| Segment {
            text: Template::try_from(text.to_owned()).unwrap_or_default(),
            align,
            foreground: None,
            background: Some(Color(gray(bg))),
        };
        let segments = [
            seg("a", Align::Left, 1),
            seg("{ssh}", Align::Left, 9),
            seg("b", Align::Right, 2),
            seg("{tab}", Align::Right, 3),
        ];
        let session = Session {
            tab: 4,
            ..Session::default()
        };
        // No ssh: that segment hides.
        let shown = shown(&segments, &session, gray(255), gray(0));
        assert_eq!(shown.len(), 3);
        // " a " + arrow, gap, arrow + " b " + arrow + " 4 ".
        assert_eq!(row_width(&shown, StatusStyle::Powerline), 4 + 1 + 8);
        let line = row(&shown, StatusStyle::Powerline, 20);
        assert_eq!(line.cells.len(), 20);
        assert_eq!(line.text(), " a \u{E0B0}        \u{E0B2} b \u{E0B2} 4");
        // The arrow between b and 4 is 4's color over b's.
        let arrow = line.cells.get(16);
        assert_eq!(arrow.and_then(|c| c.fg).map(|c| c.r), Some(3));
        assert_eq!(arrow.and_then(|c| c.bg).map(|c| c.r), Some(2));
        // Too narrow: cut, never wider.
        assert_eq!(row(&shown, StatusStyle::Plain, 5).cells.len(), 5);
        let rows = column(&shown, 6);
        let text: Vec<String> = rows.iter().map(|l| l.text()).collect();
        assert_eq!(text, [" a", " b", " 4"]);
    }
}
