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
