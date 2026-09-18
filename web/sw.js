// Cache the app shell so an installed copy opens with no network. The cache
// name carries a version; bumping it on deploy makes activate() drop the old
// shell, so a stale service worker can never pin an old build for good.
const CACHE = "gba-shell-v1";
const SHELL = [
  ".",
  "index.html",
  "style.css",
  "app.js",
  "audio-worklet.js",
  "manifest.webmanifest",
  "icon-192.png",
  "icon-512.png",
  "pkg/gba.js",
  "pkg/gba_bg.wasm",
];

self.addEventListener("install", (e) => {
  e.waitUntil(
    caches
      .open(CACHE)
      .then((c) => c.addAll(SHELL))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

// Network first so a normal online visit always gets the newest build; the
// cache is the fallback for offline and flaky connections. Only same-origin
// GETs are handled — the jsmolka test ROM fetch passes straight through.
self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin) return;
  e.respondWith(
    fetch(e.request)
      .then((res) => {
        const copy = res.clone();
        caches.open(CACHE).then((c) => c.put(e.request, copy));
        return res;
      })
      .catch(() => caches.match(e.request, { ignoreSearch: true }))
  );
});
