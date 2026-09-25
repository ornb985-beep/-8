#!/usr/bin/env bash
# Regenerate guest/vendor-minimal/etc/selinux from a GSI system.img (Linux:
# needs debugfs and secilc). Android 12's init loads
# /vendor/etc/selinux/precompiled_sepolicy when its
# precompiled_sepolicy.plat_sepolicy_and_mapping.sha256 matches the GSI's
# /system/etc/selinux/plat_sepolicy_and_mapping.sha256, so the policy is tied
# to the GSI build pinned in scripts/launch-android.sh.
#
#   scripts/gen-vendor-sepolicy.sh images/system.img
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYS="${1:?usage: $0 <system.img>}"
DST="$ROOT/guest/vendor-minimal/etc/selinux"
VER=32.0
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
for f in plat_sepolicy.cil plat_sepolicy_and_mapping.sha256 "mapping/$VER.cil"; do
    debugfs -R "dump /system/etc/selinux/$f $TMP/$(basename "$f")" "$SYS" 2>/dev/null
    [[ -s "$TMP/$(basename "$f")" ]] || { echo "missing /system/etc/selinux/$f in $SYS" >&2; exit 1; }
done
mkdir -p "$DST"
# A vendor without its own policy: declare the versioned attributes the
# platform mapping expects, plus an empty vendor policy.
grep -oE "^\(typeattributeset [A-Za-z0-9_]+_${VER/./_}" "$TMP/$VER.cil" | awk '{print "(typeattribute "$2")"}' | sort -u >"$DST/plat_pub_versioned.cil"
echo '(typeattribute vendor_minimal_placeholder)' >"$DST/vendor_sepolicy.cil"
echo "$VER" >"$DST/plat_sepolicy_vers.txt"
cp "$TMP/plat_sepolicy_and_mapping.sha256" "$DST/precompiled_sepolicy.plat_sepolicy_and_mapping.sha256"
secilc -m -M true -G -c 30 -N -o "$DST/precompiled_sepolicy" -f /dev/null \
    "$TMP/plat_sepolicy.cil" "$TMP/$VER.cil" "$DST/plat_pub_versioned.cil" "$DST/vendor_sepolicy.cil"
echo "wrote $DST"
