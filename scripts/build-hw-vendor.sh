#!/usr/bin/env bash
# Assemble vendor_mumu_hw120_pure64.img: the pure 64-bit Android 12L vendor
# (apex-android12-vendor) with its goldfish/ranchu graphics and emulator HALs
# removed and the virtio-gpu DRM stack put in:
#   gralloc  : allocator@2.0-service -> allocator@2.0-impl -> gralloc.minigbm.so,
#              mapper@2.0-impl-2.1 (scripts/build-drm-hals.sh)
#   composer : composer@2.1-service -> hwcomposer.drm_minigbm.so (KMS /dev/dri/card0)
#   GLES     : Mesa virgl/zink (egl/lib*_mesa.so, libgallium_dri.so)
#   Vulkan   : Mesa venus (hw/vulkan.virtio.so)          (scripts/build-mesa-ndk.sh)
# No SwiftShader, no ANGLE, no software renderer.
#
#   scripts/build-hw-vendor.sh   (after build-drm-hals.sh and build-mesa-ndk.sh)
# Output: out/hw-vendor/vendor_mumu_hw120_pure64.img.gz (+ .sha256), REPORT.txt.
# Linux, root (or sudo) for SELinux xattrs.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out/hw-vendor"; WORK="${WORK:-$ROOT/work/hw-vendor}"; mkdir -p "$OUT" "$WORK"
HALS="$ROOT/out/drm-hals/vendor"; MESA="$ROOT/out/mesa/vendor"
BASE_GZ="${BASE_VENDOR:-$ROOT/out/cloud/vendor.img.gz}"
REL=https://github.com/ornb985-beep/-8/releases/download/apex-android12-vendor
SUDO=""; [[ "$(id -u)" == 0 ]] || SUDO="sudo"
say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
[[ -d "$HALS" && -d "$MESA" ]] || { echo "run scripts/build-drm-hals.sh and scripts/build-mesa-ndk.sh first" >&2; exit 1; }
cd "$WORK"

if [[ ! -f "$BASE_GZ" ]]; then curl -fsSL -o base.img.gz "$REL/vendor.img.gz"; BASE_GZ="$WORK/base.img.gz"; fi
gunzip -c "$BASE_GZ" >base.img
$SUDO rm -rf v && $SUDO python3 "$ROOT/tools/ext4tree.py" extract base.img v

say "pruning goldfish / ranchu / emulator graphics"
$SUDO python3 - v "$ROOT/tools" <<'PY' | tee prune.log
import os, re, subprocess, sys
V = sys.argv[1]
def rel(p): return os.path.relpath(p, V)
PAT = re.compile(r'goldfish|ranchu|emulation|qemu|libOpengl|_enc\.so$|libandroidemu|swiftshader|pastel|_angle\.so$|libfeature_support_angle', re.I)
# The emulator graphics services: goldfish allocator 3.0 and composer 2.4 (EmuHWC2).
EXPLICIT = {'bin/hw/android.hardware.graphics.allocator@3.0-service',
            'bin/hw/android.hardware.graphics.composer@2.4-service',
            'etc/init/android.hardware.graphics.allocator@3.0-service.rc',
            'etc/init/android.hardware.graphics.composer@2.4-service.rc',
            'etc/vintf/manifest/android.hardware.graphics.composer@2.4.xml'}
# The emulator's legacy audio wrapper is AOSP's generic
# android.hardware.audio@7.0-impl (loads audio.primary.default /
# audio.r_submix.default); only its file name carries the emulator suffix.
# Keep it under the standard name; the TinyALSA "ranchu" HAL goes.
legacy = os.path.join(V, 'lib64/hw/android.hardware.audio.legacy@7.0-impl.ranchu.so')
if os.path.exists(legacy):
    os.rename(legacy, os.path.join(V, 'lib64/hw/android.hardware.audio@7.0-impl.so'))
    print('renamed lib64/hw/android.hardware.audio.legacy@7.0-impl.ranchu.so -> android.hardware.audio@7.0-impl.so')
removed = set()
for d, _, fs in os.walk(V):
    for f in fs:
        p = os.path.join(d, f)
        r = rel(p)
        if PAT.search(f) or r in EXPLICIT:
            removed.add(r)
# Dependency closure: drop ELF files whose NEEDED can no longer be resolved
# from /vendor/lib64 or the platform (anything not provided by vendor is
# assumed to come from the system/VNDK, except removed vendor libraries).
def needed(p):
    try:
        out = subprocess.run(['readelf', '-d', p], capture_output=True, text=True).stdout
    except Exception:
        return []
    return re.findall(r'\(NEEDED\).*\[(.+)\]', out)
removed_libs = {os.path.basename(r) for r in removed if r.startswith('lib64/')}
changed = True
while changed:
    changed = False
    for d, _, fs in os.walk(V):
        for f in fs:
            p = os.path.join(d, f); r = rel(p)
            if r in removed or os.path.islink(p):
                continue
            with open(p, 'rb') as fh:
                if fh.read(4) != b'\x7fELF':
                    continue
            bad = [n for n in needed(p) if n in removed_libs]
            if bad:
                removed.add(r); changed = True
                if r.startswith('lib64/'):
                    removed_libs.add(f)
                print(f'dependency: {r} needs removed {bad}')
# HALs that dlopen removed ranchu implementations by name.
for r in ['bin/hw/android.hardware.sensors@2.1-service.multihal',
          'bin/hw/android.hardware.camera.provider@2.4-service_64', 'bin/hw/android.hardware.camera.provider@2.7-service-google',
          'bin/hw/android.hardware.biometrics.fingerprint@2.1-service']:
    if os.path.exists(os.path.join(V, r)):
        removed.add(r)
# init scripts whose service binary is gone, and the HALs they declared.
gone_ifaces = set()
for f in sorted(os.listdir(os.path.join(V, 'etc/init'))):
    p = os.path.join(V, 'etc/init', f)
    if not os.path.isfile(p):
        continue
    txt = open(p, errors='replace').read()
    bins = re.findall(r'^service\s+\S+\s+/vendor/(\S+)', txt, re.M)
    if bins and all(b in removed for b in bins):
        removed.add(rel(p))
        gone_ifaces |= set(re.findall(r'interface\s+([\w.]+)@', txt))
gone_ifaces |= {'android.hardware.graphics.allocator', 'android.hardware.graphics.mapper', 'android.hardware.graphics.composer',
                'android.hardware.soundtrigger',
                'android.hardware.sensors', 'android.hardware.camera.provider', 'android.hardware.biometrics.fingerprint',
                'android.hardware.gnss', 'android.hardware.media.c2', 'android.hardware.radio', 'android.hardware.radio.config'}
for r in sorted(removed):
    p = os.path.join(V, r)
    if os.path.lexists(p):
        os.remove(p)
        print('removed', r)
# VINTF: drop <hal> entries of removed HALs (main manifest + fragments).
def prune_manifest(path):
    x = open(path).read()
    def keep(m):
        name = re.search(r'<name>([^<]+)</name>', m.group(0)).group(1).strip()
        if name in gone_ifaces:
            print(f'vintf: dropped {name} from {rel(path)}')
            return ''
        return m.group(0)
    y = re.sub(r'[ \t]*<hal\b.*?</hal>\s*\n', keep, x, flags=re.S)
    if y != x:
        lbl = os.getxattr(path, 'security.selinux')
        open(path, 'w').write(y)
        os.setxattr(path, 'security.selinux', lbl)
    return y
prune_manifest(os.path.join(V, 'etc/vintf/manifest.xml'))
md = os.path.join(V, 'etc/vintf/manifest')
for f in sorted(os.listdir(md)):
    p = os.path.join(md, f)
    y = prune_manifest(p)
    if '<hal' not in y:
        os.remove(p); print('removed', rel(p))
PY

say "installing the DRM stack"
lbl() { $SUDO python3 "$ROOT/tools/ext4tree.py" label v "$1" "$2"; }
inst() {  # src dst mode label
    $SUDO install -D -m "$3" -o 0 -g "${5:-0}" "$1" "v/$2"; lbl "$2" "$4"
}
SPH=u:object_r:same_process_hal_file:s0
VF=u:object_r:vendor_file:s0
CF=u:object_r:vendor_configs_file:s0
inst "$HALS/bin/hw/android.hardware.graphics.allocator@2.0-service" bin/hw/android.hardware.graphics.allocator@2.0-service 0755 u:object_r:hal_graphics_allocator_default_exec:s0 2000
inst "$HALS/bin/hw/android.hardware.graphics.composer@2.1-service" bin/hw/android.hardware.graphics.composer@2.1-service 0755 u:object_r:hal_graphics_composer_default_exec:s0 2000
inst "$HALS/lib64/hw/android.hardware.graphics.allocator@2.0-impl.so" lib64/hw/android.hardware.graphics.allocator@2.0-impl.so 0644 "$VF"
inst "$HALS/lib64/hw/android.hardware.graphics.mapper@2.0-impl-2.1.so" lib64/hw/android.hardware.graphics.mapper@2.0-impl-2.1.so 0644 "$SPH"
inst "$HALS/lib64/hw/gralloc.minigbm.so" lib64/hw/gralloc.minigbm.so 0644 "$SPH"
$SUDO ln -sf gralloc.minigbm.so v/lib64/hw/gralloc.default.so.new && $SUDO mv -f v/lib64/hw/gralloc.default.so.new v/lib64/hw/gralloc.default.so
lbl lib64/hw/gralloc.default.so "$SPH"
inst "$HALS/lib64/hw/hwcomposer.drm_minigbm.so" lib64/hw/hwcomposer.drm_minigbm.so 0644 "$VF"
inst "$HALS/lib64/libhwc2on1adapter.so" lib64/libhwc2on1adapter.so 0644 "$VF"
inst "$HALS/lib64/libhwc2onfbadapter.so" lib64/libhwc2onfbadapter.so 0644 "$VF"
inst "$MESA/lib64/egl/libEGL_mesa.so" lib64/egl/libEGL_mesa.so 0644 "$SPH"
inst "$MESA/lib64/egl/libGLESv2_mesa.so" lib64/egl/libGLESv2_mesa.so 0644 "$SPH"
inst "$MESA/lib64/egl/libGLESv1_CM_mesa.so" lib64/egl/libGLESv1_CM_mesa.so 0644 "$SPH"
inst "$MESA/lib64/libgallium_dri.so" lib64/libgallium_dri.so 0644 "$SPH"
inst "$MESA/lib64/hw/vulkan.virtio.so" lib64/hw/vulkan.virtio.so 0644 "$SPH"
lbl lib64/libdrm.so "$SPH"

$SUDO tee v/etc/init/apex-graphics.rc >/dev/null <<'EOF'
# Apex virtio-gpu DRM graphics HALs (scripts/build-hw-vendor.sh).
service vendor.gralloc-2-0 /vendor/bin/hw/android.hardware.graphics.allocator@2.0-service
    interface android.hardware.graphics.allocator@2.0::IAllocator default
    class hal animation
    user system
    group graphics drmrpc
    capabilities SYS_NICE
    onrestart restart surfaceflinger

service vendor.hwcomposer-2-1 /vendor/bin/hw/android.hardware.graphics.composer@2.1-service
    interface android.hardware.graphics.composer@2.1::IComposer default
    class hal animation
    user system
    group graphics drmrpc
    capabilities SYS_NICE
    onrestart restart surfaceflinger
    writepid /dev/cpuset/system-background/tasks
EOF
lbl etc/init/apex-graphics.rc "$CF"
$SUDO tee v/etc/vintf/manifest/apex-graphics.xml >/dev/null <<'EOF'
<manifest version="1.0" type="device">
    <hal format="hidl">
        <name>android.hardware.graphics.allocator</name>
        <transport>hwbinder</transport>
        <fqname>@2.0::IAllocator/default</fqname>
    </hal>
    <hal format="hidl">
        <name>android.hardware.graphics.mapper</name>
        <transport arch="64">passthrough</transport>
        <fqname>@2.1::IMapper/default</fqname>
    </hal>
    <hal format="hidl">
        <name>android.hardware.graphics.composer</name>
        <transport>hwbinder</transport>
        <fqname>@2.1::IComposer/default</fqname>
    </hal>
    <hal format="hidl">
        <name>android.hardware.audio</name>
        <transport>hwbinder</transport>
        <fqname>@7.0::IDevicesFactory/default</fqname>
    </hal>
</manifest>
EOF
lbl etc/vintf/manifest/apex-graphics.xml "$CF"

# GLES 3.2 / Vulkan 1.3 capability declarations.
$SUDO tee v/etc/permissions/android.hardware.vulkan.version-1_3.xml >/dev/null <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<!-- Vulkan 1.3 (0x00403000 = 4206592): Mesa venus over virtio-gpu. -->
<permissions>
    <feature name="android.hardware.vulkan.version" version="4206592" />
</permissions>
EOF
$SUDO tee v/etc/permissions/android.hardware.vulkan.level-1.xml >/dev/null <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<permissions>
    <feature name="android.hardware.vulkan.level" version="1" />
</permissions>
EOF
$SUDO tee v/etc/permissions/android.hardware.opengles.aep.xml >/dev/null <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<permissions>
    <feature name="android.hardware.opengles.aep" />
</permissions>
EOF
for f in android.hardware.vulkan.version-1_3.xml android.hardware.vulkan.level-1.xml android.hardware.opengles.aep.xml; do
    lbl "etc/permissions/$f" "$CF"
done

# /dev/dri for the GPU (platform policy already maps it; kept explicit here).
$SUDO tee -a v/etc/selinux/vendor_file_contexts >/dev/null <<'EOF'

# Apex virtio-gpu DRM stack
/dev/dri(/.*)?	u:object_r:gpu_device:s0
/vendor/bin/hw/android\.hardware\.graphics\.composer@2\.1-service	u:object_r:hal_graphics_composer_default_exec:s0
/vendor/lib(64)?/hw/gralloc\.minigbm\.so	u:object_r:same_process_hal_file:s0
/vendor/lib(64)?/hw/vulkan\.virtio\.so	u:object_r:same_process_hal_file:s0
/vendor/lib(64)?/egl/lib(EGL|GLESv1_CM|GLESv2)_mesa\.so	u:object_r:same_process_hal_file:s0
/vendor/lib(64)?/libgallium_dri\.so	u:object_r:same_process_hal_file:s0
EOF

say "properties"
PROPS=(
    ro.hardware.gralloc=minigbm
    ro.hardware.hwcomposer=drm_minigbm
    ro.hardware.egl=mesa
    ro.hardware.vulkan=virtio
    ro.opengles.version=196610
    ro.sf.lcd_density=420
    debug.hwui.renderer=skiagl
    ro.surface_flinger.max_frame_buffer_acquired_buffers=3
    ro.surface_flinger.use_color_management=false
    debug.stagefright.ccodec=0
    ro.product.vendor.brand=nubia
    ro.product.vendor.model=NX769J
    ro.product.vendor.name=redmagic9pro
)
$SUDO python3 - v/build.prop "${PROPS[@]}" <<'PY'
import os, sys
path, props = sys.argv[1], sys.argv[2:]
lbl = os.getxattr(path, 'security.selinux')
keys = {p.split('=', 1)[0]: p for p in props}
out, seen = [], set()
for l in open(path).read().splitlines():
    k = l.split('=', 1)[0].strip()
    if k in keys and not l.lstrip().startswith('#'):
        out.append(keys[k]); seen.add(k)
    else:
        out.append(l)
out += ['', '# Apex virtio-gpu DRM stack (scripts/build-hw-vendor.sh)'] + [keys[k] for k in keys if k not in seen]
open(path, 'w').write('\n'.join(out) + '\n')
os.setxattr(path, 'security.selinux', lbl)
PY
# The composer's HWC2 module id is "hwcomposer" + ro.hardware.hwcomposer:
# hwcomposer.drm_minigbm.so. apex.rc copies ro.boot.hardware.egl/vulkan into
# ro.hardware.*; build.prop wins because it is read first.

say "gate"
GATE="$($SUDO python3 "$ROOT/tools/ext4tree.py" elfscan v)" || { echo "$GATE"; exit 1; }
echo "$GATE" | tail -n 1
if $SUDO find v -iname '*swiftshader*' -o -iname '*pastel*' -o -iname '*goldfish*' -o -iname '*ranchu*' | grep .; then
    echo "FAIL: forbidden files present" >&2; exit 1
fi
if $SUDO grep -rIl -i 'swiftshader' v/etc v/build.prop 2>/dev/null | grep .; then echo "FAIL: swiftshader config" >&2; exit 1; fi

say "image"
kib=$($SUDO du -sk --apparent-size v | cut -f1); mib=$(( kib * 115 / 100 / 1024 + 64 ))
rm -f "$OUT/vendor_mumu_hw120_pure64.img"
E2FSPROGS_FAKE_TIME=1707346340 $SUDO mke2fs -q -t ext4 -b 4096 -I 256 -m 0 -L vendor \
    -O ^metadata_csum,^64bit,^orphan_file -E root_owner=0:0 -d v "$OUT/vendor_mumu_hw120_pure64.img" "${mib}M"
$SUDO chown "$(id -u):$(id -g)" "$OUT/vendor_mumu_hw120_pure64.img"
e2fsck -fn "$OUT/vendor_mumu_hw120_pure64.img" >/dev/null
RAW="$(sha256sum "$OUT/vendor_mumu_hw120_pure64.img" | cut -d' ' -f1)"
gzip -6 -n -f "$OUT/vendor_mumu_hw120_pure64.img"
( cd "$OUT" && sha256sum vendor_mumu_hw120_pure64.img.gz >vendor_mumu_hw120_pure64.img.gz.sha256 )
{
    echo "vendor_mumu_hw120_pure64.img (raw sha256 $RAW, ${mib} MiB)"
    echo "base: apex-android12-vendor vendor.img (sdk_phone64_arm64 SE1B.240122.005, pure 64-bit)"
    echo; echo "removed:"; grep '^removed' prune.log | sed 's/^/  /'
    echo; echo "vintf:"; grep '^vintf' prune.log | sed 's/^/  /'
    echo; echo "gate:"; echo "$GATE" | sed 's/^/  /'
} >"$OUT/REPORT.txt"
cat "$OUT/vendor_mumu_hw120_pure64.img.gz.sha256"
