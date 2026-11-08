import { DEFAULTS, shouldHandle, isHttp, classifyMedia, addMedia, parseHlsVariants } from './core.js';
import { handOver, sendToHost } from './host.js';

const api = globalThis.browser ?? globalThis.chrome;
async function settings() {
  return { ...DEFAULTS, ...(await api.storage.local.get(DEFAULTS)) };
}

async function report(error) {
  await api.action.setBadgeText({ text: error ? '!' : '' });
  await api.action.setBadgeBackgroundColor({ color: '#c0392b' });
  await api.action.setTitle({ title: error ? `ZDM: ${error.message}` : 'ZDM Download Manager' });
  await api.storage.local.set({ lastError: error ? String(error.message) : null });
}

const taken = new Set();

// Modifier keys held at the last click on a link (reported by content.js); only fresh ones count.
let lastClick = { mods: {}, at: 0 };
const freshMods = () => (Date.now() - lastClick.at < 5000 ? lastClick.mods : {});

api.downloads.onCreated.addListener(async (item) => {
  if (taken.has(item.id)) return;
  const verdict = shouldHandle(item, await settings(), freshMods());
  if (!verdict.handle) return;
  taken.add(item.id);
  try {
    await handOver({ url: item.finalUrl || item.url, referer: item.referrer });
    // Only stop the browser once ZDM has accepted it; on any failure the browser keeps downloading.
    await api.downloads.cancel(item.id);
    await api.downloads.erase({ id: item.id });
    await report(null);
  } catch (e) {
    taken.delete(item.id);
    await report(e);
  }
});

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.create({
    id: 'zdm-download',
    title: 'Download with ZDM',
    contexts: ['link', 'video', 'audio', 'image'],
  });
  api.contextMenus.create({
    id: 'zdm-all-links',
    title: 'Download links from this page with ZDM…',
    contexts: ['page'],
  });
});

// ---- video sniffer: remember media responses per tab, show the count on the toolbar icon ----

const mediaKey = (tabId) => `media:${tabId}`;

async function remember(tabId, entry) {
  const store = api.storage.session ?? api.storage.local;
  const key = mediaKey(tabId);
  const list = addMedia((await store.get(key))[key] ?? [], entry);
  await store.set({ [key]: list });
  await api.action.setBadgeBackgroundColor({ tabId, color: '#1a73e8' });
  await api.action.setBadgeText({ tabId, text: String(list.length) });
}

api.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0 || d.statusCode >= 300) return;
    const h = (n) => (d.responseHeaders || []).find((x) => x.name.toLowerCase() === n)?.value;
    const len = Number(h('content-length'));
    const range = h('content-range'); // "bytes 0-1/12345": the total is the real size
    const total = range ? Number(range.split('/')[1]) : len;
    const kind = classifyMedia({ url: d.url, contentType: h('content-type') || '', size: Number.isFinite(total) ? total : -1 });
    if (kind) remember(d.tabId, { url: d.url, kind, page: d.documentUrl || d.originUrl || '', size: Number.isFinite(total) ? total : -1, at: Date.now() }).catch(() => {});
  },
  { urls: ['http://*/*', 'https://*/*'], types: ['media', 'xmlhttprequest', 'other'] },
  ['responseHeaders'],
);

async function forget(tabId) {
  await (api.storage.session ?? api.storage.local).remove(mediaKey(tabId));
  await api.action.setBadgeText({ tabId, text: '' });
}

api.tabs.onUpdated.addListener((tabId, change) => {
  if (change.url) forget(tabId).catch(() => {}); // navigated to another page: its media list starts over
});
api.tabs.onRemoved.addListener((tabId) => forget(tabId).catch(() => {}));

api.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === 'zdm-all-links' && tab) {
    await api.tabs.create({ url: api.runtime.getURL(`links.html?tab=${tab.id}`) });
    return;
  }
  const url = info.linkUrl || info.srcUrl;
  if (info.menuItemId !== 'zdm-download' || !url || !isHttp(url)) return;
  try {
    await handOver({ url, referer: info.pageUrl });
    await report(null);
  } catch (e) {
    await report(e);
  }
});


// ---- answers for the on-page button (content.js) ----

/** What the tab has loaded: files as they are, and for HLS the list of resolutions from the master playlist. */
async function mediaFor(tabId) {
  const key = mediaKey(tabId);
  const list = (await (api.storage.session ?? api.storage.local).get(key))[key] ?? [];
  const out = [];
  let looked = 0;
  for (const m of list.slice().reverse()) {
    if (m.kind === 'hls' && looked < 3) {
      looked++;
      try {
        const text = await (await fetch(m.url, { credentials: 'include' })).text();
        const variants = parseHlsVariants(text.slice(0, 200_000), m.url);
        if (variants.length) {
          out.push(...variants.map((v) => ({ url: v.url, kind: 'hls', label: v.label, size: -1 })));
          continue;
        }
      } catch {}
    }
    out.push({ url: m.url, kind: m.kind, size: m.size });
  }
  return out;
}

api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  (async () => {
    const tab = sender.tab;
    if (!tab) throw new Error('no tab');
    switch (msg.type) {
      case 'click-mods':
        lastClick = { mods: { alt: !!msg.alt, ctrl: !!msg.ctrl, shift: !!msg.shift, meta: !!msg.meta }, at: Date.now() };
        return { ok: true };
      case 'media-list':
        return { sniffed: await mediaFor(tab.id) };
      case 'formats': {
        const r = await sendToHost({ type: 'formats', url: tab.url });
        return { title: r.title, options: r.options };
      }
      case 'download': {
        const a = msg.action || {};
        if (a.type === 'ytdlp') await handOver({ url: tab.url, referer: tab.url, format: a.id, filename: a.filename });
        else if (a.type === 'url' && isHttp(a.url)) await handOver({ url: a.url, referer: tab.url });
        else throw new Error('unknown action');
        await report(null);
        return { ok: true };
      }
      default:
        throw new Error(`unknown message ${msg.type}`);
    }
  })().then(sendResponse, (e) => sendResponse({ error: String(e?.message || e) }));
  return true; // answer asynchronously
});
