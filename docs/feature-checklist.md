# Linux Download Manager — feature checklist

Goal set by the owner: the app (product name **Linux Download Manager**, crates/binaries keep the `zdm` prefix) should
offer the functionality of the well-known Windows download manager. Clean-room: behavior from public documentation and
observation only, our own icons/strings/code. `[x]` done, `[~]` partly, `[ ]` not started.

## Download engine
- [x] Multi-connection segmented download (8), resume with If-Range, retries, redirects, Content-Disposition names
- [x] Proxy (HTTP/SOCKS5), Basic auth, cookies/referer/user-agent hand-off, per-task rate limit
- [x] Digest auth (MD5/SHA-256, checked against the RFC test vectors), plain FTP (resume with REST, anonymous or login), global speed limiter (changes live), connections per download 1–32; [ ] NTLM, FTPS (TLS), `-sess`/`auth-int` Digest variants
- [x] Dynamic segmentation (work stealing: an idle connection takes half of the largest remaining piece); [ ] mirror/refresh-link (give a dead task a new address)
- [x] Clipboard link offer (Options, off by default), drag-and-drop of links onto the window, import/export URL list
- [x] After-download command (virus scan etc.; file in `$ZDM_FILE`), duplicate-link detection (returns/resumes the existing task); [ ] "file exists" dialog (today a numbered name `name (1).ext` is used)
- [x] Automatic retry after network errors (growing pause, 0–20 tries, not for 4xx); [ ] speed-aware connection reduction

## Queues, scheduler, categories
- [x] Named queues with max-parallel and weekly time windows (daemon), categories by extension with folders
- [x] Sidebar categories/queues, edit queues, move task between queues (daemon `move_task`), Start/Stop Queue; [ ] editing categories, reorder within a queue
- [x] Scheduler dialog (per-queue parallel limit, time window, days, create/delete queues); [ ] "stop after N files", per-queue speed limit
- [x] One-shot "when downloads finish": sleep, shut down or run a command (30 s grace, shown in the status bar); [ ] per-queue actions, exit the app, hang up

## Window (main UI)
- [x] Toolbar, category tree, sortable table (name, queue, size, status, time left, rate, date, link), status bar
- [x] Colourful theme, logo, desktop launcher (`packaging/install-user.sh`)
- [x] Per-download progress window (per-connection bars, rate, time left, Pause/Resume, close-when-finished); [ ] speed graph, auto-open on start
- [x] Right-click menu (open, open folder, show progress, pause/resume, download again, copy link, properties, delete); [ ] refresh link, move to queue (needs a daemon request)
- [x] Properties dialog (read-only), batch add from text (clipboard prefill), import/export link list, About; [x] sign-in fields in the Add dialog; [ ] referer/category/save-as fields
- [x] Options dialog: save-to folder and a folder per category, connections (1-32), per-download and global speed limit, automatic retries, proxy, after-download command, interface switches, browser-helper installer; [ ] file-type list, sounds, language
- [x] Menu bar (Tasks/File/Downloads/View/Help), shortcuts, toolbar with Options/Scheduler/Start-Stop Queue, view toggles
- [x] Tray icon (StatusNotifierItem) with menu + speed tooltip, close-to-tray, `--background`, optional start at login, notifications on finish/error
- [ ] Search/filter box, tray icon animation, sounds
- [ ] Push updates from the daemon instead of polling

## Browser integration
- [x] Extension + native host: takeover of downloads (Firefox deb, Brave verified; Chrome same code)
- [~] Context menu "Download with ZDM"
- [x] "Download links from this page" (type filters, tick boxes) and a video/stream sniffer with per-tab list and toolbar badge (logic unit-tested; [ ] not yet tried in a real browser); [ ] DASH, on-video overlay button
- [ ] Per-site exceptions UI polish, file-type list, Shift/Alt-click bypass, store packaging

## Grabber / media
- [x] Site grabber: crawl a page/site by depth, same-site rule and file types (daemon `Grab` + window dialog, referrer kept); [ ] saved grabber projects, include/exclude patterns, login-protected sites
- [x] HLS download (master/media playlists, byte ranges, AES-128, resume, pause, MP4 remux via ffmpeg, `.ts` fallback); [ ] DASH, live streams, SAMPLE-AES; YouTube-style page parsers stay out of scope unless the owner asks
- [ ] Video conversion on completion (optional, via ffmpeg)

## Packaging
- [~] systemd user unit written (not enabled/tested); [ ] .deb with launcher, icons, unit, native-host manifests
