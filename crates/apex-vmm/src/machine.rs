//! Assembles the `apex-virt` machine from a [`VmConfig`]: guest RAM,
//! interrupt controller, devices, boot payload and device tree.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use apex_arm64::boot::{self, android, image};
use apex_arm64::cpu::EntryState;
use apex_arm64::gic::{self, GicV3};
use apex_arm64::layout::{self, *};
use apex_core::bus::{MmioBus, MmioDevice};
use apex_core::hv::{GicMode, HvConfig, Hypervisor, MemFlags};
use apex_core::irq::{InterruptController, IrqLine};
use apex_core::mem::{GuestAddress, GuestMemory};
use apex_core::{Error, Result};
use apex_devices::battery::GoldfishBattery;
use apex_devices::display::DisplayHub;
use apex_devices::pl011::Pl011;
use apex_devices::pl031::Pl031;
use apex_devices::virtio::blk::Block;
use apex_devices::virtio::console::{Console, ConsoleInput};
use apex_devices::virtio::disk::{CompositeDisk, DiskBackend, PartitionSpec, RawDisk};
use apex_devices::virtio::gpu::gfxstream::{self, Gfxstream};
use apex_devices::virtio::gpu::renderer::{HostMapper, Renderer3d};
use apex_devices::virtio::gpu::{Gpu, GpuOptions, RendererFactory};
use apex_devices::virtio::input::{Input, InputHandle, InputSpec};
use apex_devices::virtio::mmio::MmioTransport;
use apex_devices::virtio::net::{self, ChannelBackend, Net, NetBackend, UnixDgramBackend, UnixStreamBackend};
use apex_devices::virtio::rng::Rng;
use apex_devices::virtio::{VirtioDevice, VirtioInterrupt};

use crate::config::*;
use crate::fdt_gen::{self, FdtParams, VirtioNode};
use crate::vcpu::{vcpu_thread, CpuSet, HwIrqChip, StopReason, VcpuHub};

pub type ByteSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Callbacks supplied by the embedding application.
#[derive(Default, Clone)]
pub struct HostHooks {
    /// Serial/console output when `console.serial = "callback"`.
    pub serial: Option<ByteSink>,
    /// Ethernet frames leaving the guest when `network.mode = "host"`.
    pub net_tx: Option<ByteSink>,
}

/// Handles the frontend uses to talk to the running phone.
#[derive(Clone)]
pub struct Controls {
    pub display: Arc<DisplayHub>,
    pub touch: InputHandle,
    pub keys: InputHandle,
    pub console: ConsoleInput,
    pub battery: Arc<GoldfishBattery>,
    pub uart: Arc<Pl011>,
    pub net_rx: Option<Arc<ChannelBackend>>,
}

pub struct Machine {
    pub controls: Controls,
    pub dtb: Vec<u8>,
    pub cmdline: String,
    cfg: VmConfig,
    hub: Arc<VcpuHub>,
    cpus: Arc<CpuSet>,
    boot_entry: EntryState,
    threads: Mutex<Vec<JoinHandle<()>>>,
    _transports: Vec<Arc<MmioTransport>>,
    _mem: GuestMemory,
}

struct HvMapper(Arc<dyn Hypervisor>);

impl HostMapper for HvMapper {
    unsafe fn map(&self, host: *mut u8, gpa: u64, size: u64) -> Result<()> {
        self.0.map_memory(host, gpa, size, MemFlags::RW)
    }
    fn unmap(&self, gpa: u64, size: u64) -> Result<()> {
        self.0.unmap_memory(gpa, size)
    }
}

fn sink_for(s: &SerialSink, hooks: &HostHooks) -> Result<ByteSink> {
    Ok(match s {
        SerialSink::Stdout => Arc::new(|b: &[u8]| {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(b);
            let _ = o.flush();
        }),
        SerialSink::Null => Arc::new(|_: &[u8]| {}),
        SerialSink::File(p) => {
            let f = Mutex::new(File::create(p).map_err(|e| Error::Config(format!("{}: {e}", p.display())))?);
            Arc::new(move |b: &[u8]| {
                let _ = f.lock().unwrap().write_all(b);
            })
        }
        SerialSink::Callback => hooks.serial.clone().unwrap_or_else(|| Arc::new(|_: &[u8]| {})),
    })
}

fn read_file(p: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(p).map_err(|e| Error::Boot(format!("cannot read {what} {}: {e}", p.display())))
}

struct Payload {
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    cmdline: String,
}

impl Machine {
    /// Build with the platform hypervisor (Hypervisor.framework).
    pub fn build(cfg: VmConfig, hooks: HostHooks) -> Result<Machine> {
        let hv = apex_hvf::create(&HvConfig {
            ipa_bits: Some(layout::IPA_BITS),
            gic: cfg.gic,
            gic_dist_base: GIC_DIST_BASE,
            gic_redist_base: GIC_REDIST_BASE,
            max_vcpus: cfg.cpus,
        })?;
        Self::build_with(cfg, hooks, hv)
    }

    pub fn build_with(cfg: VmConfig, hooks: HostHooks, hv: Arc<dyn Hypervisor>) -> Result<Machine> {
        let geo = hv.gic_geometry();
        if cfg.cpus > layout::max_vcpus(geo.redist_stride) {
            return Err(Error::Config(format!("at most {} vCPUs fit the redistributor window", layout::max_vcpus(geo.redist_stride))));
        }

        // Guest RAM, mapped into stage 2.
        let mem = GuestMemory::new(&[(GuestAddress(RAM_BASE), cfg.memory)])?;
        for r in mem.regions() {
            // SAFETY: guest memory lives as long as the machine (hv is dropped first).
            unsafe { hv.map_memory(r.host_ptr(), r.base().raw(), r.size(), MemFlags::RWX)? };
        }

        let cpus = CpuSet::new(hv.clone(), cfg.cpus);
        let mut bus = MmioBus::new();

        // Interrupt controller.
        let (irqchip, gic): (Arc<dyn InterruptController>, Option<Arc<GicV3>>) = match hv.gic_mode() {
            GicMode::Hardware => (Arc::new(HwIrqChip::new(cpus.clone())), None),
            GicMode::Emulated => {
                let g = GicV3::new(cfg.cpus, 96);
                let c = cpus.clone();
                g.set_notifier(Box::new(move |cpu| c.kick(cpu)));
                bus.insert(GIC_DIST_BASE, gic::GICD_SIZE, Arc::new(gic::Distributor(g.clone())))?;
                bus.insert(GIC_REDIST_BASE, gic::GICR_STRIDE * cfg.cpus as u64, Arc::new(gic::Redistributors(g.clone())))?;
                (g.clone(), Some(g))
            }
        };
        let line = |spi: u32| IrqLine::new(irqchip.clone(), spi);

        // Legacy platform devices.
        let serial_sink = sink_for(&cfg.serial, &hooks)?;
        let uart = Arc::new(Pl011::new(line(UART_SPI), {
            let s = serial_sink.clone();
            Box::new(move |b| s(b))
        }));
        bus.insert(UART_BASE, UART_SIZE, uart.clone())?;
        bus.insert(RTC_BASE, RTC_SIZE, Arc::new(Pl031::new(line(RTC_SPI))))?;
        let battery = Arc::new(GoldfishBattery::new(line(BATTERY_SPI)));
        bus.insert(BATTERY_BASE, BATTERY_SIZE, battery.clone())?;

        // virtio devices, one 4 KiB slot each.
        let display = DisplayHub::new(cfg.display.clone());
        let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
        let mut os_disk_slot = None;
        for (i, d) in cfg.disks.iter().enumerate() {
            let backend: Arc<dyn DiskBackend> = match d {
                DiskConfig::Raw { path, readonly, .. } => Arc::new(
                    RawDisk::open(path, *readonly, cfg.disk_cache).map_err(|e| Error::Config(format!("disk {}: {e}", path.display())))?,
                ),
                DiskConfig::Composite { name, partitions } => {
                    let mut specs = Vec::new();
                    for p in partitions {
                        let raw = RawDisk::open(&p.path, p.readonly, cfg.disk_cache)
                            .map_err(|e| Error::Config(format!("partition {} ({}): {e}", p.name, p.path.display())))?;
                        specs.push(PartitionSpec { name: p.name.clone(), backend: Box::new(raw) });
                    }
                    if os_disk_slot.is_none() {
                        os_disk_slot = Some(devices.len());
                    }
                    Arc::new(CompositeDisk::new(name, specs).map_err(|e| Error::Config(format!("composite disk {name}: {e}")))?)
                }
            };
            let serial = match d {
                DiskConfig::Raw { serial, .. } => serial.clone(),
                DiskConfig::Composite { name, .. } => name.clone(),
            };
            let _ = i;
            devices.push(Box::new(Block::new(backend, &serial, cfg.block_queues.min(cfg.cpus as u16).max(1))));
        }
        let console = Console::new("console", {
            let s = serial_sink.clone();
            Box::new(move |b| s(b))
        });
        let console_in = console.input();
        devices.push(Box::new(console));

        let renderer: Option<RendererFactory> = match &cfg.renderer {
            RendererConfig::Guest => None,
            RendererConfig::Gfxstream(lib) => {
                let lib = lib.to_string_lossy().into_owned();
                let (w, h) = (cfg.display.width, cfg.display.height);
                Some(Box::new(move |sink| {
                    let flags = gfxstream::flags::USE_GLES | gfxstream::flags::USE_VK | gfxstream::flags::USE_SURFACELESS;
                    Ok(Box::new(Gfxstream::load(&lib, w, h, flags, sink)?) as Box<dyn Renderer3d>)
                }))
            }
        };
        devices.push(Box::new(Gpu::new(GpuOptions {
            display: display.clone(),
            renderer,
            hostmem: Some((HOSTMEM_BASE, HOSTMEM_SIZE, Arc::new(HvMapper(hv.clone())) as Arc<dyn HostMapper>)),
            pace_flushes: true,
            edid_serial: 1,
        })?));

        let touch = Input::new(InputSpec::touchscreen(cfg.display.width, cfg.display.height, cfg.touch_slots, cfg.display.dpi));
        let touch_h = touch.handle();
        devices.push(Box::new(touch));
        let keys = Input::new(InputSpec::keyboard());
        let keys_h = keys.handle();
        devices.push(Box::new(keys));
        devices.push(Box::new(Rng::new()));

        let mut net_rx = None;
        let net_backend: Option<Arc<dyn NetBackend>> = match &cfg.net {
            NetConfig::None => None,
            NetConfig::UnixGram(p) => {
                Some(Arc::new(UnixDgramBackend::connect(p).map_err(|e| Error::Config(format!("network socket {}: {e}", p.display())))?))
            }
            NetConfig::UnixStream(p) => {
                Some(Arc::new(UnixStreamBackend::connect(p).map_err(|e| Error::Config(format!("network socket {}: {e}", p.display())))?))
            }
            NetConfig::Host => {
                let tx = hooks.net_tx.clone().ok_or_else(|| Error::Config("network.mode = \"host\" needs a frame callback".into()))?;
                let ch = ChannelBackend::new(Box::new(move |f| tx(f)));
                net_rx = Some(ch.clone());
                Some(Arc::new(ch))
            }
        };
        if let Some(b) = net_backend {
            devices.push(Box::new(Net::new(net::mac_from_seed(&cfg.identity.serial), b)));
        }

        if devices.len() > VIRTIO_MMIO_SLOTS as usize {
            return Err(Error::Config("too many virtio devices".into()));
        }
        let mut transports = Vec::new();
        let mut nodes = Vec::new();
        for (i, dev) in devices.into_iter().enumerate() {
            let (base, spi) = layout::virtio_slot(i as u32);
            let t = Arc::new(MmioTransport::new(dev, mem.clone(), VirtioInterrupt::new(line(spi))));
            bus.insert(base, VIRTIO_MMIO_STRIDE, t.clone() as Arc<dyn MmioDevice>)?;
            nodes.push(VirtioNode { base, size: VIRTIO_MMIO_STRIDE, spi });
            transports.push(t);
        }
        apex_core::debug!("MMIO map: {bus:?}");

        // Boot payload.
        let payload = Self::load_payload(&cfg, os_disk_slot.map(|s| layout::virtio_slot(s as u32).0))?;
        let (kernel, hdr) = image::prepare_kernel(&payload.kernel)?;
        let placement = boot::plan(RAM_BASE, cfg.memory, &hdr, kernel.len(), payload.initrd.len())?;
        boot::load(&mem, &placement, &kernel, &payload.initrd)?;
        let dtb = fdt_gen::build(&FdtParams {
            cpus: cfg.cpus,
            ram_base: RAM_BASE,
            ram_size: cfg.memory,
            cmdline: &payload.cmdline,
            initrd: placement.initrd.map(|(a, l)| (a.raw(), l)),
            gic_mode: hv.gic_mode(),
            gic: geo,
            virtio: &nodes,
            timer_freq: hv.counter_frequency(),
            model: &format!("{} {}", cfg.identity.manufacturer, cfg.identity.model),
            serial_console: true,
        })?;
        boot::write_dtb(&mem, &placement, &dtb)?;
        apex_core::info!(
            "kernel {} MiB at {:#x}, initrd {} KiB, dtb at {:#x}, {} vCPUs, {} MiB RAM, {:?} GIC",
            kernel.len() >> 20,
            placement.kernel.raw(),
            payload.initrd.len() >> 10,
            placement.dtb.raw(),
            cfg.cpus,
            cfg.memory >> 20,
            hv.gic_mode()
        );
        apex_core::info!("cmdline: {}", payload.cmdline);

        let hub = VcpuHub::new(cpus.clone(), Arc::new(bus), gic);
        Ok(Machine {
            controls: Controls { display, touch: touch_h, keys: keys_h, console: console_in, battery, uart, net_rx },
            dtb,
            cmdline: payload.cmdline,
            boot_entry: EntryState { pc: placement.kernel.raw(), x0: placement.dtb.raw() },
            cfg,
            hub,
            cpus,
            threads: Mutex::new(Vec::new()),
            _transports: transports,
            _mem: mem,
        })
    }

    fn load_payload(cfg: &VmConfig, os_disk_base: Option<u64>) -> Result<Payload> {
        match &cfg.boot {
            BootSource::Kernel { kernel, initrd, cmdline } => Ok(Payload {
                kernel: read_file(kernel, "kernel")?,
                initrd: match initrd {
                    Some(p) => read_file(p, "initrd")?,
                    None => Vec::new(),
                },
                cmdline: cmdline.clone(),
            }),
            BootSource::Android { boot, init_boot, vendor_boot, cmdline } => {
                let b = android::parse_boot(&read_file(boot, "boot image")?)?;
                let ib = match init_boot {
                    Some(p) => Some(android::parse_boot(&read_file(p, "init_boot image")?)?),
                    None => None,
                };
                let vb = match vendor_boot {
                    Some(p) => Some(android::parse_vendor_boot(&read_file(p, "vendor_boot image")?)?),
                    None => None,
                };
                apex_core::info!("Android boot image v{} (OS {})", b.header_version, b.os_version_string());
                let params = Self::bootconfig(cfg, os_disk_base);
                let a = android::assemble(&b, ib.as_ref(), vb.as_ref(), &params, cmdline)?;
                Ok(Payload { kernel: a.kernel, initrd: a.initrd, cmdline: a.cmdline })
            }
        }
    }

    /// `androidboot.*` parameters a real bootloader would pass, describing
    /// this virtual phone. Profile values override the defaults.
    fn bootconfig(cfg: &VmConfig, os_disk_base: Option<u64>) -> Vec<(String, String)> {
        let d = &cfg.display;
        let gfx = matches!(cfg.renderer, RendererConfig::Gfxstream(_));
        let mut v: Vec<(String, String)> = vec![
            ("androidboot.hardware".into(), "apex".into()),
            ("androidboot.serialno".into(), cfg.identity.serial.clone()),
            ("androidboot.lcd_density".into(), d.dpi.to_string()),
            ("androidboot.slot_suffix".into(), "_a".into()),
            ("androidboot.force_normal_boot".into(), "1".into()),
            ("androidboot.verifiedbootstate".into(), "orange".into()),
            ("androidboot.vbmeta.device_state".into(), "unlocked".into()),
            ("androidboot.hardware.gralloc".into(), "minigbm".into()),
            ("androidboot.hardware.hwcomposer".into(), if gfx { "ranchu" } else { "drm" }.into()),
            ("androidboot.hardware.egl".into(), if gfx { "emulation" } else { "angle" }.into()),
            ("androidboot.hardware.vulkan".into(), if gfx { "ranchu" } else { "pastel" }.into()),
            ("androidboot.opengles.version".into(), "196610".into()),
            ("androidboot.apex.display".into(), format!("{}x{}", d.width, d.height)),
            ("androidboot.apex.refresh_rate".into(), d.refresh_hz.to_string()),
            ("androidboot.apex.manufacturer".into(), cfg.identity.manufacturer.clone()),
            ("androidboot.apex.brand".into(), cfg.identity.brand.clone()),
            ("androidboot.apex.model".into(), cfg.identity.model.clone()),
            ("androidboot.apex.device".into(), cfg.identity.device.clone()),
        ];
        if let Some(base) = os_disk_base {
            v.push(("androidboot.boot_devices".into(), fdt_gen::virtio_platform_name(base)));
        }
        for (k, val) in &cfg.bootconfig {
            v.retain(|(ek, _)| ek != k);
            v.push((k.clone(), val.clone()));
        }
        v
    }

    pub fn config(&self) -> &VmConfig {
        &self.cfg
    }

    pub fn cpus(&self) -> &Arc<CpuSet> {
        &self.cpus
    }

    pub fn guest_memory(&self) -> &GuestMemory {
        &self._mem
    }

    pub fn boot_entry(&self) -> EntryState {
        self.boot_entry
    }

    /// Start vCPU threads (and the internal vsync if configured).
    pub fn start(&self) -> Result<()> {
        let mut threads = self.threads.lock().unwrap();
        if !threads.is_empty() {
            return Err(Error::Hypervisor("machine already started".into()));
        }
        if self.cfg.vsync == VsyncSource::Internal {
            self.controls.display.start_internal_vsync();
        }
        for i in 0..self.cfg.cpus {
            let hub = self.hub.clone();
            let entry = (i == 0).then_some(self.boot_entry);
            let t = std::thread::Builder::new()
                .name(format!("vcpu-{i}"))
                .stack_size(4 << 20)
                .spawn(move || {
                    apex_core::sys::set_thread_latency_critical();
                    vcpu_thread(hub, i, entry)
                })
                .map_err(Error::Io)?;
            threads.push(t);
        }
        Ok(())
    }

    /// Block until the guest powers off, reboots or fails.
    pub fn wait(&self) -> StopReason {
        let r = self.cpus.wait_stopped();
        self.join();
        r
    }

    pub fn request_stop(&self) {
        self.cpus.request_stop(StopReason::Requested);
    }

    fn join(&self) {
        let threads = std::mem::take(&mut *self.threads.lock().unwrap());
        for t in threads {
            let _ = t.join();
        }
        self.controls.display.stop_internal_vsync();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if !self.threads.lock().unwrap().is_empty() {
            self.cpus.request_stop(StopReason::Requested);
            self.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcpu::mock::{exit_esr, mmio_exit, MockCpu, MockHv, Script};
    use apex_arm64::boot::bootconfig;
    use apex_arm64::esr::ec;
    use apex_core::fdt;
    use apex_core::hv::{Reg, VcpuExit};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("apex-machine-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_kernel() -> Vec<u8> {
        let mut v = vec![0u8; 256 * 1024];
        v[16..24].copy_from_slice(&(8u64 << 20).to_le_bytes()); // image_size
        v[24..32].copy_from_slice(&0b1010u64.to_le_bytes());
        v[56..60].copy_from_slice(&0x644d_5241u32.to_le_bytes());
        v
    }

    fn boot_v4(kernel: &[u8], ramdisk: &[u8], cmdline: &str) -> Vec<u8> {
        let mut d = vec![0u8; 4096];
        d[..8].copy_from_slice(b"ANDROID!");
        d[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
        d[12..16].copy_from_slice(&(ramdisk.len() as u32).to_le_bytes());
        d[40..44].copy_from_slice(&4u32.to_le_bytes());
        d[44..44 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        d.extend_from_slice(kernel);
        d.resize(d.len().div_ceil(4096) * 4096, 0);
        d.extend_from_slice(ramdisk);
        d.resize(d.len().div_ceil(4096) * 4096, 0);
        d
    }

    #[test]
    fn android_phone_boots_to_kernel_entry() {
        let dir = tmpdir();
        std::fs::write(dir.join("boot.img"), boot_v4(&fake_kernel(), b"", "console=hvc0")).unwrap();
        std::fs::write(dir.join("init_boot.img"), boot_v4(&[], b"GENERIC-RAMDISK", "")).unwrap();
        std::fs::write(dir.join("super.img"), vec![0u8; 1 << 20]).unwrap();
        std::fs::write(dir.join("userdata.img"), vec![0u8; 1 << 20]).unwrap();
        let profile = r#"
[device]
model = "Apex Test"
serial = "APEXT0001"
[vm]
cpus = 2
memory = "512M"
gic = "emulated"
[boot]
boot_image = "boot.img"
init_boot_image = "init_boot.img"
[boot.bootconfig]
"androidboot.selinux" = "permissive"
[console]
serial = "callback"
[[disk]]
name = "apex-os"
[[disk.partition]]
name = "super"
path = "super.img"
readonly = true
[[disk.partition]]
name = "userdata"
path = "userdata.img"
"#;
        let cfg = VmConfig::parse(profile, &dir).unwrap();

        let serial = Arc::new(Mutex::new(Vec::new()));
        let s2 = serial.clone();
        let hooks = HostHooks { serial: Some(Arc::new(move |b: &[u8]| s2.lock().unwrap().extend_from_slice(b))), net_tx: None };

        let mut scripts: HashMap<usize, Script> = HashMap::new();
        let seen = Arc::new(Mutex::new((0u64, 0u64)));
        let seen2 = seen.clone();
        scripts.insert(
            0,
            vec![
                Box::new(move |c: &mut MockCpu| {
                    *seen2.lock().unwrap() = (c.regs[&Reg::Pc.hvf_id()], c.regs[&0]);
                    mmio_exit(0x0a00_0000, false, 2, 4) // virtio magic
                }),
                Box::new(|c: &mut MockCpu| {
                    assert_eq!(c.regs[&4], 0x7472_6976);
                    c.regs.insert(1, b'A' as u64);
                    mmio_exit(0x0900_0000, true, 0, 1) // PL011 DR
                }),
                Box::new(|_c: &mut MockCpu| VcpuExit::Canceled),
                Box::new(|c: &mut MockCpu| {
                    c.regs.insert(0, apex_arm64::psci::fid::SYSTEM_OFF as u64);
                    exit_esr(ec::HVC64, 0)
                }),
            ],
        );
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHv { scripts: Mutex::new(scripts) });
        let m = Machine::build_with(cfg, hooks, hv).unwrap();

        // Device tree sanity.
        let (root, _) = fdt::parse(&m.dtb).unwrap();
        let chosen = root.find("/chosen").unwrap();
        let bootargs = chosen.prop_str("bootargs").unwrap();
        assert!(bootargs.contains("console=hvc0") && bootargs.ends_with("bootconfig"), "{bootargs}");
        let start = u64::from_be_bytes(chosen.prop("linux,initrd-start").unwrap().try_into().unwrap());
        let end = u64::from_be_bytes(chosen.prop("linux,initrd-end").unwrap().try_into().unwrap());
        let mut initrd = vec![0u8; (end - start) as usize];
        m.guest_memory().read(&mut initrd, GuestAddress(start)).unwrap();
        assert!(initrd.starts_with(b"GENERIC-RAMDISK"));
        let (_, bc) = bootconfig::find_trailer(&initrd).unwrap();
        let bc = String::from_utf8(bc.to_vec()).unwrap();
        assert!(bc.contains("androidboot.boot_devices = \"a000000.virtio_mmio\""), "{bc}");
        assert!(bc.contains("androidboot.apex.refresh_rate = \"120\""), "{bc}");
        assert!(bc.contains("androidboot.apex.model = \"Apex Test\""), "{bc}");
        assert!(bc.contains("androidboot.selinux = \"permissive\""), "{bc}");
        // disk, console, gpu, touch, keys, rng
        assert_eq!(root.children.iter().filter(|n| n.name.starts_with("virtio_mmio@")).count(), 6);

        m.start().unwrap();
        assert_eq!(m.wait(), StopReason::PowerOff);
        let (pc, x0) = *seen.lock().unwrap();
        assert_eq!(pc, m.boot_entry().pc);
        assert_eq!(pc, RAM_BASE);
        assert_eq!(x0, m.boot_entry().x0, "x0 must carry the DTB address");
        assert_eq!(serial.lock().unwrap().as_slice(), b"A");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
