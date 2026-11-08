// Talking to zdm-nmhost, shared by the background script, the popup and the links page.
import { cookieHeader, buildAddMessage } from './core.js';

const api = globalThis.browser ?? globalThis.chrome;
export const HOST = 'com.zdm.host';

export async function sendToHost(msg) {
  const reply = await api.runtime.sendNativeMessage(HOST, msg);
  if (!reply || !reply.ok) throw new Error((reply && reply.error) || 'no reply from zdm-nmhost');
  return reply;
}

/** Queue `url` in ZDM with the page it came from and the browser's cookies for it. */
export async function handOver({ url, referer, filename, format }) {
  const cookies = await api.cookies.getAll({ url });
  await sendToHost(buildAddMessage({ url, referer, filename, format, cookie: cookieHeader(cookies), userAgent: navigator.userAgent }));
}
