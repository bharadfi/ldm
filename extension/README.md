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
  The extension ID is fixed (`oihpkfhbmggccackdffcmhlbficmdigk`, set by the `key` in the manifest), which is what the
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
- Popup settings: on/off, "keep small files in the browser" threshold, sites the browser should keep handling.

## Security notes

- The host accepts only `ping` and `add`, only `http(s)` URLs, and forwards only a `Cookie` header; anything else the
  extension sends is dropped or rejected. Pause/remove/file deletion are not reachable from the browser.
- Cookies are stored in the daemon database so a download can resume; that file is mode 0600.
- Permissions requested and why: `downloads` (see and cancel downloads), `cookies` + all-sites access (cookies for the
  download URL), `nativeMessaging`, `storage` (settings), `contextMenus`.

## Not implemented / not verified

- Video/stream sniffing (HLS/DASH) and "Download all links" are not built.
- Verified: extension logic (`cd extension && npm test` or `node --test test/`), the host protocol against a real
  daemon on Windows (including auto-start). **Not verified in a real browser**, on Linux, or with Firefox's snap
  package (a snap-confined Firefox may be unable to launch the host; install Firefox from a `.deb`/Mozilla's apt repo
  if the popup says it is not connected).
