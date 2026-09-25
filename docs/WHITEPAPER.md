# Project Apex-AOSP 技术白皮书

> Apple Silicon 原生安卓虚拟手机引擎 —— 无 QEMU、无第三方依赖，直接基于 Hypervisor.framework、统一内存架构（UMA）与 Linux virtio 驱动，面向 120 FPS 渲染、低延迟触控与真机硬件拟态。

本文档对应仓库当前代码，每一节都标明对应的源码位置。第 14 节列出**哪些已经验证、哪些还没有**，请在评估前先读那一节。

---

## 0. 目标与设计原则

| 目标 | 设计手段 | 代码位置 |
|---|---|---|
| 120 FPS 满帧 | EDID 声明 120 Hz 面板；VMM 内部高精度虚拟 VSync；按 VSync 释放翻页 fence，使 SurfaceFlinger 锁定 120 Hz；三缓冲交换链 + `CAMetalDisplayLink` | `apex-devices/src/display.rs`、`virtio/gpu/` |
| 零拷贝上屏 | 交换链按 16 KiB 页对齐分配，Metal 用 `newBufferWithBytesNoCopy` 直接采样（UMA） | `display.rs`、`frontend/.../Renderer.swift` |
| 低延迟触控 | `NSEvent` → C ABI → 直接写入 guest virtio-input 事件缓冲 → 立即触发 SPI；无中间线程、无队列轮询 | `virtio/input.rs`、`input.rs` |
| 硬件级中断 | macOS 15+ 使用内核态 vGICv3（`hv_gic_*`），定时器与 IPI 不经过用户态 | `apex-hvf/src/vm.rs` |
| 真机拟态 | 设备身份经 bootconfig 注入；GPT 分区表与 `/dev/block/by-name`；电池镜像 Mac 真实电量；EDID 物理尺寸/DPI；PL031 RTC；SMCCC TRNG 早期熵 | `machine.rs`、`battery.rs`、`virtio/disk.rs` |
| 无黑盒 | 零第三方 crate；Hypervisor.framework FFI、FDT、gzip/LZ4、TOML、GICv3 全部自研并有单元测试 | 全仓库 |

明确放弃：QEMU/TCG（软件翻译与通用设备模型开销）、Wayland 转发层、LSPosed 类运行时注入。

---

## 1. 总体架构

```
┌──────────────────────── ApexStudio.app (Swift / AppKit / Metal) ────────────────────────┐
│  PhoneView: 鼠标/触控板多指/键盘 → ApexTouch/按键        HardwarePanel: 电源 音量 返回 主页 多任务 │
│  Renderer: CAMetalDisplayLink(120 Hz, 独立渲染线程) → apex_display_acquire → MTLBuffer(no-copy) │
│  BatteryMirror: IOKit 电源 → apex_battery_set                                              │
└───────────────────────────────────────────┬──────────────────────────────────────────────┘
                                   C ABI  include/apex.h  (libapex_vmm.a)
┌───────────────────────────────────────────┴──────────────────────────────────────────────┐
│ apex-vmm      Machine 装配 · 设备树 · vCPU 运行循环 · PSCI · C ABI · `apex` CLI          │
│ apex-devices  virtio-mmio v2 · blk · gpu · input · console · net · rng · PL011 · PL031 · 电池 │
│ apex-arm64    ESR 解码 · 系统寄存器 · PSCI/SMCCC/TRNG · GICv3 模型 · 引导协议 · 解压        │
│ apex-core     客户机内存 · MMIO 总线 · IRQ · FDT · TOML · 同步原语 · 最小 libc FFI            │
│ apex-hvf      Hypervisor.framework 绑定（vGIC 等新 API 运行时 dlsym）                       │
└───────────────────────────────────────────┬──────────────────────────────────────────────┘
                     Hypervisor.framework（XNU 管理 EL2、Stage-2 页表、vGICv3）
                                  Apple Silicon（M1 … M4+）
```

单进程、单地址空间：前端、VMM、设备模型运行在同一进程，guest 内存就是本进程的匿名映射，GPU 通过 UMA 直接读取。

---

## 2. CPU 虚拟化（`apex-hvf`、`apex-vmm/src/vcpu.rs`）

* **一 vCPU 一线程**：HVF 要求 vCPU 在创建线程上运行与销毁。线程启动即设置 `QOS_CLASS_USER_INTERACTIVE`，调度器将其放到 P 核。
* **统一时基**：VM 创建时取一次 `mach_absolute_time()` 作为所有 vCPU 的 `vtimer_offset`，保证各核 `CNTVCT_EL0` 一致。
* **MPIDR**：`Aff0 = idx % 16`、`Aff1 = idx / 16`，使任意 16 核可被一条 `ICC_SGI1R_EL1` 目标列表寻址。
* **退出处理**（全部在 vCPU 线程内同步完成，无跨线程往返）：

| 异常类 (EC) | 处理 |
|---|---|
| 0x24 数据异常 | 用 ISS（SAS/SRT/SSE/SF/WnR）解码 MMIO，经无锁二分查找总线分发，符号扩展后写回寄存器，PC+4 |
| 0x18 MSR/MRS | 用户态 GIC 模式下服务 `ICC_*`；调试/PMU/OS lock 寄存器 RAZ/WI；未知寄存器告警一次 |
| 0x01 WFI | 读取 `CNTV_CTL/CVAL` 计算到期时间，条件变量睡眠；设备中断或 IPI 通过 kick 唤醒 |
| 0x16 HVC / 0x17 SMC | PSCI 1.1：`CPU_ON/OFF/SUSPEND`、`AFFINITY_INFO`、`SYSTEM_OFF/RESET/RESET2`、`PSCI_FEATURES`；SMCCC 1.1；**SMCCC TRNG**（内核在任何驱动加载前即可获得硬件级熵） |
| VTIMER_ACTIVATED | 用户态 GIC 模式：拉高 PPI 27，并在 guest 清除条件后解除屏蔽 |

运行循环针对 `VirtualCpu` trait 编写，单元测试使用脚本化 vCPU 覆盖 MMIO、PSCI CPU_ON 次核启动、GIC 系统寄存器、异常停机等路径。

---

## 3. 中断（`apex-arm64/src/gic.rs`、`apex-hvf/src/vm.rs`）

**模式一：内核态 vGICv3（macOS 15+，默认）**
`hv_gic_config_create → set_distributor_base/redistributor_base → hv_gic_create`，在任何 vCPU 创建前完成。设备中断 `hv_gic_set_spi(intid, level)`；虚拟定时器与 SGI 由宿主内核直接注入，不产生用户态退出。几何参数（重分发器步长、SPI 范围、定时器 INTID）运行时向框架查询并写入设备树。

**模式二：用户态 GICv3（macOS 14 或 `gic = "emulated"`）**
完整实现分发器、重分发器、`ICC_*` CPU 接口：组 1、5 位优先级、抢占（BPR1）、`EOImode` 0/1、`ICC_SGI1R` 亲和路由与广播、`ICC_AP1R0` 活动优先级。IRQ 线通过 `hv_vcpu_set_pending_interrupt` 注入；电平变化时 `hv_vcpus_exit` 把目标 vCPU 踢出。

新 API（macOS 13 的 VM 配置、macOS 15 的 vGIC）全部用 `dlsym(RTLD_DEFAULT, …)` 运行时解析——**同一个二进制**在 macOS 14 上自动回退到模式二。

---

## 4. 内存（`apex-arm64/src/layout.rs`、`apex-core/src/mem.rs`）

| IPA | 用途 | 中断 |
|---|---|---|
| `0x0800_0000` | GICv3 分发器 64 KiB | — |
| `0x080a_0000` | GICv3 重分发器，每 vCPU 128 KiB（最多 123 核） | — |
| `0x0900_0000` | PL011 UART | SPI 1 |
| `0x0901_0000` | PL031 RTC | SPI 2 |
| `0x0902_0000` | goldfish 电池 | SPI 3 |
| `0x0a00_0000` | virtio-mmio × 32，每槽 4 KiB | SPI 16–47 |
| `0x8000_0000` | RAM（最多 30 GiB） | — |
| `0x8_0000_0000` | GPU host-visible 共享窗口 8 GiB（virtio shm id 1） | — |

全部位于 36 位 IPA 空间内（所有 Apple Silicon 都支持）。RAM 为 `MAP_NORESERVE` 匿名映射，按需提交：8 GiB guest 启动时常驻内存仅数 MB。virtqueue 环索引使用 acquire/release 原子访问。

---

## 5. 120 Hz 显示流水线（`display.rs`、`virtio/gpu/`）

```
Android App ─ RenderThread ─ SurfaceFlinger ─ HWC3 (drm_hwcomposer) ─ DRM atomic commit
                                                                        │ virtio-gpu
      ┌─────────────────────────────── TRANSFER_TO_HOST_2D / guest blob ┘
      ▼
  virtio-gpu 工作线程 ──RESOURCE_FLUSH──▶ 交换链槽位（16 KiB 对齐，stride 256 B 对齐）
      ▲                                           │
      │ 在下一个虚拟 VSync 释放 flush fence         ▼
  虚拟 VSync 节拍器（120 Hz，sleep+自旋）   CAMetalDisplayLink（120 Hz ProMotion）
                                           newBufferWithBytesNoCopy → 采样 → present
```

1. **声明 120 Hz**：virtio-gpu 协商 `VIRTIO_GPU_F_EDID`，VMM 生成 EDID 1.4（CVT-RB 时序，像素时钟四舍五入后回算刷新率误差 < 0.05 Hz，含物理尺寸 mm）。Linux DRM 据此创建 1080×2400@120 模式，drm_hwcomposer 与 SurfaceFlinger 得到 120 Hz 显示配置。
2. **节拍锁定**：Linux virtio-gpu 在 dumb buffer 翻页时对 `RESOURCE_FLUSH` 附带 fence 并等待。VMM 把带 fence 的 flush 响应**暂扣到下一次虚拟 VSync** 才归还（同一时间线上后续 fence 按序排队，保持 fence 单调语义）。结果：guest 的每次翻页严格对齐 8.333 ms 节拍。
3. **与宿主显示解耦**：`vsync = "internal"` 时节拍来自 VMM 自己的节拍线程：线程申请 Mach 实时调度（`THREAD_TIME_CONSTRAINT_POLICY`，与 CoreAudio/CoreVideo 同类），以宿主计数器上的**绝对截止时间**调用 `mach_wait_until`，最后 100 µs 自旋；宿主过载时错过的周期计入 `missed_vsyncs` 而不是补发突发 VSync。即使外接 60 Hz 显示器，guest 仍按 120 Hz 运行；`vsync = "host"` 时由前端的 `CAMetalDisplayLink` 回调驱动。
4. **三缓冲零拷贝**：一个槽在写、一个是最新帧、一个被 GPU 占用。前端为每个槽只创建一次 `MTLBuffer(bytesNoCopy:)`，`makeTexture(descriptor:offset:bytesPerRow:)` 得到线性纹理，着色器按 guest 像素格式做通道重排。槽位在最后一个引用它的命令缓冲完成后才归还（`PinnedFrame`）。
5. **guest blob**：Linux 在支持 `RESOURCE_BLOB` 时把 dumb buffer 创建为 guest 内存 blob 并用 `SET_SCANOUT_BLOB` 扫描输出——此时连 `TRANSFER_TO_HOST_2D` 都不需要，VMM 直接从 guest 物理页组帧。

**单帧延迟预算（1080×2400，理论值）**

| 阶段 | 典型耗时 |
|---|---|
| guest 合成完成 → flush 陷入 VMM | < 10 µs（一次 MMIO 退出） |
| 组帧拷贝 10 MB（M 系列内存带宽 100 GB/s 级） | ≈ 0.1–0.3 ms |
| 等待 VSync（平均半帧） | 0–8.3 ms |
| Metal 采样 + 合成 + 上屏 | 1 帧（`preferredFrameLatency = 1`） |

---

## 6. GPU 渲染路径

| 模式 | guest 栈 | 宿主 | 适用 |
|---|---|---|---|
| `renderer = "guest"`（默认） | ANGLE(GLES) → SwiftShader(Vulkan) → minigbm → drm_hwcomposer | virtio-gpu 2D/blob | 开箱即用，UI 与轻量应用 |
| `renderer = "gfxstream"` | gfxstream guest GLES/Vulkan 编码器 → ranchu HWC | `libgfxstream_backend.dylib`（MoltenVK/ANGLE-Metal） | 3D 游戏、重负载 |

gfxstream 通过其稳定 C ABI（`stream_renderer_*`，API 0.1.2）以 `dlopen` 加载，实现了上下文、3D 资源、`SUBMIT_3D`、按上下文/环的 fence 时间线（`CONTEXT_INIT`）、`RESOURCE_CREATE_BLOB`（HOST3D）与 `RESOURCE_MAP_BLOB`：宿主可见内存经 `hv_vm_map` 映射进 shm 窗口，guest CPU 与宿主 GPU 共享同一物理页。

---

## 7. 输入（`input.rs`、`virtio/input.rs`、`PhoneView.swift`）

* **触摸屏**：`INPUT_PROP_DIRECT`、MT 协议 B（`ABS_MT_SLOT/TRACKING_ID/POSITION_X/Y/PRESSURE/TOUCH_MAJOR/TOOL_TYPE`）、10 个触点、分辨率单位按 DPI 换算为 dots/mm；`TouchTracker` 负责槽位分配与增量编码，每帧以 `SYN_REPORT` 结束，未变化帧不产生事件。
* **映射方式**：鼠标 = 单指；⌥+拖动 = 以屏幕中心镜像的双指捏合/旋转；右键 = 返回；**触控板模式**：Mac 触控板上的每根手指绝对映射为手机上的一个触点（最多 10 指）。
* **按键**：拆分为「Apex Buttons」（电源/音量/返回/主页/多任务，非字母键盘，因此 Android 保留软键盘）与可选「Apex Keyboard」（Mac 键盘打字）。
* **延迟路径**：UI 线程 → `apex_touch_frame` → 互斥锁内直接写入 guest eventq 缓冲 → `hv_gic_set_spi`。无额外线程切换。

---

## 8. 存储（`virtio/disk.rs`、`virtio/blk.rs`）

* **复合 GPT 磁盘**：配置文件列出分区（`misc/vbmeta/super/metadata/userdata …`），VMM 在内存中合成保护性 MBR、主/备 GPT（CRC32 校验、1 MiB 对齐、确定性 GUID），分区内容映射到各自镜像文件。guest 看到一块真正带分区表的盘，ueventd 创建 `/dev/block/by-name/*`，与 UFS 真机一致。
* **virtio-blk**：多队列、`SEG_MAX`、4 KiB 物理块拓扑、FLUSH、DISCARD（APFS `F_PUNCHHOLE` 回收空间）、WRITE_ZEROES、只读分区；`pread/pwrite` 直接读写 guest 内存页，无中间缓冲。缓存模式 `writeback`（FLUSH → `F_FULLFSYNC`）/ `unsafe`。

---

## 9. 真机硬件拟态

| 项 | 实现 |
|---|---|
| 设备身份 | `[device]` → bootconfig `androidboot.serialno` / `androidboot.apex.{manufacturer,brand,model,device}` |
| 屏幕 | EDID 型号、物理尺寸、DPI → `androidboot.lcd_density` → `ro.sf.lcd_density` |
| 电池 | goldfish 电池（容量、充电状态、电压、温度、电流、循环次数），`BatteryMirror` 每 30 s 同步 Mac 电源状态 |
| 时钟 | PL031 RTC（可写入、支持闹钟中断） |
| 熵 | FDT `rng-seed` + `kaslr-seed`、SMCCC TRNG、virtio-rng（全部来自宿主 `getentropy`） |
| 存储 | GPT + by-name，同 UFS 手机布局 |
| 网络 | virtio-net，后端 gvproxy（用户态 NAT、端口转发，免 root）/ socket_vmnet / 前端 vmnet |

---

## 10. 引导：VMM 即 Bootloader（`apex-arm64/src/boot/`）

1. 解析 `boot.img`（头 v0–v4）、`init_boot.img`、`vendor_boot.img`（v3/v4，含 ramdisk 表与 bootconfig 段）。
2. 内核支持原始 `Image`、`Image.gz`（自研 inflate，CRC32 校验）、`Image.lz4`（内核 legacy 格式与标准帧格式，XXH32 校验）。
3. initrd = vendor ramdisk（按表跳过 recovery 片段）+ generic ramdisk + bootconfig 尾（`#BOOTCONFIG\n`，4 字节对齐、校验和），并自动追加 `bootconfig` 内核参数。
4. VMM 生成 `androidboot.*`：`hardware=apex`、`boot_devices=a000000.virtio_mmio`、`lcd_density`、图形栈选择、`apex.refresh_rate` 等；配置文件 `[boot.bootconfig]` 可覆盖。
5. 布局：内核放在 RAM 起始 2 MiB 对齐处，DTB 放在顶端 2 MiB，initrd 紧贴其下；vCPU0 以 `x0 = DTB`、`PSTATE = EL1h|DAIF`、MMU 关闭进入内核。
6. 设备树：CPU（PSCI/HVC）、armv8 定时器 PPI、GICv3、PL011/PL031、goldfish 电池、virtio-mmio 节点（`dma-coherent`）、`/chosen`（bootargs、initrd、随机种子）。

---

## 11. 构建与运行

### 11.1 宿主要求
* Apple Silicon Mac，macOS 14+（macOS 15+ 启用内核 vGIC），Xcode 15+（Swift 5.9+）
* Rust stable（`rustup`），1.85+

### 11.2 构建 VMM 与前端
```bash
scripts/build-macos.sh          # cargo build + swift build + 组装 ApexStudio.app + 带 hypervisor 授权签名
out/apex caps                   # 查看 HVF 能力：最大 vCPU、IPA 位宽、是否有内核 vGIC
```
`com.apple.security.hypervisor` 授权为必需；ad-hoc 签名即可本机运行。

### 11.3 构建 guest 内核
在 Android Common Kernel（`android16-6.12` 或更新）上叠加 `guest/kernel/apex_virt.fragment`（关键：`CONFIG_VIRTIO_MMIO=y`，GKI 默认只有 virtio-pci 且为模块）。

### 11.4 构建 Android
```bash
cp -r guest/aosp/device/apex $AOSP/device/
cd $AOSP && source build/envsetup.sh
lunch apex_phone-trunk_staging-userdebug
m bootimage initbootimage vendorbootimage superimage
```
产物：`boot.img`、`init_boot.img`、`vendor_boot.img`、`super.img`；另建空的 `misc.img`（1 MiB）、`metadata.img`（16 MiB）、`userdata.img`（如 16 GiB 稀疏文件）、`vbmeta.img`，放入 `images/`。

### 11.5 运行
```bash
gvproxy -listen-vfkit unixgram:///tmp/apex-net.sock &      # 可选：网络 + adb
out/apex run profiles/phone-120hz.toml                      # 无界面
open -a out/ApexStudio.app --args $PWD/profiles/phone-120hz.toml   # 图形界面
out/apex inspect profiles/phone-120hz.toml --dts            # 不运行，仅装配并打印设备树（任意平台）
out/apex bootimg images/vendor_boot.img                     # 解析 Android 引导镜像
out/apex selftest                                           # 内置裸机客户机自检（见 13 节）
```
先用任意 arm64 Linux 内核验证 VMM：`apex boot --kernel Image --cmdline "console=ttyAMA0 earlycon"`。

---

## 12. 配置文件参考（`profiles/*.toml`）

| 段 | 键 | 说明 |
|---|---|---|
| `[device]` | `manufacturer` `brand` `model` `name` `serial` | 设备身份 |
| `[vm]` | `cpus`(1–64) `memory`("8G") `gic`(auto/hardware/emulated) `disk_cache`(writeback/unsafe) `block_queues` | |
| `[boot]` | `kernel`+`initrd`+`cmdline` 或 `boot_image`+`init_boot_image`+`vendor_boot_image`+`cmdline` | 二选一 |
| `[boot.bootconfig]` | 任意 `androidboot.*` | 覆盖默认值 |
| `[display]` | `width` `height` `refresh`(1–240) `dpi` `name` `vsync`(internal/host) `renderer`(guest/gfxstream) `gfxstream_library` | |
| `[[disk]]` | `path` `readonly` `serial`，或 `name` + `[[disk.partition]]`(`name` `path` `readonly`) | 原始盘 / 复合 GPT 盘 |
| `[network]` | `mode`(none/unixgram/unixstream/host) `socket` | |
| `[console]` | `serial`(stdout/null/callback/file:路径) | |
| `[input]` | `touch_slots` `keyboard` | |

未知键会报错（防止拼写错误静默失效）。

---

## 13. 代码结构与测试

| crate | 行为 | 单元测试 |
|---|---|---|
| `apex-core` | 内存、总线、IRQ、FDT 读写、TOML、CRC32、同步、FFI | 15 |
| `apex-arm64` | ESR、sysreg（数值与 `hv_sys_reg_t` 对照）、PSCI/TRNG、GICv3、引导镜像、bootconfig、gzip、LZ4 | 37 |
| `apex-devices` | virtqueue（EVENT_IDX、间接描述符、环路检测）、virtio-mmio 握手、blk、GPT、gpu（VSync 节拍 fence）、EDID、input、console、net、显示交换链、PL011/PL031/电池 | 37 |
| `apex-vmm` | 配置、设备树、vCPU 循环（脚本化 vCPU）、C ABI、**整机端到端**（Android 镜像 → 装配 → vCPU 从内核入口启动 → MMIO → UART → PSCI 关机） | 11 |

```bash
scripts/check.sh    # fmt + clippy(-D warnings) + 全部测试 + C ABI 冒烟测试 + CLI 装配 + 自检镜像一致性
```

**`apex selftest`（真机自检）**：`guest/selftest/selftest.S` 是一段手写 AArch64 裸机程序，伪装成 Linux `Image` 走正常引导路径，依次验证：PL011 MMIO 写（带 ISV 的数据异常）、virtio-mmio 魔数读取、GICv3 分发器/重分发器/ICC 初始化、vCPU 处于 WFI 时虚拟定时器中断经 GIC 投递、PSCI `CPU_ON` 拉起 vCPU 1、PSCI `SYSTEM_OFF`。分别在用户态 GIC 与内核态 vGIC 两种模式下运行，期望控制台输出 `APEX selftest: MGT2`。宿主无法创建虚拟机时返回退出码 77（跳过）。
CI（`.github/workflows/ci.yml`）：Linux 跑全部检查并对 `aarch64-apple-darwin` 做 clippy；macOS arm64 跑测试、构建签名 `ApexStudio.app` 并上传产物。

---

## 14. 验证状态与已知限制（请务必阅读）

**已验证（本仓库 CI / 本地）**
* 全部 Rust 代码在 Linux 上编译、100 个单元/集成测试通过、clippy 零警告；HVF 后端在 `aarch64-apple-darwin` 目标下通过类型检查与 clippy。
* C 头文件与 `libapex_vmm.a` 链接通过，结构体布局经 `_Static_assert` 校验。
* 整机装配路径（Android v4 镜像 → initrd/bootconfig → 设备树 → vCPU 启动状态）在模拟 hypervisor 上端到端通过。
* **在 GitHub Actions 的 Apple Silicon（macOS 15）runner 上**：Rust 原生测试全部通过；Swift 前端编译链接成功并打包签名为 `ApexStudio.app`；C ABI 冒烟测试通过；`apex caps` 在真机上确认内核态 vGICv3 符号可运行时解析、`CNTFRQ_EL0 = 24 MHz`。

**尚未验证（需要 Apple Silicon 真机）**
* 真实 `hv_vcpu_run` 下启动 Linux / Android——HVF 绑定按 Apple 头文件与 QEMU/applevisor 交叉核对编写。CI runner 本身是虚拟机、没有嵌套虚拟化（实测 Hypervisor.framework 返回 `HV_UNSUPPORTED`），`apex selftest` 在那里如实报告跳过；请在本机 Mac 上运行 `out/apex selftest` 作为第一步验证。
* vCPU 按索引串行创建、全部创建完成后才统一放行（与 QEMU 一致），以保证内核 vGIC 的重分发器顺序与 MPIDR 对应——该假设需在真机自检中确认。
* Swift 前端需在 macOS CI 或本机 `swift build` 验证；120 Hz 实测帧率与触控延迟需真机测量。
* 内核态 vGIC 模式下，若 HVF 仍将 WFI 陷入用户态，VMM 以 ≤500 µs 的睡眠上限兜底（无法感知宿主内核内的 SGI）；QEMU 的实现表明该模式下 WFI 由框架内部处理，需实测确认。
* gfxstream 集成按官方头文件实现，需自行为 macOS 构建 `libgfxstream_backend.dylib` 后验证。
* `guest/aosp` 设备树为参考实现，需在完整 AOSP 源码树中构建验证。

**已知限制 / 路线图**
1. 传输层只有 virtio-mmio：原版 GKI/Cuttlefish 镜像（virtio-pci）需使用本仓库内核片段重编内核。计划：virtio-pci + ECAM + MSI（`hv_gic_send_msi`）。
2. 无 virtio-snd（音频）、virtio-vsock（adb 目前走网络）、摄像头、传感器 HAL 通道、蓝牙/Wi-Fi 模拟、电话。
3. 无快照/热迁移（HVF 提供 `hv_gic_state_*` 与寄存器读写，可实现）。
4. MMIO 访问缺少 ISV 时（非常规加载/存储指令）目前报错停机，未做指令解码回退。
5. 多显示器、旋转、折叠屏形态尚未实现（virtio-gpu 支持多 scanout，DisplayHub 需扩展）。
