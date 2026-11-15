#!/usr/bin/env bash
# Writes THIRD-PARTY-LICENSES.txt: the license of every crate the app is
# built with (for every platform, from about.toml), and of the code adapted
# into it. Run it after changing dependencies; the app shows the file
# (Licenses on the welcome screen, View → Licenses) and the bundles carry it.
#
# Needs cargo-about:  cargo install cargo-about --locked --features cli
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
out="THIRD-PARTY-LICENSES.txt"
cargo about generate --workspace --fail -o "$out.tmp" about.hbs
{
    cat "$out.tmp"
    echo "--------------------------------------------------------------------------------"
    echo "MIT License: the ext4 code adapted from ruxis"
    echo
    echo "Used by:"
    echo "  - dix-drivers (crates/dix-drivers/src/ext4/ruxis)"
    echo
    cat crates/dix-drivers/src/ext4/ruxis/LICENSE-ruxis
    # NOTICE files that crates ship (Apache-2.0 asks for them to be passed on).
    cargo metadata --format-version 1 | python3 -c '
import glob, json, os, sys
for p in sorted(json.load(sys.stdin)["packages"], key=lambda p: p["name"]):
    if p["source"] is None:
        continue
    for f in sorted(glob.glob(os.path.join(os.path.dirname(p["manifest_path"]), "NOTICE*"))):
        print("-" * 80)
        print("NOTICE of %s %s" % (p["name"], p["version"]))
        print()
        print(open(f, encoding="utf-8", errors="replace").read())
'
} > "$out"
rm -f "$out.tmp"
echo "Wrote $out ($(wc -c < "$out") bytes)"
