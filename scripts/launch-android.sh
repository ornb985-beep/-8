#!/usr/bin/env bash
# One-command Android 12 bring-up on an Apple Silicon Mac.
#
#   scripts/launch-android.sh --probe     5 s check: kernel + probe initramfs, prints "APEX VMM OK"
#   scripts/launch-android.sh             Android 12L GSI in ApexStudio.app (Metal window, 120 Hz)
#   scripts/launch-android.sh --headless  same, console only
#   scripts/launch-android.sh --first-stage [SECS]
#                                         GSI + minimal vendor, headless; PASS when
#                                         first-stage init mounted /vendor and
#                                         second-stage init runs (default 120 s)
#
# Options: --cpus N (6)  --memory SIZE (8G)  --vendor <vendor.img>
#
# The default vendor partition is out/vendor_minimal.img (fstab.apex,
# precompiled SELinux policy, build.prop; scripts/make-minimal-vendor.sh).
# It carries no HALs (composer, gralloc, keymaster, health...), so Android
# runs init/vold/servicemanager but does not reach the launcher; see
# docs/ANDROID12.md.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out"; IMG="$ROOT/images"; mkdir -p "$OUT" "$IMG"
REPO="ornb985-beep/-8"
RELEASE="https://github.com/$REPO/releases/download/apex-android12-kernel"
GSI_URL="https://dl.google.com/developers/android/sc/images/gsi/aosp_arm64-exp-SQ3A.220705.003.A1-8672226-6554a6c4.zip"
GSI_SIZE=776920909

MODE=gui CPUS=6 MEM=8G VENDOR="" FS_SECS=120
while [[ $# -gt 0 ]]; do
    case "$1" in
        --probe) MODE=probe ;;
        --first-stage) MODE=first-stage; if [[ "${2:-}" =~ ^[0-9]+$ ]]; then FS_SECS="$2"; shift; fi ;;
        --headless) MODE=headless ;;
        --cpus) CPUS="$2"; shift ;;
        --memory) MEM="$2"; shift ;;
        --vendor) VENDOR="$2"; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
sha() { shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1 || sha256sum "$1" | cut -d' ' -f1; }

# 1. VMM + frontend
if [[ ! -x "$OUT/apex" || ! -d "$OUT/ApexStudio.app" ]]; then
    say "building Apex VMM and ApexStudio.app"
    "$ROOT/scripts/build-macos.sh"
fi
APEX="$OUT/apex"

# 2. Android 12 GKI kernel (android12-5.10 + guest/kernel/android12-5.10.fragment)
fetch() {
    curl -fL --retry 3 --progress-bar -o "$2.part" "$1" && { mv "$2.part" "$2"; return 0; }
    rm -f "$2.part"
    # Private repository: anonymous downloads return 401/404; use the GitHub CLI login.
    if [[ "$1" == */releases/download/* ]] && command -v gh >/dev/null; then
        local tag="${1#*/releases/download/}"; tag="${tag%%/*}"
        gh release download "$tag" -R ornb985-beep/-8 -p "$(basename "$1")" -O "$2" --clobber && return 0
    fi
    echo "download failed: $1 (private repo? run: brew install gh && gh auth login)" >&2
    return 1
}
if [[ ! -f "$OUT/Image.gz" ]]; then
    say "downloading the Apex Android 12 kernel (built by CI)"
    fetch "$RELEASE/Image.gz" "$OUT/Image.gz"
    fetch "$RELEASE/kernel.sha256" "$OUT/kernel.sha256"
    [[ "$(sha "$OUT/Image.gz")" == "$(cut -d' ' -f1 "$OUT/kernel.sha256")" ]] || { echo "kernel checksum mismatch" >&2; exit 1; }
fi
[[ -f "$OUT/initramfs.cpio.gz" ]] || fetch "$RELEASE/initramfs.cpio.gz" "$OUT/initramfs.cpio.gz" || "$ROOT/scripts/build-probe.sh"

if [[ "$MODE" == probe ]]; then
    cat > "$OUT/probe.toml" <<TOML
[vm]
cpus = 2
memory = "1G"
[boot]
kernel = "Image.gz"
initrd = "initramfs.cpio.gz"
cmdline = "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000 loglevel=4"
[input]
keyboard = false
TOML
    say "probe: booting kernel + initramfs (5 s budget)"
    # macOS has no GNU timeout; perl's alarm gives the same guarantee.
    if perl -e 'alarm shift; exec @ARGV' 5 "$APEX" run "$OUT/probe.toml" | tee "$OUT/probe.log" | grep -m1 "APEX VMM OK"; then
        say "PASS: the VMM boots the Android 12 kernel"
        exit 0
    fi
    echo "FAIL: no 'APEX VMM OK' within 5 s — see $OUT/probe.log and run 'out/apex selftest'" >&2
    exit 1
fi

# 3. Android 12L GSI (system.img)
if [[ ! -f "$IMG/system.img" ]]; then
    say "downloading the Android 12L (API 32) ARM64 GSI from Google (740 MiB)"
    fetch "$GSI_URL" "$IMG/gsi.zip"
    [[ "$(stat -f %z "$IMG/gsi.zip" 2>/dev/null || stat -c %s "$IMG/gsi.zip")" == "$GSI_SIZE" ]] || { echo "GSI download size mismatch" >&2; exit 1; }
    ( cd "$IMG" && unzip -o -q gsi.zip system.img vbmeta.img && rm gsi.zip )
fi

# 4. GPT disk: vda1 misc, vda2 system, vda3 vendor, vda4 userdata
if [[ -z "$VENDOR" ]]; then
    VENDOR="$OUT/vendor_minimal.img"
    if [[ ! -f "$VENDOR" ]]; then
        say "fetching the minimal vendor partition (fstab.apex + SELinux policy)"
        fetch "$RELEASE/vendor_minimal.img" "$VENDOR" || "$ROOT/scripts/make-minimal-vendor.sh"
    fi
fi
VSPEC="vendor=$VENDOR:ro"
if [[ ! -f "$OUT/android_disk.raw" || "$IMG/system.img" -nt "$OUT/android_disk.raw" || "$VENDOR" -nt "$OUT/android_disk.raw" ]]; then
    say "composing out/android_disk.raw (GPT)"
    "$APEX" mkdisk --out "$OUT/android_disk.raw" --name apex-android12 \
        misc=@1M system="$IMG/system.img":ro "$VSPEC" userdata=@8G
fi

# 5. boot.img (header v4) with the Android 12 command line
# The VMM also passes these via bootconfig and describes /vendor in the device
# tree (/firmware/android/fstab); the OS disk is virtio slot 0.
LOGLEVEL=4; [[ "$MODE" == first-stage ]] && LOGLEVEL=6
CMDLINE="console=hvc0 earlycon=pl011,mmio32,0x09000000 loglevel=$LOGLEVEL root=/dev/vda2 ro rootwait init=/init androidboot.hardware=apex androidboot.console=hvc0 androidboot.selinux=permissive androidboot.boot_devices=a000000.virtio_mmio"
"$APEX" mkbootimg --kernel "$OUT/Image.gz" --cmdline "$CMDLINE" --out "$OUT/boot.img" >/dev/null

# 6. device profile
cat > "$OUT/android12.toml" <<TOML
[device]
model = "Apex One"
serial = "APEX12000001"
[vm]
cpus = $CPUS
memory = "$MEM"
gic = "auto"
[boot]
boot_image = "boot.img"
[boot.bootconfig]
"androidboot.hardware" = "apex"
"androidboot.console" = "hvc0"
"androidboot.selinux" = "permissive"
[display]
width = 1080
height = 2400
refresh = 120
dpi = 420
vsync = "internal"   # Mach real-time 120 Hz pacer
[[disk]]
path = "android_disk.raw"
[console]
serial = "stdout"
TOML

# 7. go
if [[ "$MODE" == first-stage ]]; then
    say "first-stage mount test: GSI + minimal vendor, headless ($FS_SECS s budget)"
    LOG="$OUT/first-stage.log"
    perl -e 'alarm shift; exec @ARGV' "$FS_SECS" "$APEX" run "$OUT/android12.toml" >"$LOG" 2>&1 || true
    grep -E "init: |APEX:|Kernel panic|fs_mgr" "$LOG" | grep -v DM_DEV_STATUS | tail -n 25 || true
    if grep -q "APEX: minimal vendor mounted" "$LOG"; then
        say "PASS: first-stage init mounted /vendor from /dev/block/by-name/vendor; second-stage init is running"
        exit 0
    elif grep -q "Could not read properties from '/vendor\|/vendor/etc/selinux" "$LOG"; then
        say "PARTIAL: /vendor is mounted (init reads /vendor/etc); second stage not confirmed — see $LOG"
        exit 3
    fi
    echo "FAIL: /vendor was not mounted — see $LOG" >&2
    exit 1
fi
say "launching Android 12L: $CPUS vCPUs, $MEM, 1080x2400@120"
if [[ "$MODE" == headless ]]; then
    exec "$APEX" run "$OUT/android12.toml"
else
    exec "$OUT/ApexStudio.app/Contents/MacOS/ApexStudio" "$OUT/android12.toml"
fi
