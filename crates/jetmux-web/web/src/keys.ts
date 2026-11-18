// Keys as the terminal would send them. Key bar entries use the same
// names as config.toml (`C-c`, `M-x`, `Esc`, `Up`); anything else is text.

const NAMED: Record<string, string> = {
  Enter: '\r',
  Tab: '\t',
  BTab: '\x1b[Z',
  BSpace: '\x7f',
  Esc: '\x1b',
  Space: ' ',
  Up: '\x1b[A',
  Down: '\x1b[B',
  Right: '\x1b[C',
  Left: '\x1b[D',
  Home: '\x1b[H',
  End: '\x1b[F',
  PageUp: '\x1b[5~',
  PageDown: '\x1b[6~',
  Insert: '\x1b[2~',
  Delete: '\x1b[3~',
};

/// Control character for `c`, or undefined if it has none.
export function ctrl(c: string): string | undefined {
  const code = c.toLowerCase().charCodeAt(0);
  if (c.length !== 1) return undefined;
  if (code >= 97 && code <= 122) return String.fromCharCode(code - 96);
  const special: Record<string, string> = {
    '@': '\0',
    ' ': '\0',
    '[': '\x1b',
    '\\': '\x1c',
    ']': '\x1d',
    '^': '\x1e',
    _: '\x1f',
  };
  return special[c];
}

export function keyBytes(name: string): string {
  const named = NAMED[name];
  if (named !== undefined) return named;
  const m = /^([CM])-(.+)$/.exec(name);
  if (m && m[2] !== undefined) {
    const inner = keyBytes(m[2]);
    if (m[1] === 'M') return '\x1b' + inner;
    return ctrl(inner) ?? inner;
  }
  return name;
}

/// What a label on the key bar shows.
export function keyLabel(name: string): string {
  const arrows: Record<string, string> = { Up: '↑', Down: '↓', Left: '←', Right: '→', BSpace: '⌫', Enter: '⏎' };
  return arrows[name] ?? name;
}

/// Bytes for a desktop keyboard event, or undefined to let the browser
/// handle it (copy, paste, browser shortcuts).
export function eventBytes(e: KeyboardEvent, appCursor: boolean): string | undefined {
  if (e.metaKey) return undefined;
  const cursor = (c: string) => (appCursor ? '\x1bO' : '\x1b[') + c;
  const special: Record<string, string> = {
    Enter: '\r',
    Backspace: '\x7f',
    Tab: e.shiftKey ? '\x1b[Z' : '\t',
    Escape: '\x1b',
    ArrowUp: cursor('A'),
    ArrowDown: cursor('B'),
    ArrowRight: cursor('C'),
    ArrowLeft: cursor('D'),
    Home: '\x1b[H',
    End: '\x1b[F',
    PageUp: '\x1b[5~',
    PageDown: '\x1b[6~',
    Delete: '\x1b[3~',
    Insert: '\x1b[2~',
  };
  let out = special[e.key];
  if (out === undefined) {
    if (e.key.length !== 1) return undefined;
    out = e.ctrlKey ? ctrl(e.key) : e.key;
    if (out === undefined) return undefined;
  }
  return e.altKey ? '\x1b' + out : out;
}
