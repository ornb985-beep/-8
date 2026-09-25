#!/usr/bin/env bash
# Cross-check out/Image.gz + out/initramfs.cpio.gz on QEMU's `virt` machine
# (GICv3, PL011, virtio-mmio: the same device classes as apex-virt).
# Used by CI on Linux; exits 0 only if the probe printed "APEX VMM OK".
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$ROOT/out/qemu-probe.log"
timeout "${TIMEOUT:-120}" qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a72 -smp 2 -m 1G \
    -nographic -no-reboot -kernel "$ROOT/out/Image.gz" -initrd "$ROOT/out/initramfs.cpio.gz" \
    -append "console=ttyAMA0 loglevel=4" | tee "$LOG" | grep --line-buffered -E "APEX VMM OK|Kernel panic|reboot: Power down" || true
grep -q "APEX VMM OK" "$LOG"
