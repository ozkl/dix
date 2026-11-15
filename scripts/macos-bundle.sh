#!/usr/bin/env bash
# Builds "Disk Image Explorer.app", the macOS application bundle, and
# optionally a .dmg disk image holding it.
#
#   scripts/macos-bundle.sh                 build for this Mac's architecture
#   scripts/macos-bundle.sh --universal     one binary for Apple silicon and Intel
#   scripts/macos-bundle.sh --dmg           also make a .dmg next to the .app
#   scripts/macos-bundle.sh --binary PATH   bundle a binary built elsewhere
#   scripts/macos-bundle.sh --out DIR       where to put them (default: target/macos)
#   scripts/macos-bundle.sh --sign ID       sign with this identity (default: ad hoc, "-")
#
# The universal build needs both Rust targets:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
app_name="Disk Image Explorer"
exe="disk-image-explorer"
bundle_id="io.github.ozkl.dix"
min_macos="11.0"

universal=0
dmg=0
binary=""
out="$root/target/macos"
sign="-"

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --universal) universal=1 ;;
        --dmg) dmg=1 ;;
        --binary) binary="${2:?--binary needs a path}"; shift ;;
        --out) out="${2:?--out needs a directory}"; shift ;;
        --sign) sign="${2:?--sign needs an identity}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

# The version, from the workspace's Cargo.toml.
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
version="${version:-0.1.0}"

# 1. The binary.
if [ -z "$binary" ]; then
    if [ "$(uname -s)" != "Darwin" ]; then
        echo "Building for macOS needs a Mac (or pass --binary with one built elsewhere)." >&2
        exit 1
    fi
    cd "$root"
    if [ "$universal" = 1 ]; then
        for t in aarch64-apple-darwin x86_64-apple-darwin; do
            echo "Building for $t"
            MACOSX_DEPLOYMENT_TARGET="$min_macos" cargo build --release -p dix-app --target "$t"
        done
        binary="$root/target/macos-universal/$exe"
        mkdir -p "$(dirname "$binary")"
        lipo -create -output "$binary" \
            "$root/target/aarch64-apple-darwin/release/$exe" \
            "$root/target/x86_64-apple-darwin/release/$exe"
    else
        echo "Building"
        MACOSX_DEPLOYMENT_TARGET="$min_macos" cargo build --release -p dix-app
        binary="$root/target/release/$exe"
    fi
fi
[ -f "$binary" ] || { echo "no binary at $binary" >&2; exit 1; }

# 2. The bundle.
app="$out/$app_name.app"
echo "Making $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/$exe"
chmod 755 "$app/Contents/MacOS/$exe"
cp "$root/crates/dix-app/assets/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
# The licenses (the app shows them too: View → Licenses).
cp "$root/LICENSE" "$root/THIRD-PARTY-LICENSES.txt" "$app/Contents/Resources/"
printf 'APPL????' > "$app/Contents/PkgInfo"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$app_name</string>
    <key>CFBundleName</key>
    <string>$app_name</string>
    <key>CFBundleExecutable</key>
    <string>$exe</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>$min_macos</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Disk Image Explorer contributors</string>
</dict>
</plist>
PLIST

# 3. Signing: ad hoc unless an identity is given. Apple silicon will not run
# an unsigned binary at all; a Developer ID (and notarization) is what lets
# other Macs open it without Gatekeeper's warning.
if command -v codesign >/dev/null; then
    echo "Signing ($sign)"
    if [ "$sign" = "-" ]; then
        codesign --force --deep -s - "$app"
    else
        # The hardened runtime and a timestamp, which notarization asks for.
        codesign --force --deep --options runtime --timestamp -s "$sign" "$app"
    fi
    codesign --verify --strict "$app"
else
    echo "codesign is not here: the bundle is unsigned."
fi

# 4. The disk image: the app and a link to /Applications to drag it to.
if [ "$dmg" = 1 ]; then
    command -v hdiutil >/dev/null || { echo "making a .dmg needs hdiutil (macOS)" >&2; exit 1; }
    stage="$(mktemp -d)"
    trap 'rm -rf "$stage"' EXIT
    cp -R "$app" "$stage/"
    ln -s /Applications "$stage/Applications"
    image="$out/Disk-Image-Explorer-$version.dmg"
    rm -f "$image"
    echo "Making $image"
    hdiutil create -volname "$app_name" -srcfolder "$stage" -ov -format UDZO "$image" >/dev/null
fi

echo "Done: $app"
