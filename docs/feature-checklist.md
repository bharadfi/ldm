# Linux Download Manager — feature checklist

Goal set by the owner: the app (product name **Linux Download Manager**, crates/binaries keep the `zdm` prefix) should
offer the functionality of the well-known Windows download manager. Clean-room: behavior from public documentation and
observation only, our own icons/strings/code. `[x]` done, `[~]` partly, `[ ]` not started.

## Download engine
- [x] Multi-connection segmented download (8), resume with If-Range, retries, redirects, Content-Disposition names
- [x] Proxy (HTTP/SOCKS5), Basic auth, cookies/referer/user-agent hand-off, per-task rate limit
- [ ] Digest/NTLM auth, FTP/FTPS, global speed limiter (all tasks), per-connection-count setting (1–32)
- [ ] Dynamic segmentation (split slowest segment), mirror/refresh-link (re-add URL for a dead link)
- [ ] Clipboard URL monitor, drag-and-drop URL onto the window, import/export URL list
- [ ] Antivirus hook after completion, duplicate-URL detection, "file exists" dialog (rename/overwrite/resume)
- [ ] Scheduled/auto-resume after network drop, speed-aware connection reduction

## Queues, scheduler, categories
- [x] Named queues with max-parallel and weekly time windows (daemon), categories by extension with folders
- [~] Sidebar shows categories/queues; [ ] editing queues & categories in the UI, move task between queues, reorder
- [ ] Scheduler dialog (start/stop time, days, per-queue, "stop after N files", download limit)
- [ ] Completion actions per queue: hang up/exit/shutdown/sleep, run program, notify

## Window (main UI)
- [x] Toolbar, category tree, sortable table (name, queue, size, status, time left, rate, date, link), status bar
- [x] Colourful theme, logo, desktop launcher (`packaging/install-user.sh`)
- [x] Per-download progress window (per-connection bars, rate, time left, Pause/Resume, close-when-finished); [ ] speed graph, auto-open on start
- [x] Right-click menu (open, open folder, show progress, pause/resume, download again, copy link, properties, delete); [ ] refresh link, move to queue (needs a daemon request)
- [~] Download properties dialog (read-only done); "Add URL" dialog with auth/referer/category/save-as fields, batch add
- [ ] Options dialog (general, connection/proxy, save-to folders, file types, sounds, tray, language)
- [ ] Tray icon + menu, minimise to tray, notifications on completion/error, keyboard shortcuts, search/filter box
- [ ] Push updates from the daemon instead of polling

## Browser integration
- [x] Extension + native host: takeover of downloads (Firefox deb, Brave verified; Chrome same code)
- [~] Context menu "Download with ZDM"
- [ ] "Download all links" / "Download all images" page tools, video/stream sniffer overlay button (MP4/WebM, HLS, DASH)
- [ ] Per-site exceptions UI polish, file-type list, Shift/Alt-click bypass, store packaging

## Grabber / media
- [ ] Site grabber (crawl a site/page for files by type, depth, filters) and project queues
- [ ] HLS/DASH download + mux (ffmpeg), YouTube-style page parsers are out of scope unless the owner asks
- [ ] Video conversion on completion (optional, via ffmpeg)

## Packaging
- [~] systemd user unit written (not enabled/tested); [ ] .deb with launcher, icons, unit, native-host manifests
