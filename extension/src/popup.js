import { DEFAULTS, MODIFIER_KEYS, parseExcludedHosts, parseTypeList, mediaName, mediaSupported } from './core.js';
import { handOver } from './host.js';

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
const MIB = 1024 * 1024;

async function load() {
  const s = { ...DEFAULTS, ...(await api.storage.local.get(DEFAULTS)) };
  $('enabled').checked = s.enabled;
  $('minSize').value = Math.round(s.minSize / MIB);
  $('excluded').value = s.excludedHosts.join('\n');
  for (const id of ['preventKey', 'forceKey']) {
    const sel = $(id);
    sel.replaceChildren(...MODIFIER_KEYS.map((k) => Object.assign(document.createElement('option'), { value: k, textContent: k === 'none' ? 'None' : k[0].toUpperCase() + k.slice(1) })));
    sel.value = s[id];
  }
  $('captureTypes').value = s.captureTypes.join(' ');
  $('showButton').checked = s.showButton;
}

async function save() {
  await api.storage.local.set({
    enabled: $('enabled').checked,
    minSize: Math.max(0, Number($('minSize').value) || 0) * MIB,
    excludedHosts: parseExcludedHosts($('excluded').value),
    preventKey: $('preventKey').value,
    forceKey: $('forceKey').value,
    captureTypes: parseTypeList($('captureTypes').value),
    showButton: $('showButton').checked,
  });
}

async function checkDaemon() {
  const el = $('status');
  try {
    const r = await api.runtime.sendNativeMessage('com.zdm.host', { type: 'ping' });
    if (!r || !r.ok) throw new Error((r && r.error) || 'no reply');
    el.textContent = `Connected to zdmd ${r.version}`;
    el.className = 'status ok';
  } catch (e) {
    el.textContent = `Not connected: ${e.message}. Run "zdm-nmhost install", then restart the browser.`;
    el.className = 'status bad';
  }
}

async function checkPermission() {
  const have = await api.permissions.contains({ origins: ['<all_urls>'] });
  $('grant').hidden = have;
}

for (const id of ['enabled', 'minSize', 'excluded', 'preventKey', 'forceKey', 'captureTypes', 'showButton']) $(id).addEventListener('change', save);
$('grantBtn').addEventListener('click', async () => {
  await api.permissions.request({ origins: ['<all_urls>'] });
  await checkPermission();
});

await load();
await checkPermission();
await checkDaemon();

// ---- sniffed videos of the current tab ----
const [tab] = await api.tabs.query({ active: true, currentWindow: true });

async function showMedia() {
  if (!tab) return;
  const key = `media:${tab.id}`;
  const list = ((await (api.storage.session ?? api.storage.local).get(key))[key] ?? []).slice().reverse();
  $('media').hidden = list.length === 0;
  const ul = $('mediaList');
  ul.replaceChildren();
  for (const m of list) {
    const li = document.createElement('li');
    const label = document.createElement('span');
    label.className = 'name';
    label.title = m.url;
    label.textContent = `${m.kind.toUpperCase()} · ${mediaName(m.url)}`;
    const btn = document.createElement('button');
    btn.textContent = mediaSupported(m.kind) ? 'Download' : 'Not supported yet';
    btn.disabled = !mediaSupported(m.kind);
    btn.addEventListener('click', async () => {
      try {
        await handOver({ url: m.url, referer: m.page || tab.url });
        btn.textContent = 'Sent to ZDM';
        btn.disabled = true;
      } catch (e) {
        btn.textContent = 'Failed';
        btn.title = e.message;
      }
    });
    li.append(label, btn);
    ul.append(li);
  }
}

$('allLinks').addEventListener('click', async () => {
  if (tab) await api.tabs.create({ url: api.runtime.getURL(`links.html?tab=${tab.id}`) });
  window.close();
});
await showMedia();
