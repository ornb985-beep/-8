//! Device profiles (`*.toml`) describing the virtual phone.

use std::path::{Path, PathBuf};

use apex_core::hv::GicRequest;
use apex_core::toml::{self, View};
use apex_core::{parse_size, Error, Result, GIB};
use apex_devices::display::DisplayConfig;
use apex_devices::virtio::disk::CacheMode;

#[derive(Clone, Debug)]
pub enum BootSource {
    /// arm64 `Image` (+ optional initrd) with an explicit command line.
    Kernel { kernel: PathBuf, initrd: Option<PathBuf>, cmdline: String },
    /// Android boot images; the VMM acts as the v4 bootloader.
    Android { boot: PathBuf, init_boot: Option<PathBuf>, vendor_boot: Option<PathBuf>, cmdline: String },
}

#[derive(Clone, Debug)]
pub struct PartitionConfig {
    pub name: String,
    pub path: PathBuf,
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub enum DiskConfig {
    Raw { path: PathBuf, readonly: bool, serial: String },
    Composite { name: String, partitions: Vec<PartitionConfig> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetConfig {
    None,
    UnixGram(PathBuf),
    UnixStream(PathBuf),
    /// Frames exchanged through the C ABI (frontend-owned vmnet).
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsyncSource {
    Internal,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererConfig {
    /// virtio-gpu 2D; the guest renders with SwiftShader/ANGLE on its CPUs.
    Guest,
    /// gfxstream host rendering (library path).
    Gfxstream(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerialSink {
    Stdout,
    File(PathBuf),
    Null,
    /// Routed to the embedding application's log callback.
    Callback,
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub manufacturer: String,
    pub brand: String,
    pub model: String,
    pub device: String,
    pub serial: String,
}

#[derive(Clone, Debug)]
pub struct VmConfig {
    pub cpus: usize,
    pub memory: u64,
    pub gic: GicRequest,
    pub boot: BootSource,
    pub bootconfig: Vec<(String, String)>,
    pub display: DisplayConfig,
    pub vsync: VsyncSource,
    pub renderer: RendererConfig,
    pub disks: Vec<DiskConfig>,
    pub disk_cache: CacheMode,
    pub net: NetConfig,
    pub serial: SerialSink,
    pub identity: Identity,
    pub touch_slots: u32,
    /// Expose a full keyboard (hides Android's soft keyboard by default).
    pub keyboard: bool,
    pub block_queues: u16,
}

impl VmConfig {
    /// Minimal config for direct kernel boot (used by `apex boot`).
    pub fn for_kernel(kernel: PathBuf, initrd: Option<PathBuf>, cmdline: &str) -> VmConfig {
        VmConfig {
            cpus: 4,
            memory: 4 * GIB,
            gic: GicRequest::Auto,
            boot: BootSource::Kernel { kernel, initrd, cmdline: cmdline.to_string() },
            bootconfig: Vec::new(),
            display: DisplayConfig::phone_default(),
            vsync: VsyncSource::Internal,
            renderer: RendererConfig::Guest,
            disks: Vec::new(),
            disk_cache: CacheMode::Writeback,
            net: NetConfig::None,
            serial: SerialSink::Stdout,
            identity: Identity {
                manufacturer: "Apex".into(),
                brand: "apex".into(),
                model: "Apex One".into(),
                device: "apex_phone".into(),
                serial: "APEX00000001".into(),
            },
            touch_slots: 10,
            keyboard: true,
            block_queues: 4,
        }
    }

    pub fn from_file(path: &Path) -> Result<VmConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::parse(&text, &base)
    }

    pub fn parse(text: &str, base: &Path) -> Result<VmConfig> {
        let root = toml::parse(text)?;
        let v = View::new(&root, "");
        v.deny_unknown(&["device", "vm", "boot", "display", "disk", "network", "console", "input"])?;
        let rel = |p: &str| -> PathBuf {
            let pb = PathBuf::from(p);
            if pb.is_absolute() {
                pb
            } else {
                base.join(pb)
            }
        };

        // [device]
        let mut identity = VmConfig::for_kernel(PathBuf::new(), None, "").identity;
        if let Some(d) = v.table("device")? {
            d.deny_unknown(&["manufacturer", "brand", "model", "name", "serial"])?;
            if let Some(s) = d.str("manufacturer")? {
                identity.manufacturer = s.into();
            }
            if let Some(s) = d.str("brand")? {
                identity.brand = s.into();
            }
            if let Some(s) = d.str("model")? {
                identity.model = s.into();
            }
            if let Some(s) = d.str("name")? {
                identity.device = s.into();
            }
            if let Some(s) = d.str("serial")? {
                identity.serial = s.into();
            }
        }

        // [vm]
        let mut cpus = 4usize;
        let mut memory = 4 * GIB;
        let mut gic = GicRequest::Auto;
        let mut disk_cache = CacheMode::Writeback;
        let mut block_queues = 4u16;
        if let Some(vm) = v.table("vm")? {
            vm.deny_unknown(&["cpus", "memory", "gic", "disk_cache", "block_queues"])?;
            if let Some(c) = vm.int("cpus")? {
                cpus = c as usize;
            }
            if let Some(m) = vm.str("memory")? {
                memory = parse_size(m)?;
            }
            if let Some(g) = vm.str("gic")? {
                gic = match g {
                    "auto" => GicRequest::Auto,
                    "hardware" => GicRequest::Hardware,
                    "emulated" => GicRequest::Emulated,
                    o => return Err(Error::Config(format!("vm.gic must be auto|hardware|emulated, got `{o}`"))),
                };
            }
            if let Some(c) = vm.str("disk_cache")? {
                disk_cache = match c {
                    "writeback" => CacheMode::Writeback,
                    "unsafe" => CacheMode::Unsafe,
                    o => return Err(Error::Config(format!("vm.disk_cache must be writeback|unsafe, got `{o}`"))),
                };
            }
            if let Some(q) = vm.int("block_queues")? {
                block_queues = q.clamp(1, 16) as u16;
            }
        }
        if cpus == 0 || cpus > 64 {
            return Err(Error::Config("vm.cpus must be 1..=64".into()));
        }
        apex_arm64::layout::validate_ram(memory)?;

        // [boot]
        let b = v.table("boot")?.ok_or_else(|| Error::Config("missing [boot] section".into()))?;
        b.deny_unknown(&["kernel", "initrd", "cmdline", "boot_image", "init_boot_image", "vendor_boot_image", "bootconfig"])?;
        let cmdline = b.str("cmdline")?.unwrap_or("").to_string();
        let boot = match (b.str("kernel")?, b.str("boot_image")?) {
            (Some(k), None) => BootSource::Kernel { kernel: rel(k), initrd: b.str("initrd")?.map(rel), cmdline },
            (None, Some(bi)) => BootSource::Android {
                boot: rel(bi),
                init_boot: b.str("init_boot_image")?.map(rel),
                vendor_boot: b.str("vendor_boot_image")?.map(rel),
                cmdline,
            },
            _ => return Err(Error::Config("[boot] needs exactly one of `kernel` or `boot_image`".into())),
        };
        let mut bootconfig = Vec::new();
        if let Some(bc) = b.table("bootconfig")? {
            for (k, val) in bc.table.iter() {
                let s = match val {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    toml::Value::Bool(x) => (if *x { "1" } else { "0" }).to_string(),
                    _ => return Err(Error::Config(format!("boot.bootconfig.{k} must be a scalar"))),
                };
                bootconfig.push((k.clone(), s));
            }
        }

        // [display]
        let mut display = DisplayConfig::phone_default();
        let mut vsync = VsyncSource::Internal;
        let mut renderer = RendererConfig::Guest;
        if let Some(d) = v.table("display")? {
            d.deny_unknown(&["width", "height", "refresh", "dpi", "name", "vsync", "renderer", "gfxstream_library"])?;
            if let Some(x) = d.int("width")? {
                display.width = x as u32;
            }
            if let Some(x) = d.int("height")? {
                display.height = x as u32;
            }
            if let Some(x) = d.int("refresh")? {
                display.refresh_hz = x as u32;
            }
            if let Some(x) = d.int("dpi")? {
                display.dpi = x as u32;
            }
            if let Some(x) = d.str("name")? {
                display.name = x.into();
            }
            if let Some(x) = d.str("vsync")? {
                vsync = match x {
                    "internal" => VsyncSource::Internal,
                    "host" => VsyncSource::Host,
                    o => return Err(Error::Config(format!("display.vsync must be internal|host, got `{o}`"))),
                };
            }
            match d.str("renderer")?.unwrap_or("guest") {
                "guest" | "2d" | "swiftshader" => {}
                "gfxstream" => {
                    let lib = d
                        .str("gfxstream_library")?
                        .map(rel)
                        .ok_or_else(|| Error::Config("display.renderer = \"gfxstream\" needs display.gfxstream_library".into()))?;
                    renderer = RendererConfig::Gfxstream(lib);
                }
                o => return Err(Error::Config(format!("display.renderer must be guest|gfxstream, got `{o}`"))),
            }
        }
        if !(16..=8192).contains(&display.width) || !(16..=8192).contains(&display.height) {
            return Err(Error::Config("display size out of range".into()));
        }
        if !(1..=240).contains(&display.refresh_hz) {
            return Err(Error::Config("display.refresh must be 1..=240 Hz".into()));
        }

        // [[disk]]
        let mut disks = Vec::new();
        for (i, d) in v.tables("disk")?.into_iter().enumerate() {
            d.deny_unknown(&["path", "readonly", "serial", "name", "partition"])?;
            let parts = d.tables("partition")?;
            if !parts.is_empty() {
                let mut partitions = Vec::new();
                for p in parts {
                    p.deny_unknown(&["name", "path", "readonly"])?;
                    partitions.push(PartitionConfig {
                        name: p.str("name")?.ok_or_else(|| Error::Config(format!("{}.name missing", p.path)))?.into(),
                        path: rel(p.str("path")?.ok_or_else(|| Error::Config(format!("{}.path missing", p.path)))?),
                        readonly: p.bool("readonly")?.unwrap_or(false),
                    });
                }
                disks.push(DiskConfig::Composite { name: d.str("name")?.unwrap_or("apex-os").into(), partitions });
            } else {
                let path = d.str("path")?.ok_or_else(|| Error::Config(format!("{}.path missing", d.path)))?;
                disks.push(DiskConfig::Raw {
                    path: rel(path),
                    readonly: d.bool("readonly")?.unwrap_or(false),
                    serial: d.str("serial")?.map(String::from).unwrap_or_else(|| format!("APEXDISK{i}")),
                });
            }
        }

        // [network]
        let mut net = NetConfig::None;
        if let Some(n) = v.table("network")? {
            n.deny_unknown(&["mode", "socket"])?;
            let sock = n.str("socket")?.map(rel);
            net = match n.str("mode")?.unwrap_or("none") {
                "none" => NetConfig::None,
                "host" => NetConfig::Host,
                "unixgram" => NetConfig::UnixGram(sock.ok_or_else(|| Error::Config("network.socket required".into()))?),
                "unixstream" => NetConfig::UnixStream(sock.ok_or_else(|| Error::Config("network.socket required".into()))?),
                o => return Err(Error::Config(format!("network.mode must be none|host|unixgram|unixstream, got `{o}`"))),
            };
        }

        // [console]
        let mut serial = SerialSink::Stdout;
        if let Some(c) = v.table("console")? {
            c.deny_unknown(&["serial"])?;
            if let Some(s) = c.str("serial")? {
                serial = match s {
                    "stdout" => SerialSink::Stdout,
                    "null" => SerialSink::Null,
                    "callback" => SerialSink::Callback,
                    f if f.starts_with("file:") => SerialSink::File(rel(&f[5..])),
                    o => return Err(Error::Config(format!("console.serial: unknown sink `{o}`"))),
                };
            }
        }

        let mut touch_slots = 10;
        let mut keyboard = true;
        if let Some(i) = v.table("input")? {
            i.deny_unknown(&["touch_slots", "keyboard"])?;
            if let Some(n) = i.int("touch_slots")? {
                touch_slots = n.clamp(1, 16) as u32;
            }
            if let Some(k) = i.bool("keyboard")? {
                keyboard = k;
            }
        }

        Ok(VmConfig {
            cpus,
            memory,
            gic,
            boot,
            bootconfig,
            display,
            vsync,
            renderer,
            disks,
            disk_cache,
            net,
            serial,
            identity,
            touch_slots,
            keyboard,
            block_queues,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
[device]
model = "Apex Ultra"
serial = "APEXTEST0001"

[vm]
cpus = 8
memory = "6G"
gic = "emulated"

[boot]
boot_image = "images/boot.img"
init_boot_image = "images/init_boot.img"
vendor_boot_image = "/abs/vendor_boot.img"
cmdline = "loglevel=4"

[boot.bootconfig]
"androidboot.hardware" = "apex"
"androidboot.lcd_density" = 420

[display]
width = 1440
height = 3200
refresh = 120
dpi = 560

[[disk]]
name = "apex-os"
[[disk.partition]]
name = "super"
path = "images/super.img"
readonly = true
[[disk.partition]]
name = "userdata"
path = "images/userdata.img"

[[disk]]
path = "extra.img"

[network]
mode = "unixgram"
socket = "/tmp/gvproxy.sock"
"#;

    #[test]
    fn parses_full_profile() {
        let c = VmConfig::parse(PROFILE, Path::new("/profiles")).unwrap();
        assert_eq!(c.cpus, 8);
        assert_eq!(c.memory, 6 * GIB);
        assert_eq!(c.gic, GicRequest::Emulated);
        assert_eq!(c.identity.model, "Apex Ultra");
        match &c.boot {
            BootSource::Android { boot, init_boot, vendor_boot, cmdline } => {
                assert_eq!(boot, &PathBuf::from("/profiles/images/boot.img"));
                assert_eq!(init_boot.as_deref(), Some(Path::new("/profiles/images/init_boot.img")));
                assert_eq!(vendor_boot.as_deref(), Some(Path::new("/abs/vendor_boot.img")));
                assert_eq!(cmdline, "loglevel=4");
            }
            _ => panic!(),
        }
        assert!(c.bootconfig.contains(&("androidboot.lcd_density".into(), "420".into())));
        assert_eq!((c.display.width, c.display.height, c.display.refresh_hz, c.display.dpi), (1440, 3200, 120, 560));
        assert_eq!(c.disks.len(), 2);
        match &c.disks[0] {
            DiskConfig::Composite { name, partitions } => {
                assert_eq!(name, "apex-os");
                assert_eq!(partitions.len(), 2);
                assert!(partitions[0].readonly);
                assert!(!partitions[1].readonly);
            }
            _ => panic!(),
        }
        assert_eq!(c.net, NetConfig::UnixGram(PathBuf::from("/tmp/gvproxy.sock")));
    }

    #[test]
    fn rejects_bad_profiles() {
        assert!(VmConfig::parse("[vm]\ncpus = 4\n", Path::new("/")).is_err()); // no boot
        assert!(VmConfig::parse("[boot]\nkernel = \"a\"\nboot_image = \"b\"\n", Path::new("/")).is_err());
        assert!(VmConfig::parse("[boot]\nkernel = \"a\"\n[vm]\ncpu = 4\n", Path::new("/")).is_err()); // typo
        assert!(VmConfig::parse("[boot]\nkernel = \"a\"\n[vm]\nmemory = \"64M\"\n", Path::new("/")).is_err());
        assert!(VmConfig::parse("[boot]\nkernel = \"a\"\n[display]\nrefresh = 500\n", Path::new("/")).is_err());
        assert!(VmConfig::parse("[boot]\nkernel = \"a\"\n[display]\nrenderer = \"gfxstream\"\n", Path::new("/")).is_err());
    }
}

#[cfg(test)]
mod shipped_profiles {
    use super::*;

    #[test]
    fn shipped_profiles_are_valid() {
        for (name, text) in [
            ("phone-120hz", include_str!("../../../profiles/phone-120hz.toml")),
            ("tablet-120hz", include_str!("../../../profiles/tablet-120hz.toml")),
            ("kernel-smoke", include_str!("../../../profiles/examples/kernel-smoke.toml")),
        ] {
            let c = VmConfig::parse(text, Path::new("/p")).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.display.refresh_hz, 120, "{name}");
        }
        let tab = VmConfig::parse(include_str!("../../../profiles/tablet-120hz.toml"), Path::new("/p")).unwrap();
        assert_eq!((tab.display.width, tab.display.height), (2560, 1600));
        assert_eq!(tab.identity.model, "Apex Tab");
    }
}
