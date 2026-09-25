#!/usr/bin/env bash
# Everything CI runs, locally. Works on Linux and macOS.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p apex-vmm
LIBS="-lpthread -lm"
if [[ "$(uname -s)" == "Linux" ]]; then LIBS="$LIBS -ldl"; else LIBS="$LIBS -framework Hypervisor"; fi
cc -std=c11 -Wall -Werror -I include -o target/abi_smoke tests/c/abi_smoke.c target/release/libapex_vmm.a $LIBS
./target/abi_smoke
# CLI end to end: assemble a machine around a synthetic arm64 Image.
T="$ROOT/target/inspect"
mkdir -p "$T"
python3 - "$T/Image" <<'PY'
import struct, sys
img = bytearray(64 * 1024)
struct.pack_into("<QQQ", img, 8, 0, 8 << 20, 0b1010)   # text_offset, image_size, flags
struct.pack_into("<I", img, 56, 0x644D5241)            # "ARM\x64"
open(sys.argv[1], "wb").write(img)
PY
cat > "$T/profile.toml" <<'TOML'
[vm]
cpus = 4
memory = "1G"
[boot]
kernel = "Image"
cmdline = "console=ttyAMA0"
TOML
./target/release/apex inspect "$T/profile.toml" --dts | tee "$T/inspect.txt" | head -8
grep -q 'compatible = "arm,gic-v3"' "$T/inspect.txt"
grep -q 'virtio_mmio@a000000' "$T/inspect.txt"
echo "all checks passed"
