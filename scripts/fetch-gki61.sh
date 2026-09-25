#!/usr/bin/env bash
# Fetch Google's prebuilt android14-6.1 GKI kernel (release build, no kernel
# compilation) plus the matching virtio vendor modules, and build the small
# initramfs that loads them before handing over to Android's /init.
#
# Why modules: GKI keeps virtio_mmio / virtio_blk / virtio-gpu / virtio_console
# / virtio_input out of the core kernel (verified from modules.builtin of this
# build). They come from the same release build's `kernel_virt_aarch64`
# target (common-modules/virtual-device), vermagic-identical to the kernel.
#
# 6.1 brings virtio-gpu RESOURCE_BLOB (host-visible memory) and CONTEXT_INIT,
# which android12-5.10 lacks.
#
# Output (out/gki61/):
#   Image-gki6.1-arm64.gz       the GKI kernel, byte-identical to Google's
#   initramfs-gki6.1.cpio.gz    guest/gki-init + modules + modules.load
#   SHA256SUMS
# Needs curl, clang, ld.lld, python3, gzip (any host).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out/gki61"; CACHE="$OUT/cache"; mkdir -p "$CACHE"

BID=16416853   # android14-6.1-2026-03 release (source.android.com GKI release builds)
KREL="6.1.162-android14-11-gddab21e1a7ae-ab16416853"
CI="https://ci.android.com/builds/submitted/$BID"

# file  target  sha256 — load order: dependencies first.
ARTIFACTS=(
    "Image.gz kernel_aarch64 e4c9795e6e431fa0c268e9ff7681b5e1eb69f16ed02973a8568c6fa6e242b0da"
    "virtio_dma_buf.ko kernel_virt_aarch64 e2854d3e160e2ce8c09f328f878f55a9d5e6dc604e4780e310a685dca58d012b"
    "virtio_mmio.ko kernel_virt_aarch64 f12b592dad7085e0ac3fcc651e6acb5a1b29f2f52613a27bf7652bdb0cb54c98"
    "virtio_blk.ko kernel_virt_aarch64 598b2a26ebddb0520069fdaf5b9d5ffd3b160760e5413aa15e700ffbb52bb870"
    "virtio_console.ko kernel_virt_aarch64 7de1865961b2a85f5d20f535f0a8879b77ed212e1650be13f332e9db8bf6b83c"
    "virtio_input.ko kernel_virt_aarch64 79e43aa4ee7d24e155da7fd603ac26916690628ee006d92236329c45bdaa199b"
    "virtio-rng.ko kernel_virt_aarch64 4bcdfb1402e445f9cd12f1fe76748bc7875ba71ece5cd1c036fc25db80f2fe1d"
    "system_heap.ko kernel_virt_aarch64 5ec5233e2da469a6216566ef149f6560ed737baa8f249cd20361a26010370e32"
    "virtio-gpu.ko kernel_virt_aarch64 1ca2f1b3dbb6686e7d37e1d30df2e04c6cb0eed5579b81cb8966f4e53c9009ae"
    "failover.ko kernel_virt_aarch64 951d40efb102c560bfb5865524a32ed857582151a2623750a7f5418b4c9d1473"
    "net_failover.ko kernel_virt_aarch64 9b25cb46a6051ab666b173340a43bcedc34afd7e098c3dc63a491219303f22d9"
    "virtio_net.ko kernel_virt_aarch64 718316016c6f284e141f67ede37255d4ab3eb754265cc17c899dec19d857b3ef"
    "goldfish_battery.ko kernel_virt_aarch64 7d3b54019559d803ccb7557dd1fa3f93d7d075432cc60f14417d398779b07935"
)
# goldfish_battery is only the power_supply driver for the VMM's battery
# device (DT google,goldfish-battery@9020000); it has nothing to do with the
# goldfish pipe.

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
MODS=()
for a in "${ARTIFACTS[@]}"; do
    read -r f t sum <<<"$a"
    if [[ ! -f "$CACHE/$f" ]] || ! echo "$sum  $CACHE/$f" | sha256sum -c --status 2>/dev/null; then
        say "fetch $t/$f"
        curl -fsSL --retry 3 -o "$CACHE/$f.part" "$CI/$t/latest/raw/$f"
        mv "$CACHE/$f.part" "$CACHE/$f"
    fi
    echo "$sum  $CACHE/$f" | sha256sum -c --quiet
    if [[ "$f" == *.ko ]]; then
        vm="$(strings "$CACHE/$f" | sed -n 's/^vermagic=\([^ ]*\).*/\1/p' | head -1)"
        [[ "$vm" == "$KREL" ]] || { echo "$f: vermagic $vm != $KREL" >&2; exit 1; }
        MODS+=("$f")
    fi
done
cp "$CACHE/Image.gz" "$OUT/Image-gki6.1-arm64.gz"

say "building the module loader (guest/gki-init/init.c)"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-stack-protector -fno-builtin -nostdlib -static \
    -fuse-ld=lld -Wl,-e,_start -Wl,--build-id=none -o "$TMP/init" "$ROOT/guest/gki-init/init.c"
printf '%s\n' "${MODS[@]}" >"$TMP/modules.load"

python3 - "$TMP" "$CACHE" "$OUT/initramfs-gki6.1.cpio" "${MODS[@]}" <<'PY'
import stat, sys
tmp, cache, dst, mods = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
out, ino = bytearray(), [1]
def entry(name, mode, data=b"", rdev=(0, 0)):
    hdr = "070701" + "".join("%08X" % v for v in (
        ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name) + 1, 0))
    ino[0] += 1
    out.extend(hdr.encode() + name.encode() + b"\0")
    while len(out) % 4: out.append(0)
    out.extend(data)
    while len(out) % 4: out.append(0)
for d in ("dev", "proc", "sys", "lib", "lib/modules"):
    entry(d, stat.S_IFDIR | 0o755)
entry("dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
entry("init", stat.S_IFREG | 0o755, open(f"{tmp}/init", "rb").read())
entry("lib/modules/modules.load", stat.S_IFREG | 0o644, open(f"{tmp}/modules.load", "rb").read())
for m in mods:
    entry(f"lib/modules/{m}", stat.S_IFREG | 0o644, open(f"{cache}/{m}", "rb").read())
entry("TRAILER!!!", 0)
open(dst, "wb").write(out)
PY
gzip -9 -n -f "$OUT/initramfs-gki6.1.cpio"
( cd "$OUT" && sha256sum Image-gki6.1-arm64.gz initramfs-gki6.1.cpio.gz >SHA256SUMS )
say "GKI $KREL (build $BID)"
cat "$OUT/SHA256SUMS"
