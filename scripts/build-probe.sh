#!/usr/bin/env bash
# Build out/initramfs.cpio.gz: a few-KB initramfs whose /init prints
# "APEX VMM OK" and powers the VM off. Needs clang + ld.lld (any host).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out"; mkdir -p "$OUT"
TMP="$(mktemp -d)"
clang --target=aarch64-linux-gnu -nostdlib -c "$ROOT/guest/probe/init.S" -o "$TMP/init.o"
ld.lld -static -e _start -o "$TMP/init" "$TMP/init.o"
python3 - "$TMP/init" "$OUT/initramfs.cpio" <<'PY'
import os, stat, sys
init = open(sys.argv[1], "rb").read()
out = bytearray()
ino = [1]
def entry(name, mode, data=b"", rdev=(0, 0)):
    hdr = "070701" + "".join("%08X" % v for v in (
        ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name) + 1, 0))
    ino[0] += 1
    out.extend(hdr.encode() + name.encode() + b"\0")
    while len(out) % 4: out.append(0)
    out.extend(data)
    while len(out) % 4: out.append(0)
entry("dev", stat.S_IFDIR | 0o755)
entry("dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
entry("proc", stat.S_IFDIR | 0o755)
entry("init", stat.S_IFREG | 0o755, init)
entry("TRAILER!!!", 0)
open(sys.argv[2], "wb").write(out)
PY
gzip -9 -n -f "$OUT/initramfs.cpio"
rm -rf "$TMP"
echo "wrote $OUT/initramfs.cpio.gz ($(wc -c < "$OUT/initramfs.cpio.gz") bytes)"
