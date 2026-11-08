#!/usr/bin/env python3
"""Build the ZDM extension for Chromium-family browsers and Firefox into extension/dist/.

    python3 extension/build.py            # dist/chrome and dist/firefox
    python3 extension/build.py --zip      # also dist/zdm-chrome.zip and dist/zdm-firefox.zip

Standard library only. Load dist/chrome via chrome://extensions ("Load unpacked") and dist/firefox via
about:debugging ("Load Temporary Add-on", pick manifest.json).
"""
import base64
import hashlib
import json
import os
import re
import subprocess
import shutil
import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src"
DIST = ROOT / "dist"

VERSION = "0.1.9"
# Public half of the extension signing key (the private half stays on the maintainer's computer): pins the Chromium extension ID so the native-messaging manifest can
# allow exactly this extension. Must match CHROME_EXTENSION_ID in crates/zdm-nmhost/src/lib.rs.
CHROME_KEY = (
    "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA91+tLwIAYTr6kkU0PDG4PVY06ywndrqcjS+V9B4LTo6apzpHf/3p2LUOYy7PP0KFuHiYToAS8yni"
    "L0xlFgPqjdFORgiMA+JDhEzC384wRKt7qTEm7er7cUWtW4mMCkMkkKHulavii3BI8Owpvky1lnRPEfnXWNb2l4fq1h5TpVRj4oii/89Nm+t2NJ/jjs7JRxnZ"
    "NVv7k7X1dbalZ9NZ+gMCQp0o901XrUnOsKjX49MnQVo527IDOtYA3GtiTN5H+R39oj/E7l6XpYTVJsaLZQe0+AApMvDiY9a8pIFa+ZkzH3UVEgWp3a6IGFKU"
    "BFQucC2rKztLsnufrkIxPgnznwIDAQAB"
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


def _varint(n):
    out = b""
    while True:
        b = n & 0x7F
        n >>= 7
        out += bytes([b | (0x80 if n else 0)])
        if not n:
            return out


def _field(num, data):
    return _varint((num << 3) | 2) + _varint(len(data)) + data


def pack_crx(zip_bytes, pem_path):
    """CRX3 file (what Chromium-family browsers install from an 'external extension' entry)."""
    der = subprocess.check_output(["openssl", "rsa", "-in", pem_path, "-pubout", "-outform", "DER"], stderr=subprocess.DEVNULL)
    crx_id = hashlib.sha256(der).digest()[:16]
    signed_data = _field(1, crx_id)
    to_sign = b"CRX3 SignedData\x00" + struct.pack("<I", len(signed_data)) + signed_data + zip_bytes
    sig = subprocess.run(["openssl", "dgst", "-sha256", "-sign", pem_path], input=to_sign, stdout=subprocess.PIPE, check=True).stdout
    header = _field(2, _field(1, der) + _field(2, sig)) + _field(10000, signed_data)
    return b"Cr24" + struct.pack("<II", 3, len(header)) + header + zip_bytes


def main():
    DIST.mkdir(exist_ok=True)
    for t in ("chrome", "firefox"):
        out = build(t)
        print(f"built {out}")
        if "--zip" in sys.argv:
            z = shutil.make_archive(str(DIST / f"zdm-{t}"), "zip", out)
            print(f"zipped {z}")
            if t == "firefox":
                shutil.copy(z, DIST / "ldm.xpi")  # same file; Firefox installs .xpi packages
        if t == "chrome" and "--crx" in sys.argv:
            pem = os.environ.get("LDM_CRX_KEY") or os.path.expanduser("~/.local/share/ldm-signing/extension.pem")
            z = shutil.make_archive(str(DIST / "ldm-unsigned"), "zip", out)
            (DIST / "ldm.crx").write_bytes(pack_crx(Path(z).read_bytes(), pem))
            Path(z).unlink()
            print(f"packed {DIST / 'ldm.crx'}")


if __name__ == "__main__":
    main()
