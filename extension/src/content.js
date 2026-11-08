// On-page "Download this video" button (like a classic download manager's). This file is a plain script, not a module:
// build.py puts the pure functions of core.js (shouldOfferButton, menuRows) in front of it.
const api = globalThis.browser ?? globalThis.chrome;

if (!window.__ldmContent) {
  window.__ldmContent = true;

  const STYLE = `
    :host { all: initial; }
    .bar { position: fixed; display: flex; align-items: stretch; font: 12px/16px "Segoe UI", Arial, sans-serif;
      border: 1px solid #5f84b3; border-radius: 3px; overflow: hidden; background: linear-gradient(#fbfdff, #cfe1f7);
      box-shadow: 0 1px 4px rgba(0,0,0,.45); color: #10243d; user-select: none; }
    .bar[hidden], .menu[hidden] { display: none; }
    .bar button { all: unset; cursor: pointer; padding: 2px 8px; display: flex; align-items: center; gap: 5px; }
    .bar button:hover { background: #b9d4f6; }
    .go::before { content: ""; border-left: 8px solid #1f9d3a; border-top: 5px solid transparent; border-bottom: 5px solid transparent; }
    .x { border-left: 1px solid #8fb0d8; padding: 2px 6px !important; color: #3b5576; }
    .menu { position: fixed; min-width: 380px; max-width: 560px; max-height: 60vh; overflow: auto; background: #fff; color: #111;
      border: 1px solid #8a8a8a; box-shadow: 0 3px 10px rgba(0,0,0,.4); font: 12px/16px "Segoe UI", Arial, sans-serif; padding: 3px 0; }
    .grp { padding: 4px 10px 2px; font-weight: 600; color: #4a5f7a; border-top: 1px solid #e3e8ef; }
    .grp:first-child { border-top: 0; }
    .row { display: flex; gap: 14px; padding: 3px 10px 3px 18px; cursor: pointer; white-space: nowrap; }
    .row:hover { background: #cfe3ff; }
    .row .t { flex: 1; overflow: hidden; text-overflow: ellipsis; }
    .row .s { color: #5b6b82; }
    .note { padding: 6px 12px; color: #4a5f7a; font-style: italic; }
    .err { padding: 6px 12px; color: #a32020; }
  `;

  let host = null, shadow = null, bar = null, goBtn = null, menu = null;
  let current = null, menuOpen = false, overHost = false, hideTimer = 0;
  const dismissed = new WeakSet();
  let enabled = true; // the "Show the on-page button" switch in the popup

  try {
    api.storage.local.get({ showButton: true }).then?.((v) => (enabled = v.showButton !== false));
    api.storage.onChanged.addListener((c) => {
      if (c.showButton) {
        enabled = c.showButton.newValue !== false;
        if (!enabled && bar) bar.hidden = true;
      }
    });
  } catch {} // storage unavailable: keep the button on

  // tell the background which modifier keys are held when a link is clicked (the "prevent/force" keys)
  const reportClick = (e) => {
    if (!e.target?.closest?.('a[href]')) return;
    try {
      api.runtime.sendMessage({ type: 'click-mods', alt: e.altKey, ctrl: e.ctrlKey, shift: e.shiftKey, meta: e.metaKey }, () => void api.runtime.lastError);
    } catch {}
  };
  document.addEventListener('mousedown', reportClick, true);
  document.addEventListener('click', reportClick, true);

  const send = (msg) =>
    new Promise((resolve) => {
      try {
        const r = api.runtime.sendMessage(msg, (res) => resolve(res ?? { error: api.runtime.lastError?.message || 'no answer' }));
        if (r && typeof r.then === 'function') r.then((res) => resolve(res ?? { error: 'no answer' }), (e) => resolve({ error: String(e?.message || e) }));
      } catch (e) {
        resolve({ error: String(e?.message || e) }); // the extension was reloaded: this page's script is orphaned
      }
    });

  function ensureHost() {
    if (host) return;
    host = document.createElement('div');
    host.style.cssText = 'all:initial;position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;';
    shadow = host.attachShadow({ mode: 'closed' });
    shadow.innerHTML = `<style>${STYLE}</style><div class="bar" hidden><button class="go"></button><button class="x" title="Hide this button">&#x2715;</button></div><div class="menu" hidden></div>`;
    bar = shadow.querySelector('.bar');
    goBtn = shadow.querySelector('.go');
    menu = shadow.querySelector('.menu');
    shadow.querySelector('.x').addEventListener('click', () => {
      if (current) dismissed.add(current);
      closeMenu();
      bar.hidden = true;
      current = null;
    });
    goBtn.addEventListener('click', () => (menuOpen ? closeMenu() : openMenu()));
    for (const el of [bar, menu]) {
      el.addEventListener('mouseenter', () => (overHost = true));
      el.addEventListener('mouseleave', () => {
        overHost = false;
        scheduleHide();
      });
    }
    attach();
  }

  function attach() {
    const parent = document.fullscreenElement || document.documentElement;
    if (host && host.parentNode !== parent) parent.appendChild(host);
  }

  function eligible(el) {
    if (!enabled || dismissed.has(el)) return false;
    const r = el.getBoundingClientRect();
    const cs = getComputedStyle(el);
    const hidden = cs.display === 'none' || cs.visibility === 'hidden' || r.bottom < 0 || r.top > innerHeight || r.right < 0 || r.left > innerWidth;
    return shouldOfferButton({ tag: el.tagName.toLowerCase(), width: r.width, height: r.height, hidden, controls: el.controls });
  }

  function players() {
    return [...document.querySelectorAll('video, audio')].filter(eligible);
  }

  function place() {
    if (!current || !bar || bar.hidden) return;
    const r = current.getBoundingClientRect();
    const w = bar.offsetWidth || 150;
    const left = Math.max(4, Math.min(innerWidth - w - 4, r.right - w - 8));
    const top = Math.max(4, Math.min(innerHeight - 30, r.top + 8));
    bar.style.left = `${left}px`;
    bar.style.top = `${top}px`;
    if (menuOpen) {
      menu.style.left = `${Math.max(4, Math.min(innerWidth - menu.offsetWidth - 4, left))}px`;
      menu.style.top = `${Math.min(innerHeight - 40, top + 26)}px`;
    }
  }

  function show(el) {
    ensureHost();
    attach();
    clearTimeout(hideTimer);
    current = el;
    goBtn.textContent = el.tagName === 'AUDIO' ? 'Download this audio' : 'Download this video';
    bar.hidden = false;
    place();
  }

  function scheduleHide(ms = 5000) {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (!menuOpen && !overHost && bar) bar.hidden = true;
    }, ms);
  }

  function closeMenu() {
    menuOpen = false;
    if (menu) menu.hidden = true;
  }

  function render(rows, extra) {
    menu.replaceChildren();
    let group = null;
    for (const r of rows) {
      if (r.group !== group) {
        group = r.group;
        const g = document.createElement('div');
        g.className = 'grp';
        g.textContent = group;
        menu.append(g);
      }
      const d = document.createElement('div');
      d.className = 'row';
      const t = document.createElement('span');
      t.className = 't';
      t.textContent = r.text;
      const s = document.createElement('span');
      s.className = 's';
      s.textContent = r.size || '';
      d.append(t, s);
      d.addEventListener('click', async () => {
        d.querySelector('.t').textContent = 'Sending…';
        const res = await send({ type: 'download', action: r.action });
        d.querySelector('.t').textContent = res.error ? `Could not send: ${res.error}` : 'Sent to Linux Download Manager';
        if (!res.error) setTimeout(closeMenu, 900);
      });
      menu.append(d);
    }
    if (extra) {
      const n = document.createElement('div');
      n.className = extra.error ? 'err' : 'note';
      n.textContent = extra.text;
      menu.append(n);
    }
    place();
  }

  async function openMenu() {
    menuOpen = true;
    menu.hidden = false;
    render([], { text: 'Looking for downloads…' });
    const media = await send({ type: 'media-list', src: current?.currentSrc || current?.src || '' });
    const sniffed = media.sniffed || [];
    render(menuRows({ sniffed }), { text: 'Asking for more formats (this can take a few seconds)…' });
    const res = await send({ type: 'formats' });
    if (!menuOpen) return;
    const rows = menuRows({ sniffed, formats: res.options || [] });
    if (res.error) {
      render(rows, rows.length ? null : { error: true, text: res.error.includes('yt-dlp') ? res.error : 'Nothing to download was found on this page.' });
    } else {
      render(rows, rows.length ? null : { text: 'Nothing to download was found on this page.' });
    }
  }

  let lastMove = 0;
  document.addEventListener(
    'mousemove',
    (e) => {
      const now = Date.now();
      if (now - lastMove < 80) return;
      lastMove = now;
      const hit = players().find((el) => {
        const r = el.getBoundingClientRect();
        return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
      });
      if (hit) {
        if (hit !== current || bar?.hidden) show(hit);
        scheduleHide(7000);
      } else if (!overHost) {
        scheduleHide(2000);
      }
    },
    { passive: true, capture: true },
  );
  document.addEventListener('play', (e) => {
    if (e.target instanceof HTMLMediaElement && eligible(e.target)) {
      show(e.target);
      scheduleHide(6000);
    }
  }, true);
  document.addEventListener('fullscreenchange', attach);
  addEventListener('scroll', place, { passive: true, capture: true });
  addEventListener('resize', place, { passive: true });
  document.addEventListener('click', (e) => {
    if (menuOpen && e.target !== host) closeMenu(); // a click anywhere else closes the menu
  }, true);
}
