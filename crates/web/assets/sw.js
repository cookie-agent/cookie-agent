// The cookie agent page's service worker. It makes the page installable as
// an app and caches nothing: every request goes to the network, so the page
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

// build: source
