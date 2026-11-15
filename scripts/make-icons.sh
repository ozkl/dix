#!/usr/bin/env bash
# Regenerates the app's icons from crates/dix-app/assets/icon.svg:
#
#   icon-256.png   the window icon (Linux, Windows) and the web page's favicon
#   AppIcon.icns   the macOS bundle's icon (16 to 1024 px, @1x and @2x)
#   icon.ico       a Windows icon (16 to 256 px)
#
# Needs Node with Playwright (to render the SVG in Chromium) and Python with
# Pillow. On macOS, iconutil is used for the .icns when it is there.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
assets="$root/crates/dix-app/assets"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Rendering icon.svg"
node "$root/scripts/render-svg.js" "$assets/icon.svg" "$work/icon-1024.png" 1024

python3 - "$work/icon-1024.png" "$assets" "$work" <<'PY'
import os, sys
from PIL import Image

src, assets, work = sys.argv[1:4]
big = Image.open(src).convert("RGBA")

def sized(n):
    return big.resize((n, n), Image.LANCZOS)

sized(256).save(os.path.join(assets, "icon-256.png"), optimize=True)
sized(256).save(os.path.join(assets, "icon.ico"),
                sizes=[(n, n) for n in (16, 24, 32, 48, 64, 128, 256)])

# An iconset, for iconutil on macOS; Pillow writes the .icns elsewhere.
iconset = os.path.join(work, "AppIcon.iconset")
os.makedirs(iconset)
for n in (16, 32, 128, 256, 512):
    sized(n).save(os.path.join(iconset, f"icon_{n}x{n}.png"))
    sized(2 * n).save(os.path.join(iconset, f"icon_{n}x{n}@2x.png"))
big.save(os.path.join(work, "AppIcon-pillow.icns"))
PY

if command -v iconutil >/dev/null; then
    iconutil -c icns "$work/AppIcon.iconset" -o "$assets/AppIcon.icns"
else
    cp "$work/AppIcon-pillow.icns" "$assets/AppIcon.icns"
fi
echo "Wrote icon-256.png, icon.ico and AppIcon.icns in $assets"
