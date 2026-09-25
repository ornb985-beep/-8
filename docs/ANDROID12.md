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
| Minimal vendor | `out/vendor_minimal.img` (32 MiB ext4, `scripts/make-minimal-vendor.sh`): `etc/fstab.apex`, precompiled SELinux policy for the pinned GSI (`scripts/gen-vendor-sepolicy.sh`), `build.prop`, `etc/init/apex.rc` | On QEMU virt with the GSI: first-stage init mounts `/vendor`, loads the policy, second-stage init runs `apex.rc` (log below) |
| Acceptance | `scripts/ignite-mac.sh` | Host checks, release download, build, then A `apex selftest`, B `--probe`, C `--first-stage`; needs an Apple Silicon Mac |
| Launcher | `scripts/launch-android.sh` (6 vCPU, 8 GB, 120 Hz real-time vsync, ApexStudio.app) | Script logic; needs an Apple Silicon Mac |

Kernel and probe are rebuilt, boot-tested on QEMU and published to the
`apex-android12-kernel` release by `.github/workflows/kernel.yml`;
`launch-android.sh` downloads them from there.

## First GSI boot (before the minimal vendor)

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

## First-stage mount: how it is solved

The GSI boots legacy system-as-root (`root=/dev/vda2 ro init=/init`), so the
kernel runs `/init` straight from `system.img`. That rules out a ramdisk:
with an initrd the kernel would execute the ramdisk's `/init` instead, and
the GSI's init is dynamically linked against `/system`. In this mode Android
12's first-stage init reads its mount table from the **device tree**, exactly
like pre-`vendor_boot` phones:

```
/firmware/android { compatible = "android,firmware";
    fstab { compatible = "android,fstab";
        vendor { compatible = "android,vendor"; dev = "/dev/block/by-name/vendor";
                 type = "ext4"; mnt_flags = "ro"; fsmgr_flags = "wait"; }; }; };
```

The Apex VMM emits this node whenever a disk is attached, and passes
`androidboot.boot_devices=a000000.virtio_mmio` (virtio slot 0, the OS disk) so
ueventd creates `/dev/block/by-name/{misc,system,vendor,userdata}` from the
GPT partition names. `/system` is the root; `/data` is mounted in the second
stage from `/vendor/etc/fstab.apex`
(`androidboot.hardware=apex` selects that file).

Split SELinux then needs the vendor half of the policy. The minimal vendor
ships `precompiled_sepolicy`, compiled with `secilc` from the GSI's
`plat_sepolicy.cil` + `mapping/32.0.cil` and a generated
`plat_pub_versioned.cil`; init uses it because its
`plat_sepolicy_and_mapping.sha256` matches the GSI.

Same GSI, `vendor_minimal.img` as vda3, QEMU virt (`androidboot.boot_devices=a003e00.virtio_mmio` there):

```
[    3.678625][   T43] audit: type=1403 audit(...): auid=4294967295 ses=4294967295 lsm=selinux res=1
[    4.214920][    T1] init: Could not read properties from '/vendor/etc/selinux/vendor_property_contexts': No such file or directory
[    4.570839][    T1] init: Couldn't load property file '/vendor/default.prop': open() failed: No such file or directory
[    6.166497][  T139] linkerconfig: Check failed: !"undefined var" SANITIZER_DEFAULT_VENDOR is not defined
[    8.928993][  T137] APEX: minimal vendor mounted, second stage init running
[   17.067666][  T183] vdc: Command: cryptfs init_user0 Failed: Status(-8, EX_SERVICE_SPECIFIC): '0: '
```

Second-stage init, vold and the early services run. This GSI build still
comes up with SELinux enforcing (`androidboot.selinux=permissive` is ignored),
and the vendor files carry no labels, hence `avc: denied ... unlabeled` for
`vendor_init`. The next missing piece is the HALs.

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
