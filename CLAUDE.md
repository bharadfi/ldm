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

## Status
Milestone 1 first cut done (Windows only): `zdm-core` (probe, work-stealing segment scheduler, `.part` +
`.part.zdm` JSON resume sidecar, If-Range with strong ETag, per-segment retries) and `zdm-cli` (`zdm <url>`).
Verified on Windows against `tools/rangesrv.py`: 8 connections, no-Range fallback, redirect, Content-Disposition
name, kill + resume (only missing ranges re-requested), sampled byte integrity. 7 unit tests pass.
Hardening pass done: proxy (incl. socks5), Basic auth (sensitive header), cookies via `-H`, shared rate limit,
probe retry, 416/200-on-range => discard partial + re-probe + restart once, 17 tests (10 in-process HTTP
integration tests in `crates/zdm-core/src/engine/http_tests.rs` using `src/testserver.rs`).
Not yet: Linux build/test, SQLite state (currently JSON sidecar), digest auth, proxy auth/real-proxy and socks
tests, mid-stream 5xx on long bodies, CI, daemon/GUI/extension.
Developed first on Windows (engine), continue on Zorin.
