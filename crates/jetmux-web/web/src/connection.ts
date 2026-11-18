// One attached session over a WebSocket. Every (re)connect is a fresh
// attach, so the daemon sends a full snapshot: that's what makes coming
// back to a backgrounded phone (iOS drops sockets) or a daemon upgrade
// seamless.
import { attachUrl } from './api';
import type { ClientIn } from './proto/ClientIn';
import type { ClientOut } from './proto/ClientOut';
import type { Event } from './proto/Event';
import type { ServerMsg } from './proto/ServerMsg';

export type Status = 'connecting' | 'open' | 'closed' | 'ended';

export interface Handlers {
  server(msg: ServerMsg): void;
  event(event: Event): void;
  status(status: Status, detail?: string): void;
}

function parse(text: string): ClientOut | undefined {
  const value: unknown = JSON.parse(text);
  if (typeof value === 'object' && value !== null && 't' in value && 'm' in value) {
    if (value.t === 'server' || value.t === 'event') return value as ClientOut;
  }
  return undefined;
}

export class Connection {
  private ws: WebSocket | undefined;
  private retry = 0;
  private timer: number | undefined;
  private ended = false;

  constructor(
    private session: string,
    private size: () => { cols: number; rows: number },
    private follow: () => boolean,
    private on: Handlers,
  ) {
    document.addEventListener('visibilitychange', this.onVisible);
    this.connect();
  }

  private onVisible = () => {
    if (document.visibilityState === 'visible' && !this.ended && this.ws?.readyState !== WebSocket.OPEN) {
      this.retry = 0;
      this.connect();
    }
  };

  private connect() {
    window.clearTimeout(this.timer);
    this.ws?.close();
    const { cols, rows } = this.size();
    const ws = new WebSocket(attachUrl(this.session, cols, rows, this.follow()));
    this.ws = ws;
    this.on.status('connecting');
    ws.onopen = () => {
      this.retry = 0;
      this.on.status('open');
    };
    ws.onmessage = (e: MessageEvent<string>) => {
      const out = parse(e.data);
      if (!out) return;
      if (out.t === 'event') return this.on.event(out.m);
      const msg = out.m;
      if (msg === 'Detached' || msg === 'Exited') {
        this.ended = true;
        this.on.status('ended', msg === 'Exited' ? 'the session ended' : 'detached');
      } else if (msg === 'Reconnect') {
        // The daemon is being upgraded; the new one will be up shortly.
        this.retry = 0;
        this.scheduleRetry(300);
      } else if (typeof msg === 'object' && 'Error' in msg) {
        this.ended = true;
        this.on.status('ended', msg.Error);
      }
      this.on.server(msg);
    };
    ws.onclose = () => {
      if (this.ws !== ws || this.ended) return;
      this.on.status('closed');
      this.scheduleRetry(Math.min(10_000, 250 * 2 ** this.retry++));
    };
  }

  private scheduleRetry(ms: number) {
    window.clearTimeout(this.timer);
    // A hidden page reconnects when it comes back instead.
    if (document.visibilityState === 'hidden') return;
    this.timer = window.setTimeout(() => this.connect(), ms);
  }

  send(msg: ClientIn) {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  close() {
    this.ended = true;
    window.clearTimeout(this.timer);
    document.removeEventListener('visibilitychange', this.onVisible);
    this.send({ t: 'detach' });
    this.ws?.close();
  }
}
