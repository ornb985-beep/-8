# Project Apex-AOSP

**在 Apple Silicon Mac 上以 120 Hz 运行 Android 的原生虚拟手机引擎。**
A native Android phone engine for Apple Silicon Macs — Hypervisor.framework, unified memory, virtio, 120 Hz. No QEMU, no third-party crates.

```
ApexStudio.app (Swift · AppKit · Metal · CAMetalDisplayLink 120 Hz)
        │  C ABI (include/apex.h)
apex-vmm ─ apex-devices ─ apex-arm64 ─ apex-core        (Rust, zero dependencies)
        │
apex-hvf → Hypervisor.framework (EL2 · stage-2 · in-kernel vGICv3 on macOS 15+)
```

## Highlights

- **120 FPS pipeline** — EDID advertises a 120 Hz panel; a precise virtual vsync releases the guest's page-flip fences, locking SurfaceFlinger to 8.33 ms; triple-buffered swapchain presented zero-copy with `newBufferWithBytesNoCopy`.
- **Hardware interrupts** — Apple's in-kernel vGICv3 on macOS 15+ (resolved at runtime), full userspace GICv3 fallback on macOS 14.
- **VMM as bootloader** — parses Android `boot.img` v0–v4, `init_boot.img`, `vendor_boot.img` v4, builds initrd + bootconfig, gzip/LZ4 kernels, generates the device tree.
- **Phone-grade devices** — virtio-gpu (2D, guest blobs, gfxstream 3D), multitouch virtio-input (MT protocol B, Mac trackpad → fingers), virtio-blk with synthesized GPT (`/dev/block/by-name/*`), virtio-net, console, rng, PL011, PL031, goldfish battery mirrored from the Mac.
- **Tested** — 100 unit/integration tests incl. an end-to-end machine boot on a scripted hypervisor; clippy-clean on Linux and `aarch64-apple-darwin`.

## Quick start (Apple Silicon, macOS 14+)

```bash
scripts/build-macos.sh                              # VMM + ApexStudio.app, signed with the hypervisor entitlement
out/apex caps                                       # host virtualization capabilities
out/apex boot --kernel Image                        # smoke test with any arm64 Linux kernel
out/apex run profiles/phone-120hz.toml              # Android, headless
open -a out/ApexStudio.app --args "$PWD/profiles/phone-120hz.toml"
```

Guest kernel fragment: `guest/kernel/apex_virt.fragment` · AOSP device: `guest/aosp/device/apex/apex_phone` (`lunch apex_phone-trunk_staging-userdebug`).

## Documentation

**[docs/WHITEPAPER.md](docs/WHITEPAPER.md)** — 完整技术白皮书：架构、CPU/中断/内存虚拟化、120 Hz 显示流水线与延迟预算、GPU 路径、输入、存储、真机拟态、引导流程、构建运行、配置参考，以及**验证状态与已知限制**（HVF 实机运行与 Swift 前端需在 Mac 上验证）。

## Layout

| Path | Contents |
|---|---|
| `crates/apex-core` | guest memory, MMIO bus, IRQ, FDT, TOML, sync, libc FFI |
| `crates/apex-arm64` | exception decode, PSCI/SMCCC/TRNG, GICv3 model, boot protocols, gzip/LZ4 |
| `crates/apex-devices` | virtio-mmio + devices, display swapchain, PL011/PL031/battery |
| `crates/apex-hvf` | Hypervisor.framework bindings (macOS/aarch64; stub elsewhere) |
| `crates/apex-vmm` | machine assembly, vCPU loop, C ABI, `apex` CLI |
| `frontend/ApexStudio` | SwiftPM macOS app (Metal renderer, input, hardware panel) |
| `guest/` | kernel config fragment, AOSP device tree |
| `profiles/` | device profiles (phone / tablet, 120 Hz) |
| `scripts/` | `build-macos.sh`, `check.sh` |

## License

Apache-2.0
