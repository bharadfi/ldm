import { DEFAULTS, shouldHandle, cookieHeader, buildAddMessage, isHttp } from './core.js';

const api = globalThis.browser ?? globalThis.chrome;
const HOST = 'com.zdm.host';

async function settings() {
  return { ...DEFAULTS, ...(await api.storage.local.get(DEFAULTS)) };
}

async function sendToHost(msg) {
  const reply = await api.runtime.sendNativeMessage(HOST, msg);
  if (!reply || !reply.ok) throw new Error((reply && reply.error) || 'no reply from zdm-nmhost');
  return reply;
}

async function report(error) {
  await api.action.setBadgeText({ text: error ? '!' : '' });
  await api.action.setBadgeBackgroundColor({ color: '#c0392b' });
  await api.action.setTitle({ title: error ? `ZDM: ${error.message}` : 'ZDM Download Manager' });
  await api.storage.local.set({ lastError: error ? String(error.message) : null });
}

async function handOver({ url, referer }) {
  const cookies = await api.cookies.getAll({ url });
  await sendToHost(buildAddMessage({ url, referer, cookie: cookieHeader(cookies), userAgent: navigator.userAgent }));
}

const taken = new Set();

api.downloads.onCreated.addListener(async (item) => {
  if (taken.has(item.id)) return;
  const verdict = shouldHandle(item, await settings());
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
});

api.contextMenus.onClicked.addListener(async (info) => {
  const url = info.linkUrl || info.srcUrl;
  if (info.menuItemId !== 'zdm-download' || !url || !isHttp(url)) return;
  try {
    await handOver({ url, referer: info.pageUrl });
    await report(null);
  } catch (e) {
    await report(e);
  }
});
