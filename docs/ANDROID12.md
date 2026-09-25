# Android 12 on Apex — status and bring-up log

Target baseline: **Android 12L (API 32), ARM64**, kernel **android12-5.10 GKI**.

## What is delivered and verified

| Part | Artifact | Verification |
|---|---|---|
| Kernel | `out/Image.gz` — android12-5.10 (5.10.269) + `guest/kernel/android12-5.10.fragment` | Built with clang/LLVM; `ARM64_4K_PAGES, VIRTIO_MMIO, VIRTIO_BLK, VIRTIO_CONSOLE, DRM_VIRTIO_GPU, VIRTIO_INPUT, ANDROID_BINDER_IPC, ASHMEM` all `=y`, **0 modules** (`scripts/build-kernel.sh` fails the build otherwise) |
| Probe | `out/initramfs.cpio.gz` (725 bytes, libc-free `/init`, `guest/probe/init.S`) | Boots on QEMU `virt` (GICv3 + PL011 + virtio-mmio) in ~2 s: prints `APEX VMM OK` and powers off (`scripts/qemu-probe.sh`) |
| GSI | Google Android 12L GSI `aosp_arm64-exp-SQ3A.220705.003.A1` (system.img 1.7 GB) | Downloaded from dl.google.com; the zip contains **only system.img + vbmeta.img** |
| Disk | `apex mkdisk` → `out/android_disk.raw` (GPT: vda1 misc, vda2 system, vda3 vendor, vda4 userdata; sparse, 10 GiB apparent / 1.6 GiB used) | Kernel sees the GPT, mounts system on /dev/vda2 |
| Boot image | `apex mkbootimg` → `out/boot.img` (header v4) | Round-trip unit test |
| Launcher | `scripts/launch-android.sh` (6 vCPU, 8 GB, 120 Hz real-time vsync, ApexStudio.app) | Script logic; needs an Apple Silicon Mac |

Kernel and probe are rebuilt, boot-tested on QEMU and published to the
`apex-android12-kernel` release by `.github/workflows/kernel.yml`;
`launch-android.sh` downloads them from there.

## Where Android 12 stops today

Booting the unmodified Google GSI with `root=/dev/vda2 ro init=/init` on QEMU virt:

```
[    1.821204][    T1] ** If you see this message and you are not debugging the          **
[    1.821285][    T1] ** kernel, report this immediately to your vendor!                **
[    1.821357][    T1] **                                                                **
[    1.821428][    T1] **     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE           **
[    1.821498][    T1] ********************************************************************
[    2.754379][    T1] init: Failed to create FirstStageMount failed to read default fstab for first stage mount
[    2.755361][    T1] init: Failed to mount required partitions early ...
[    2.769961][    T1] init: InitFatalReboot: signal 6
[    2.882824][    T1] init: #00 pc 0000000000125550  /system/bin/init (android::init::InitFatalReboot(int)+104)
[    2.883316][    T1] init: #01 pc 00000000000bd894  /system/bin/init (android::init::InitAborter(char const*)+48)
[    2.883655][    T1] init: #02 pc 000000000001595c  /system/lib64/libbase.so (android::base::SetAborter(std::__1::function<void (char const*)>&&)::$_3::__invoke(char const*)+76)
[    2.889932][    T1] reboot: Restarting system with command 'bootloader'
qemu exit 0
```

The kernel and the GSI are fine; **the device half of Android is missing**.
A GSI is designed to run on top of a device's own `vendor` partition
(fstab, init scripts, and the HALs: composer, gralloc, keymaster, health,
audio, ...). Google does not publish a generic vendor image, so no
download-only combination can reach the launcher. The GSI plus an empty
vendor partition cannot render anything even after the fstab issue: without
a composer HAL SurfaceFlinger cannot start.

## What is needed for the launcher (next step)

Build **only the vendor side** for this machine from AOSP `android-12.1.0_r*`:
`vendor.img` + `vendor_boot.img` (first-stage fstab) for the `apex_phone`
device, containing drm_hwcomposer (HWC2 on virtio-gpu, which already has an
EDID-driven 120 Hz mode), minigbm gralloc, SwiftShader/ANGLE, the software
keymaster and health HALs. The system half stays Google's GSI. This needs a
full AOSP checkout (~250 GB disk, 32+ cores for a reasonable build time),
more than this CI container has. The device tree in `guest/aosp` currently
targets Android 16 HAL versions and must be ported to Android 12 (HWC2 instead
of HWC3, AIDL/HIDL versions of that release).

Then: `scripts/launch-android.sh --vendor out/vendor.img`.

An alternative worth evaluating first is the vendor image of Google's
Android Emulator system image for API 32 (arm64): it is a ready-made vendor
for a virtual device, but it expects QEMU "goldfish pipe" devices that the
Apex VMM does not implement.
