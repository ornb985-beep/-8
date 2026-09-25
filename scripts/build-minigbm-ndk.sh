#!/usr/bin/env bash
# Build minigbm (upstream ChromiumOS) for Android's bionic with the NDK —
# pure 64-bit AArch64, virtio-gpu backend (virtgpu: virgl / cross-domain /
# 2D), no goldfish anything — plus apex_gbm_probe, a guest test that
# allocates, maps, verifies and dma-buf-exports a 1080x2400 scanout buffer.
#
# This is the gbm/driver core only. The Android gralloc HAL on top of it
# (cros_gralloc/gralloc4: allocator@4.0 service + mapper@4.0) includes
# HIDL-generated headers and links platform C++ libraries; it cannot be built
# from the NDK alone (see docs/ANDROID12.md, "DRM graphics stack").
#
# Output (out/minigbm/): libminigbm.so, apex_gbm_probe, apex_gbm_probe.rc,
# build.log, SHA256SUMS. Needs curl, unzip, git, debugfs, and the vendor
# image from the apex-android12-vendor release (for its libdrm.so) or
# VENDOR_IMG=<path>.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out/minigbm"; WORK="${WORK:-$ROOT/work/minigbm}"; mkdir -p "$OUT" "$WORK"
NDK_VER=r26d
NDK_SHA1=fcdad75a765a46a9cf6560353f480db251d14765
MINIGBM_REV=96fcdc4be671d488c6e96363d7bbbaea13f38e8a
LIBDRM_TAG=libdrm-2.4.123
API=32

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
cd "$WORK"

if [[ ! -d android-ndk-$NDK_VER ]]; then
    say "NDK $NDK_VER"
    curl -fsSL -o ndk.zip "https://dl.google.com/android/repository/android-ndk-$NDK_VER-linux.zip"
    [[ "$(sha1sum ndk.zip | cut -d' ' -f1)" == "$NDK_SHA1" ]] || { echo "NDK checksum mismatch" >&2; exit 1; }
    unzip -q ndk.zip && rm ndk.zip
fi
BIN="$WORK/android-ndk-$NDK_VER/toolchains/llvm/prebuilt/linux-x86_64/bin"
CC="$BIN/aarch64-linux-android$API-clang"

if [[ ! -d minigbm ]]; then
    say "minigbm $MINIGBM_REV"
    git init -q minigbm
    git -C minigbm fetch -q --depth 1 https://chromium.googlesource.com/chromiumos/platform/minigbm "$MINIGBM_REV"
    git -C minigbm checkout -q FETCH_HEAD
fi
[[ -d libdrm ]] || git clone -q -c advice.detachedHead=false --depth 1 --branch "$LIBDRM_TAG" https://gitlab.freedesktop.org/mesa/drm.git libdrm

# Link against the vendor's own libdrm.so (the one loaded at run time).
if [[ ! -f libdrm.so ]]; then
    VIMG="${VENDOR_IMG:-}"
    if [[ -z "$VIMG" ]]; then
        curl -fsSL -o vendor.img.gz https://github.com/ornb985-beep/-8/releases/download/apex-android12-vendor/vendor.img.gz
        gunzip -f vendor.img.gz; VIMG="$WORK/vendor.img"
    fi
    debugfs -R "dump /lib64/libdrm.so $WORK/libdrm.so" "$VIMG" 2>/dev/null
    [[ -s libdrm.so ]] || { echo "no /lib64/libdrm.so in $VIMG" >&2; exit 1; }
fi

say "compiling (aarch64-linux-android$API, bionic)"
rm -rf obj && mkdir obj
{
    "$CC" --version | head -1
    for f in minigbm/*.c; do
        s="$(basename "$f" .c)"
        echo "CC $s.c"
        "$CC" -c -O2 -fPIC -std=gnu11 -Wall -Wno-implicit-fallthrough -Wno-unreachable-code \
            -D_GNU_SOURCE=1 -D_FILE_OFFSET_BITS=64 -I"$ROOT/guest/minigbm-ndk/shim" -Iminigbm \
            -Ilibdrm -Ilibdrm/include/drm -o "obj/$s.o" "$f"
    done
    echo "LD libminigbm.so"
    "$CC" -shared -o "$OUT/libminigbm.so" -Wl,-soname,libminigbm.so -Wl,--no-undefined -Wl,--build-id=sha1 \
        obj/*.o libdrm.so -llog
    echo "CC apex_gbm_probe"
    "$CC" -O2 -Wall -Wextra -Werror -Iminigbm -o "$OUT/apex_gbm_probe" \
        "$ROOT/guest/minigbm-ndk/gbm_probe.c" "$OUT/libminigbm.so"
} 2>&1 | tee "$OUT/build.log"
"$BIN/llvm-strip" --strip-unneeded "$OUT/libminigbm.so" "$OUT/apex_gbm_probe"
cp "$ROOT/guest/minigbm-ndk/apex_gbm_probe.rc" "$OUT/"

say "checks"
for f in libminigbm.so apex_gbm_probe; do
    file "$OUT/$f" | grep -q "ELF 64-bit LSB .*ARM aarch64" || { echo "$f is not AArch64 64-bit" >&2; exit 1; }
    NEEDED="$("$BIN/llvm-readelf" -d "$OUT/$f" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')"
    echo "$f: $(file -b "$OUT/$f" | cut -d, -f1-2); NEEDED: $NEEDED"
    if echo "$NEEDED" | grep -qi goldfish; then echo "$f links goldfish" >&2; exit 1; fi
done
echo "exported gbm_* symbols: $("$BIN/llvm-nm" -D --defined-only "$OUT/libminigbm.so" | grep -c ' T gbm_')"
( cd "$OUT" && sha256sum libminigbm.so apex_gbm_probe >SHA256SUMS && cat SHA256SUMS )
