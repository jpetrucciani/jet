//! The status segments: what their fields say about the focused pane's
//! session, and how a row or column of them is laid out in cells. Plain
//! data, so it tests on any platform.

use jet_config::status::{Align, Shown, StatusStyle};
use jet_core::{Cell, Line, Rgb, ssh_destination};

/// `argv`, an `ssh` command line, run again on the same host (a remote
/// command it had is dropped): `command` there, else a login shell; in
/// `dir` if given.
pub fn ssh_again(argv: &[String], dir: Option<&str>, command: Option<&str>) -> Option<Vec<String>> {
    let (at, _) = ssh_destination(argv)?;
    let mut again = argv.get(..at)?.to_vec();
    // A terminal for the remote shell, even with a command.
    again.push("-t".into());
    again.push(argv.get(at)?.clone());
    let cd = dir.map(|dir| format!("cd -- {} 2>/dev/null; ", sh_quote(dir)));
    let run = command.map(|c| format!("-lc {}", sh_quote(c)));
    if cd.is_some() || run.is_some() {
        again.push(format!(
            "{}exec \"${{SHELL:-sh}}\" {}",
            cd.unwrap_or_default(),
            run.as_deref().unwrap_or("-l")
        ));
    }
    Some(again)
}

/// `text` as one POSIX shell word.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `argv`, a `wsl.exe` command line, run again for a shell in the same
/// distro as the same user (a command it ran is dropped), in the Linux
/// directory `dir` if given.
pub fn wsl_again(argv: &[String], dir: Option<&str>) -> Option<Vec<String>> {
    let (program, args) = argv.split_first()?;
    let name = program.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if name != "wsl" && name != "wsl.exe" {
        return None;
    }
    let mut again = vec![program.clone()];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-d" | "--distribution" | "--distribution-id" | "-u" | "--user" | "--shell-type" => {
                again.push(arg.clone());
                again.push(args.next()?.clone());
            }
            "--cd" => {
                args.next();
            }
            // A command (`-e`, `--`, or bare words) from here on.
            _ => break,
        }
    }
    if let Some(dir) = dir {
        again.extend(["--cd".into(), dir.to_owned()]);
    }
    Some(again)
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

#[cfg(test)]
mod tests {
    use jet_config::Color;
    use jet_config::status::{Align, Segment, Session, StatusStyle, Template, shown};
    use jet_core::Rgb;

    use crate::status::{column, row, row_width, sh_quote, ssh_again, wsl_again};

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn ssh_runs_again_into_a_directory() {
        let again = |line: &str, dir| ssh_again(&argv(line), dir, None).map(|a| a.join(" "));
        assert_eq!(
            again("ssh -p 2222 -J hop me@box htop", None).as_deref(),
            Some("ssh -p 2222 -J hop -t me@box")
        );
        assert_eq!(
            again("ssh box", Some("/srv/it's")).as_deref(),
            Some(r#"ssh -t box cd -- '/srv/it'\''s' 2>/dev/null; exec "${SHELL:-sh}" -l"#)
        );
        assert_eq!(again("ssh -V", None), None);
        // A command runs in a login shell; matched text stays one word.
        let view = ssh_again(&argv("ssh box"), None, Some(&sh_quote("a;b"))).map(|a| a.join(" "));
        assert_eq!(
            view.as_deref(),
            Some(r#"ssh -t box exec "${SHELL:-sh}" -lc ''\''a;b'\'''"#)
        );
    }

    #[test]
    fn wsl_runs_again_into_a_directory() {
        let again = |line: &str, dir| wsl_again(&argv(line), dir).map(|a| a.join(" "));
        assert_eq!(
            again("wsl.exe -d NixOS --cd /tmp -- htop", Some("/srv")).as_deref(),
            Some("wsl.exe -d NixOS --cd /srv")
        );
        assert_eq!(again("wsl -u root", None).as_deref(), Some("wsl -u root"));
        assert_eq!(again("ssh box", None), None);
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
