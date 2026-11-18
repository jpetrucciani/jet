// jetmux's service worker: keeps the app shell cached so the PWA opens
// instantly on a bad connection, and shows web push notifications from
// jetmux-notify. The API and the attach socket always go to the network.

const CACHE = 'jetmux-shell-v1';

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(['/', '/manifest.webmanifest', '/icon.svg'])));
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== 'GET' || url.origin !== location.origin || url.pathname.startsWith('/api/')) {
    return;
  }
  if (url.pathname.startsWith('/assets/')) {
    // Hashed build output: cached forever once seen.
    event.respondWith(
      caches.match(event.request).then(
        (hit) =>
          hit ||
          fetch(event.request).then((res) => {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(event.request, copy));
            return res;
          }),
      ),
    );
    return;
  }
  // The shell: fresh when online, cached when not.
  event.respondWith(
    fetch(event.request)
      .then((res) => {
        if (event.request.mode === 'navigate') {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put('/', copy));
        }
        return res;
      })
      .catch(() => caches.match(event.request).then((hit) => hit || caches.match('/'))),
  );
});

self.addEventListener('push', (event) => {
  let alert = { title: 'jetmux', body: '', session: '' };
  try {
    alert = { ...alert, ...event.data.json() };
  } catch {
    alert.body = event.data ? event.data.text() : '';
  }
  event.waitUntil(
    self.registration.showNotification(alert.title, {
      body: alert.body,
      icon: '/icon.svg',
      tag: alert.session || undefined,
      data: { session: alert.session },
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const session = event.notification.data && event.notification.data.session;
  const target = '/' + (session ? '#' + encodeURIComponent(session) : '');
  event.waitUntil(
    self.clients.matchAll({ type: 'window' }).then((windows) => {
      for (const w of windows) {
        if ('focus' in w) {
          w.navigate(target);
          return w.focus();
        }
      }
      return self.clients.openWindow(target);
    }),
  );
});
