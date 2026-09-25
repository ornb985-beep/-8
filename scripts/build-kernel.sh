#!/usr/bin/env bash
# Build the Android 12 GKI kernel (android12-5.10) for the Apex VMM.
#   scripts/build-kernel.sh [kernel-src-dir]
# Output: out/Image.gz, out/kernel.config, out/kernel.sha256
# Works on Linux (CI) and macOS (brew install llvm lld make flex bison).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-$ROOT/build/kernel-android12}"
BRANCH="${APEX_KERNEL_BRANCH:-android12-5.10}"
OUT="$ROOT/out"
KOUT="$ROOT/build/kernel-obj"
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN)}"

if [[ ! -d "$SRC/.git" ]]; then
    git clone --depth 1 --branch "$BRANCH" https://android.googlesource.com/kernel/common "$SRC"
fi
# 5.10 derives the clang --target from CROSS_COMPILE.
MAKE=(make -C "$SRC" O="$KOUT" ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- LLVM=1 LLVM_IAS=1 -j"$JOBS")
mkdir -p "$KOUT" "$OUT"
"${MAKE[@]}" gki_defconfig
"$SRC/scripts/kconfig/merge_config.sh" -m -O "$KOUT" "$KOUT/.config" "$ROOT/guest/kernel/android12-5.10.fragment"
"${MAKE[@]}" olddefconfig

# Every requested option must have survived Kconfig as built-in.
for opt in ARM64_4K_PAGES VIRTIO_MMIO VIRTIO_BLK VIRTIO_CONSOLE DRM_VIRTIO_GPU VIRTIO_INPUT ANDROID_BINDER_IPC ASHMEM SERIAL_AMBA_PL011_CONSOLE BLK_DEV_INITRD; do
    grep -q "^CONFIG_${opt}=y" "$KOUT/.config" || { echo "error: CONFIG_${opt} is not =y" >&2; exit 1; }
done

"${MAKE[@]}" Image
gzip -9 -n -c "$KOUT/arch/arm64/boot/Image" > "$OUT/Image.gz"
cp "$KOUT/.config" "$OUT/kernel.config"
( cd "$OUT" && shasum -a 256 Image.gz > kernel.sha256 2>/dev/null || sha256sum Image.gz > kernel.sha256 )
echo "kernel $(make -s -C "$SRC" kernelversion) -> $OUT/Image.gz ($(wc -c < "$OUT/Image.gz") bytes)"
