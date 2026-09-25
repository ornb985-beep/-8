#!/usr/bin/env bash
# Build out/vendor_minimal.img: a 32 MiB ext4 vendor partition holding
# etc/fstab.apex, the precompiled SELinux policy for the pinned GSI
# (etc/selinux, see scripts/gen-vendor-sepolicy.sh), build.prop and an init
# marker (guest/vendor-minimal/).
# Needs mke2fs with -d (Linux e2fsprogs; macOS: brew install e2fsprogs).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out"; mkdir -p "$OUT"
MKE2FS="$(command -v mke2fs || true)"
[[ -z "$MKE2FS" && -x /opt/homebrew/opt/e2fsprogs/sbin/mke2fs ]] && MKE2FS=/opt/homebrew/opt/e2fsprogs/sbin/mke2fs
[[ -n "$MKE2FS" ]] || { echo "mke2fs not found (brew install e2fsprogs)" >&2; exit 1; }
TMP="$(mktemp -d)"
cp -R "$ROOT/guest/vendor-minimal/." "$TMP/"
find "$TMP" -name .keep -delete
rm -f "$OUT/vendor_minimal.img"
# Fixed UUID/timestamps keep the image reproducible.
E2FSPROGS_FAKE_TIME=1656633600 "$MKE2FS" -q -t ext4 -b 4096 -L vendor -U 5a5a5a5a-0000-4000-8000-617065780001 \
    -E root_owner=0:0,hash_seed=5a5a5a5a-0000-4000-8000-617065780002 -O ^metadata_csum,^64bit \
    -d "$TMP" "$OUT/vendor_minimal.img" 32M
rm -rf "$TMP"
echo "wrote $OUT/vendor_minimal.img ($(du -h "$OUT/vendor_minimal.img" | cut -f1) used of 32M)"
