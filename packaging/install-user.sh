#!/bin/bash
# Per-user install of the Linux Download Manager launcher + icon (no root needed).
# Usage: packaging/install-user.sh [path-to-zdm-gui]   (default: target/release/zdm-gui in this repo)
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$(readlink -f "${1:-$REPO/target/release/zdm-gui}")"
ID=org.linuxdm.Manager
[ -x "$BIN" ] || { echo "not found/executable: $BIN (run: cargo build --release -p zdm-gui)"; exit 1; }
for s in 48 64 128 256 512; do
  d="$HOME/.local/share/icons/hicolor/${s}x${s}/apps"; mkdir -p "$d"
  cp "$REPO/crates/zdm-gui/assets/logo-$s.png" "$d/$ID.png"
done
mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/$ID.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=Linux Download Manager
Comment=Fast, resumable, multi-connection downloads
Exec=$BIN
Icon=$ID
Terminal=false
Categories=Network;FileTransfer;
StartupWMClass=$ID
DESK
command -v update-desktop-database >/dev/null && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "installed launcher for $BIN"
