// The JSON API. Share links carry their token as `?share=`, and every
// request passes it on; changes carry `X-Jetmux: 1` (see auth.rs, CSRF).
import type { BlockInfo } from './proto/BlockInfo';
import type { Captured } from './proto/Captured';
import type { Me } from './proto/Me';
import type { PaneInfo } from './proto/PaneInfo';
import type { SessionInfo } from './proto/SessionInfo';
import type { ShareLink } from './proto/ShareLink';
import type { UiConfig } from './proto/UiConfig';

export const shareToken = new URLSearchParams(location.search).get('share');

function withShare(path: string): string {
  if (!shareToken) return path;
  return path + (path.includes('?') ? '&' : '?') + 'share=' + encodeURIComponent(shareToken);
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(withShare(path));
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

async function post<T>(path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'x-jetmux': '1', 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

const s = encodeURIComponent;

export const api = {
  me: () => get<Me>('/api/me'),
  config: () => get<UiConfig>('/api/config'),
  sessions: () => get<SessionInfo[]>('/api/sessions'),
  panes: (session: string) => get<PaneInfo[]>(`/api/sessions/${s(session)}/panes`),
  blocks: (session: string, pane: number) => get<BlockInfo[]>(`/api/sessions/${s(session)}/blocks?pane=${pane}`),
  block: (session: string, pane: number, id: number) =>
    get<Captured>(`/api/sessions/${s(session)}/blocks/${id}?pane=${pane}`),
  launch: (profile: string) => post<null>(`/api/profiles/${s(profile)}/up`),
  share: (session: string, ttl_secs: number) => post<ShareLink>('/api/share', { session, ttl_secs }),
  subscribe: (sub: PushSubscriptionJSON) => post<null>('/api/push/subscribe', sub),
  unsubscribe: (endpoint: string) => post<null>('/api/push/unsubscribe', { endpoint }),
};

export function attachUrl(session: string, cols: number, rows: number, follow: boolean): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const path = `/api/sessions/${s(session)}/attach?cols=${cols}&rows=${rows}&follow=${follow ? 1 : 0}`;
  return proto + '//' + location.host + withShare(path);
}
