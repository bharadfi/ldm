# zorin-dm (ZDM)

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
Per-segment resume state is still the JSON sidecar next to each `.part` (the SQLite DB holds task-level state).
Developed first on Windows (engine), continue on Zorin.
