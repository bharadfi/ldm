// Pure logic shared by the background script and the tests. No browser APIs in here.

export const DEFAULTS = {
  enabled: true,
  // Known-size downloads smaller than this many bytes stay with the browser (0 = take everything).
  minSize: 0,
  // Hosts (and their subdomains) the browser should keep downloading itself.
  excludedHosts: [],
  // Hold this key while clicking a download link to let the browser handle it ('none' to switch it off).
  preventKey: 'alt',
  // Hold this key to take the download over even if it would normally stay with the browser.
  forceKey: 'none',
  // Only take over these file types (extensions, lower case); empty = every download.
  captureTypes: [],
  // Show the "Download this video" button on players.
  showButton: true,
};

export const MODIFIER_KEYS = ['none', 'alt', 'ctrl', 'shift', 'meta'];

/** 'prevent' | 'force' | null for the modifier keys held when a link was clicked. Prevent wins if both match. */
export function decideByKeys(mods = {}, { preventKey = 'alt', forceKey = 'none' } = {}) {
  const held = (k) => k !== 'none' && MODIFIER_KEYS.includes(k) && mods[k] === true;
  if (held(preventKey)) return 'prevent';
  if (held(forceKey)) return 'force';
  return null;
}

/** `zip, .RAR 7z` -> ['zip', 'rar', '7z'] */
export function parseTypeList(text) {
  const out = [];
  for (const t of String(text).split(/[\s,;]+/)) {
    const e = t.replace(/^\./, '').toLowerCase();
    if (/^[a-z0-9]{1,12}$/.test(e) && !out.includes(e)) out.push(e);
  }
  return out;
}

/** Is the file's type in the capture list? (An empty list captures everything.) */
export function matchesType(urlOrName, types = []) {
  if (!types.length) return true;
  let path = String(urlOrName);
  try {
    path = new URL(path).pathname;
  } catch {}
  const last = decodeURIComponent(path.split('/').pop() || '');
  const i = last.lastIndexOf('.');
  return i > 0 && types.includes(last.slice(i + 1).toLowerCase());
}

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
export function shouldHandle(item, settings, mods = {}) {
  const s = { ...DEFAULTS, ...settings };
  const key = decideByKeys(mods, s);
  if (key === 'prevent') return { handle: false, reason: 'prevent key held' };
  const forced = key === 'force';
  if (!s.enabled && !forced) return { handle: false, reason: 'disabled' };
  if (item.byExtensionId) return { handle: false, reason: 'started by an extension' };
  if (item.state && item.state !== 'in_progress') return { handle: false, reason: `state ${item.state}` };
  const url = item.finalUrl || item.url;
  if (!isHttp(url)) return { handle: false, reason: 'not an http(s) URL' };
  if (!forced && hostMatches(url, s.excludedHosts)) return { handle: false, reason: 'excluded host' };
  if (!forced && !matchesType(item.filename || url, s.captureTypes)) return { handle: false, reason: 'file type not captured' };
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : 0;
  if (!forced && size > 0 && size < s.minSize) return { handle: false, reason: 'below minimum size' };
  return { handle: true, reason: 'ok' };
}

/** `Cookie` header value from browser cookie objects (more specific paths first, as browsers send them). */
export function cookieHeader(cookies) {
  return [...cookies]
    .sort((a, b) => (b.path || '').length - (a.path || '').length)
    .map((c) => `${c.name}=${c.value}`)
    .join('; ');
}

export function buildAddMessage({ url, referer, cookie, userAgent, filename, format }) {
  const msg = { type: 'add', url };
  if (filename) msg.filename = filename;
  if (format) msg.format = format; // a yt-dlp choice: `url` is then the video page
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

/** Pieces of a player's own stream (YouTube's `videoplayback` chunks, byte-range URLs): not files to offer. */
export function isStreamChunk(url) {
  try {
    const u = new URL(url);
    return /\/videoplayback\b/.test(u.pathname) || u.searchParams.has('range') || u.searchParams.has('bytestart') || /\/range\/\d+/.test(u.pathname);
  } catch {
    return false;
  }
}

/** 'hls' | 'dash' | 'video' | 'audio' for a response worth offering for download, else null. */
export function classifyMedia({ url, contentType = '', size = -1 }) {
  if (!isHttp(url) || isStreamChunk(url)) return null;
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

// ---- the on-page "Download this video" button and its menu -----------------------------------------------------

/** Should a player element get the button? (Big enough to be the main player, visible, audio only with controls.) */
export function shouldOfferButton({ tag, width, height, hidden = false, controls = false }) {
  if (hidden) return false;
  if (tag === 'video') return width >= 160 && height >= 90;
  if (tag === 'audio') return controls && width >= 100 && height >= 20;
  return false;
}

export function humanSize(bytes) {
  if (!(bytes > 0)) return '';
  const u = ['B', 'KB', 'MB', 'GB'];
  let v = bytes;
  let i = 0;
  while (v >= 1000 && i < u.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}

/** Variants of an HLS master playlist (best first); an ordinary media playlist has none. */
export function parseHlsVariants(text, baseUrl) {
  const lines = String(text).split(/\r?\n/).map((l) => l.trim());
  if (lines[0] !== '#EXTM3U') return [];
  const out = [];
  for (let i = 0; i < lines.length; i++) {
    if (!lines[i].startsWith('#EXT-X-STREAM-INF:')) continue;
    const attrs = lines[i].slice(18);
    const bw = Number(/(?:^|,)BANDWIDTH=(\d+)/.exec(attrs)?.[1] ?? 0);
    const res = /RESOLUTION=(\d+)x(\d+)/.exec(attrs);
    const next = lines.slice(i + 1).find((l) => l && !l.startsWith('#'));
    if (!next) continue;
    let url;
    try {
      url = new URL(next, baseUrl).href;
    } catch {
      continue;
    }
    const height = res ? Number(res[2]) : 0;
    out.push({ url, bandwidth: bw, height, label: height ? `HLS stream, quality ${height}p` : `HLS stream, ${Math.round(bw / 1000)} kbps` });
  }
  return out.sort((a, b) => b.height - a.height || b.bandwidth - a.bandwidth);
}

const KIND_TITLE = { video: 'Video', audio: 'Audio only', subtitle: 'Subtitles', stream: 'Found on this page' };

/**
 * Rows of the menu. `formats` are the daemon's (yt-dlp) options, `sniffed` what the page loaded
 * (progressive files, HLS variants, DASH). Each row: { group, text, size, action } where action is
 * `{type:'ytdlp', id, filename}` or `{type:'url', url}`.
 */
export function menuRows({ sniffed = [], formats = [] }) {
  const rows = [];
  for (const kind of ['video', 'audio', 'subtitle']) {
    for (const o of formats.filter((f) => f.kind === kind)) {
      rows.push({ group: KIND_TITLE[kind], text: o.label, size: humanSize(o.size), action: { type: 'ytdlp', id: o.id, filename: o.filename } });
    }
  }
  const seen = new Set();
  for (const s of sniffed) {
    if (!s.url || seen.has(s.url)) continue;
    seen.add(s.url);
    const text = s.label || (s.kind === 'dash' ? 'DASH stream, best quality' : s.kind === 'audio' ? `${(extOf(s.url) || 'audio').toUpperCase()} audio file` : `${(extOf(s.url) || 'video').toUpperCase()} file`);
    rows.push({ group: KIND_TITLE.stream, text, size: humanSize(s.size), action: { type: 'url', url: s.url } });
  }
  return rows;
}
