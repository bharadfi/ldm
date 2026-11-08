#!/usr/bin/env python3
"""Build the ZDM extension for Chromium-family browsers and Firefox into extension/dist/.

    python3 extension/build.py            # dist/chrome and dist/firefox
    python3 extension/build.py --zip      # also dist/zdm-chrome.zip and dist/zdm-firefox.zip

Standard library only. Load dist/chrome via chrome://extensions ("Load unpacked") and dist/firefox via
about:debugging ("Load Temporary Add-on", pick manifest.json).
"""
import json
import re
import shutil
import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src"
DIST = ROOT / "dist"

VERSION = "0.1.0"
# Public half of a throwaway key pair: pins the Chromium extension ID so the native-messaging manifest can
# allow exactly this extension. Must match CHROME_EXTENSION_ID in crates/zdm-nmhost/src/lib.rs.
CHROME_KEY = (
    "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAi59QVd2ctkGw+FFVLJsgCHnJY5MYWh70J31DLHGP0X4qfiUE5lbDj4Q3a7ADK9tOylMZaaTpp79s"
    "sbrPfFtDoo3RcaBkUttEIJsbIB9byikgqQ4NFLEG5VMrEg5OLWuyheUjHbBoNXoOgWX/uEVIhtEVakDzh2mf7uYiWwK35tuvIqvBlZdW4SCrQr+IlyP3zz6B"
    "DtJ80xXEg/wpPt46vuAgxOTToM9/YZgAE7CT/M39ujoQU1V3kZ8Etgc8MLp3OdDpP7YoAiCTagi6TktGdZ9CyZOBa4cxYCxkEv8iJMfwwRBNydPSwFgArC//"
    "yNsb8BQz3eMF9nfMFlA09nKFJQIDAQAB"
)
FIREFOX_ID = "zdm@zdm.invalid"


def base_manifest():
    return {
        "manifest_version": 3,
        "name": "ZDM Download Manager",
        "version": VERSION,
        "description": "Hands browser downloads to the ZDM download manager for segmented, resumable downloads.",
        "permissions": ["downloads", "cookies", "nativeMessaging", "storage", "contextMenus", "webRequest", "scripting"],
        "host_permissions": ["http://*/*", "https://*/*"],
        "content_scripts": [{"matches": ["http://*/*", "https://*/*"], "js": ["content.js"], "all_frames": True, "run_at": "document_idle"}],
        "action": {"default_popup": "popup.html", "default_title": "ZDM Download Manager"},
        "icons": {"16": "icon16.png", "48": "icon48.png", "128": "icon128.png"},
    }


def manifest_for(target):
    m = base_manifest()
    if target == "chrome":
        m["key"] = CHROME_KEY
        m["background"] = {"service_worker": "background.js", "type": "module"}
        m["minimum_chrome_version"] = "110"
    else:
        m["background"] = {"scripts": ["background.js"], "type": "module"}
        m["browser_specific_settings"] = {"gecko": {"id": FIREFOX_ID, "strict_min_version": "115.0"}}
    return m


def png(size, pixel):
    """Minimal RGBA PNG writer; `pixel(x, y)` returns (r, g, b, a)."""
    raw = b"".join(b"\x00" + b"".join(bytes(pixel(x, y)) for x in range(size)) for y in range(size))

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def icon(size):
    """Blue rounded square with a white down arrow."""
    r = size * 0.22

    def inside_round_rect(x, y):
        cx = min(max(x, r), size - 1 - r)
        cy = min(max(y, r), size - 1 - r)
        return (x - cx) ** 2 + (y - cy) ** 2 <= r * r

    def arrow(x, y):
        u, v = x / size, y / size
        shaft = 0.40 <= u <= 0.60 and 0.18 <= v <= 0.52
        head = 0.62 <= v <= 0.80 and abs(u - 0.5) <= (0.80 - v) * 1.4 + 0.0
        bar = 0.80 <= v <= 0.86 and 0.25 <= u <= 0.75
        return shaft or head or bar

    def pixel(x, y):
        if not inside_round_rect(x, y):
            return (0, 0, 0, 0)
        return (255, 255, 255, 255) if arrow(x, y) else (26, 115, 232, 255)

    return png(size, pixel)


def content_script():
    """Content scripts cannot import modules: put core.js (without its `export`s) in front of the page script."""
    core = re.sub(r"^export ", "", (SRC / "core.js").read_text(), flags=re.M)
    return "(() => {\n" + core + "\n" + (SRC / "content.js").read_text() + "\n})();\n"


def build(target):
    out = DIST / target
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    for f in ("core.js", "host.js", "background.js", "popup.html", "popup.css", "popup.js", "links.html", "links.js"):
        shutil.copy(SRC / f, out / f)
    (out / "content.js").write_text(content_script())
    for s in (16, 48, 128):
        (out / f"icon{s}.png").write_bytes(icon(s))
    (out / "manifest.json").write_text(json.dumps(manifest_for(target), indent=2) + "\n")
    return out


def main():
    DIST.mkdir(exist_ok=True)
    for t in ("chrome", "firefox"):
        out = build(t)
        print(f"built {out}")
        if "--zip" in sys.argv:
            z = shutil.make_archive(str(DIST / f"zdm-{t}"), "zip", out)
            print(f"zipped {z}")


if __name__ == "__main__":
    main()
