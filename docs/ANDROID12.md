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

## Cloud vendor + system: pure 64-bit, from Google's prebuilt emulator build

`scripts/build-cloud-vendor.sh` (CI: `.github/workflows/cloud-vendor.yml`,
release `apex-android12-vendor`) produces a matched `system.img` +
`vendor.img` **without compiling AOSP**. The source is Google's official
Android 12L emulator image `system-images;android-32;default;arm64-v8a` r02,
i.e. `sdk_phone64_arm64-userdebug` build `SE1B.240122.005`, the 64-bit-only
product Google made for Apple Silicon hosts:

* `super` is unpacked (`tools/lpunpack.py`); `system_ext` and `product` are
  folded into `system` (GSI layout) so the Apex boot flow is unchanged:
  legacy system-as-root on vda2, first-stage mount of vendor only.
* labels and capabilities are preserved file by file (`tools/ext4tree.py`
  extracts with xattrs, `mke2fs -d` copies them back).
* vendor overlay (`guest/vendor-overlay`): `fstab.apex`, `apex.rc` (the
  device-independent parts of the emulator's `init.ranchu.rc`, which is not
  imported), the Apex input IDC/key layout, block-device labels for the
  Apex GPT, and the device identity (nubia NX769J / `pineapple` / QTI SM8650,
  `ro.opengles.version=196610`, `ro.vndk.version=32`, `ro.zygote=zygote64`).
* gate: 1555 ELF files across both images and the 23 APEX payloads — 1549
  AArch64, **0 32-bit**, and 6 eBPF programs (`e_machine` 247, loaded into
  the kernel by bpfloader, never executed by the CPU). A `--require-64bit`
  check must accept `EM_BPF`.
* userdebug: `androidboot.selinux=permissive` is honored, `adb root` works.

Vendor HALs (all 64-bit): keymaster 4.1 + gatekeeper (software), health 2.1,
power, thermal, lights, vibrator, usb, identity, rebootescrow, audio,
camera, sensors, gnss, wifi, NN samples, composer 2.4, allocator 3.0,
`/vendor/etc/vintf/manifest.xml` + fragments for all of them.

Boot on QEMU virt (android12-5.10 GKI, device-tree fstab, same disk layout):

```
[   11.277490] APEX: vendor mounted (cloud vendor), second stage init running
[   13.7     ] init: [libfs_mgr]fs_mgr_do_format: Format /dev/block/by-name/userdata as 'ext4'
[   69.830771] APEX: device nubia NX769J (redmagic9pro/NX769J), hardware qcom, platform pineapple, SoC QTI SM8650, GLES 196610, ABIs arm64-v8a
[   80.888922] APEX: keystore2 running                  <- starts once and stays up (no SIGSEGV)
[   80.830687] APEX: surfaceflinger running             <- restarts: no working composer/GLES
init: process with updatable components 'vendor.hwcomposer-2-4' exited 4 times before boot completed
```

### Graphics: what this vendor does and does not solve

The vendor's graphics stack is the emulator's **goldfish-opengl** stack:
`libEGL_emulation`/`libGLESv2_emulation`, `allocator@3.0` + `mapper@3.0`
(ranchu), `composer@2.4` (ranchu HWC), `vulkan.ranchu`, plus ANGLE
(`libEGL_angle`, which needs a Vulkan driver). All of them forward rendering
to a **host** renderer: through the goldfish pipe on QEMU, or through
virtio-gpu 3D with gfxstream. None of them draws in the guest, so on the 2D
virtio-gpu path SurfaceFlinger still cannot start. Two ways forward:

1. **gfxstream (no compilation):** keep this vendor and give it its host
   renderer: the Apex VMM already loads `libgfxstream_backend.dylib`
   (`display.renderer = "gfxstream"`, from the Android Emulator for macOS
   arm64). What is missing: the android12-5.10 guest kernel has no virtio-gpu
   blob/context-init, so the VMM must serve gfxstream through the classic 3D
   commands (`CTX_CREATE`, `SUBMIT_3D`, `RESOURCE_CREATE_3D`, transfers),
   and the boot configuration must select the emulator's virtio-gpu transport
   (`androidboot.hardware.gltransport=virtio-gpu-pipe`, gralloc/egl/hwc
   `ranchu`/`emulation`). Needs work and validation on the Mac.
2. **Guest software rendering:** SwiftShader (`vulkan.pastel` + ANGLE),
   minigbm gralloc 4.0, drm_hwcomposer on `/dev/dri/card0`. No prebuilt
   64-bit Android 12 binaries of these could be found. Building them needs an
   AOSP `android-12.1.0_r*` tree (~300 GB disk, 32+ GB RAM), which is more
   than this cloud container or a GitHub-hosted runner has.
