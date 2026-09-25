#!/usr/bin/env bash
# One-shot physical acceptance on an Apple Silicon Mac (M-series, macOS 14+).
#
#   ./scripts/ignite-mac.sh
#
#   0. host checks: Apple Silicon, macOS version, Xcode command line tools, Rust
#   1. downloads the prebuilt Android 12 kernel, probe initramfs and minimal
#      vendor partition from the GitHub release (no local kernel/AOSP build)
#   2. scripts/build-macos.sh (apex CLI + ApexStudio.app, hypervisor-signed)
#   A. out/apex selftest                    Hypervisor.framework, GIC, timer, SMP, PSCI
#   B. scripts/launch-android.sh --probe    Android 12 kernel boots: "APEX VMM OK"
#   C. scripts/launch-android.sh --first-stage
#                                           GSI + minimal vendor: first-stage mount
#                                           of /vendor, second-stage init running
#
# Logs: out/ignite/*.log. Exit status 0 only when A, B and C all pass.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/out"; LOGS="$OUT/ignite"; mkdir -p "$LOGS"
RELEASE="https://github.com/ornb985-beep/-8/releases/download/apex-android12-kernel"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '    \033[1;32m✓\033[0m %s\n' "$*"; }
bad()  { printf '    \033[1;31m✗\033[0m %s\n' "$*"; }
die()  { bad "$*"; exit 1; }
fetch() { curl -fL --retry 3 --progress-bar -o "$2.part" "$1" && mv "$2.part" "$2"; }
sha()  { shasum -a 256 "$1" | cut -d' ' -f1; }

# ---------------------------------------------------------------- 0. host
say "host checks"
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || die "needs an Apple Silicon Mac (got $(uname -sm))"
OSV="$(sw_vers -productVersion)"; OSMAJ="${OSV%%.*}"
CHIP="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo Apple)"
ok "$CHIP, macOS $OSV, $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) >> 30 )) GiB"
(( OSMAJ >= 14 )) || die "macOS 14 or newer required"
if (( OSMAJ >= 15 )); then ok "in-kernel vGICv3 available (macOS 15+)"; else ok "macOS 14: userspace GICv3 will be used"; fi
[[ "$(sysctl -n kern.hv_support 2>/dev/null)" == 1 ]] && ok "Hypervisor.framework supported" || die "kern.hv_support != 1"
xcode-select -p >/dev/null 2>&1 || { bad "Xcode command line tools missing — running xcode-select --install"; xcode-select --install; exit 1; }
command -v swift >/dev/null || die "swift not found (install Xcode or the command line tools)"
ok "Xcode CLT: $(xcode-select -p); $(swift --version 2>&1 | head -1)"
if ! command -v cargo >/dev/null; then
    [[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null; then
    say "installing Rust (rustup, minimal profile)"
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal || die "rustup failed"
    . "$HOME/.cargo/env"
fi
ok "$(rustc --version)"

# ---------------------------------------------------------------- 1. artifacts
say "prebuilt guest artifacts (release apex-android12-kernel)"
if [[ ! -f "$OUT/Image.gz" ]]; then
    fetch "$RELEASE/Image.gz" "$OUT/Image.gz" || die "kernel download failed"
    fetch "$RELEASE/kernel.sha256" "$OUT/kernel.sha256" || die "checksum download failed"
    [[ "$(sha "$OUT/Image.gz")" == "$(cut -d' ' -f1 "$OUT/kernel.sha256")" ]] || { rm -f "$OUT/Image.gz"; die "kernel checksum mismatch"; }
fi
ok "kernel  out/Image.gz ($(du -h "$OUT/Image.gz" | cut -f1))"
[[ -f "$OUT/initramfs.cpio.gz" ]] || fetch "$RELEASE/initramfs.cpio.gz" "$OUT/initramfs.cpio.gz" || die "probe download failed"
ok "probe   out/initramfs.cpio.gz ($(du -h "$OUT/initramfs.cpio.gz" | cut -f1))"
if [[ ! -f "$OUT/vendor_minimal.img" ]]; then
    fetch "$RELEASE/vendor_minimal.img" "$OUT/vendor_minimal.img" || "$ROOT/scripts/make-minimal-vendor.sh" \
        || die "no vendor_minimal.img: release asset missing and mke2fs unavailable (brew install e2fsprogs)"
fi
ok "vendor  out/vendor_minimal.img ($(du -h "$OUT/vendor_minimal.img" | cut -f1) used)"

# ---------------------------------------------------------------- 2. build
say "building Apex VMM + ApexStudio.app"
if ! "$ROOT/scripts/build-macos.sh" >"$LOGS/build.log" 2>&1; then
    tail -n 30 "$LOGS/build.log" | sed 's/^/    /'
    die "build failed — see $LOGS/build.log"
fi
grep "^==>" "$LOGS/build.log" | sed 's/^/    /'
[[ -x "$OUT/apex" ]] || die "out/apex missing after build"
ok "$("$OUT/apex" version 2>/dev/null || echo apex built)"
"$OUT/apex" caps | tee "$LOGS/caps.log" | sed 's/^/    /'

# ---------------------------------------------------------------- acceptance
declare -a NAMES RESULTS
record() { NAMES+=("$1"); RESULTS+=("$2"); }

say "A. out/apex selftest"
"$OUT/apex" selftest 2>&1 | tee "$LOGS/A-selftest.log" | sed 's/^/    /'
case "${PIPESTATUS[0]}" in
    0) record "A selftest" PASS ;;
    77) record "A selftest" SKIP ;;
    *) record "A selftest" FAIL ;;
esac

say "B. scripts/launch-android.sh --probe"
if "$ROOT/scripts/launch-android.sh" --probe >"$LOGS/B-probe.log" 2>&1; then
    record "B kernel probe" PASS
else
    record "B kernel probe" FAIL
fi
tail -n 5 "$LOGS/B-probe.log" | sed 's/^/    /'

say "C. scripts/launch-android.sh --first-stage (downloads the 740 MiB GSI on first run)"
"$ROOT/scripts/launch-android.sh" --first-stage 120 2>&1 | tee "$LOGS/C-first-stage.log" | sed 's/^/    /'
case "${PIPESTATUS[0]}" in
    0) record "C first-stage mount" PASS ;;
    3) record "C first-stage mount" PARTIAL ;;
    *) record "C first-stage mount" FAIL ;;
esac

# ---------------------------------------------------------------- summary
echo
say "acceptance summary ($CHIP, macOS $OSV)"
FAILED=0
for i in "${!NAMES[@]}"; do
    r="${RESULTS[$i]}"
    case "$r" in PASS) c=32 ;; SKIP|PARTIAL) c=33 ;; *) c=31; FAILED=1 ;; esac
    printf '    %-22s \033[1;%sm%s\033[0m\n' "${NAMES[$i]}" "$c" "$r"
done
echo "    logs: $LOGS/  (guest console: out/probe.log, out/first-stage.log)"
[[ "${RESULTS[*]}" == *PARTIAL* ]] && FAILED=1
exit "$FAILED"
