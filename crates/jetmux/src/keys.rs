//! Turns raw bytes from the outer terminal into keys, mouse events and
//! pastes, keeping the original bytes so unbound input passes through as is.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    /// Ctrl plus a lowercase letter or one of `@ \ ] ^ _`.
    Ctrl(char),
    Alt(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Anything else: function keys, modified arrows, focus reports.
    Other,
    /// A kitty keyboard release or bare modifier press: it goes to the app
    /// in the normal layer and is ignored everywhere else, so pressing
    /// shift after the prefix doesn't cancel it.
    Event,
}

/// An SGR mouse report with zero-based cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub button: u16,
    pub x: u16,
    pub y: u16,
    pub press: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Key(Key, Vec<u8>),
    Mouse(Mouse),
    /// Pasted bytes, including the bracketed-paste markers.
    Paste(Vec<u8>),
    /// The outer terminal answered the kitty keyboard query with its
    /// current flags, so it speaks the protocol.
    KittyKeyboard,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Default)]
pub struct Parser {
    /// Inside a bracketed paste that hasn't ended yet.
    pasting: Option<Vec<u8>>,
}

impl Parser {
    pub fn parse(&mut self, mut buf: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            if let Some(paste) = &mut self.pasting {
                match find(buf, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&buf[..end + PASTE_END.len()]);
                        buf = &buf[end + PASTE_END.len()..];
                        out.extend(self.pasting.take().map(Input::Paste));
                    }
                    None => {
                        paste.extend_from_slice(buf);
                        break;
                    }
                }
                continue;
            }
            if buf.starts_with(PASTE_START) {
                self.pasting = Some(PASTE_START.to_vec());
                buf = &buf[PASTE_START.len()..];
                continue;
            }
            let (input, used) = parse_one(buf);
            out.push(input);
            buf = &buf[used.max(1)..];
        }
        out
    }
}

impl Key {
    /// Parse a key name as written in the config: a single char, `C-x`,
    /// `M-x`, or a name like `Enter`, `Tab`, `Esc`, `Space`, `Up`.
    pub fn parse(name: &str) -> Result<Self, String> {
        let single = |s: &str| {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        if let Some(c) = single(name) {
            return Ok(Self::Char(c));
        }
        if let Some(rest) = name.strip_prefix("C-") {
            return match rest {
                "Space" | "@" => Ok(Self::Ctrl('@')),
                r => match single(r) {
                    Some(c @ ('a'..='z' | 'A'..='Z')) => Ok(Self::Ctrl(c.to_ascii_lowercase())),
                    Some(c @ ('\\' | ']' | '^' | '_')) => Ok(Self::Ctrl(c)),
                    _ => Err(format!("can't bind {name}")),
                },
            };
        }
        if let Some(rest) = name.strip_prefix("M-") {
            return match rest {
                "Space" => Ok(Self::Alt(' ')),
                r => single(r)
                    .map(Self::Alt)
                    .ok_or_else(|| format!("can't bind {name}")),
            };
        }
        Ok(match name {
            "Enter" | "Return" => Self::Enter,
            "Tab" => Self::Tab,
            "BTab" | "S-Tab" => Self::BackTab,
            "BSpace" | "Backspace" => Self::Backspace,
            "Esc" | "Escape" => Self::Esc,
            "Space" => Self::Char(' '),
            "Up" => Self::Up,
            "Down" => Self::Down,
            "Left" => Self::Left,
            "Right" => Self::Right,
            "Home" => Self::Home,
            "End" => Self::End,
            "PageUp" | "PPage" => Self::PageUp,
            "PageDown" | "NPage" => Self::PageDown,
            "Insert" | "IC" => Self::Insert,
            "Delete" | "DC" => Self::Delete,
            _ => return Err(format!("unknown key {name}")),
        })
    }

    /// The bytes a terminal sends for this key in normal mode, for
    /// sending the prefix key through.
    pub fn bytes(self) -> Vec<u8> {
        let mut buf = [0u8; 4];
        match self {
            Self::Char(c) => c.encode_utf8(&mut buf).as_bytes().to_vec(),
            Self::Ctrl('@') => vec![0],
            Self::Ctrl(c) if c.is_ascii_lowercase() => vec![c as u8 - b'a' + 1],
            Self::Ctrl(c) => vec![(c as u8) & 0x1f],
            Self::Alt(c) => {
                let mut out = vec![0x1b];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                out
            }
            Self::Enter => vec![b'\r'],
            Self::Tab => vec![b'\t'],
            Self::BackTab => b"\x1b[Z".to_vec(),
            Self::Backspace => vec![0x7f],
            Self::Esc => vec![0x1b],
            Self::Up => b"\x1b[A".to_vec(),
            Self::Down => b"\x1b[B".to_vec(),
            Self::Right => b"\x1b[C".to_vec(),
            Self::Left => b"\x1b[D".to_vec(),
            Self::Home => b"\x1b[H".to_vec(),
            Self::End => b"\x1b[F".to_vec(),
            Self::PageUp => b"\x1b[5~".to_vec(),
            Self::PageDown => b"\x1b[6~".to_vec(),
            Self::Insert => b"\x1b[2~".to_vec(),
            Self::Delete => b"\x1b[3~".to_vec(),
            Self::Other | Self::Event => Vec::new(),
        }
    }
}

/// Written as [`Key::parse`] reads it.
impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Char(' ') => f.write_str("Space"),
            Self::Char(c) => write!(f, "{c}"),
            Self::Ctrl('@') => f.write_str("C-Space"),
            Self::Ctrl(c) => write!(f, "C-{c}"),
            Self::Alt(' ') => f.write_str("M-Space"),
            Self::Alt(c) => write!(f, "M-{c}"),
            Self::Enter => f.write_str("Enter"),
            Self::Tab => f.write_str("Tab"),
            Self::BackTab => f.write_str("BTab"),
            Self::Backspace => f.write_str("BSpace"),
            Self::Esc => f.write_str("Esc"),
            Self::Up => f.write_str("Up"),
            Self::Down => f.write_str("Down"),
            Self::Left => f.write_str("Left"),
            Self::Right => f.write_str("Right"),
            Self::Home => f.write_str("Home"),
            Self::End => f.write_str("End"),
            Self::PageUp => f.write_str("PageUp"),
            Self::PageDown => f.write_str("PageDown"),
            Self::Insert => f.write_str("Insert"),
            Self::Delete => f.write_str("Delete"),
            Self::Other | Self::Event => f.write_str("?"),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_one(buf: &[u8]) -> (Input, usize) {
    let key = |k: Key, n: usize| (Input::Key(k, buf[..n].to_vec()), n);
    match buf[0] {
        0x1b => match buf.get(1) {
            None => key(Key::Esc, 1),
            Some(b'[') => parse_csi(buf),
            Some(b'O') if buf.len() >= 3 => key(
                match buf[2] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => Key::Other,
                },
                3,
            ),
            Some(0x1b) => key(Key::Esc, 1),
            Some(_) => {
                let (ch, n) = decode_char(&buf[1..]);
                match ch {
                    Some(c) if !c.is_control() => key(Key::Alt(c), 1 + n),
                    _ => key(Key::Other, 1 + n),
                }
            }
        },
        b'\r' | b'\n' => key(Key::Enter, 1),
        b'\t' => key(Key::Tab, 1),
        0x7f | 0x08 => key(Key::Backspace, 1),
        0x00 => key(Key::Ctrl('@'), 1),
        b @ 0x01..=0x1a => key(Key::Ctrl(char::from(b'a' + b - 1)), 1),
        b @ 0x1c..=0x1f => key(Key::Ctrl(char::from(b'\\' + b - 0x1c)), 1),
        _ => {
            let (ch, n) = decode_char(buf);
            key(ch.map_or(Key::Other, Key::Char), n)
        }
    }
}

fn parse_csi(buf: &[u8]) -> (Input, usize) {
    // ESC [ params intermediates final
    let Some(end) = buf[2..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
        return (Input::Key(Key::Other, buf.to_vec()), buf.len());
    };
    let len = end + 3;
    let params = &buf[2..len - 1];
    let fin = buf[len - 1];
    if let Some(sgr) = params.strip_prefix(b"<")
        && matches!(fin, b'M' | b'm')
        && let Some(mouse) = parse_sgr_mouse(sgr, fin == b'M')
    {
        return (Input::Mouse(mouse), len);
    }
    if fin == b'u' {
        if params.starts_with(b"?") {
            return (Input::KittyKeyboard, len);
        }
        if let Some(key) = kitty_key(params) {
            return (Input::Key(key, buf[..len].to_vec()), len);
        }
    }
    let key = match (params, fin) {
        (b"" | b"1", b'A') => Key::Up,
        (b"" | b"1", b'B') => Key::Down,
        (b"" | b"1", b'C') => Key::Right,
        (b"" | b"1", b'D') => Key::Left,
        (b"" | b"1", b'H') | (b"1" | b"7", b'~') => Key::Home,
        (b"" | b"1", b'F') | (b"4" | b"8", b'~') => Key::End,
        (b"", b'Z') => Key::BackTab,
        (b"2", b'~') => Key::Insert,
        (b"3", b'~') => Key::Delete,
        (b"5", b'~') => Key::PageUp,
        (b"6", b'~') => Key::PageDown,
        _ => Key::Other,
    };
    (Input::Key(key, buf[..len].to_vec()), len)
}

/// A kitty keyboard protocol key, `CSI code[:shifted] ; mods[:event] u`,
/// as the bindings see it. Combinations bindings can't name are
/// [`Key::Other`], so they only pass through.
fn kitty_key(params: &[u8]) -> Option<Key> {
    const SHIFT: u8 = 1;
    const ALT: u8 = 2;
    const CTRL: u8 = 4;
    /// Caps lock and num lock don't change what a binding means.
    const LOCKS: u8 = 64 | 128;
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    let mut codes = fields.next()?.split(':');
    let code: u32 = codes.next()?.parse().ok()?;
    let shifted = codes
        .next()
        .and_then(|c| c.parse().ok())
        .and_then(char::from_u32);
    let mut modifiers = fields.next().unwrap_or("").split(':');
    let mods = match modifiers.next() {
        None | Some("") => 0,
        Some(m) => m.parse::<u8>().ok()?.saturating_sub(1) & !LOCKS,
    };
    /// Kitty's codes for the shift, ctrl, alt, super, hyper and meta keys.
    const MODIFIER_KEYS: std::ops::RangeInclusive<u32> = 57441..=57452;
    if modifiers.next() == Some("3") || MODIFIER_KEYS.contains(&code) {
        return Some(Key::Event);
    }
    let key = match (code, mods) {
        (13, 0) => Key::Enter,
        (9, 0) => Key::Tab,
        (9, SHIFT) => Key::BackTab,
        (127, 0) => Key::Backspace,
        (27, 0) => Key::Esc,
        (13 | 9 | 127 | 27, _) => Key::Other,
        _ => {
            let ch = char::from_u32(code)?;
            match mods {
                0 => Key::Char(ch),
                SHIFT => Key::Char(shifted.unwrap_or_else(|| ch.to_ascii_uppercase())),
                ALT => Key::Alt(ch),
                CTRL if ch.is_ascii_lowercase() || "@\\]^_".contains(ch) => Key::Ctrl(ch),
                _ => Key::Other,
            }
        }
    };
    Some(key)
}

fn parse_sgr_mouse(params: &[u8], press: bool) -> Option<Mouse> {
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';').map(str::parse::<u16>);
    let (button, x, y) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    Some(Mouse {
        button,
        x: x.checked_sub(1)?,
        y: y.checked_sub(1)?,
        press,
    })
}

/// Decode one UTF-8 char; invalid bytes are consumed one at a time.
fn decode_char(buf: &[u8]) -> (Option<char>, usize) {
    let len = match buf[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return (None, 1),
    };
    match buf.get(..len).and_then(|b| std::str::from_utf8(b).ok()) {
        Some(s) => (s.chars().next(), len),
        None => (None, 1),
    }
}

/// Encode a mouse event for a pane, relative to the pane's origin, in the
/// format the pane asked for. X10-style encodings can't express positions
/// past 222, so those events are dropped.
pub fn encode_mouse(m: Mouse, format: jetmux_proto::MouseFormat) -> Option<Vec<u8>> {
    use jetmux_proto::MouseFormat;
    match format {
        MouseFormat::Sgr | MouseFormat::SgrPixels => Some(
            format!(
                "\x1b[<{};{};{}{}",
                m.button,
                m.x + 1,
                m.y + 1,
                if m.press { 'M' } else { 'm' }
            )
            .into_bytes(),
        ),
        MouseFormat::Urxvt => Some(
            format!(
                "\x1b[{};{};{}M",
                32 + if m.press { m.button } else { 3 },
                m.x + 1,
                m.y + 1
            )
            .into_bytes(),
        ),
        MouseFormat::X10 | MouseFormat::Utf8 => {
            let button = if m.press {
                m.button
            } else {
                3 | (m.button & !3)
            };
            let byte = |v: u16| u8::try_from(32 + v).ok().filter(|b| *b < 255);
            Some(vec![
                0x1b,
                b'[',
                b'M',
                byte(button)?,
                byte(m.x + 1)?,
                byte(m.y + 1)?,
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::keys::{Input, Key, Mouse, Parser};

    fn keys(bytes: &[u8]) -> Vec<Key> {
        Parser::default()
            .parse(bytes)
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k, _) => Some(k),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_keys_and_keeps_raw_bytes() {
        assert_eq!(
            keys("a\x02\x1b[A\x1bh\r\x7fé\x1b[5~\x1bOD".as_bytes()),
            vec![
                Key::Char('a'),
                Key::Ctrl('b'),
                Key::Up,
                Key::Alt('h'),
                Key::Enter,
                Key::Backspace,
                Key::Char('é'),
                Key::PageUp,
                Key::Left,
            ]
        );
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
        let raw = Parser::default().parse(b"\x1b[1;5C");
        assert_eq!(raw, vec![Input::Key(Key::Other, b"\x1b[1;5C".to_vec())]);
    }

    #[test]
    fn kitty_keys_resolve_like_legacy_ones() {
        assert_eq!(
            keys(b"\x1b[98;5u\x1b[99u\x1b[13u\x1b[104;3u\x1b[97;2u\x1b[98;5:3u\x1b[57441;2u"),
            vec![
                Key::Ctrl('b'),
                Key::Char('c'),
                Key::Enter,
                Key::Alt('h'),
                Key::Char('A'),
                Key::Event,
                Key::Event,
            ]
        );
        assert_eq!(
            Parser::default().parse(b"\x1b[?1u"),
            vec![Input::KittyKeyboard]
        );
    }

    #[test]
    fn pastes_span_reads_and_mouse_is_decoded() {
        let mut p = Parser::default();
        assert_eq!(
            p.parse(b"x\x1b[200~ab\x02"),
            vec![Input::Key(Key::Char('x'), b"x".to_vec())]
        );
        assert_eq!(
            p.parse(b"c\x1b[201~y"),
            vec![
                Input::Paste(b"\x1b[200~ab\x02c\x1b[201~".to_vec()),
                Input::Key(Key::Char('y'), b"y".to_vec()),
            ]
        );
        assert_eq!(
            p.parse(b"\x1b[<0;10;5M"),
            vec![Input::Mouse(Mouse {
                button: 0,
                x: 9,
                y: 4,
                press: true
            })]
        );
    }
}
