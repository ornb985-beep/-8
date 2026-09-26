#!/usr/bin/env bash
# Build Mesa for Android's bionic with the NDK: hardware-forwarding drivers
# only (no software rasterizer, no SwiftShader, no LLVM):
#   gallium virgl  - GLES over virtio-gpu 3D (host: virglrenderer)
#   gallium zink   - GLES on top of Vulkan (here: on venus)
#   vulkan  virtio - venus, Vulkan over virtio-gpu blob + context-init
# libdrm is linked statically, so the result does not depend on the
# vendor's (older) libdrm.
#
# Output (out/mesa/vendor/...): lib64/egl/lib{EGL,GLESv2,GLESv1_CM}_mesa.so,
# lib64/libgallium_dri.so, lib64/hw/vulkan.virtio.so, plus build.log and
# SHA256SUMS. Gate: 64-bit AArch64 only, NEEDED limited to Android platform
# libraries (no glibc, X11, wayland).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out/mesa"; WORK="${WORK:-$ROOT/work/mesa}"; mkdir -p "$OUT" "$WORK"
NDK_VER=r26d
NDK_SHA1=fcdad75a765a46a9cf6560353f480db251d14765
MESA_TAG=mesa-26.2.3
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
TC="$WORK/android-ndk-$NDK_VER/toolchains/llvm/prebuilt/linux-x86_64/bin"
command -v meson >/dev/null || pip install -q meson mako pyyaml packaging
python3 -c 'import mako, yaml' 2>/dev/null || pip install -q mako pyyaml packaging

cat >android-aarch64.ini <<EOF
[binaries]
ar = '$TC/llvm-ar'
c = ['$TC/aarch64-linux-android$API-clang']
cpp = ['$TC/aarch64-linux-android$API-clang++', '-fno-exceptions', '-fno-unwind-tables', '-fno-asynchronous-unwind-tables', '-static-libstdc++']
c_ld = 'lld'
cpp_ld = 'lld'
strip = '$TC/llvm-strip'
pkg-config = ['env', 'PKG_CONFIG_LIBDIR=$WORK/sysroot/lib/pkgconfig', '/usr/bin/pkg-config']

[host_machine]
system = 'android'
cpu_family = 'aarch64'
cpu = 'armv8'
endian = 'little'
EOF

{
say "libdrm $LIBDRM_TAG (static)"
[[ -d libdrm ]] || git clone -q -c advice.detachedHead=false --depth 1 --branch "$LIBDRM_TAG" https://gitlab.freedesktop.org/mesa/drm.git libdrm
rm -rf libdrm/build-android
meson setup libdrm/build-android libdrm --cross-file android-aarch64.ini --prefix="$WORK/sysroot" --libdir=lib \
    -Ddefault_library=static -Dintel=disabled -Dradeon=disabled -Damdgpu=disabled -Dnouveau=disabled \
    -Dvmwgfx=disabled -Dfreedreno=disabled -Dvc4=disabled -Detnaviv=disabled -Dexynos=disabled -Domap=disabled \
    -Dtegra=disabled -Dman-pages=disabled -Dvalgrind=disabled -Dcairo-tests=disabled -Dtests=false \
    -Dinstall-test-programs=false
ninja -C libdrm/build-android install

say "Mesa $MESA_TAG"
[[ -d mesa ]] || git clone -q -c advice.detachedHead=false --depth 1 --branch "$MESA_TAG" https://gitlab.freedesktop.org/mesa/mesa.git mesa
rm -rf mesa/build-android mesa-install
meson setup mesa/build-android mesa --cross-file android-aarch64.ini --prefix=/vendor --libdir=lib64 \
    --buildtype=release -Db_ndebug=true \
    -Dplatforms=android -Dplatform-sdk-version=$API -Dandroid-stub=true -Dandroid-libbacktrace=disabled \
    -Dgallium-drivers=virgl,zink -Dvulkan-drivers=virtio -Dllvm=disabled -Dglx=disabled -Dgbm=disabled \
    -Degl=enabled -Dgles1=enabled -Dgles2=enabled -Dvalgrind=disabled -Dlibunwind=disabled -Dzstd=disabled \
    -Dexpat=disabled -Dbuild-tests=false -Dvideo-codecs=[] -Dgallium-va=disabled -Dlmsensors=disabled
ninja -C mesa/build-android
DESTDIR="$WORK/mesa-install" ninja -C mesa/build-android install
} 2>&1 | tee "$OUT/build.log"

say "packaging for /vendor"
I="$WORK/mesa-install/vendor/lib64"; V="$OUT/vendor/lib64"
rm -rf "$OUT/vendor"; mkdir -p "$V/egl" "$V/hw"
cp "$I/libEGL.so" "$V/egl/libEGL_mesa.so"
cp "$I/libGLESv2.so" "$V/egl/libGLESv2_mesa.so"
cp "$I/libGLESv1_CM.so" "$V/egl/libGLESv1_CM_mesa.so"
cp "$I/libgallium_dri.so" "$V/libgallium_dri.so"
cp "$I/libvulkan_virtio.so" "$V/hw/vulkan.virtio.so"
"$TC/llvm-strip" --strip-unneeded "$V"/egl/*.so "$V"/*.so "$V"/hw/*.so

say "gate"
ALLOWED=" libc.so libm.so libdl.so liblog.so libsync.so libnativewindow.so libhardware.so libz.so libgallium_dri.so "
fail=0
while IFS= read -r f; do
    rel="${f#"$OUT"/}"
    file "$f" | grep -q "ELF 64-bit LSB shared object, ARM aarch64" || { echo "not AArch64 64-bit: $rel"; fail=1; }
    needed="$("$TC/llvm-readelf" -d "$f" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')"
    for n in $needed; do [[ "$ALLOWED" == *" $n "* ]] || { echo "$rel: disallowed NEEDED $n"; fail=1; }; done
    printf '%-38s %9s  NEEDED: %s\n' "$rel" "$(stat -c %s "$f")" "$needed"
done < <(find "$OUT/vendor" -name '*.so' | sort)
"$TC/llvm-nm" -D --defined-only "$V/hw/vulkan.virtio.so" | grep -q ' HMI$' || { echo "vulkan.virtio.so: no HMI (Android HAL module)"; fail=1; }
# SwiftShader is not built; the only textual hit allowed is Vulkan's
# VK_DRIVER_ID_GOOGLE_SWIFTSHADER enum name (driver-ID string table).
if find "$OUT/vendor" -iname '*swiftshader*' | grep -q .; then echo "SwiftShader file present"; fail=1; fi
[[ "$fail" == 0 ]] || exit 1
( cd "$OUT" && find vendor -name '*.so' | sort | xargs sha256sum >SHA256SUMS && cat SHA256SUMS )
