//! OSC reports libghostty parses but doesn't hand out, found by scanning
//! output for them: ConEmu's progress (`ESC ] 9 ; 4 ; state [; percent]`,
//! sent by winget, cargo, PowerShell and others) and iTerm2/WezTerm user
//! variables (`ESC ] 1337 ; SetUserVar=name=base64`), which shells and
//! scripts use to tell the terminal things; desktop notifications (iTerm2's
//! `ESC ] 9 ; text`, rxvt's `ESC ] 777 ; notify ; title ; body`); and the end
//! of a command with its exit status (`ESC ] 133 ; D [; status]`).

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Progress {
    /// No progress (state 0): clear it.
    #[default]
    Hidden,
    Normal(u8),
    Error(u8),
    /// Busy, no percentage.
    Indeterminate,
    Paused(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Escape,
    /// Inside an OSC: its body so far is in the buffer.
    Body,
}

/// Finds reports in output, across write boundaries.
#[derive(Debug, Default)]
pub(crate) struct Scanner {
    scan: Scan,
    body: Vec<u8>,
    /// The last progress report since the previous take.
    pub progress: Option<Progress>,
    /// User variables set since the previous take, in order.
    pub vars: Vec<(String, String)>,
    /// Notifications asked for since the previous take.
    pub notifications: Vec<Notification>,
    /// Commands that finished since the previous take, with their exit
    /// status when the shell gave one.
    pub finished: Vec<Option<i32>>,
}

/// A desktop notification a program asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// Empty for OSC 9, which has only a body.
    pub title: String,
    pub body: String,
}

/// Other OSC bodies (hyperlinks, titles) are dropped once this long...
const MAX_BODY: usize = 16;
/// ...but a user variable can be longer...
const MAX_VAR: usize = 4096;
/// ...and so can a notification.
const MAX_NOTIFICATION: usize = 1024;
const SET_USER_VAR: &[u8] = b"1337;SetUserVar=";

impl Scanner {
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // Between sequences only an ESC matters: jump to the next one
            // (a vectorized search) instead of stepping through text.
            if self.scan == Scan::Ground {
                let Some(i) = bytes.iter().position(|&b| b == 0x1b) else {
                    return;
                };
                bytes = &bytes[i..];
            }
            let Some((&b, rest)) = bytes.split_first() else {
                return;
            };
            bytes = rest;
            self.scan = match (self.scan, b) {
                (Scan::Ground, 0x1b) | (Scan::Escape, 0x1b) => Scan::Escape,
                (Scan::Escape, b']') => {
                    self.body.clear();
                    Scan::Body
                }
                // BEL ends it; so does ESC, the start of ST (`ESC \`).
                (Scan::Body, 0x07 | 0x1b) => {
                    self.finish();
                    if b == 0x1b {
                        Scan::Escape
                    } else {
                        Scan::Ground
                    }
                }
                (Scan::Body, b) if self.wants_more() => {
                    self.body.push(b);
                    Scan::Body
                }
                _ => Scan::Ground,
            };
        }
    }

    fn wants_more(&self) -> bool {
        let n = self.body.len();
        let body = &self.body;
        n < MAX_BODY
            || (n < MAX_VAR && body.starts_with(SET_USER_VAR))
            || (n < MAX_NOTIFICATION && (body.starts_with(b"9;") || body.starts_with(b"777;")))
    }

    fn finish(&mut self) {
        if let Some(progress) = parse_progress(&self.body) {
            self.progress = Some(progress);
        } else if let Some(var) = parse_var(&self.body) {
            self.vars.push(var);
        } else if let Some(notification) = parse_notification(&self.body) {
            self.notifications.push(notification);
        } else if let Some(rest) = self.body.strip_prefix(b"133;D") {
            // `133;D;status`, maybe followed by more `;key=value`s.
            let status = std::str::from_utf8(rest)
                .ok()
                .and_then(|r| r.strip_prefix(';')?.split(';').next()?.parse().ok());
            self.finished.push(status);
        }
    }
}

fn parse_progress(body: &[u8]) -> Option<Progress> {
    let rest = std::str::from_utf8(body.strip_prefix(b"9;4;")?).ok()?;
    let mut parts = rest.split(';');
    let state: u8 = parts.next()?.parse().ok()?;
    let percent = parts
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .map_or(0, |p| p.min(100) as u8);
    Some(match state {
        0 => Progress::Hidden,
        1 => Progress::Normal(percent),
        2 => Progress::Error(percent),
        3 => Progress::Indeterminate,
        4 => Progress::Paused(percent),
        _ => return None,
    })
}

/// OSC 9 with text (not one of ConEmu's numbered `9;N` commands, like
/// progress), or `777;notify;title;body`.
fn parse_notification(body: &[u8]) -> Option<Notification> {
    let text = std::str::from_utf8(body).ok()?;
    if let Some(rest) = text.strip_prefix("777;notify;") {
        let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
        return Some(Notification {
            title: title.to_owned(),
            body: body.to_owned(),
        });
    }
    let rest = text.strip_prefix("9;")?;
    let code = rest.split(';').next().unwrap_or("");
    if rest.is_empty() || (!code.is_empty() && code.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(Notification {
        title: String::new(),
        body: rest.to_owned(),
    })
}

/// `name=base64`; an empty value unsets it.
fn parse_var(body: &[u8]) -> Option<(String, String)> {
    let rest = std::str::from_utf8(body.strip_prefix(SET_USER_VAR)?).ok()?;
    let (name, value) = rest.split_once('=')?;
    if name.is_empty() {
        return None;
    }
    let value = String::from_utf8(base64(value)?).ok()?;
    Some((name.to_owned(), value))
}

/// Standard base64, padding optional.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for b in text.trim_end_matches('=').bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use crate::osc::{Notification, Progress, Scanner};

    #[test]
    fn progress_is_found_across_writes() {
        let mut scan = Scanner::default();
        scan.feed(b"building\x1b]9;4;1;4");
        assert_eq!(scan.progress.take(), None);
        scan.feed(b"2\x07 done");
        assert_eq!(scan.progress.take(), Some(Progress::Normal(42)));
        scan.feed(b"\x1b]9;4;2;100\x1b\\");
        assert_eq!(scan.progress.take(), Some(Progress::Error(100)));
        scan.feed(b"\x1b]9;4;3\x07\x1b]9;4;0;0\x07");
        assert_eq!(scan.progress.take(), Some(Progress::Hidden));
    }

    #[test]
    fn other_sequences_are_ignored() {
        let mut scan = Scanner::default();
        scan.feed(b"\x1b]0;9;4;1;50\x07");
        scan.feed(b"\x1b]9;9;C:\\\x07\x1b[31m");
        scan.feed(b"\x1b]9;4;7;1\x07");
        assert_eq!((scan.progress, scan.vars.len()), (None, 0));
        assert_eq!(scan.notifications, vec![]);
    }

    #[test]
    fn notifications_and_finished_commands() {
        let mut scan = Scanner::default();
        scan.feed(b"\x1b]9;build done\x07\x1b]777;notify;cargo;tests passed\x1b\\");
        scan.feed(b"\x1b]133;D;2\x07\x1b]133;D\x07\x1b]133;D;0;aid=7\x07");
        let note = |title: &str, body: &str| Notification {
            title: title.into(),
            body: body.into(),
        };
        assert_eq!(
            scan.notifications,
            vec![note("", "build done"), note("cargo", "tests passed")]
        );
        assert_eq!(scan.finished, vec![Some(2), None, Some(0)]);
    }

    #[test]
    fn user_vars_decode() {
        let mut scan = Scanner::default();
        // "prod-cluster", and an unset.
        scan.feed(b"\x1b]1337;SetUserVar=k8s=cHJvZC1jbHVzdGVy\x07\x1b]1337;SetUserVar=");
        scan.feed(b"venv=\x1b\\");
        let vars = std::mem::take(&mut scan.vars);
        let want = [("k8s", "prod-cluster"), ("venv", "")];
        assert!(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).eq(want));
    }
}
