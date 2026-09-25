#!/usr/bin/env bash
# Assemble guest/selftest/selftest.S into the raw image embedded in `apex selftest`.
set -euo pipefail
D="$(cd "$(dirname "$0")/.." && pwd)/guest/selftest"
TMP="$(mktemp -d)"
clang --target=aarch64-linux-gnu -nostdlib -c "$D/selftest.S" -o "$TMP/selftest.o"
llvm-objcopy -O binary --only-section=.text "$TMP/selftest.o" "$D/selftest.bin"
rm -rf "$TMP"
echo "wrote $D/selftest.bin ($(wc -c < "$D/selftest.bin") bytes)"
