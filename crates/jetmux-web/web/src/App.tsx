import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js';
import { api } from './api';
import { Connection, type Status } from './connection';
import { CanvasGrid, type GridRenderer } from './grid';
import { ctrl, eventBytes, keyBytes, keyLabel } from './keys';
import type { Action } from './proto/Action';
import type { BlockInfo } from './proto/BlockInfo';
import type { Me } from './proto/Me';
import type { Snippet } from './proto/Snippet';
import type { UiConfig } from './proto/UiConfig';
import type { View } from './proto/View';

/** Always on the key bar; config and profiles add rows below it. */
const BASE_KEYS = ['Esc', 'Ctrl', 'Alt', 'Tab', 'Left', 'Down', 'Up', 'Right', '|', '~', '-', '/'];
const LONG_PRESS_MS = 550;
/** Following a wide session, text shrinks to fit the screen but no
 * smaller than this; past it, one finger pans. */
const MIN_FIT_SCALE = 0.7;
const SWIPE_PX = 70;

function sessionFromHash(): string | undefined {
  const s = decodeURIComponent(location.hash.slice(1));
  return s === '' ? undefined : s;
}

export function App() {
  const [me] = createResource(api.me);
  const [config] = createResource(async () => {
    const config = await api.config();
    // The page around the panes in jet's colors too.
    const root = document.documentElement.style;
    root.setProperty('--bg', config.theme.background);
    root.setProperty('--fg', config.theme.foreground);
    return config;
  });
  const [session, setSession] = createSignal(sessionFromHash());
  const onHash = () => setSession(sessionFromHash());
  window.addEventListener('hashchange', onHash);
  onCleanup(() => window.removeEventListener('hashchange', onHash));

  return (
    <Show when={me() && config()} fallback={<p class="note">{me.error ? String(me.error) : 'connecting…'}</p>}>
      <Show when={session() ?? me()?.session ?? undefined} keyed fallback={<Sessions me={me()!} config={config()!} />}>
        {(name) => <Term session={name} me={me()!} config={config()!} />}
      </Show>
    </Show>
  );
}

function Sessions(props: { me: Me; config: UiConfig }) {
  const [sessions, { refetch }] = createResource(api.sessions);
  const timer = window.setInterval(refetch, 3000);
  onCleanup(() => window.clearInterval(timer));
  const [note, setNote] = createSignal<string>();
  const launch = async (profile: string) => {
    try {
      await api.launch(profile);
      location.hash = encodeURIComponent(profile);
    } catch (e) {
      setNote(String(e));
    }
  };
  return (
    <main class="sessions">
      <header>
        <h1>jetmux</h1>
        <span class="who">
          {props.me.user ?? 'local'} · {props.me.role}
        </span>
      </header>
      <For each={sessions() ?? []} fallback={<p class="note">no sessions</p>}>
        {(s) => (
          <a class="card" href={'#' + encodeURIComponent(s.name)}>
            <strong>{s.name}</strong>
            <span>
              {s.tabs} tabs · {s.panes} panes · {s.size.cols}×{s.size.rows}
              {s.clients > 0 ? ` · ${s.clients} attached` : ''}
            </span>
          </a>
        )}
      </For>
      <Show when={props.me.role === 'owner' && props.config.profiles.length > 0}>
        <h2>start</h2>
        <div class="launchers">
          <For each={props.config.profiles}>{(p) => <button onClick={() => launch(p.name)}>▶ {p.name}</button>}</For>
        </div>
      </Show>
      <Push me={props.me} onNote={setNote} />
      <Show when={note()}>
        <p class="note">{note()}</p>
      </Show>
    </main>
  );
}

function b64ToBytes(s: string): Uint8Array<ArrayBuffer> {
  const bin = atob(s.replace(/-/g, '+').replace(/_/g, '/'));
  const out = new Uint8Array(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function Push(props: { me: Me; onNote: (n: string) => void }) {
  const ios = /iPhone|iPad|iPod/.test(navigator.userAgent);
  const standalone = window.matchMedia('(display-mode: standalone)').matches;
  const enable = async () => {
    const key = props.me.push_key;
    if (!key) return;
    try {
      const reg = await navigator.serviceWorker.ready;
      if ((await Notification.requestPermission()) !== 'granted') {
        return props.onNote('notifications are blocked for this site');
      }
      const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(key) });
      await api.subscribe(sub.toJSON());
      props.onNote('notifications on');
    } catch (e) {
      props.onNote(String(e));
    }
  };
  return (
    <Show when={props.me.push_key && 'serviceWorker' in navigator}>
      <Show
        when={!ios || standalone}
        fallback={
          <p class="note">
            To get notifications on iOS, add jetmux to your home screen first (Share → Add to Home Screen).
          </p>
        }
      >
        <button class="push" onClick={enable}>
          enable notifications
        </button>
      </Show>
    </Show>
  );
}

function Term(props: { session: string; me: Me; config: UiConfig }) {
  let wrap!: HTMLDivElement;
  let canvas!: HTMLCanvasElement;
  let input!: HTMLTextAreaElement;
  let grid: GridRenderer | undefined;
  let conn: Connection | undefined;

  const canType = props.me.role !== 'viewer';
  const shared = props.me.session !== null;
  const [view, setView] = createSignal<View>();
  const [status, setStatus] = createSignal<Status>('connecting');
  const [detail, setDetail] = createSignal<string>();
  const [follow, setFollow] = createSignal(true);
  const [zoom, setZoom] = createSignal<number | null>(null);
  const [sticky, setSticky] = createSignal({ ctrl: false, alt: false });
  const [drawer, setDrawer] = createSignal<'blocks' | 'actions' | 'select' | null>(null);
  const [blocks, setBlocks] = createSignal<BlockInfo[]>([]);
  const [selection, setSelection] = createSignal('');
  const [toast, setToast] = createSignal<string>();

  const profile = () => props.config.profiles.find((p) => p.name === props.session);
  const keyRows = () => [BASE_KEYS, ...props.config.keybar, ...(profile()?.keybar ?? [])];
  const snippets = (): Snippet[] => [...props.config.actions, ...(profile()?.actions ?? [])];
  const focused = () => view()?.panes.find((p) => p.focused);

  let toastTimer: number | undefined;
  const say = (text: string) => {
    setToast(text);
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => setToast(undefined), 2500);
  };

  const sizeInCells = () => {
    const cell = grid?.baseCell() ?? { w: 8, h: 17 };
    return {
      cols: Math.max(2, Math.floor(wrap.clientWidth / cell.w)),
      rows: Math.max(2, Math.floor(wrap.clientHeight / cell.h)),
    };
  };

  /** Following, a wide session shrinks to the screen's width until pinched. */
  const applyScale = () => {
    const area = view()?.area;
    if (!grid || !area) return;
    const base = grid.baseCell();
    const fit = follow() ? Math.max(MIN_FIT_SCALE, Math.min(1, wrap.clientWidth / (area.cols * base.w))) : 1;
    grid.setScale(zoom() ?? fit);
  };

  const send = (data: string) => {
    if (!canType || data === '') return;
    const s = sticky();
    let out = data;
    if (s.ctrl) out = ctrl(out.slice(0, 1)) ?? out;
    if (s.alt) out = '\x1b' + out;
    if (s.ctrl || s.alt) setSticky({ ctrl: false, alt: false });
    conn?.send({ t: 'input', data: out });
  };
  const act = (action: Action) => conn?.send({ t: 'action', action });

  const loadBlocks = async () => {
    const pane = focused();
    if (!pane) return;
    try {
      setBlocks((await api.blocks(props.session, pane.id)).filter((b) => b.state !== 'prompt').reverse());
    } catch (e) {
      say(String(e));
    }
  };

  onMount(() => {
    grid = new CanvasGrid(canvas, props.config.theme);
    conn = new Connection(props.session, sizeInCells, follow, {
      server(msg) {
        if (typeof msg !== 'object') {
          if (msg === 'Bell') navigator.vibrate?.(40);
          return;
        }
        if ('View' in msg) {
          setView(msg.View);
          grid?.setView(msg.View);
          applyScale();
        } else if ('PaneFrame' in msg) {
          grid?.frame(msg.PaneFrame.pane, msg.PaneFrame.frame);
        } else if ('Clipboard' in msg || 'AppClipboard' in msg) {
          const text = 'Clipboard' in msg ? msg.Clipboard : msg.AppClipboard;
          navigator.clipboard?.writeText(text).then(
            () => say('copied'),
            () => say("couldn't copy"),
          );
        } else if ('Message' in msg) {
          say(msg.Message);
        }
      },
      event(event) {
        if (event.type === 'block_done' && drawer() === 'blocks') loadBlocks();
        if (event.type === 'notify') say(event.title ? `${event.title}: ${event.body}` : event.body);
      },
      status(s, d) {
        setStatus(s);
        setDetail(d);
      },
    });
    const observer = new ResizeObserver(() => {
      const { cols, rows } = sizeInCells();
      conn?.send({ t: 'resize', cols, rows });
      applyScale();
    });
    observer.observe(wrap);
    const onKey = (e: KeyboardEvent) => {
      if (document.activeElement !== input && document.activeElement !== document.body) return;
      const bytes = eventBytes(e, grid?.modes(focused()?.id ?? -1)?.app_cursor ?? false);
      // Plain text arrives through `beforeinput`, so IMEs and soft keyboards work.
      if (bytes === undefined || (e.key.length === 1 && !e.ctrlKey && !e.altKey)) return;
      e.preventDefault();
      send(bytes);
    };
    window.addEventListener('keydown', onKey);
    onCleanup(() => {
      observer.disconnect();
      window.removeEventListener('keydown', onKey);
      conn?.close();
    });
  });

  const onBeforeInput = (e: InputEvent) => {
    if (e.isComposing || e.inputType === 'insertCompositionText') return;
    e.preventDefault();
    if (e.inputType === 'insertText' || e.inputType === 'insertReplacementText') send(e.data ?? '');
    else if (e.inputType === 'insertLineBreak' || e.inputType === 'insertParagraph') send('\r');
    else if (e.inputType === 'deleteContentBackward') send('\x7f');
    else if (e.inputType === 'insertFromPaste')
      send((e.dataTransfer?.getData('text') ?? e.data ?? '').replace(/\n/g, '\r'));
  };
  const onCompositionEnd = (e: CompositionEvent) => {
    send(e.data);
    input.value = '';
  };

  // Gestures: tap focuses a pane (or opens a link), a swipe switches tabs,
  // a long press opens the pane's text for selecting, two fingers pinch to
  // zoom or scroll back through history.
  const pointers = new Map<number, { x: number; y: number; x0: number; y0: number; t0: number }>();
  let longPress: number | undefined;
  let pinch: { d0: number; s0: number; y0: number; scrolled: number; mode: 'pinch' | 'scroll' | null } | undefined;
  const canvasPoint = (x: number, y: number) => {
    const r = canvas.getBoundingClientRect();
    return { x: x - r.left, y: y - r.top };
  };
  const twoFinger = () => {
    const [a, b] = [...pointers.values()];
    if (!a || !b) return undefined;
    return { d: Math.hypot(a.x - b.x, a.y - b.y), y: (a.y + b.y) / 2 };
  };
  const onDown = (e: PointerEvent) => {
    wrap.setPointerCapture(e.pointerId);
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY, t0: performance.now() });
    window.clearTimeout(longPress);
    if (pointers.size === 1 && e.pointerType === 'touch') {
      longPress = window.setTimeout(() => {
        const at = grid?.hit(canvasPoint(e.clientX, e.clientY).x, canvasPoint(e.clientX, e.clientY).y);
        if (!at || !grid) return;
        pointers.clear();
        setSelection(grid.text(at.pane));
        setDrawer('select');
      }, LONG_PRESS_MS);
    }
    const two = twoFinger();
    if (two) pinch = { d0: two.d, s0: zoom() ?? 1, y0: two.y, scrolled: 0, mode: null };
  };
  const onMove = (e: PointerEvent) => {
    const p = pointers.get(e.pointerId);
    if (!p) return;
    const [dx, dy] = [e.clientX - p.x, e.clientY - p.y];
    p.x = e.clientX;
    p.y = e.clientY;
    if (Math.hypot(p.x - p.x0, p.y - p.y0) > 10) window.clearTimeout(longPress);
    const two = twoFinger();
    if (two && pinch) {
      if (pinch.mode === null) {
        if (Math.abs(two.d - pinch.d0) > 24) pinch.mode = 'pinch';
        else if (Math.abs(two.y - pinch.y0) > 24) pinch.mode = 'scroll';
      }
      if (pinch.mode === 'pinch') {
        setZoom(Math.min(3, Math.max(0.3, (pinch.s0 * two.d) / pinch.d0)));
        applyScale();
      } else if (pinch.mode === 'scroll' && canType) {
        const rows = Math.round((pinch.y0 - two.y) / 20);
        if (rows !== pinch.scrolled) {
          if (pinch.scrolled === 0 && !focused()?.copy_mode) act('EnterCopyMode');
          act({ Copy: { Scroll: rows - pinch.scrolled } });
          pinch.scrolled = rows;
        }
      }
      return;
    }
    if (pointers.size === 1 && e.pointerType === 'touch') {
      // One finger pans a session bigger than the screen.
      wrap.scrollLeft -= dx;
      wrap.scrollTop -= dy;
    }
  };
  const onUp = (e: PointerEvent) => {
    window.clearTimeout(longPress);
    const p = pointers.get(e.pointerId);
    pointers.delete(e.pointerId);
    if (pointers.size < 2) pinch = undefined;
    if (!p || pointers.size > 0) return;
    const [dx, dy, dt] = [p.x - p.x0, p.y - p.y0, performance.now() - p.t0];
    const overflows = wrap.scrollWidth > wrap.clientWidth + 2;
    if (!overflows && Math.abs(dx) > SWIPE_PX && Math.abs(dy) < SWIPE_PX / 2 && dt < 600) {
      act(dx < 0 ? 'NextTab' : 'PrevTab');
      return;
    }
    if (Math.hypot(dx, dy) < 10 && dt < LONG_PRESS_MS) {
      const at = canvasPoint(p.x, p.y);
      const link = grid?.linkAt(at.x, at.y);
      if (link) {
        window.open(link, '_blank', 'noopener');
        return;
      }
      const hit = grid?.hit(at.x, at.y);
      if (hit && hit.pane !== focused()?.id) act({ FocusPane: hit.pane });
      if (canType) input.focus({ preventScroll: true });
    }
  };
  const onWheel = (e: WheelEvent) => {
    if (!canType) return;
    e.preventDefault();
    const rows = Math.round(e.deltaY / 40) || Math.sign(e.deltaY);
    if (rows < 0 && !focused()?.copy_mode) act('EnterCopyMode');
    if (rows !== 0) act({ Copy: { Scroll: rows } });
  };

  const toggleFit = () => {
    const next = !follow();
    setFollow(next);
    setZoom(null);
    conn?.send({ t: 'follow', on: next });
    applyScale();
  };
  const share = async () => {
    try {
      const link = await api.share(props.session, 3600);
      await navigator.clipboard?.writeText(link.url);
      say('read-only link copied (1 hour)');
    } catch (e) {
      say(String(e));
    }
  };
  const pressKey = (name: string) => {
    if (name === 'Ctrl') return setSticky((s) => ({ ...s, ctrl: !s.ctrl }));
    if (name === 'Alt') return setSticky((s) => ({ ...s, alt: !s.alt }));
    send(keyBytes(name));
  };

  return (
    <main class="term">
      <header>
        <Show when={!shared}>
          <a class="back" href="#" aria-label="sessions">
            ‹
          </a>
        </Show>
        <nav class="tabs">
          <For each={view()?.tabs ?? []}>
            {(tab, i) => (
              <button classList={{ active: i() === view()?.active_tab }} onClick={() => act({ SelectTab: i() })}>
                {tab.name || i() + 1}
              </button>
            )}
          </For>
        </nav>
        <span class={'dot ' + status()} title={detail() ?? status()} />
        <button onClick={() => (drawer() === 'blocks' ? setDrawer(null) : (setDrawer('blocks'), loadBlocks()))}>
          blocks
        </button>
        <Show when={canType && snippets().length > 0}>
          <button onClick={() => setDrawer(drawer() === 'actions' ? null : 'actions')}>run</button>
        </Show>
        <Show when={canType}>
          <button classList={{ on: !follow() }} onClick={toggleFit} title="resize the session to this screen">
            fit
          </button>
        </Show>
        <Show when={props.me.role === 'owner'}>
          <button onClick={share}>share</button>
        </Show>
      </header>

      <div
        class="screen"
        ref={wrap}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onWheel={onWheel}
      >
        <canvas ref={canvas} />
        <Show when={status() !== 'open'}>
          <div class="overlay">{detail() ?? status()}</div>
        </Show>
      </div>

      <Show when={focused()?.copy_mode && canType}>
        <button class="exit-scroll" onClick={() => act({ Copy: 'Exit' })}>
          back to live
        </button>
      </Show>

      <Show when={drawer() === 'blocks'}>
        <section class="drawer">
          <For each={blocks()} fallback={<p class="note">no commands yet (see jetmux shell-init)</p>}>
            {(b) => <BlockCard block={b} session={props.session} pane={focused()?.id ?? 0} say={say} />}
          </For>
        </section>
      </Show>
      <Show when={drawer() === 'actions'}>
        <section class="drawer snippets">
          <For each={snippets()}>{(s) => <button onClick={() => send(s.send)}>{s.name}</button>}</For>
        </section>
      </Show>
      <Show when={drawer() === 'select'}>
        <section class="drawer select">
          <pre>{selection()}</pre>
          <button onClick={() => setDrawer(null)}>done</button>
        </section>
      </Show>

      <Show when={canType}>
        <div class="keybar" onPointerDown={(e) => e.preventDefault()}>
          <For each={keyRows()}>
            {(row) => (
              <div class="row">
                <For each={row}>
                  {(key) => (
                    <button
                      classList={{ on: (key === 'Ctrl' && sticky().ctrl) || (key === 'Alt' && sticky().alt) }}
                      onClick={() => pressKey(key)}
                    >
                      {keyLabel(key)}
                    </button>
                  )}
                </For>
              </div>
            )}
          </For>
        </div>
        <textarea
          ref={input}
          class="sink"
          autocapitalize="off"
          autocomplete="off"
          spellcheck={false}
          onBeforeInput={onBeforeInput}
          onCompositionEnd={onCompositionEnd}
        />
      </Show>
      <Show when={toast()}>
        <div class="toast">{toast()}</div>
      </Show>
    </main>
  );
}

function BlockCard(props: { block: BlockInfo; session: string; pane: number; say: (s: string) => void }) {
  const [output, setOutput] = createSignal<string>();
  const b = props.block;
  const failed = b.exit !== null && b.exit !== 0;
  const toggle = async () => {
    if (output() !== undefined) return setOutput(undefined);
    try {
      setOutput((await api.block(props.session, props.pane, b.id)).text);
    } catch (e) {
      props.say(String(e));
    }
  };
  const copy = async () => {
    const text = output() ?? (await api.block(props.session, props.pane, b.id)).text;
    await navigator.clipboard?.writeText(text);
    props.say('output copied');
  };
  return (
    <article class="block" classList={{ failed, running: b.state === 'running' }}>
      <div class="head" onClick={toggle}>
        <span class="status">{b.state === 'running' ? '…' : failed ? `✗ ${b.exit}` : '✓'}</span>
        <code>{b.command || '(no command)'}</code>
        <span class="time">{b.duration_ms === null ? '' : `${(b.duration_ms / 1000).toFixed(1)}s`}</span>
        <button onClick={(e) => (e.stopPropagation(), copy())}>copy</button>
      </div>
      <Show when={output() !== undefined}>
        <pre>{output()}</pre>
      </Show>
    </article>
  );
}
