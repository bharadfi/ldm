import { LINK_KINDS, filterLinks, mediaName } from './core.js';
import { handOver } from './host.js';

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
const tabId = Number(new URLSearchParams(location.search).get('tab'));
const kinds = new Set(LINK_KINDS);
const picked = new Set();
let page = { url: '', links: [] };

// Runs inside the page: every URL that could be a file.
function collect() {
  const out = [];
  const add = (u, t) => {
    try {
      const a = new URL(u, document.baseURI);
      if (a.protocol === 'http:' || a.protocol === 'https:') out.push({ url: a.href, text: (t || '').trim().slice(0, 120) });
    } catch {}
  };
  document.querySelectorAll('a[href]').forEach((e) => add(e.getAttribute('href'), e.textContent || e.title));
  document.querySelectorAll('img[src],video[src],audio[src],source[src],embed[src],track[src]').forEach((e) => add(e.getAttribute('src'), e.alt || e.title));
  document.querySelectorAll('video[poster]').forEach((e) => add(e.getAttribute('poster'), 'poster'));
  return { url: location.href, title: document.title, links: out };
}

function render() {
  const shown = filterLinks(page.links, kinds, $('text').value);
  const ul = $('list');
  ul.replaceChildren();
  for (const l of shown) {
    const li = document.createElement('li');
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = picked.has(l.url);
    cb.addEventListener('change', () => {
      cb.checked ? picked.add(l.url) : picked.delete(l.url);
      update(shown);
    });
    const kind = document.createElement('span');
    kind.className = 'kind';
    kind.textContent = l.kind;
    const name = document.createElement('span');
    name.className = 'name';
    name.title = l.url;
    name.textContent = mediaName(l.url) + (l.text && l.text !== mediaName(l.url) ? `  –  ${l.text}` : '');
    li.append(cb, kind, name);
    ul.append(li);
  }
  update(shown);
}

function update(shown) {
  const n = shown.filter((l) => picked.has(l.url)).length;
  $('go').disabled = n === 0;
  $('go').textContent = n ? `Download ${n} selected` : 'Download selected';
  $('where').textContent = `${shown.length} file link(s) on ${page.url}`;
  $('where').className = 'status ok';
}

function buildKinds() {
  for (const k of LINK_KINDS) {
    const label = document.createElement('label');
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = true;
    cb.addEventListener('change', () => {
      cb.checked ? kinds.add(k) : kinds.delete(k);
      render();
    });
    label.append(cb, ` ${k}`);
    $('kinds').append(label);
  }
}

$('text').addEventListener('input', render);
$('all').addEventListener('click', () => {
  filterLinks(page.links, kinds, $('text').value).forEach((l) => picked.add(l.url));
  render();
});
$('none').addEventListener('click', () => {
  picked.clear();
  render();
});
$('go').addEventListener('click', async () => {
  const urls = [...picked];
  let ok = 0;
  let failed = '';
  for (const url of urls) {
    try {
      await handOver({ url, referer: page.url });
      ok++;
    } catch (e) {
      failed = e.message;
      break;
    }
  }
  $('where').textContent = failed ? `Sent ${ok} of ${urls.length}; stopped: ${failed}` : `Sent ${ok} download(s) to ZDM.`;
  $('where').className = failed ? 'status bad' : 'status ok';
  if (!failed) picked.clear(), render();
});

buildKinds();
try {
  const [res] = await api.scripting.executeScript({ target: { tabId }, func: collect });
  page = res.result;
  render();
} catch (e) {
  $('where').textContent = `Could not read that page (${e.message}). Browser pages and the web store cannot be read.`;
  $('where').className = 'status bad';
}
