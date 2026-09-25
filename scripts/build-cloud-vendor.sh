#!/usr/bin/env bash
# Build the pure 64-bit Android 12L system.img + vendor.img pair for the Apex
# VMM, WITHOUT compiling AOSP: the partitions come from Google's official
# 64-bit-only Android 12L emulator build (sdk_phone64_arm64-userdebug,
# SE1B.240122.005, "system-images;android-32;default;arm64-v8a"), repacked
# for Apex's disk layout and boot flow:
#
#   * super.img is unpacked; system_ext and product are folded into system
#     (GSI layout), because Apex boots legacy system-as-root with one system
#     partition and first-stage-mounts only vendor.
#   * every file keeps its SELinux label and capabilities (tools/ext4tree.py).
#   * vendor gets guest/vendor-overlay (fstab.apex, apex.rc, input configs),
#     the device identity (profiles/redmagic9pro, see VENDOR_PROPS) and
#     block-device labels for the Apex GPT.
#   * gate: zero 32-bit / non-AArch64 ELF in the images and inside every APEX.
#
# Output (out/cloud/): system.img.gz, vendor.img.gz, *.sha256, BUILD_INFO.txt.
# Linux only (debugfs, mke2fs >= 1.43, python3, root for xattrs).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$ROOT/work/cloud-vendor}"
OUT="$ROOT/out/cloud"
EMU_URL="https://dl.google.com/android/repository/sys-img/android/arm64-v8a-32_r02.zip"
EMU_SHA1="c57d92a4131590b2a3b62f2a728766aa6bbec57f"
SUDO=""; [[ "$(id -u)" == 0 ]] || SUDO="sudo"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
mkdir -p "$WORK" "$OUT"
cd "$WORK"

# 1. Google's image
if [[ ! -f parts/system.img ]]; then
    if [[ ! -f emu.zip ]]; then
        say "downloading $EMU_URL"
        curl -fL --retry 3 -o emu.zip.part "$EMU_URL" && mv emu.zip.part emu.zip
    fi
    [[ "$(sha1sum emu.zip | cut -d' ' -f1)" == "$EMU_SHA1" ]] || { echo "emulator image checksum mismatch" >&2; exit 1; }
    say "unpacking super (system, system_ext, product, vendor)"
    unzip -o -j -q emu.zip arm64-v8a/system.img arm64-v8a/build.prop
    python3 "$ROOT/tools/lpunpack.py" system.img parts
    rm -f system.img
fi

# 2. trees with labels
for p in system system_ext product vendor; do
    $SUDO rm -rf "t_$p"
    $SUDO python3 "$ROOT/tools/ext4tree.py" extract "parts/$p.img" "t_$p"
done

# 3. system = system + system_ext + product (GSI layout)
say "folding system_ext and product into system"
for p in system_ext product; do
    $SUDO rm -f "t_system/system/$p"                   # symlink -> /$p
    $SUDO mv "t_$p" "t_system/system/$p"               # keeps the tree's labels
    $SUDO rmdir "t_system/$p"                          # mount point
    $SUDO ln -s "/system/$p" "t_system/$p"
    $SUDO python3 "$ROOT/tools/ext4tree.py" label t_system "$p" u:object_r:system_file:s0
done

# 4. vendor overlay + identity
say "vendor overlay"
V=t_vendor
lbl() { $SUDO python3 "$ROOT/tools/ext4tree.py" label "$V" "$1" "$2"; }
$SUDO install -m 0644 -o 0 -g 0 "$ROOT/guest/vendor-overlay/etc/fstab.apex" "$V/etc/fstab.apex"
$SUDO install -m 0644 -o 0 -g 0 "$ROOT/guest/vendor-overlay/etc/init/apex.rc" "$V/etc/init/apex.rc"
$SUDO install -m 0644 -o 0 -g 0 "$ROOT"/guest/vendor-overlay/usr/idc/*.idc "$V/usr/idc/"
$SUDO install -m 0644 -o 0 -g 0 "$ROOT"/guest/vendor-overlay/usr/keylayout/*.kl "$V/usr/keylayout/"
lbl etc/fstab.apex u:object_r:vendor_configs_file:s0
lbl etc/init/apex.rc u:object_r:vendor_configs_file:s0
for f in "$ROOT"/guest/vendor-overlay/usr/idc/*.idc; do lbl "usr/idc/$(basename "$f")" u:object_r:vendor_keylayout_file:s0; done
for f in "$ROOT"/guest/vendor-overlay/usr/keylayout/*.kl; do lbl "usr/keylayout/$(basename "$f")" u:object_r:vendor_keylayout_file:s0; done

# Device identity (nubia RedMagic 9 Pro, NX769J / SM8650 "pineapple") and the
# 64-bit-only ABI. ro.product.* are set as ro.product.vendor.* plus a source
# order (vendor_init may not set ro.product.*); ro.hardware=qcom comes from
# the VMM's bootconfig.
VENDOR_PROPS=(
    ro.product.vendor.brand=nubia
    ro.product.vendor.manufacturer=nubia
    ro.product.vendor.model=NX769J
    ro.product.vendor.name=redmagic9pro
    ro.product.vendor.device=NX769J
    ro.product.property_source_order=vendor,odm,product,system_ext,system
    ro.product.board=pineapple
    ro.board.platform=pineapple
    ro.soc.manufacturer=QTI
    ro.soc.model=SM8650
    ro.opengles.version=196610
    ro.vndk.version=32
    ro.zygote=zygote64
    ro.vendor.product.cpu.abilist=arm64-v8a
    ro.vendor.product.cpu.abilist32=
    ro.vendor.product.cpu.abilist64=arm64-v8a
    ro.bionic.2nd_arch=
)
$SUDO python3 - "$V/build.prop" "${VENDOR_PROPS[@]}" <<'PY'
import os, sys
path, props = sys.argv[1], sys.argv[2:]
label = os.getxattr(path, 'security.selinux')
lines = open(path).read().splitlines()
keys = {p.split('=', 1)[0]: p for p in props}
out, seen = [], set()
for l in lines:
    k = l.split('=', 1)[0].strip()
    if k in keys and not l.lstrip().startswith('#'):
        out.append(keys[k]); seen.add(k)
    else:
        out.append(l)
out.append('')
out.append('# Apex: device identity and 64-bit-only ABI (scripts/build-cloud-vendor.sh)')
out += [keys[k] for k in keys if k not in seen]
open(path, 'w').write('\n'.join(out) + '\n')
os.setxattr(path, 'security.selinux', label)
PY

# Block devices of the Apex GPT (vda1 misc, vda2 system, vda3 vendor, vda4
# userdata); ueventd labels them through the by-name links.
$SUDO tee -a "$V/etc/selinux/vendor_file_contexts" >/dev/null <<'EOF'

# Apex VMM GPT (androidboot.boot_devices=a000000.virtio_mmio)
/dev/block/by-name/misc		u:object_r:misc_block_device:s0
/dev/block/by-name/system	u:object_r:system_block_device:s0
/dev/block/by-name/vendor	u:object_r:system_block_device:s0
/dev/block/by-name/userdata	u:object_r:userdata_block_device:s0
EOF

# 5. 64-bit gate, including APEX payloads
say "64-bit gate"
$SUDO rm -rf apexscan && mkdir apexscan
while IFS= read -r a; do
    d="apexscan/$(basename "$a")"; mkdir -p "$d"
    if [[ "$a" == *.capex ]]; then $SUDO unzip -p "$a" original_apex >"$d/o.apex"; a="$d/o.apex"; fi
    $SUDO unzip -p "$a" apex_payload.img >"$d/p.img" && debugfs -R "rdump / $d" "$d/p.img" >/dev/null 2>&1
    rm -f "$d/p.img" "$d/o.apex"
done < <($SUDO find t_system t_vendor -type f -path '*/apex/*' \( -name '*.apex' -o -name '*.capex' \))
GATE="$($SUDO python3 "$ROOT/tools/ext4tree.py" elfscan t_system t_vendor apexscan)" || { echo "$GATE"; echo "FAIL: 32-bit ELF found" >&2; exit 1; }
echo "$GATE" | tail -n 1
NAPEX="$(ls apexscan | wc -l)"
$SUDO rm -rf apexscan

# 6. images
mkimg() {  # tree label out
    local kib; kib=$($SUDO du -sk --apparent-size "$1" | cut -f1)
    local mib=$(( kib * 115 / 100 / 1024 + 64 ))
    rm -f "$3"
    E2FSPROGS_FAKE_TIME=1707346340 $SUDO mke2fs -q -t ext4 -b 4096 -I 256 -m 0 -L "$2" \
        -O ^metadata_csum,^64bit,^orphan_file -E root_owner=0:0 -d "$1" "$3" "${mib}M"
    $SUDO chown "$(id -u):$(id -g)" "$3"
    e2fsck -fn "$3" >/dev/null
}
say "building ext4 images"
mkimg t_system system "$OUT/system.img"
mkimg t_vendor vendor "$OUT/vendor.img"
debugfs -R 'ea_list /system/bin/surfaceflinger' "$OUT/system.img" 2>/dev/null | grep -q surfaceflinger_exec
debugfs -R 'ea_get /bin/hw/android.hardware.keymaster@4.1-service security.selinux' "$OUT/vendor.img" 2>/dev/null | grep -q hal_keymaster_default_exec

# 7. package
say "packaging"
FPR="$(grep '^ro.system.build.fingerprint=' build.prop | cut -d= -f2)"
PSHA="$(debugfs -R 'cat /system/etc/selinux/plat_sepolicy_and_mapping.sha256' "$OUT/system.img" 2>/dev/null)"
for f in system vendor; do
    RAW_SHA="$(sha256sum "$OUT/$f.img" | cut -d' ' -f1)"
    gzip -6 -n -f -k "$OUT/$f.img"
    ( cd "$OUT" && sha256sum "$f.img.gz" >"$f.img.gz.sha256" )
    eval "${f^^}_RAW_SHA=$RAW_SHA"
done
cat >"$OUT/BUILD_INFO.txt" <<EOF
Apex Android 12L cloud vendor/system (scripts/build-cloud-vendor.sh)
built: $(date -u +%Y-%m-%dT%H:%M:%SZ)  repo commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)

source: Google Android Emulator system image, API 32, arm64-v8a, r02
  $EMU_URL (sha1 $EMU_SHA1)
  lunch target: sdk_phone64_arm64-userdebug (64-bit only), build id SE1B.240122.005 / 11418786
  fingerprint: $FPR
  no AOSP compilation: prebuilt partitions repacked (labels and capabilities preserved)

system.img  raw sha256 $SYSTEM_RAW_SHA  ($(du -h --apparent-size "$OUT/system.img" | cut -f1), system + system_ext + product, legacy system-as-root)
vendor.img  raw sha256 $VENDOR_RAW_SHA  ($(du -h --apparent-size "$OUT/vendor.img" | cut -f1))
plat_sepolicy_and_mapping.sha256: $PSHA
  (vendor precompiled_sepolicy is from the same build; use this system.img, not Google's GSI)

64-bit gate (images + $NAPEX APEX payloads):
$(echo "$GATE" | sed 's/^/  /')

vendor HALs (all 64-bit): $($SUDO ls t_vendor/bin/hw | tr '\n' ' ')
vendor identity: ${VENDOR_PROPS[*]}

graphics: the vendor's GLES/gralloc/composer are the emulator's goldfish-opengl
stack (libEGL_emulation, allocator@3.0, composer@2.4 ranchu) plus ANGLE. They
need a host renderer (gfxstream over virtio-gpu), not the 2D virtio-gpu path;
no SwiftShader / minigbm / drm_hwcomposer is included. See docs/ANDROID12.md.
EOF
rm -f "$OUT/system.img" "$OUT/vendor.img"
ls -la "$OUT"
cat "$OUT"/*.sha256
