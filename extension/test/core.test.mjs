import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { shouldHandle, cookieHeader, buildAddMessage, hostMatches, parseExcludedHosts, isHttp } from '../src/core.js';

const item = (o = {}) => ({ id: 1, url: 'https://example.com/a.zip', state: 'in_progress', totalBytes: -1, ...o });

test('takes ordinary http(s) downloads', () => {
  assert.equal(shouldHandle(item(), {}).handle, true);
  assert.equal(shouldHandle(item({ url: 'http://example.com/a.zip' }), {}).handle, true);
});

test('prefers finalUrl when judging the scheme', () => {
  assert.equal(shouldHandle(item({ url: 'https://x/redirect', finalUrl: 'https://cdn/x.zip' }), {}).handle, true);
  assert.equal(shouldHandle(item({ url: 'https://x/r', finalUrl: 'blob:https://x/uuid' }), {}).handle, false);
});

test('leaves non-http, extension, finished and disabled cases to the browser', () => {
  for (const [it, s] of [
    [item({ url: 'blob:https://example.com/uuid' }), {}],
    [item({ url: 'data:text/plain,hi' }), {}],
    [item({ url: 'file:///tmp/x' }), {}],
    [item({ url: 'ftp://h/f' }), {}],
    [item({ byExtensionId: 'abc' }), {}],
    [item({ state: 'complete' }), {}],
    [item(), { enabled: false }],
  ]) {
    assert.equal(shouldHandle(it, s).handle, false, JSON.stringify([it, s]));
  }
});

test('minimum size applies only when the size is known', () => {
  const s = { minSize: 1000 };
  assert.equal(shouldHandle(item({ totalBytes: 500 }), s).handle, false);
  assert.equal(shouldHandle(item({ totalBytes: 5000 }), s).handle, true);
  assert.equal(shouldHandle(item({ totalBytes: -1 }), s).handle, true);
  assert.equal(shouldHandle(item({ totalBytes: 0, fileSize: 10 }), s).handle, false);
});

test('excluded hosts match exactly and by subdomain, not by suffix text', () => {
  const s = { excludedHosts: ['example.com'] };
  assert.equal(shouldHandle(item({ url: 'https://example.com/f' }), s).handle, false);
  assert.equal(shouldHandle(item({ url: 'https://dl.example.com/f' }), s).handle, false);
  assert.equal(shouldHandle(item({ url: 'https://notexample.com/f' }), s).handle, true);
  assert.equal(hostMatches('https://a.b.test/x', ['*.b.test']), true);
  assert.equal(hostMatches('not a url', ['x']), false);
  assert.equal(hostMatches('https://x.test/', ['', '  ']), false);
});

test('cookie header orders specific paths first', () => {
  assert.equal(
    cookieHeader([
      { name: 'a', value: '1', path: '/' },
      { name: 'b', value: '2', path: '/deep/er' },
      { name: 'c', value: '3', path: '/deep' },
    ]),
    'b=2; c=3; a=1',
  );
  assert.equal(cookieHeader([]), '');
});

test('add message shape matches what zdm-nmhost accepts', () => {
  assert.deepEqual(buildAddMessage({ url: 'https://x/y', referer: 'https://x/', cookie: 'a=b', userAgent: 'UA' }), {
    type: 'add',
    url: 'https://x/y',
    referer: 'https://x/',
    user_agent: 'UA',
    headers: [['Cookie', 'a=b']],
  });
  assert.deepEqual(buildAddMessage({ url: 'https://x/y' }), { type: 'add', url: 'https://x/y', headers: [] });
});

test('excluded-hosts text parsing', () => {
  assert.deepEqual(parseExcludedHosts('a.com, b.com\n\n  c.org '), ['a.com', 'b.com', 'c.org']);
  assert.deepEqual(parseExcludedHosts(''), []);
});

test('isHttp', () => {
  assert.equal(isHttp('https://a/'), true);
  assert.equal(isHttp('javascript:alert(1)'), false);
  assert.equal(isHttp('nonsense'), false);
});

test('Chromium extension ID in build.py matches the one the native host allows', () => {
  const py = readFileSync(new URL('../build.py', import.meta.url), 'utf8');
  const rs = readFileSync(new URL('../../crates/zdm-nmhost/src/lib.rs', import.meta.url), 'utf8');
  const key = py.match(/CHROME_KEY = \(([\s\S]*?)\n\)/)[1].replace(/["\s]/g, '');
  // ID = first 16 bytes of sha256(DER) mapped to a-p; recompute and compare with the Rust constant.
  const { createHash } = await_crypto();
  const id = [...createHash('sha256').update(Buffer.from(key, 'base64')).digest().subarray(0, 16)]
    .map((b) => String.fromCharCode(97 + (b >> 4)) + String.fromCharCode(97 + (b & 15)))
    .join('');
  assert.ok(rs.includes(`CHROME_EXTENSION_ID: &str = "${id}"`), `ID ${id} not found in zdm-nmhost`);
  assert.ok(rs.includes('FIREFOX_EXTENSION_ID: &str = "zdm@zdm.invalid"'));
  assert.ok(py.includes('FIREFOX_ID = "zdm@zdm.invalid"'));
});

import { createRequire } from 'node:module';
function await_crypto() {
  return createRequire(import.meta.url)('node:crypto');
}

import { classifyMedia, addMedia, mediaName, mediaSupported, linkKind, filterLinks, dedupeLinks, extOf } from '../src/core.js';

test('recognises streams and media by URL or content type', () => {
  assert.equal(classifyMedia({ url: 'https://v.test/a/index.m3u8?t=1' }), 'hls');
  assert.equal(classifyMedia({ url: 'https://v.test/x', contentType: 'application/vnd.apple.mpegurl' }), 'hls');
  assert.equal(classifyMedia({ url: 'https://v.test/x', contentType: 'application/x-mpegURL; charset=utf-8' }), 'hls');
  assert.equal(classifyMedia({ url: 'https://v.test/m.mpd' }), 'dash');
  assert.equal(classifyMedia({ url: 'https://v.test/clip.mp4', size: 5_000_000 }), 'video');
  assert.equal(classifyMedia({ url: 'https://v.test/stream', contentType: 'video/webm' }), 'video');
  assert.equal(classifyMedia({ url: 'https://v.test/song', contentType: 'audio/mpeg' }), 'audio');
});

test('ignores segments, tracking clips and non-media', () => {
  assert.equal(classifyMedia({ url: 'https://v.test/seg1.ts' }), null);
  assert.equal(classifyMedia({ url: 'https://v.test/x', contentType: 'video/MP2T' }), null);
  assert.equal(classifyMedia({ url: 'https://v.test/seg.m4s', contentType: 'video/mp4' }), null);
  assert.equal(classifyMedia({ url: 'https://v.test/pixel.mp4', size: 900 }), null);
  assert.equal(classifyMedia({ url: 'https://v.test/page.html', contentType: 'text/html' }), null);
  assert.equal(classifyMedia({ url: 'blob:https://v.test/uuid', contentType: 'video/mp4' }), null);
  assert.equal(classifyMedia({ url: 'https://v.test/unknown-size.mp4', size: -1 }), 'video', 'unknown size is kept');
});

test('media list is deduped and capped, names are readable, dash is flagged', () => {
  let l = [];
  l = addMedia(l, { url: 'https://a/1.mp4' });
  l = addMedia(l, { url: 'https://a/1.mp4' });
  assert.equal(l.length, 1);
  for (let i = 0; i < 50; i++) l = addMedia(l, { url: `https://a/${i}.mp4` }, 10);
  assert.equal(l.length, 10);
  assert.equal(l.at(-1).url, 'https://a/49.mp4', 'newest are kept');
  assert.equal(mediaName('https://v.test/dir/My%20Clip.mp4?x=1'), 'My Clip.mp4');
  assert.equal(mediaName('https://v.test/'), 'v.test');
  assert.equal(mediaSupported('hls'), true);
  assert.equal(mediaSupported('dash'), true, 'DASH goes through ffmpeg in the app');
  assert.equal(mediaSupported('flash'), false);
});

test('link kinds, filtering and dedupe for "download all links"', () => {
  assert.equal(extOf('https://x/y/File.PDF?a=1'), 'pdf');
  assert.equal(linkKind('https://x/a.zip'), 'archive');
  assert.equal(linkKind('https://x/a.jpg'), 'image');
  assert.equal(linkKind('https://x/a.m3u8'), 'video');
  assert.equal(linkKind('https://x/about'), 'page');
  assert.equal(linkKind('https://x/about.html'), 'page');
  assert.equal(linkKind('https://x/data.xyz'), 'other');
  const links = [
    { url: 'https://x/a.zip', text: 'Archive' }, { url: 'https://x/a.zip', text: 'dup' }, { url: 'https://x/p.pdf', text: 'Paper' },
    { url: 'https://x/about', text: 'About' }, { url: 'mailto:a@b.c' }, { url: 'https://x/i.png', text: '' },
  ];
  assert.equal(dedupeLinks(links).length, 4);
  assert.deepEqual(filterLinks(links, new Set(['archive', 'document', 'image'])).map((l) => l.kind), ['archive', 'document', 'image']);
  assert.deepEqual(filterLinks(links, new Set(['archive'])).map((l) => l.url), ['https://x/a.zip']);
  assert.deepEqual(filterLinks(links, new Set(['archive', 'document']), 'paper').map((l) => l.url), ['https://x/p.pdf']);
});

test('built manifests list the permissions and files the new features need', () => {
  const src = readFileSync(new URL('../build.py', import.meta.url), 'utf8');
  for (const needle of ['"webRequest"', '"scripting"', '"links.html"', '"links.js"', '"host.js"']) assert.ok(src.includes(needle), needle);
});

import { isStreamChunk, shouldOfferButton, humanSize, parseHlsVariants, menuRows } from '../src/core.js';

test('player chunks are not offered as files, real files still are', () => {
  assert.equal(isStreamChunk('https://rr1---sn.googlevideo.com/videoplayback?expire=1&mime=video%2Fmp4'), true);
  assert.equal(isStreamChunk('https://cdn.test/v.mp4?range=0-999999'), true);
  assert.equal(isStreamChunk('https://cdn.test/v.mp4?bytestart=1000'), true);
  assert.equal(isStreamChunk('https://cdn.test/v.mp4'), false);
  assert.equal(classifyMedia({ url: 'https://rr1.googlevideo.com/videoplayback?x=1', contentType: 'video/mp4', size: 9_000_000 }), null);
  assert.equal(classifyMedia({ url: 'https://cdn.test/movie.mp4', size: 9_000_000 }), 'video');
});

test('which players get a button', () => {
  assert.equal(shouldOfferButton({ tag: 'video', width: 640, height: 360 }), true);
  assert.equal(shouldOfferButton({ tag: 'video', width: 100, height: 60 }), false, 'tiny preview or ad pixel');
  assert.equal(shouldOfferButton({ tag: 'video', width: 640, height: 360, hidden: true }), false);
  assert.equal(shouldOfferButton({ tag: 'audio', width: 300, height: 40, controls: true }), true);
  assert.equal(shouldOfferButton({ tag: 'audio', width: 0, height: 0, controls: false }), false, 'background audio without a player');
  assert.equal(shouldOfferButton({ tag: 'img', width: 900, height: 900 }), false);
});

test('sizes read like the download list', () => {
  assert.equal(humanSize(0), '');
  assert.equal(humanSize(-1), '');
  assert.equal(humanSize(512), '512 B');
  assert.equal(humanSize(21_000_000), '21.0 MB');
  assert.equal(humanSize(267_890_000), '268 MB');
  assert.equal(humanSize(1_372_500_000), '1.4 GB');
});

test('HLS master playlists list their resolutions, best first', () => {
  const m = '#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/p.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,CODECS="a,b"\n/abs/high.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=64000\naudio.m3u8\n';
  const v = parseHlsVariants(m, 'https://v.test/show/master.m3u8');
  assert.deepEqual(v.map((x) => x.label), ['HLS stream, quality 720p', 'HLS stream, quality 360p', 'HLS stream, 64 kbps']);
  assert.equal(v[0].url, 'https://v.test/abs/high.m3u8');
  assert.equal(v[1].url, 'https://v.test/show/low/p.m3u8');
  assert.deepEqual(parseHlsVariants('#EXTM3U\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST\n', 'https://v.test/a.m3u8'), [], 'a media playlist is not a master');
  assert.deepEqual(parseHlsVariants('<html>not a playlist</html>', 'https://v.test/'), []);
});

test('menu rows: yt-dlp choices first by group, then what the page loaded, without duplicates', () => {
  const formats = [
    { id: '299+140', kind: 'video', label: 'MP4 file, quality 1080p HD', size: 268_000_000, filename: 'T [1080p].mp4' },
    { id: '140', kind: 'audio', label: 'M4A audio only, 129 kbps', size: 10_300_000, filename: 'T [audio].m4a' },
    { id: 'sub:en', kind: 'subtitle', label: 'SRT subtitles, en', filename: 'T [en].srt' },
  ];
  const sniffed = [
    { url: 'https://cdn.test/a.m3u8', kind: 'hls', label: 'HLS stream, quality 720p' },
    { url: 'https://cdn.test/a.m3u8', kind: 'hls', label: 'duplicate' },
    { url: 'https://cdn.test/clip.mp4', kind: 'video', size: 5_000_000 },
    { url: 'https://cdn.test/song.mp3', kind: 'audio' },
    { url: 'https://cdn.test/m.mpd', kind: 'dash' },
  ];
  const rows = menuRows({ sniffed, formats });
  assert.deepEqual(rows.map((r) => r.group), ['Video', 'Audio only', 'Subtitles', 'Found on this page', 'Found on this page', 'Found on this page', 'Found on this page']);
  assert.deepEqual(rows[0], { group: 'Video', text: 'MP4 file, quality 1080p HD', size: '268 MB', action: { type: 'ytdlp', id: '299+140', filename: 'T [1080p].mp4' } });
  assert.deepEqual(rows.slice(3).map((r) => r.text), ['HLS stream, quality 720p', 'MP4 file', 'MP3 audio file', 'DASH stream, best quality']);
  assert.equal(rows[4].size, '5.0 MB');
  assert.deepEqual(rows[3].action, { type: 'url', url: 'https://cdn.test/a.m3u8' });
  assert.deepEqual(menuRows({}), []);
});

test('the add message carries a chosen yt-dlp format and file name', () => {
  assert.deepEqual(buildAddMessage({ url: 'https://v.test/watch?v=1', format: '299+140', filename: 'T [1080p].mp4', referer: 'https://v.test/', cookie: 'a=b', userAgent: 'UA' }), {
    type: 'add', url: 'https://v.test/watch?v=1', filename: 'T [1080p].mp4', format: '299+140', referer: 'https://v.test/', user_agent: 'UA', headers: [['Cookie', 'a=b']],
  });
  assert.deepEqual(buildAddMessage({ url: 'https://v.test/f.zip' }), { type: 'add', url: 'https://v.test/f.zip', headers: [] });
});

import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';

test('the build puts a self-contained content script in both packages', () => {
  execFileSync('python3', [new URL('../build.py', import.meta.url).pathname], { stdio: 'pipe' });
  for (const t of ['chrome', 'firefox']) {
    const dir = new URL(`../dist/${t}/`, import.meta.url);
    assert.ok(existsSync(new URL('content.js', dir)), `${t}: content.js`);
    const code = readFileSync(new URL('content.js', dir), 'utf8');
    assert.ok(!/^export /m.test(code), 'a content script cannot contain ES exports');
    assert.ok(!/^import /m.test(code), 'a content script cannot import');
    assert.doesNotThrow(() => new Function(code), 'it must at least parse');
    assert.ok(code.includes('function shouldOfferButton') && code.includes('function menuRows'), 'core functions are bundled in');
    const m = JSON.parse(readFileSync(new URL('manifest.json', dir), 'utf8'));
    assert.deepEqual(m.content_scripts[0].js, ['content.js']);
    assert.ok(m.permissions.includes('webRequest'));
  }
});

import { decideByKeys, parseTypeList, matchesType, MODIFIER_KEYS } from '../src/core.js';

test('modifier keys decide between prevent, force and the normal rules', () => {
  assert.equal(decideByKeys({ alt: true }, { preventKey: 'alt', forceKey: 'none' }), 'prevent');
  assert.equal(decideByKeys({ shift: true }, { preventKey: 'alt', forceKey: 'shift' }), 'force');
  assert.equal(decideByKeys({ alt: true, shift: true }, { preventKey: 'alt', forceKey: 'shift' }), 'prevent', 'prevent wins');
  assert.equal(decideByKeys({}, { preventKey: 'alt', forceKey: 'shift' }), null);
  assert.equal(decideByKeys({ alt: true }, { preventKey: 'none', forceKey: 'none' }), null, 'switched off');
  assert.equal(decideByKeys({ alt: true }, { preventKey: 'bogus' }), null);
  assert.deepEqual(MODIFIER_KEYS, ['none', 'alt', 'ctrl', 'shift', 'meta']);
});

test('the capture list takes only the chosen file types', () => {
  assert.deepEqual(parseTypeList(' zip, .RAR 7z;iso zip bad/ext x'), ['zip', 'rar', '7z', 'iso', 'x']);
  assert.deepEqual(parseTypeList(''), []);
  assert.equal(matchesType('https://h.test/a/File.ZIP?x=1', ['zip']), true);
  assert.equal(matchesType('Report final.pdf', ['zip', 'pdf']), true);
  assert.equal(matchesType('https://h.test/page.html', ['zip']), false);
  assert.equal(matchesType('https://h.test/noext', ['zip']), false);
  assert.equal(matchesType('https://h.test/page.html', []), true, 'an empty list takes everything');
});

test('shouldHandle applies the keys and the capture list', () => {
  const item = (o = {}) => ({ id: 1, url: 'https://h.test/a.zip', state: 'in_progress', totalBytes: 100, ...o });
  assert.equal(shouldHandle(item(), {}, { alt: true }).handle, false, 'Alt (default prevent key) leaves it to the browser');
  assert.equal(shouldHandle(item(), {}, { alt: true }).reason, 'prevent key held');
  assert.equal(shouldHandle(item(), { preventKey: 'none' }, { alt: true }).handle, true);
  assert.equal(shouldHandle(item({ url: 'https://h.test/page.html' }), { captureTypes: ['zip'] }).handle, false);
  assert.equal(shouldHandle(item({ url: 'https://h.test/page.html' }), { captureTypes: ['zip'] }).reason, 'file type not captured');
  assert.equal(shouldHandle(item(), { captureTypes: ['zip'] }).handle, true);
  // the force key beats the host list, the minimum size, the capture list and even "disabled"
  const strict = { enabled: false, excludedHosts: ['h.test'], minSize: 1_000_000, captureTypes: ['iso'], forceKey: 'shift' };
  assert.equal(shouldHandle(item(), strict, {}).handle, false);
  assert.equal(shouldHandle(item(), strict, { shift: true }).handle, true);
  assert.equal(shouldHandle(item({ url: 'blob:https://h.test/x' }), strict, { shift: true }).handle, false, 'still only http(s)');
  assert.equal(shouldHandle(item({ byExtensionId: 'x' }), strict, { shift: true }).handle, false, 'and never other extensions');
});
