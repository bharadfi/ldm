# zorin-dm — product name: Linux Download Manager (crates/binaries keep the `zdm` prefix)

**New session? Read `HANDOFF.md` first, then `docs/idm-analysis.md` and `docs/idm-behavior-spec.md`.**

A segmented, resumable download manager for Zorin OS 18.x (Ubuntu 24.04 base) and later releases.
Inspired by the *observable behavior* of a commercial Windows download manager. Clean-room: never copy its
code, strings or data files; implement from public standards (RFC 9110 Range, RFC 8216 HLS, WebExtension and
native-messaging docs). Behavior notes live in `docs/idm-behavior-spec.md`.

## Stack
Rust (tokio, reqwest+rustls, rusqlite) core; GTK4/libadwaita GUI; WebExtension + native-messaging host.

## Planned layout
- `crates/zdm-core`  engine: probe, segment planner, writers, resume state (SQLite)
- `crates/zdmd`      daemon (systemd --user), unix-socket API
- `crates/zdm-cli`   CLI
- `crates/zdm-gui`   GTK4 UI
- `crates/zdm-nmhost` native-messaging stdio host
- `extension/`       Chrome/Firefox WebExtension
- `tools/rangesrv.py` logging range test server: `python tools/rangesrv.py req.jsonl 8765`
  routes: /file.bin 40MB, /norange.bin, /redir.bin, /cd, /slow.bin, /vslow.bin

## Milestones
1. Engine + CLI, resumable segmented downloads, tests against rangesrv.py
2. Daemon, SQLite state, queue/scheduler/categories/rate limit
3. GTK4 UI  4. Native host + extension  5. HLS/video capture, .deb

## Verification plan
Run the engine against `tools/rangesrv.py` and compare the request log with `docs/idm-behavior-spec.md`
(8 connections, ranged resume after kill, fallback when Range is unsupported). Linux-only parts (GTK, .deb,
native messaging paths) must be tested on Zorin; note unverified items in PRs.

## Daemon (milestone 2, first cut)
- `zdm-proto`: JSON-lines protocol types + client. Unix socket `$XDG_RUNTIME_DIR/zdm/zdm.sock` (0600) on Linux;
  on Windows dev builds loopback TCP whose first line must be the token in `%LOCALAPPDATA%\zdm\daemon.json`
  (a bare localhost port would let web pages talk to it).
- `zdmd`: SQLite store (`~/.local/share/zdm/zdm.db`, 0600 because tasks may hold cookies), `Manager`
  (scheduler tick + wake, per-queue `max_parallel`, weekly time windows incl. overnight, pause/resume/remove,
  crash recovery: `running` -> `queued` on start), categories by extension -> `~/Downloads/ZDM/<Category>`,
  unique names `name (1).ext`. Env: `ZDM_DATA_DIR`, `ZDM_DOWNLOAD_DIR`.
- CLI: `zdm get <url>` (direct, no daemon), `zdm add|list|show|pause|resume|rm|queue ...|ping`.
- Lock order: `running` then `store`; never take them the other way round.
- Engine cancellation: `Options.cancel` (keeps partial data + sidecar); `zdm_core::Cancelled` marks it.

## Browser integration (milestone 4, first cut)
- `crates/zdm-nmhost`: native-messaging host (4-byte native-endian length + JSON). Accepts only `ping`/`add`,
  http(s) only, forwards only `Cookie`; starts `zdmd` if it is not running (Windows: clears handle inheritance
  on std handles first, otherwise the browser never sees the pipe close). `install`/`uninstall [--dry-run]
  [--browser NAME] [--chrome-extension-id ID]` write manifests (Linux: per-browser NativeMessagingHosts dirs;
  Windows: HKCU keys). Additive, idempotent, `.bak` on differing existing manifest.
- `extension/`: MV3, `src/core.js` pure logic (tested with `node --test extension/test`), `build.py` makes
  `dist/chrome` + `dist/firefox`. Chromium ID is pinned by the manifest `key`; the same ID is in
  `zdm-nmhost/src/lib.rs` (a test checks they match). Firefox ID `zdm@zdm.invalid`.
- See `extension/README.md` for install steps and security notes.

## Status
Milestone 1 first cut done (Windows only): `zdm-core` (probe, work-stealing segment scheduler, `.part` +
`.part.zdm` JSON resume sidecar, If-Range with strong ETag, per-segment retries) and `zdm-cli` (`zdm <url>`).
Verified on Windows against `tools/rangesrv.py`: 8 connections, no-Range fallback, redirect, Content-Disposition
name, kill + resume (only missing ranges re-requested), sampled byte integrity. 7 unit tests pass.
Hardening pass done: proxy (incl. socks5), Basic auth (sensitive header), cookies via `-H`, shared rate limit,
probe retry, 416/200-on-range => discard partial + re-probe + restart once, 17 tests (10 in-process HTTP
integration tests in `crates/zdm-core/src/engine/http_tests.rs` using `src/testserver.rs`).
Daemon + queue first cut done and tested on Windows (34 tests: 11 manager/protocol integration tests, plus a
manual end-to-end run of the real `zdmd` + `zdm` against tools/rangesrv.py incl. hard-kill recovery).
Not yet: any Linux build/test (Unix socket path, systemd unit), digest auth, real proxy/socks tests, mid-stream
5xx on long bodies, global (cross-task) rate limit, per-queue completion actions (shutdown etc.), progress
push/subscribe (clients poll `list`), hand-off of cookies/auth from the browser, CI, GUI, extension.
Browser extension + native host first cut: host protocol tested end-to-end on Windows (auto-start of zdmd,
UA/cookie/referer forwarding, header allow-list, rejection of bad input); extension logic has 10 node tests.
NOT verified: in a real browser, on Linux (the unix-only manifest-install test never ran on Windows), Firefox snap.
Not built: HLS/video sniffing, "download all links", extension store packaging.
Per-segment resume state is still the JSON sidecar next to each `.part` (the SQLite DB holds task-level state).
Developed first on Windows (engine), continue on Zorin.
Linux verification (Zorin session 1): `cargo test --workspace` (all pass, incl. the unix-only nmhost install test),
`cargo clippy --workspace --all-targets` clean, node extension tests 10/10. Smoke-tested real `zdmd` + `zdm` against
rangesrv.py: socket 0600, DB 0600, 8 ranged connections, pause/resume. Added `packaging/systemd/zdmd.service`
(user unit, ExecStart `~/.cargo/bin/zdmd`; not yet enabled/tested via systemctl). This machine had no browser and no
libgtk-4-dev/libadwaita-1-dev yet (needs sudo apt), so extension-in-browser and GUI are still pending.
Browser takeover verified in a real browser on Linux (Zorin 18, Mozilla-deb Firefox 159, temporary add-on): a click on a
download link was cancelled in Firefox and completed by ZDM (UA forwarded, files numbered `name (N).ext`). Chrome/Brave
and the systemd unit are still untested. GTK dev libs (libgtk-4-dev 4.14, libadwaita 1.5) are now installed.
GUI first cut (`crates/zdm-gui`, GTK4/libadwaita, `cargo run -p zdm-gui`): classic layout (toolbar, category tree, sortable
GtkTreeView table, status bar), polls `list` once a second, starts `zdmd` if missing. Uses the deprecated-but-supported
TreeView on purpose (in-place row updates keep selection). Not yet: per-download progress window, context menu,
scheduler/queue editor, tray, options, push updates. Verified by launching only; button behaviour judged by the owner.
Product name is **Linux Download Manager** (owner's request); logo = `crates/zdm-gui/assets/logo-*.png` (owner-supplied image). App id `org.linuxdm.Manager`. Colourful light theme in `crates/zdm-gui/assets/style.css`. Feature roadmap: `docs/feature-checklist.md`.
Daemon now has runtime settings (`Request::GetSettings/SetSettings`, saved to `<data dir>/settings.json`, 0600) and `MoveTask`.
GUI: menu bar, Options, Scheduler, batch/import/export, About (`crates/zdm-gui/src/{menus,dialogs,helpers,prefs}.rs`).
Headless GUI check: start `gtk4-broadwayd :5`, then run
`G_DEBUG=fatal-criticals GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 LDM_SELFTEST=http://127.0.0.1:8765 target/release/zdm-gui`
(needs tools/rangesrv.py + zdmd running; exits 0 if add/delete/dialogs survive). It caught a real crash: never keep
GtkListStore row handles across a `remove` (see `row_index`/`apply_tasks`); the debug build hid it, release crashed.
Tray: `crates/zdm-gui/src/tray.rs` (ksni, blocking API; shown by GNOME AppIndicator). Notifications use GApplication
(`org.gtk.Notifications` on GNOME; verified with `busctl --user monitor org.gtk.Notifications`). `LDM_NO_TRAY=1` disables the
tray; the self-test skips it. `zdm-gui --background` starts hidden (autostart entry in ~/.config/autostart, toggled in Options).
Grabber/video: `zdm-core/src/grab.rs` (crawler, own tag scanner), `zdm-core/src/hls.rs` (HLS + AES-128 + ffmpeg remux, `ZDM_FFMPEG=none|/path`),
daemon `Request::Grab` and `.m3u8` tasks (`Manager::execute_hls`, work dir `<dest>.hls/`), GUI `grabber.rs`, extension sniffer + `links.html`.
`testserver::serve_site` serves canned pages for crawler/HLS tests; `tools/rangesrv.py` has `/site/` pages for the GUI self-test.

