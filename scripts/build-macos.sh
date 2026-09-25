#!/usr/bin/env bash
# Build Apex-AOSP on an Apple Silicon Mac:
#   1. the Rust VMM core (libapex_vmm.a + the `apex` CLI)
#   2. the Swift/Metal frontend (ApexStudio.app)
# and ad-hoc sign both with the Hypervisor.framework entitlement.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENT="$ROOT/frontend/ApexStudio/Support/ApexStudio.entitlements"
OUT="$ROOT/out"
SIGN_ID="${APEX_SIGN_IDENTITY:--}"   # "-" = ad-hoc

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "error: Apex-AOSP runs on Apple Silicon Macs (macOS 14+)." >&2
    exit 1
fi

echo "==> cargo build --release"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p apex-vmm

mkdir -p "$OUT"
cp "$ROOT/target/release/apex" "$OUT/apex"
codesign --force --sign "$SIGN_ID" --entitlements "$ENT" "$OUT/apex"

echo "==> swift build (ApexStudio)"
( cd "$ROOT/frontend/ApexStudio" && swift build -c release )
BIN="$(cd "$ROOT/frontend/ApexStudio" && swift build -c release --show-bin-path)/ApexStudio"

APP="$OUT/ApexStudio.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/ApexStudio"
cp "$ROOT/frontend/ApexStudio/Support/Info.plist" "$APP/Contents/Info.plist"
codesign --force --sign "$SIGN_ID" --entitlements "$ENT" --options runtime "$APP"

echo
echo "Built:"
echo "  $OUT/apex                 (headless CLI)"
echo "  $APP   (open with a profile: open -a \"$APP\" --args profiles/phone-120hz.toml)"
"$OUT/apex" caps || true
