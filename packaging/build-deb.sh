#!/bin/bash
# Builds the .deb from target/release and extension/dist (run `cargo build --release --workspace` and
# `python3 extension/build.py --zip --crx` first). Usage: packaging/build-deb.sh OUTPUT_DIR
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:?output directory}"
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
P="$OUT/pkg-$VER"
rm -rf "${P:?}"
mkdir -p "$P/DEBIAN" "$P/usr/bin" "$P/usr/share/applications" "$P/usr/share/linux-download-manager" "$P/usr/share/doc/linux-download-manager"
for b in zdm-gui zdmd zdm zdm-nmhost; do install -m755 "$REPO/target/release/$b" "$P/usr/bin/$b"; done
for s in 48 64 128 256 512; do
  d="$P/usr/share/icons/hicolor/${s}x${s}/apps"; mkdir -p "$d"
  cp "$REPO/crates/zdm-gui/assets/logo-$s.png" "$d/org.linuxdm.Manager.png"
done
cat > "$P/usr/share/applications/org.linuxdm.Manager.desktop" <<D
[Desktop Entry]
Type=Application
Name=Linux Download Manager
Comment=Fast, resumable, multi-connection downloads
Exec=zdm-gui
Icon=org.linuxdm.Manager
Terminal=false
Categories=Network;FileTransfer;
StartupWMClass=org.linuxdm.Manager
D
cp -r "$REPO/extension/dist/chrome" "$REPO/extension/dist/firefox" "$REPO/extension/dist/ldm.crx" "$REPO/extension/dist/ldm.xpi" "$P/usr/share/linux-download-manager/"
cp "$REPO/LICENSE-MIT" "$REPO/LICENSE-APACHE" "$P/usr/share/doc/linux-download-manager/"
cat > "$P/DEBIAN/control" <<C
Package: linux-download-manager
Version: $VER
Section: net
Priority: optional
Architecture: amd64
Depends: libgtk-4-1 (>= 4.14), libadwaita-1-0 (>= 1.5), libc6
Recommends: ffmpeg, yt-dlp
Maintainer: bharadfi <fbharadwaj02@gmail.com>
Description: Linux Download Manager
 Segmented, resumable downloads with browser integration, queues,
 scheduler, site grabber and video downloads.
C
cat > "$P/DEBIAN/postinst" <<'S'
#!/bin/sh
# An upgrade must not leave the old window or background service running next to the new files: they would
# speak different versions. Stop them; the window and service start again the next time LDM is opened.
set -e
pkill -x zdm-gui 2>/dev/null || true
pkill -x zdmd 2>/dev/null || true
exit 0
S
chmod 755 "$P/DEBIAN/postinst"
dpkg-deb --build --root-owner-group "$P" "$OUT/linux-download-manager_${VER}_amd64.deb"
