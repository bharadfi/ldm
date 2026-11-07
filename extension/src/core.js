// Pure logic shared by the background script and the tests. No browser APIs in here.

export const DEFAULTS = {
  enabled: true,
  // Known-size downloads smaller than this many bytes stay with the browser (0 = take everything).
  minSize: 0,
  // Hosts (and their subdomains) the browser should keep downloading itself.
  excludedHosts: [],
};

export function isHttp(url) {
  try {
    const p = new URL(url).protocol;
    return p === 'http:' || p === 'https:';
  } catch {
    return false;
  }
}

export function hostMatches(url, hosts) {
  let h;
  try {
    h = new URL(url).hostname.toLowerCase();
  } catch {
    return false;
  }
  return hosts.some((raw) => {
    const x = String(raw).trim().toLowerCase().replace(/^\*?\./, '');
    return x !== '' && (h === x || h.endsWith('.' + x));
  });
}

/** Decide whether a browser download item should be taken over by ZDM. */
export function shouldHandle(item, settings) {
  const s = { ...DEFAULTS, ...settings };
  if (!s.enabled) return { handle: false, reason: 'disabled' };
  if (item.byExtensionId) return { handle: false, reason: 'started by an extension' };
  if (item.state && item.state !== 'in_progress') return { handle: false, reason: `state ${item.state}` };
  const url = item.finalUrl || item.url;
  if (!isHttp(url)) return { handle: false, reason: 'not an http(s) URL' };
  if (hostMatches(url, s.excludedHosts)) return { handle: false, reason: 'excluded host' };
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : 0;
  if (size > 0 && size < s.minSize) return { handle: false, reason: 'below minimum size' };
  return { handle: true, reason: 'ok' };
}

/** `Cookie` header value from browser cookie objects (more specific paths first, as browsers send them). */
export function cookieHeader(cookies) {
  return [...cookies]
    .sort((a, b) => (b.path || '').length - (a.path || '').length)
    .map((c) => `${c.name}=${c.value}`)
    .join('; ');
}

export function buildAddMessage({ url, referer, cookie, userAgent }) {
  const msg = { type: 'add', url };
  if (referer) msg.referer = referer;
  if (userAgent) msg.user_agent = userAgent;
  msg.headers = cookie ? [['Cookie', cookie]] : [];
  return msg;
}

export function parseExcludedHosts(text) {
  return String(text)
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}
