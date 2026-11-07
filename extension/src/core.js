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

// ---- video / media sniffing -------------------------------------------------------------------------------

const MEDIA_EXT = {
  m3u8: 'hls', mpd: 'dash',
  mp4: 'video', m4v: 'video', webm: 'video', mkv: 'video', mov: 'video', flv: 'video', avi: 'video', ogv: 'video',
  mp3: 'audio', m4a: 'audio', aac: 'audio', ogg: 'audio', opus: 'audio', flac: 'audio', wav: 'audio',
};
const MEDIA_TYPES = [[/mpegurl/i, 'hls'], [/dash\+xml/i, 'dash'], [/^video\//i, 'video'], [/^audio\//i, 'audio']];
// Pieces of a stream: listing every one of them would bury the real video.
const SEGMENT_EXT = new Set(['ts', 'm4s', 'cmfv', 'cmfa']);
const MIN_MEDIA_BYTES = 200 * 1024;

export function extOf(url) {
  try {
    const last = new URL(url).pathname.split('/').pop() || '';
    const i = last.lastIndexOf('.');
    return i > 0 ? last.slice(i + 1).toLowerCase() : '';
  } catch {
    return '';
  }
}

/** 'hls' | 'dash' | 'video' | 'audio' for a response worth offering for download, else null. */
export function classifyMedia({ url, contentType = '', size = -1 }) {
  if (!isHttp(url)) return null;
  const ext = extOf(url);
  if (SEGMENT_EXT.has(ext)) return null;
  const ct = String(contentType).split(';')[0].trim().toLowerCase();
  if (ct === 'video/mp2t') return null; // a bare transport-stream segment
  let kind = MEDIA_EXT[ext] ?? null;
  if (!kind) for (const [re, k] of MEDIA_TYPES) if (re.test(ct)) { kind = k; break; }
  if (!kind) return null;
  // tiny clips are tracking pixels, previews or ads (the size is unknown for ranged/streamed responses)
  if ((kind === 'video' || kind === 'audio') && size >= 0 && size < MIN_MEDIA_BYTES) return null;
  return kind;
}

/** Add a sniffed item once per URL, keeping at most `max` of the newest. */
export function addMedia(list, entry, max = 40) {
  if (list.some((e) => e.url === entry.url)) return list;
  const next = [...list, entry];
  return next.length > max ? next.slice(next.length - max) : next;
}

export function mediaName(url) {
  try {
    const u = new URL(url);
    const last = decodeURIComponent(u.pathname.split('/').filter(Boolean).pop() || '');
    return last || u.hostname;
  } catch {
    return url;
  }
}

/** Every kind we list can be downloaded (DASH needs ffmpeg on the computer; the app says so if it is missing). */
export function mediaSupported(kind) {
  return ['hls', 'dash', 'video', 'audio'].includes(kind);
}

// ---- "download all links" ---------------------------------------------------------------------------------

const KIND_EXT = {
  document: 'pdf doc docx xls xlsx ppt pptx odt ods odp txt epub rtf csv',
  image: 'jpg jpeg png gif bmp webp svg tif tiff ico',
  video: 'mp4 mkv avi mov wmv webm flv m4v mpg mpeg 3gp ogv m3u8',
  audio: 'mp3 flac aac ogg opus wav wma m4a',
  archive: 'zip 7z rar gz bz2 xz zst tar tgz',
  program: 'exe msi deb rpm appimage apk iso img bin sh run',
};
const EXT_KIND = Object.fromEntries(Object.entries(KIND_EXT).flatMap(([k, v]) => v.split(' ').map((e) => [e, k])));
export const LINK_KINDS = ['document', 'image', 'video', 'audio', 'archive', 'program', 'other'];
const PAGE_EXT = new Set(['', 'html', 'htm', 'xhtml', 'php', 'asp', 'aspx', 'jsp', 'cgi', 'shtml']);

/** Kind of a link: a known file type, 'page' for ordinary web pages, else 'other' (unknown file type). */
export function linkKind(url) {
  const e = extOf(url);
  if (EXT_KIND[e]) return EXT_KIND[e];
  return PAGE_EXT.has(e) ? 'page' : 'other';
}

export function dedupeLinks(links) {
  const seen = new Set();
  return links.filter((l) => isHttp(l.url) && !seen.has(l.url) && seen.add(l.url));
}

/** Links that are downloadable files of the wanted kinds (a Set), matching the optional text. */
export function filterLinks(links, kinds, text = '') {
  const q = text.trim().toLowerCase();
  return dedupeLinks(links)
    .map((l) => ({ ...l, kind: linkKind(l.url) }))
    .filter((l) => l.kind !== 'page' && kinds.has(l.kind) && (!q || l.url.toLowerCase().includes(q) || (l.text || '').toLowerCase().includes(q)));
}
