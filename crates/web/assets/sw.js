// The cookie agent page's service worker. It makes the page installable as
// an app, shows push notifications, and caches nothing: every request goes to the network, so the page
// always matches its daemon's build (web client spec, section 9). A daemon
// that is not reachable shows the browser's own error, as without it.
//
// It lives beside index.html, so its scope is the page's directory, and it
// takes over at once, so a new build replaces it on the next load.
// scripts/build-web.sh stamps the build on the last line, which changes the
// file whenever the page does.

self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const request = event.request;
  // Only the page's own files: anything else is left to the browser.
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // The daemon's WebSocket endpoints are never touched. A browser sends no
  // fetch event for a WebSocket; this keeps a plain request off them too.
  if (/\/(ws|runtime)$/.test(url.pathname)) return;
  // A failed fetch (the daemon restarting, the network gone) becomes the
  // network error the browser would show anyway, instead of a rejection
  // the console reports as uncaught.
  event.respondWith(fetch(request).catch(() => Response.error()));
});

// Push notifications (push notifications spec section 9.2). The daemon
// sends a small JSON payload: a root session settled, or an approval or a
// question waits. Every push shows a notification: Safari revokes a
// subscription whose pushes show nothing, and Chrome shows its own.
self.addEventListener("push", (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch (_) {
    payload = {};
  }
  const title = typeof payload.title === "string" && payload.title ? payload.title : "Cookie Agent";
  const options = {
    body: typeof payload.body === "string" ? payload.body : "",
    icon: "icons/icon-192.png",
    badge: "icons/icon-192.png",
    data: { session: typeof payload.session === "string" ? payload.session : null },
  };
  // A newer notification with the same tag replaces the older one: one per
  // session tree, one for the daemon's restarts.
  const tag =
    typeof payload.tag === "string" ? payload.tag : typeof payload.root === "string" ? "session:" + payload.root : null;
  if (tag) {
    options.tag = tag;
    options.renotify = true;
  }
  if (payload.kind === "approval" || payload.kind === "question") {
    options.requireInteraction = true;
  }
  event.waitUntil(self.registration.showNotification(title, options));
});

// A click opens the session: in a window of the app already open, else in
// a new one at `#session=<id>`.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const session = event.notification.data && event.notification.data.session;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const open = windows.find((client) => client.url.startsWith(self.registration.scope));
      if (open) {
        await open.focus();
        if (session) open.postMessage({ type: "open-session", session });
        return;
      }
      const url = new URL(self.registration.scope);
      if (session) url.hash = "session=" + session;
      await self.clients.openWindow(url.href);
    })(),
  );
});

// build: source
