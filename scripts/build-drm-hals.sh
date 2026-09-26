#!/usr/bin/env bash
# Build the Android 12L graphics HALs for the virtio-gpu DRM stack from AOSP
# sources (android-12.1.0_r27), without an AOSP tree or Soong:
#
#   bin/hw/android.hardware.graphics.allocator@2.0-service   passthrough allocator
#   lib64/hw/android.hardware.graphics.allocator@2.0-impl.so   -> hw_get_module("gralloc")
#   lib64/hw/android.hardware.graphics.mapper@2.0-impl-2.1.so  in-process mapper (sphal)
#   lib64/hw/gralloc.minigbm.so                                minigbm gralloc0 HAL
#   bin/hw/android.hardware.graphics.composer@2.1-service      composer (HWC2 passthrough)
#   lib64/hw/hwcomposer.drm_minigbm.so                         drm_hwcomposer, KMS /dev/dri/card0
#   lib64/libhwc2on1adapter.so, lib64/libhwc2onfbadapter.so    composer service deps
#
# How: hidl-gen and aidl are built for the host from system/tools/{hidl,aidl}
# and generate the HIDL/AIDL C++ headers; the device code is compiled with
# NDK clang against AOSP's libc++ headers (std::__1, -fno-rtti
# -fno-exceptions, like Soong) and linked with --no-undefined against the
# platform libraries of the target system image (VNDK/LLNDK), so the ABI is
# the platform's, not the NDK's.
#
# Inputs: SYSTEM_IMG (ext4, with the VNDK APEX) and VENDOR_IMG (for
# composer@2.1-resources / libdrm) of the pure 64-bit 12L build; defaults to
# the apex-android12-vendor release. Output: out/drm-hals/vendor/...
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out/drm-hals"; WORK="${WORK:-$ROOT/work/drm-hals}"; mkdir -p "$OUT" "$WORK"
TAG=android-12.1.0_r27
NDK_VER=r26d
NDK_SHA1=fcdad75a765a46a9cf6560353f480db251d14765
LIBDRM_TAG=libdrm-2.4.123
REL=https://github.com/ornb985-beep/-8/releases/download/apex-android12-vendor
say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
cd "$WORK"
A="$WORK/aosp"; mkdir -p "$A"

# ---------------------------------------------------------------- sources
say "AOSP $TAG sources (shallow)"
for p in external/minigbm external/drm_hwcomposer external/libcxx external/libcxxabi external/fmtlib \
         hardware/libhardware hardware/interfaces frameworks/native system/core system/logging \
         system/libhidl system/libbase system/libfmq system/libhwbinder system/tools/hidl system/tools/aidl; do
    [[ -d "$A/$p" ]] || git clone -q -c advice.detachedHead=false --depth 1 --branch "$TAG" \
        "https://android.googlesource.com/platform/$p" "$A/$p"
done
[[ -d libdrm ]] || git clone -q -c advice.detachedHead=false --depth 1 --branch "$LIBDRM_TAG" https://gitlab.freedesktop.org/mesa/drm.git libdrm
if [[ ! -d android-ndk-$NDK_VER ]]; then
    curl -fsSL -o ndk.zip "https://dl.google.com/android/repository/android-ndk-$NDK_VER-linux.zip"
    [[ "$(sha1sum ndk.zip | cut -d' ' -f1)" == "$NDK_SHA1" ]] || { echo "NDK checksum mismatch" >&2; exit 1; }
    unzip -q ndk.zip && rm ndk.zip
fi
NDK="$WORK/android-ndk-$NDK_VER/toolchains/llvm/prebuilt/linux-x86_64"

# ---------------------------------------------------------------- platform libraries to link against
say "platform libraries (link-time) from the target system/vendor images"
if [[ ! -f plat/.done ]]; then
    SYSTEM_IMG="${SYSTEM_IMG:-}"; VENDOR_IMG="${VENDOR_IMG:-}"
    if [[ -z "$SYSTEM_IMG" ]]; then curl -fsSL "$REL/system.img.gz" | gunzip >system.img; SYSTEM_IMG="$WORK/system.img"; fi
    if [[ -z "$VENDOR_IMG" ]]; then curl -fsSL "$REL/vendor.img.gz" | gunzip >vendor.img; VENDOR_IMG="$WORK/vendor.img"; fi
    rm -rf plat vndk && mkdir -p plat/lib64 vndk
    debugfs -R "dump /system/apex/com.android.vndk.current.apex vndk/vndk.apex" "$SYSTEM_IMG" 2>/dev/null
    unzip -p vndk/vndk.apex apex_payload.img >vndk/payload.img
    debugfs -R "rdump /lib64 vndk" vndk/payload.img >/dev/null 2>&1
    for l in libc++ libcutils libutils libhidlbase libhardware libbase libsync libnativewindow liblog libui \
             libbinder libfmq android.hardware.graphics.allocator@2.0 android.hardware.graphics.mapper@2.0 \
             android.hardware.graphics.mapper@2.1 android.hardware.graphics.common@1.0 \
             android.hardware.graphics.common@1.1 android.hardware.graphics.common@1.2 \
             android.hardware.graphics.composer@2.1; do
        if [[ -f "vndk/lib64/$l.so" ]]; then cp "vndk/lib64/$l.so" plat/lib64/
        else debugfs -R "dump /system/lib64/$l.so plat/lib64/$l.so" "$SYSTEM_IMG" 2>/dev/null; fi
        [[ -s "plat/lib64/$l.so" ]] || { echo "missing platform library $l.so" >&2; exit 1; }
    done
    for l in libdrm android.hardware.graphics.composer@2.1-resources; do
        debugfs -R "dump /lib64/$l.so plat/lib64/$l.so" "$VENDOR_IMG" 2>/dev/null
        [[ -s "plat/lib64/$l.so" ]] || { echo "missing vendor library $l.so" >&2; exit 1; }
    done
    touch plat/.done
fi

# ---------------------------------------------------------------- host tools: hidl-gen, aidl
HOSTLOG="$A/liblog_host_stub.cpp"
cat >"$HOSTLOG" <<'EOF'
// Minimal host liblog for libbase's logging.cpp (code generators only).
#include <android/log.h>
#include <cstdio>
#include <cstdlib>
extern "C" {
static __android_logger_function g_logger;
static __android_aborter_function g_aborter;
static int g_min = ANDROID_LOG_INFO;
void __android_log_set_logger(__android_logger_function f) { g_logger = f; }
void __android_log_logd_logger(const struct __android_log_message* m) { fprintf(stderr, "%s: %s\n", m->tag ? m->tag : "", m->message); }
void __android_log_stderr_logger(const struct __android_log_message* m) { __android_log_logd_logger(m); }
void __android_log_write_log_message(struct __android_log_message* m) { if (g_logger) g_logger(m); else __android_log_logd_logger(m); }
void __android_log_set_aborter(__android_aborter_function f) { g_aborter = f; }
void __android_log_call_aborter(const char* msg) { if (g_aborter) g_aborter(msg); abort(); }
void __android_log_default_aborter(const char*) { abort(); }
int __android_log_is_loggable(int prio, const char*, int def) { return prio >= (g_min ? g_min : def); }
int32_t __android_log_set_minimum_priority(int32_t p) { int32_t o = g_min; g_min = p; return o; }
int32_t __android_log_get_minimum_priority() { return g_min; }
void __android_log_set_default_tag(const char*) {}
}
EOF
BASE_SRCS="logging strings stringprintf file threads errors_unix parsebool"
if [[ ! -x host/hidl-gen ]]; then
    say "host hidl-gen"
    rm -rf host/hidl && mkdir -p host/hidl && pushd host/hidl >/dev/null
    H="$A/system/tools/hidl"
    bison -Wno-other --defines=hidl-gen_y.h -o hidl-gen_y.cpp "$H/hidl-gen_y.yy"
    flex -o hidl-gen_l.cpp "$H/hidl-gen_l.ll"
    sed -i 's/^using token = yy::parser::token;/using token = yy::parser::token;\ntypedef yy::parser::value_type YYSTYPE;\ntypedef yy::parser::location_type YYLTYPE;/' hidl-gen_l.cpp
    INC="-include cstdint -include cstring -I. -I$H -I$H/utils/include -I$H/utils/include/hidl-util -I$H/host_utils/include
         -I$H/host_utils/include/hidl-util -I$H/hashing/include -I$A/system/libbase/include -I$A/system/logging/liblog/include
         -I$A/system/libhwbinder/include -I$A/system/core/libutils/include -I$A/system/core/libcutils/include -I/usr/include/jsoncpp"
    for s in $(ls "$H"/*.cpp) "$H"/utils/FQName.cpp "$H"/utils/FqInstance.cpp "$H"/host_utils/Formatter.cpp \
             "$H"/host_utils/StringHelper.cpp "$H"/hashing/Hash.cpp hidl-gen_y.cpp hidl-gen_l.cpp "$HOSTLOG"; do
        clang++ -std=gnu++17 -O1 -w -c $INC -o "h_$(basename "${s%.*}").o" "$s"
    done
    for s in $BASE_SRCS; do clang++ -std=gnu++17 -O1 -w -c $INC -o "b_$s.o" "$A/system/libbase/$s.cpp"; done
    clang++ -o ../hidl-gen ./*.o -lcrypto -ljsoncpp
    popd >/dev/null
fi
if [[ ! -x host/aidl ]]; then
    say "host aidl"
    rm -rf host/aidl.d && mkdir -p host/aidl.d && pushd host/aidl.d >/dev/null
    X="$A/system/tools/aidl"
    bison -Wno-other --defines=aidl_language_y.h -o aidl_language_y.cpp "$X/aidl_language_y.yy"
    flex -o aidl_language_l.cpp "$X/aidl_language_l.ll"
    sed -i '0,/#include "aidl_language_y.h"/s//#include "aidl_language_y.h"\ntypedef yy::parser::value_type YYSTYPE;\ntypedef yy::parser::location_type YYLTYPE;/' aidl_language_l.cpp
    INC="-include cstdint -include cstring -include memory -DFMT_HEADER_ONLY -I. -I$X -I$A/external/fmtlib/include
         -I$A/system/libbase/include -I$A/system/logging/liblog/include -I$A/system/core/libcutils/include -I$A/system/core/libutils/include"
    for s in $(sed -n '/name: "libaidl-common"/,/^}/p' "$X/Android.bp" | grep -oE '"[a-z_]+\.cpp"' | tr -d '"'); do
        clang++ -std=gnu++20 -O1 -w -c $INC -o "a_${s%.cpp}.o" "$X/$s"
    done
    for s in "$X/main.cpp" aidl_language_y.cpp aidl_language_l.cpp; do clang++ -std=gnu++20 -O1 -w -c $INC -o "m_$(basename "${s%.*}").o" "$s"; done
    for s in $BASE_SRCS; do clang++ -std=gnu++20 -O1 -w -c $INC -o "b_$s.o" "$A/system/libbase/$s.cpp"; done
    clang++ -std=gnu++20 -c -I"$A/system/logging/liblog/include" -o logstub.o "$HOSTLOG"
    clang++ -o ../aidl ./*.o -lgtest
    popd >/dev/null
fi

# ---------------------------------------------------------------- generated headers
say "HIDL / AIDL headers"
rm -rf gen && mkdir -p gen/hidl gen/aidl
for fq in android.hidl.base@1.0 android.hidl.manager@1.0 android.hidl.manager@1.1 \
          android.hardware.graphics.common@1.0 android.hardware.graphics.common@1.1 android.hardware.graphics.common@1.2 \
          android.hardware.graphics.allocator@2.0 android.hardware.graphics.allocator@3.0 android.hardware.graphics.allocator@4.0 \
          android.hardware.graphics.mapper@2.0 android.hardware.graphics.mapper@2.1 android.hardware.graphics.mapper@3.0 \
          android.hardware.graphics.mapper@4.0 android.hardware.graphics.bufferqueue@1.0 android.hardware.graphics.bufferqueue@2.0 \
          android.hidl.token@1.0 android.hardware.media@1.0 android.hardware.graphics.composer@2.1; do
    host/hidl-gen -o gen/hidl -L c++-headers -p "$A" -r "android.hardware:$A/hardware/interfaces" \
        -r "android.hidl:$A/system/libhidl/transport" "$fq"
done
GC="$A/hardware/interfaces/graphics/common/aidl/aidl_api/android.hardware.graphics.common/2"
HC="$A/hardware/interfaces/common/aidl/aidl_api/android.hardware.common/2"
for f in Cta861_3 ExtendableType PlaneLayout PlaneLayoutComponent Rect Smpte2086 XyColor; do
    host/aidl --lang=ndk --structured --stability=vintf --version=2 -I "$GC" -I "$HC" -h gen/aidl -o gen/aidl-src \
        "$GC/android/hardware/graphics/common/$f.aidl"
done
host/aidl --lang=ndk --structured --stability=vintf --version=2 -I "$HC" -h gen/aidl -o gen/aidl-src "$HC/android/hardware/common/NativeHandle.aidl"
host/aidl --lang=ndk --structured --stability=vintf --version=2 -I "$GC" -I "$HC" -h gen/aidl -o gen/aidl-src \
    "$GC/android/hardware/graphics/common/HardwareBuffer.aidl"
# Enums: the host aidl's enum path crashes with this bison; same NDK layout from a small generator.
python3 "$ROOT/tools/aidl_ndk_enum.py" gen/aidl $(find "$GC" -name '*.aidl')

# ---------------------------------------------------------------- device builds
say "device code (platform ABI: AOSP libc++ std::__1, -fno-rtti -fno-exceptions)"
TGT="--target=aarch64-linux-android32 --sysroot=$NDK/sysroot"
PCC="$NDK/bin/clang $TGT -fPIC -O2"
PCXX="$NDK/bin/clang++ $TGT -fPIC -O2 -std=gnu++17 -fno-rtti -fno-exceptions -nostdinc++ -isystem $A/external/libcxx/include -isystem $A/external/libcxxabi/include"
PLD="-nostdlib++ -L$WORK/plat/lib64 -L$WORK/obj -Wl,--no-undefined -Wl,-rpath-link,$WORK/plat/lib64 -Wl,--build-id=sha1"
G="$A/hardware/interfaces/graphics"; C="$G/composer/2.1/utils"; D="$A/external/drm_hwcomposer"; M="$A/external/minigbm"
BN="$A/frameworks/native/libs/binder/ndk"
INC="-I$A/hardware/libhardware/include -I$A/frameworks/native/libs/nativebase/include -I$A/frameworks/native/libs/nativewindow/include
     -I$A/frameworks/native/libs/arect/include -I$A/system/core/libsystem/include -I$A/system/core/libcutils/include
     -I$A/system/logging/liblog/include -I$A/system/core/libsync/include -I$A/system/core/libutils/include
     -I$WORK/libdrm -I$WORK/libdrm/include/drm"
HINC="-Igen/hidl -I$A/system/libhidl/base/include -I$A/system/libhidl/transport/include -I$A/system/libhwbinder/include
      -I$A/system/libbase/include -I$A/system/libfmq/include -I$A/system/libfmq/base
      -I$G/allocator/2.0/utils/hal/include -I$G/allocator/2.0/utils/passthrough/include -I$G/mapper/2.0/utils/hal/include
      -I$G/mapper/2.0/utils/passthrough/include -I$G/mapper/2.1/utils/hal/include -I$G/mapper/2.1/utils/passthrough/include $INC"
DINC="-I$D -I$D/include -I$M/cros_gralloc -I$A/frameworks/native/libs/ui/include_vndk -I$A/frameworks/native/libs/ui/include
      -I$A/frameworks/native/libs/math/include -I$A/frameworks/native/libs/gralloc/types/include -Igen/aidl
      -I$BN/include_cpp -I$BN/include_ndk -I$BN/include_platform -I$A/frameworks/native/include $HINC"
CINC="-I$C/hal/include -I$C/passthrough/include -I$C/resources/include -I$C/command-buffer/include -I$C/hwc2on1adapter/include
      -I$C/hwc2onfbadapter/include -I$A/frameworks/native/libs/binder/include $DINC"
V="$OUT/vendor"; rm -rf "$V" obj; mkdir -p "$V/bin/hw" "$V/lib64/hw" obj/g obj/h

{
set -x
# minigbm gralloc0 HAL
for s in amdgpu drv drv_array_helpers drv_helpers dumb_driver i915 mediatek msm rockchip vc4 virtgpu virtgpu_cross_domain virtgpu_virgl; do
    $PCC -c -D_GNU_SOURCE=1 -D_FILE_OFFSET_BITS=64 -DANDROID_API_LEVEL=32 -Wall -Wno-unused-parameter $INC -I$M -o obj/g/$s.o $M/$s.c
done
for s in cros_gralloc/cros_gralloc_buffer cros_gralloc/cros_gralloc_helpers cros_gralloc/cros_gralloc_driver cros_gralloc/gralloc0/gralloc0; do
    $PCXX -c -D_GNU_SOURCE=1 -D_FILE_OFFSET_BITS=64 -DANDROID_API_LEVEL=32 -Wall -Wno-unused-parameter $INC -I$M -o "obj/g/$(basename $s).o" $M/$s.cc
done
$PCXX -shared -o "$V/lib64/hw/gralloc.minigbm.so" -Wl,-soname,gralloc.minigbm.so obj/g/*.o $PLD -lc++ -lcutils -ldrm -lnativewindow -lsync -llog
# passthrough allocator / mapper
$PCXX -Wall '-DLOG_TAG="AllocatorHal"' $HINC -shared -o "$V/lib64/hw/android.hardware.graphics.allocator@2.0-impl.so" \
    -Wl,-soname,android.hardware.graphics.allocator@2.0-impl.so $G/allocator/2.0/default/passthrough.cpp \
    $PLD -lc++ -l:android.hardware.graphics.allocator@2.0.so -lbase -lcutils -lhardware -lhidlbase -llog -lutils
$PCXX -Wall '-DLOG_TAG="MapperHal"' $HINC -shared -o "$V/lib64/hw/android.hardware.graphics.mapper@2.0-impl-2.1.so" \
    -Wl,-soname,android.hardware.graphics.mapper@2.0-impl-2.1.so $G/mapper/2.1/default/passthrough.cpp \
    $PLD -lc++ -l:android.hardware.graphics.mapper@2.0.so -l:android.hardware.graphics.mapper@2.1.so -lbase -lcutils -lhardware -lhidlbase -llog -lsync -lutils
$PCXX -Wall $HINC -pie -o "$V/bin/hw/android.hardware.graphics.allocator@2.0-service" $G/allocator/2.0/default/service.cpp \
    $PLD -lc++ -l:android.hardware.graphics.allocator@2.0.so -lhidlbase -llog -lutils
# drm_hwcomposer (minigbm buffer info)
for s in utils/Worker DrmHwcTwo bufferinfo/BufferInfoGetter bufferinfo/BufferInfoMapperMetadata compositor/DrmDisplayComposition \
         compositor/DrmDisplayCompositor compositor/Planner drm/DrmConnector drm/DrmCrtc drm/DrmDevice drm/DrmEncoder \
         drm/DrmEventListener drm/DrmFbImporter drm/DrmMode drm/DrmPlane drm/DrmProperty drm/ResourceManager drm/VSyncWorker \
         utils/autolock utils/hwcutils backend/Backend backend/BackendClient backend/BackendManager backend/BackendRCarDu \
         bufferinfo/legacy/BufferInfoMinigbm; do
    $PCXX -c -Wall -DHWC2_INCLUDE_STRINGIFICATION -DHWC2_USE_CPP11 -DPLATFORM_SDK_VERSION=32 $DINC -o "obj/h/$(echo $s | tr / _).o" $D/$s.cpp
done
$PCXX -shared -o "$V/lib64/hw/hwcomposer.drm_minigbm.so" -Wl,-soname,hwcomposer.drm_minigbm.so obj/h/*.o \
    $PLD -lc++ -lcutils -ldrm -lhardware -lhidlbase -llog -lsync -lui -lutils
# composer 2.1 service + adapters
$PCXX -Wall '-DLOG_TAG="HWC2On1Adapter"' $CINC -shared -o obj/libhwc2on1adapter.so -Wl,-soname,libhwc2on1adapter.so \
    $C/hwc2on1adapter/HWC2On1Adapter.cpp $C/hwc2on1adapter/MiniFence.cpp $PLD -lc++ -lcutils -lhardware -llog -lsync -lutils
$PCXX -Wall '-DLOG_TAG="HWC2OnFb"' $CINC -shared -o obj/libhwc2onfbadapter.so -Wl,-soname,libhwc2onfbadapter.so \
    $C/hwc2onfbadapter/HWC2OnFbAdapter.cpp $PLD -lc++ -lcutils -lhardware -llog -lsync -lutils
$PCXX -Wall $CINC -pie -o "$V/bin/hw/android.hardware.graphics.composer@2.1-service" $G/composer/2.1/default/service.cpp \
    $PLD -lc++ -l:android.hardware.graphics.composer@2.1.so -l:android.hardware.graphics.composer@2.1-resources.so -lbase -lbinder \
    -lcutils -lfmq -lhardware -lhidlbase -lhwc2on1adapter -lhwc2onfbadapter -llog -lsync -lutils
cp obj/libhwc2on1adapter.so obj/libhwc2onfbadapter.so "$V/lib64/"
set +x
} 2>&1 | tee "$OUT/build.log" | grep -E "error|warning: " || true
"$NDK/bin/llvm-strip" --strip-unneeded "$V"/bin/hw/* "$V"/lib64/*.so "$V"/lib64/hw/*.so

say "result"
( cd "$OUT" && find vendor -type f | sort | while read -r f; do
    printf '%-60s %s\n' "$f" "$("$NDK/bin/llvm-readelf" -d "$f" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')"
  done
  find vendor -type f | sort | xargs sha256sum >SHA256SUMS )
for f in "$V/lib64/hw/gralloc.minigbm.so" "$V/lib64/hw/hwcomposer.drm_minigbm.so"; do
    syms="$("$NDK/bin/llvm-nm" -D --defined-only "$f")"
    grep -qw HMI <<<"$syms" || { echo "$f: no HMI" >&2; exit 1; }
done
