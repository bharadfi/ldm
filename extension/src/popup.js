import { DEFAULTS, parseExcludedHosts } from './core.js';

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
const MIB = 1024 * 1024;

async function load() {
  const s = { ...DEFAULTS, ...(await api.storage.local.get(DEFAULTS)) };
  $('enabled').checked = s.enabled;
  $('minSize').value = Math.round(s.minSize / MIB);
  $('excluded').value = s.excludedHosts.join('\n');
}

async function save() {
  await api.storage.local.set({
    enabled: $('enabled').checked,
    minSize: Math.max(0, Number($('minSize').value) || 0) * MIB,
    excludedHosts: parseExcludedHosts($('excluded').value),
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

for (const id of ['enabled', 'minSize', 'excluded']) $(id).addEventListener('change', save);
$('grantBtn').addEventListener('click', async () => {
  await api.permissions.request({ origins: ['<all_urls>'] });
  await checkPermission();
});

await load();
await checkPermission();
await checkDaemon();
