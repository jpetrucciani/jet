//! OSC reports libghostty parses but doesn't hand out, found by scanning
//! output for them: ConEmu's progress (`ESC ] 9 ; 4 ; state [; percent]`,
//! sent by winget, cargo, PowerShell and others) and iTerm2/WezTerm user
//! variables (`ESC ] 1337 ; SetUserVar=name=base64`), which shells and
//! scripts use to tell the terminal things; desktop notifications (iTerm2's
//! `ESC ] 9 ; text`, rxvt's `ESC ] 777 ; notify ; title ; body`); and the end
//! shell's prompt marks (`ESC ] 133 ; A`, `B`, `C`, `D [; status]`), which
//! the writer splits output at so each lands where the cursor was; kitty's
//! notifications (`ESC ] 99 ; metadata ; payload`); the mouse pointer shape
//! (`ESC ] 22 ; name`); and the context a shell, `ssh`, `run0` or a
//! container announces (`ESC ] 3008 ; start=id ; field=value...`).

use std::collections::HashMap;

use jetmux_proto::pane::{Context, ContextKind, Notification, Occasion, Progress};

use crate::blocks::Mark;

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
    /// The prompt mark the last sequence was, for [`Scanner::next_mark`].
    mark: Option<Mark>,
    /// The last pointer shape asked for since the previous take.
    pub pointer: Option<PointerShape>,
    /// Answers to write back (OSC 99 capability queries).
    pub reply: Vec<u8>,
    /// Open contexts by id, innermost last. Resets (RIS) leave them be.
    contexts: Vec<(String, Context)>,
    /// The innermost context changed since the previous take.
    pub context_changed: bool,
    /// OSC 99 notifications still arriving in chunks, by id.
    chunks: HashMap<String, Notification>,
}

/// A mouse pointer, by its CSS name (`text`, `pointer`, `ew-resize`...).
/// `default` is the arrow; [`PointerShape::RESET`] gives jet's own back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerShape(&'static str);

/// CSS cursor names, then the X11 ones xterm accepts, as CSS.
const POINTERS: &[(&str, &str)] = &[
    ("default", "default"),
    ("context-menu", "context-menu"),
    ("help", "help"),
    ("pointer", "pointer"),
    ("progress", "progress"),
    ("wait", "wait"),
    ("cell", "cell"),
    ("crosshair", "crosshair"),
    ("text", "text"),
    ("vertical-text", "vertical-text"),
    ("alias", "alias"),
    ("copy", "copy"),
    ("move", "move"),
    ("no-drop", "no-drop"),
    ("not-allowed", "not-allowed"),
    ("grab", "grab"),
    ("grabbing", "grabbing"),
    ("all-scroll", "all-scroll"),
    ("col-resize", "col-resize"),
    ("row-resize", "row-resize"),
    ("n-resize", "n-resize"),
    ("e-resize", "e-resize"),
    ("s-resize", "s-resize"),
    ("w-resize", "w-resize"),
    ("ne-resize", "ne-resize"),
    ("nw-resize", "nw-resize"),
    ("se-resize", "se-resize"),
    ("sw-resize", "sw-resize"),
    ("ew-resize", "ew-resize"),
    ("ns-resize", "ns-resize"),
    ("nesw-resize", "nesw-resize"),
    ("nwse-resize", "nwse-resize"),
    ("zoom-in", "zoom-in"),
    ("zoom-out", "zoom-out"),
    ("left_ptr", "default"),
    ("xterm", "text"),
    ("hand1", "pointer"),
    ("hand2", "pointer"),
    ("watch", "wait"),
    ("question_arrow", "help"),
    ("fleur", "move"),
    ("sb_h_double_arrow", "ew-resize"),
    ("sb_v_double_arrow", "ns-resize"),
    ("cross", "crosshair"),
    ("tcross", "crosshair"),
];

impl PointerShape {
    /// No shape asked for: whatever jet shows there.
    pub const RESET: Self = Self("");

    pub fn parse(name: &str) -> Option<Self> {
        if name.is_empty() {
            return Some(Self::RESET);
        }
        let name = name.to_ascii_lowercase();
        POINTERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, css)| Self(css))
    }

    /// The CSS name; empty for [`PointerShape::RESET`].
    pub fn name(self) -> &'static str {
        self.0
    }
}

/// Other OSC bodies (hyperlinks, titles) are dropped once this long...
const MAX_BODY: usize = 16;
/// ...but a user variable can be longer...
const MAX_VAR: usize = 4096;
/// ...and so can a notification.
const MAX_NOTIFICATION: usize = 1024;
const SET_USER_VAR: &[u8] = b"1337;SetUserVar=";
/// Contexts this deep and more are ignored...
const MAX_CONTEXTS: usize = 64;
/// ...as are fields longer than this...
const MAX_FIELD: usize = 256;
/// ...and notifications still arriving in chunks beyond this many.
const MAX_CHUNKED: usize = 16;

impl Scanner {
    /// Scan `bytes` up to the first prompt mark, returning it and the
    /// offset just past it; `None` once all of `bytes` is scanned. Other
    /// reports are collected on the way.
    pub fn next_mark(&mut self, bytes: &[u8]) -> Option<(usize, Mark)> {
        let mut at = 0;
        while at < bytes.len() {
            // Between sequences only an ESC matters: jump to the next one
            // (a vectorized search) instead of stepping through text.
            if self.scan == Scan::Ground {
                at += find_esc(&bytes[at..])?;
            }
            let b = bytes[at];
            at += 1;
            self.scan = match (self.scan, b) {
                (Scan::Ground, 0x1b) | (Scan::Escape, 0x1b) => Scan::Escape,
                (Scan::Escape, b']') => {
                    self.body.clear();
                    Scan::Body
                }
                // BEL ends it; so does ESC, the start of ST (`ESC \`).
                (Scan::Body, 0x07 | 0x1b) => {
                    self.finish();
                    let scan = if b != 0x1b {
                        Scan::Ground
                    } else if bytes.get(at) == Some(&b'\\') {
                        // The whole ST, so a split here leaves no half.
                        at += 1;
                        Scan::Ground
                    } else {
                        Scan::Escape
                    };
                    if let Some(mark) = self.mark.take() {
                        self.scan = scan;
                        return Some((at, mark));
                    }
                    scan
                }
                (Scan::Body, b) if self.wants_more() => {
                    self.body.push(b);
                    Scan::Body
                }
                _ => Scan::Ground,
            };
        }
        None
    }

    /// Scan all of `bytes`, marks included.
    #[cfg(test)]
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while let Some((end, _)) = self.next_mark(bytes) {
            bytes = &bytes[end..];
        }
    }

    fn wants_more(&self) -> bool {
        let n = self.body.len();
        let body = &self.body;
        n < MAX_BODY
            || (n < MAX_VAR && body.starts_with(SET_USER_VAR))
            || (n < MAX_VAR && body.starts_with(b"3008;"))
            || (n < 64 && body.starts_with(b"22;"))
            || (n < MAX_NOTIFICATION
                && [&b"9;"[..], b"777;", b"99;"]
                    .iter()
                    .any(|p| body.starts_with(p)))
    }

    fn finish(&mut self) {
        if let Some(progress) = parse_progress(&self.body) {
            self.progress = Some(progress);
        } else if let Some(var) = parse_var(&self.body) {
            self.vars.push(var);
        } else if let Some(notification) = parse_notification(&self.body) {
            self.notifications.push(notification);
        } else if let Some(rest) = self.body.strip_prefix(b"99;") {
            let rest = String::from_utf8_lossy(rest).into_owned();
            self.kitty_notification(&rest);
        } else if let Some(rest) = self.body.strip_prefix(b"22;") {
            let name = String::from_utf8_lossy(rest);
            if let Some(shape) = PointerShape::parse(name.trim()) {
                self.pointer = Some(shape);
            }
        } else if let Some(rest) = self.body.strip_prefix(b"3008;") {
            let rest = String::from_utf8_lossy(rest).into_owned();
            self.context(&rest);
        } else if let Some(rest) = self.body.strip_prefix(b"133;") {
            // A letter, maybe followed by `;status` and `;key=value`s.
            let rest = String::from_utf8_lossy(rest);
            let mut parts = rest.split(';');
            self.mark = match parts.next() {
                Some("A" | "N") => Some(Mark::Prompt),
                Some("B") => Some(Mark::Input),
                Some("C") => Some(Mark::Output),
                Some("D") => {
                    let status = parts.next().and_then(|s| s.parse().ok());
                    self.finished.push(status);
                    Some(Mark::Done(status))
                }
                _ => None,
            };
        }
    }
}

impl Scanner {
    /// The innermost open context.
    pub fn active_context(&self) -> Option<&Context> {
        self.contexts.last().map(|(_, context)| context)
    }

    /// `metadata;payload`, metadata being `key=value`s split by `:`. Parts
    /// with `d=0` wait for the rest under the same `i=`.
    fn kitty_notification(&mut self, body: &str) {
        let (meta, payload) = body.split_once(';').unwrap_or((body, ""));
        let (mut id, mut done, mut part, mut base64ed, mut occasion) =
            ("", true, "title", false, None);
        for pair in meta.split(':') {
            match pair.split_once('=') {
                Some(("i", v)) => id = v,
                Some(("d", v)) => done = v != "0",
                Some(("p", v)) => part = v,
                Some(("e", v)) => base64ed = v == "1",
                Some(("o", "always")) => occasion = Some(Occasion::Always),
                Some(("o", "unfocused")) => occasion = Some(Occasion::Unwatched),
                Some(("o", "invisible")) => occasion = Some(Occasion::Hidden),
                _ => {}
            }
        }
        if part == "?" {
            let id: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
            self.reply.extend_from_slice(
                format!("\x1b]99;i={id}:p=?;p=title,body,?:o=always,unfocused,invisible\x1b\\")
                    .as_bytes(),
            );
            return;
        }
        let text = if base64ed {
            match base64(payload).and_then(|b| String::from_utf8(b).ok()) {
                Some(text) => text,
                None => return,
            }
        } else {
            payload.to_owned()
        };
        if !self.chunks.contains_key(id) && self.chunks.len() >= MAX_CHUNKED {
            return;
        }
        let note = self.chunks.entry(id.to_owned()).or_insert(Notification {
            occasion: Occasion::Always,
            ..Notification::default()
        });
        if let Some(occasion) = occasion {
            note.occasion = occasion;
        }
        match part {
            "title" => note.title.push_str(&text),
            "body" => note.body.push_str(&text),
            // Icons, buttons, closing: not shown.
            _ => {}
        }
        if done
            && let Some(note) = self.chunks.remove(id)
            && (!note.title.is_empty() || !note.body.is_empty())
        {
            self.notifications.push(note);
        }
    }

    /// `start=id;field=value...` opens (or redoes) a context; `end=id`
    /// closes it and any inside it.
    fn context(&mut self, body: &str) {
        let mut fields = body.split(';');
        let Some((verb, id)) = fields.next().and_then(|f| f.split_once('=')) else {
            return;
        };
        if id.is_empty() || id.len() > 64 {
            return;
        }
        let at = self.contexts.iter().position(|(open, _)| open == id);
        match verb {
            "end" => {
                if let Some(at) = at {
                    self.contexts.truncate(at);
                    self.context_changed = true;
                }
            }
            "start" => {
                let mut context = Context::default();
                for (key, value) in fields.filter_map(|f| f.split_once('=')) {
                    if value.len() > MAX_FIELD {
                        continue;
                    }
                    let value = unescape(value);
                    match key {
                        "type" => context.kind = context_kind(&value),
                        "user" => context.user = Some(value),
                        "hostname" => context.hostname = Some(value),
                        "comm" => context.comm = Some(value),
                        "cwd" => context.cwd = Some(value),
                        "container" => context.container = Some(value),
                        "vm" => context.vm = Some(value),
                        "targetuser" => context.target_user = Some(value),
                        "targethost" => context.target_host = Some(value),
                        _ => {}
                    }
                }
                match at {
                    // Redone: reset, and whatever it held ends.
                    Some(at) => self.contexts.truncate(at),
                    None if self.contexts.len() >= MAX_CONTEXTS => return,
                    None => {}
                }
                self.contexts.push((id.to_owned(), context));
                self.context_changed = true;
            }
            _ => {}
        }
    }
}

fn context_kind(name: &str) -> Option<ContextKind> {
    Some(match name {
        "boot" => ContextKind::Boot,
        "container" => ContextKind::Container,
        "vm" => ContextKind::Vm,
        "elevate" => ContextKind::Elevate,
        "chpriv" => ContextKind::Chpriv,
        "subcontext" => ContextKind::Subcontext,
        "remote" => ContextKind::Remote,
        "shell" => ContextKind::Shell,
        "command" => ContextKind::Command,
        "app" => ContextKind::App,
        "service" => ContextKind::Service,
        "session" => ContextKind::Session,
        _ => return None,
    })
}

/// OSC 3008 values escape `;` and `\` (and anything else) as `\xNN`.
fn unescape(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 2..i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b, bytes.get(i + 1), hex) {
            (b'\\', Some(b'x'), Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
            occasion: Occasion::Unwatched,
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
        occasion: Occasion::Unwatched,
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

/// Where the first ESC in `bytes` is. Text between escapes is most of a
/// flood, so this checks 32 bytes a step with no early exit inside one,
/// which compiles to SIMD compares; `position` alone goes a byte at a
/// time, and how far a profile-guided build unrolls it varies.
pub(crate) fn find_esc(bytes: &[u8]) -> Option<usize> {
    const STEP: usize = 32;
    let (chunks, rest) = bytes.as_chunks::<STEP>();
    for (n, chunk) in chunks.iter().enumerate() {
        if chunk.iter().fold(false, |any, &b| any | (b == 0x1b)) {
            return chunk.iter().position(|&b| b == 0x1b).map(|i| n * STEP + i);
        }
    }
    let done = chunks.len() * STEP;
    rest.iter().position(|&b| b == 0x1b).map(|i| done + i)
}

#[cfg(test)]
mod tests {
    use jetmux_proto::pane::{ContextKind, Notification, Occasion, Progress};

    use crate::osc::{PointerShape, Scanner, find_esc};

    #[test]
    fn esc_is_found_in_and_between_steps() {
        let mut text = vec![b'a'; 100];
        assert_eq!(find_esc(&text), None);
        for at in [0, 31, 32, 63, 64, 99] {
            text.fill(b'a');
            text[at] = 0x1b;
            text[99.min(at + 5)] = 0x1b;
            assert_eq!(find_esc(&text), Some(at));
        }
        assert_eq!(find_esc(b""), None);
    }

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
            occasion: Occasion::Unwatched,
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

    #[test]
    fn kitty_notifications_join_their_parts() {
        let mut scan = Scanner::default();
        scan.feed(b"\x1b]99;i=1:d=0;Build\x1b\\\x1b]99;i=1:d=0:p=body:e=1;ZG9uZQ==\x1b\\");
        assert_eq!(scan.notifications, vec![]);
        scan.feed(b"\x1b]99;i=1:p=body:o=invisible; ok\x1b\\\x1b]99;;hi\x07");
        let note = |title: &str, body: &str, occasion| Notification {
            title: title.into(),
            body: body.into(),
            occasion,
        };
        assert_eq!(
            scan.notifications,
            vec![
                note("Build", "done ok", Occasion::Hidden),
                note("hi", "", Occasion::Always)
            ]
        );
        scan.feed(b"\x1b]99;i=q:p=?;\x1b\\");
        let reply = String::from_utf8_lossy(&scan.reply);
        assert!(reply.starts_with("\x1b]99;i=q:p=?;p=title,body"), "{reply}");
    }

    #[test]
    fn pointer_shapes_by_css_or_x11_name() {
        let mut scan = Scanner::default();
        scan.feed(b"\x1b]22;hand2\x07");
        assert_eq!(scan.pointer.take().map(PointerShape::name), Some("pointer"));
        scan.feed(b"\x1b]22;bogus\x07");
        assert_eq!(scan.pointer.take(), None);
        scan.feed(b"\x1b]22;\x07");
        assert_eq!(scan.pointer.take(), Some(PointerShape::RESET));
    }

    #[test]
    fn contexts_nest_and_end() {
        let mut scan = Scanner::default();
        scan.feed(b"\x1b]3008;start=a;type=shell;user=me;hostname=desk\x1b\\");
        scan.feed(b"\x1b]3008;start=b;type=remote;user=me;hostname=desk;targethost=box\x1b\\");
        scan.feed(b"\x1b]3008;start=c;type=elevate;user=me;targetuser=root;cwd=/a\\x3bb\x1b\\");
        let top = scan.active_context().cloned().unwrap_or_default();
        assert_eq!(top.kind, Some(ContextKind::Elevate));
        assert_eq!(
            (top.run_as(), top.cwd.as_deref()),
            (Some("root"), Some("/a;b"))
        );
        // Ending the remote one ends what's inside it too.
        scan.feed(b"\x1b]3008;end=b;exit=success\x1b\\");
        let top = scan.active_context().cloned().unwrap_or_default();
        assert_eq!(top.host(), Some("desk"));
        scan.feed(b"\x1b]3008;end=a\x1b\\\x1b]3008;end=zz\x1b\\");
        assert_eq!(scan.active_context(), None);
        assert!(scan.context_changed);
    }
}
