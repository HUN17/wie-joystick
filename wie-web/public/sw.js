// Keeps the web app available offline.
// The build writes precache.json with every emitted file so the whole app is cached on install.
const CACHE = "wie-offline";

self.addEventListener("install", (event) => {
  event.waitUntil((async () => {
    const files = await (await fetch("precache.json", { cache: "no-store" })).json();
    const cache = await caches.open(CACHE);
    await cache.addAll(["./", ...files]);
    await self.skipWaiting();
  })());
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

const fetchAndCache = async (request) => {
  const response = await fetch(request);
  if (response.ok) {
    const cache = await caches.open(CACHE);
    await cache.put(request, response.clone());
  }
  return response;
};

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET" || new URL(request.url).origin !== location.origin) return;

  // Pages come from the network first so updates arrive when online.
  // Other files have content hashes or never change, so the cache is used first.
  if (request.mode === "navigate") {
    event.respondWith(fetchAndCache(request).catch(async () => (await caches.match(request)) ?? caches.match("./")));
  } else {
    event.respondWith((async () => (await caches.match(request)) ?? fetchAndCache(request))());
  }
});
