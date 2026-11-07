# HANDOFF — read this first

This project (ZDM, a download manager for Zorin OS 18.x and later) was started on the owner's Windows PC in a Claude Code
session. That conversation is **not** available here; this file and `CLAUDE.md` are the memory. Everything done so far is
committed in git (`git log` shows the order).

## About the owner
- Not comfortable with Linux terminals. **Run commands yourself** with your tools, explain in plain words what you are
  doing and why, and give click-by-click instructions for anything that must be done by hand (e.g. a password prompt,
  a browser sign-in). Do not paste long command lists and expect them to be run.
- They have approved this work in full (reverse-engineering the Windows app for behavior, building the Linux app).
  Normal safety rules still apply: say before anything destructive or outward-facing (publishing, deleting, system-wide changes).
- Dual-boots Windows 11 and Zorin OS 18. Only one runs at a time, so the Windows IDM install **cannot** be re-checked from
  Linux. `docs/idm-behavior-spec.md` and `docs/idm-analysis.md` hold everything learned. If a question really needs the
  binaries, say what to look for and ask the owner to open the Windows session.

## What the project is
A clean-room reimplementation of the *observable behavior* of Internet Download Manager: segmented, resumable downloads,
queues/scheduler, categories, browser takeover. No IDM code, strings or binaries are used or may be added to the repo.
Stack: Rust (tokio, reqwest+rustls, rusqlite), GTK4/libadwaita for the UI, a MV3 WebExtension + native-messaging host.

## Done (all built and tested on WINDOWS only)
1. `zdm-core` engine: probe, work-stealing segments, resume sidecar, retries, proxy, Basic auth, rate limit, cancellation.
2. `zdmd` daemon + `zdm-proto` + `zdm` CLI: SQLite state, queues, time windows, categories, pause/resume/remove, crash recovery.
3. `zdm-nmhost` + `extension/`: browser takeover for Chrome-family and Firefox.
Tests: 40 Rust tests + 10 node tests pass on Windows; real `zdmd` + CLI + host were exercised end to end against `tools/rangesrv.py`.

## NOT verified (this is the work for Zorin, in order)
1. **Build and test on Linux**: `cargo test --workspace`, `cargo clippy --workspace --all-targets`. Expect first failures in the
   Unix-only code: `crates/zdmd/src/server.rs` (`#[cfg(unix)] listen`), `crates/zdm-proto/src/lib.rs` (`connect`),
   `crates/zdm-nmhost/src/install.rs` test `linux_plan_and_idempotent_apply` (never ran), `crates/zdmd/src/store.rs` (0600 file).
2. Smoke test like on Windows: start `python3 tools/rangesrv.py /tmp/req.jsonl 8765`, run `zdmd`, then `zdm add/list/pause/resume`.
   Compare requests with `docs/idm-behavior-spec.md`.
3. Run the extension in a real browser (`extension/README.md`). Check Firefox install type: a **snap** Firefox may not be able to
   launch the native host; if so tell the owner to use the Mozilla apt/deb Firefox or Chrome/Chromium/Brave.
4. systemd **user service** for `zdmd` (socket path `$XDG_RUNTIME_DIR/zdm/zdm.sock`), and a `.deb`.
5. GTK4/libadwaita window (needs `libgtk-4-dev libadwaita-1-dev`): task list with progress (clients currently poll `list`;
   consider a subscribe/push message in `zdm-proto`), queue manager, tray/indicator.
6. HLS/video capture, "download all links", completion actions, global rate limit, digest auth.

## Conventions
- Conventional Commit messages. Keep `CLAUDE.md` "Status" current. Tests first for engine/daemon behavior; use
  `crates/zdm-core/src/testserver.rs` (in-process HTTP server) rather than the network.
- Lock order in the daemon: `running` then `store`, never the reverse.
- Do not commit binaries, `target/`, `extension/dist/`, databases, or cookies.
