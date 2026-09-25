#!/usr/bin/env bash
# One-command Android 12 bring-up on an Apple Silicon Mac.
#
#   scripts/launch-android.sh --probe     5 s check: kernel + probe initramfs, prints "APEX VMM OK"
#   scripts/launch-android.sh             Android 12L GSI in ApexStudio.app (Metal window, 120 Hz)
#   scripts/launch-android.sh --headless  same, console only
#
# Options: --cpus N (6)  --memory SIZE (8G)  --vendor <vendor.img>
#
# IMPORTANT: Google's GSI ships system.img only. Android needs a vendor
# partition (HALs: composer, gralloc, keymaster, health...) built for this
# virtual hardware. Without --vendor the boot reaches the GSI's init and
# stops there; see docs/ANDROID12.md.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out"; IMG="$ROOT/images"; mkdir -p "$OUT" "$IMG"
REPO="ornb985-beep/-8"
RELEASE="https://github.com/$REPO/releases/download/apex-android12-kernel"
GSI_URL="https://dl.google.com/developers/android/sc/images/gsi/aosp_arm64-exp-SQ3A.220705.003.A1-8672226-6554a6c4.zip"
GSI_SIZE=776920909

MODE=gui CPUS=6 MEM=8G VENDOR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --probe) MODE=probe ;;
        --headless) MODE=headless ;;
        --cpus) CPUS="$2"; shift ;;
        --memory) MEM="$2"; shift ;;
        --vendor) VENDOR="$2"; shift ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
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
fetch() { curl -fL --retry 3 --progress-bar -o "$2.part" "$1" && mv "$2.part" "$2"; }
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
    echo "warning: no vendor image (--vendor). The GSI has no HALs to talk to; Android will not reach the launcher." >&2
    VSPEC="vendor=@256M"
else
    VSPEC="vendor=$VENDOR"
fi
if [[ ! -f "$OUT/android_disk.raw" || "$IMG/system.img" -nt "$OUT/android_disk.raw" || -n "$VENDOR" ]]; then
    say "composing out/android_disk.raw (GPT)"
    "$APEX" mkdisk --out "$OUT/android_disk.raw" --name apex-android12 \
        misc=@1M system="$IMG/system.img":ro "$VSPEC" userdata=@8G
fi

# 5. boot.img (header v4) with the Android 12 command line
CMDLINE="console=hvc0 earlycon=pl011,mmio32,0x09000000 loglevel=4 root=/dev/vda2 ro rootwait init=/init androidboot.hardware=apex androidboot.console=hvc0 androidboot.selinux=permissive"
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
say "launching Android 12L: $CPUS vCPUs, $MEM, 1080x2400@120"
if [[ "$MODE" == headless ]]; then
    exec "$APEX" run "$OUT/android12.toml"
else
    exec "$OUT/ApexStudio.app/Contents/MacOS/ApexStudio" "$OUT/android12.toml"
fi
