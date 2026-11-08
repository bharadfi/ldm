# ZDM browser extension

Takes over browser downloads and hands them to the ZDM daemon. Chromium family (Chrome, Chromium, Brave, Edge)
and Firefox, Manifest V3.

```
browser ── extension ── native messaging ──> zdm-nmhost ── socket ──> zdmd
```

## Build and install (Linux / Zorin OS)

```bash
cargo build --release                       # zdmd, zdm, zdm-nmhost in target/release/
python3 extension/build.py                  # extension/dist/chrome and extension/dist/firefox
target/release/zdm-nmhost install --dry-run # shows exactly which manifests it would write
target/release/zdm-nmhost install           # writes them (backs up a differing existing file as .bak)
```

Then restart the browser and load the extension:

- **Chrome/Chromium/Brave/Edge**: `chrome://extensions` → Developer mode → *Load unpacked* → `extension/dist/chrome`.
  The extension ID is fixed (`odbjkklmbbepkigbapicgcoedgjcaoed`, set by the `key` in the manifest), which is what the
  native-messaging manifest allows. A copy published to a store gets a different ID: install with
  `--chrome-extension-id <id>` to allow it too.
- **Firefox**: `about:debugging#/runtime/this-firefox` → *Load Temporary Add-on* → `extension/dist/firefox/manifest.json`.
  Firefox treats the all-sites access of MV3 extensions as optional: open the toolbar popup and press
  *Allow access to all sites* (needed to read cookies for sign-in-protected downloads).

The popup shows whether the extension can reach the daemon. If the daemon is not running, `zdm-nmhost` starts
`zdmd` itself (it looks next to its own executable first, then in `PATH`).

`zdm-nmhost uninstall` removes exactly the files `install` created.

## What it does

- Every browser download of an `http(s)` URL is cancelled in the browser **after** ZDM accepted it, and queued in ZDM
  with the page's referer, the browser's User-Agent and the cookies for that URL. If ZDM cannot be reached the browser
  keeps the download and the toolbar badge shows `!`.
- Right-click a link / video / audio / image → *Download with ZDM*.
- **Video sniffer**: streams (`.m3u8`/HLS, MP4, WebM, audio) that a page loads are counted on the toolbar icon and listed in the
  popup with a *Download* button (HLS is downloaded segment by segment and converted to MP4 when ffmpeg is installed).
  DASH (`.mpd`) is downloaded through ffmpeg (needs `ffmpeg` installed; no partial resume). The list starts over when the tab navigates to another address.
- **On-page button**: a small *Download this video* (or audio) button appears in the corner of a player when you move the mouse
  over it or press play. Its menu lists what you can get: the page's own files, HLS resolutions from the stream's playlist, and
  — through the app's yt-dlp support (YouTube and many other sites) — one row per resolution (MP4 1080p HD, 720p ...), audio-only
  tracks (M4A, Opus, MP3) and subtitles, each with its size. Needs `yt-dlp` (and `ffmpeg` to join video and audio) on the computer.
  The × hides the button on that player until the page is reloaded.
- **Download links from this page…** (popup button or right-click on the page): lists every file link of the page, filter by type
  or text, tick, send to ZDM.
- Popup settings: on/off, "keep small files in the browser" threshold, sites the browser should keep handling.

## Security notes

- The host accepts only `ping` and `add`, only `http(s)` URLs, and forwards only a `Cookie` header; anything else the
  extension sends is dropped or rejected. Pause/remove/file deletion are not reachable from the browser.
- Cookies are stored in the daemon database so a download can resume; that file is mode 0600.
- Permissions requested and why: `downloads` (see and cancel downloads), `webRequest` (observe response types to spot videos; nothing is blocked or changed), `scripting` (read the links of the page you ask about), `cookies` + all-sites access (cookies for the
  download URL), `nativeMessaging`, `storage` (settings), `contextMenus`.

## Not implemented / not verified

- Sniffing inside iframes served from other origins that the browser hides from extensions, are not handled.
- Verified: extension logic (`cd extension && npm test` or `node --test test/`), the host protocol against a real
  daemon on Windows (including auto-start). The new video sniffer and links page are unit-tested for their logic only and **not yet verified in a real browser**; the earlier takeover was verified in Firefox and Brave. **Not verified in Chrome**, on Linux, or with Firefox's snap
  package (a snap-confined Firefox may be unable to launch the host; install Firefox from a `.deb`/Mozilla's apt repo
  if the popup says it is not connected).
