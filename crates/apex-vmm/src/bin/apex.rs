//! `apex` — headless runner and inspection tool for Apex-AOSP.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use apex_arm64::boot::android;
use apex_core::fdt;
use apex_core::hv::{GicMode, GicRequest, Hypervisor};
use apex_core::log::{self, Level};
use apex_core::{parse_size, Result};
use apex_vmm::config::VmConfig;
use apex_vmm::inspect::DryRunHypervisor;
use apex_vmm::{HostHooks, Machine, StopReason};

const USAGE: &str = "\
apex — Project Apex-AOSP virtual phone (Apple Silicon, Hypervisor.framework)

USAGE:
    apex run <profile.toml>              Boot the phone headless (console on stdout)
    apex boot --kernel <Image> [--initrd <file>] [--cmdline <text>]
              [--cpus <n>] [--memory <size>] [--gic auto|hardware|emulated]
    apex inspect <profile.toml> [--dts]  Assemble the machine without running it
    apex bootimg <image>                 Describe an Android boot/init_boot/vendor_boot image
    apex selftest                        Run a built-in guest: MMIO, GIC, timer IRQ, SMP, PSCI
    apex caps                            Host virtualization capabilities
    apex version

OPTIONS:
    --log <error|warn|info|debug|trace>  Log level (default: info, env APEX_LOG)
";

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn take_opt(&mut self, name: &str) -> Option<String> {
        let i = self.rest.iter().position(|a| a == name)?;
        if i + 1 >= self.rest.len() {
            return None;
        }
        let v = self.rest.remove(i + 1);
        self.rest.remove(i);
        Some(v)
    }
    fn take_flag(&mut self, name: &str) -> bool {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }
    fn positional(&mut self) -> Option<String> {
        if self.rest.is_empty() || self.rest[0].starts_with("--") {
            None
        } else {
            Some(self.rest.remove(0))
        }
    }
}

fn forward_stdin(m: &Machine) {
    let console = m.controls.console.clone();
    let uart = m.controls.uart.clone();
    let to_hvc = m.cmdline.contains("console=hvc0");
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = [0u8; 256];
            let mut stdin = std::io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if to_hvc {
                    console.send(&buf[..n]);
                } else {
                    uart.push_input(&buf[..n]);
                }
            }
        })
        .ok();
}

fn run_config(cfg: VmConfig) -> Result<StopReason> {
    loop {
        let m = Machine::build(cfg.clone(), HostHooks::default())?;
        forward_stdin(&m);
        m.start()?;
        let r = m.wait();
        drop(m);
        match r {
            StopReason::Reset => {
                log::log(Level::Info, "apex", format_args!("guest requested reboot, restarting VM"));
                continue;
            }
            other => return Ok(other),
        }
    }
}

fn cmd_inspect(path: &str, dts: bool) -> Result<()> {
    let cfg = VmConfig::from_file(&PathBuf::from(path))?;
    let mode = match cfg.gic {
        GicRequest::Hardware => GicMode::Hardware,
        _ => GicMode::Emulated,
    };
    let hv: Arc<dyn Hypervisor> = Arc::new(DryRunHypervisor { mode });
    let m = Machine::build_with(cfg, HostHooks::default(), hv)?;
    let c = m.config();
    println!("profile      : {path}");
    println!("device       : {} {} ({})", c.identity.manufacturer, c.identity.model, c.identity.serial);
    println!("cpus/memory  : {} vCPUs, {} MiB", c.cpus, c.memory >> 20);
    println!(
        "display      : {}x{} @ {} Hz, {} dpi, vsync {:?}, renderer {:?}",
        c.display.width, c.display.height, c.display.refresh_hz, c.display.dpi, c.vsync, c.renderer
    );
    println!("entry        : pc={:#x} x0(dtb)={:#x}", m.boot_entry().pc, m.boot_entry().x0);
    println!("cmdline      : {}", m.cmdline);
    println!("dtb          : {} bytes", m.dtb.len());
    if dts {
        let (root, _) = fdt::parse(&m.dtb)?;
        println!("\n{}", root.to_dts());
    }
    Ok(())
}

fn cmd_bootimg(path: &str) -> Result<()> {
    let data = std::fs::read(path)?;
    if data.starts_with(android::VENDOR_BOOT_MAGIC) {
        let v = android::parse_vendor_boot(&data)?;
        println!("vendor_boot v{} (page {})", v.header_version, v.page_size);
        println!("  name       : {}", v.name);
        println!("  cmdline    : {}", v.cmdline);
        println!("  ramdisk    : {} bytes", v.ramdisk.len());
        for r in &v.ramdisks {
            println!("    - {:<20} type {} {} bytes", r.name, r.kind, r.size);
        }
        println!("  dtb        : {} bytes", v.dtb.len());
        println!("  bootconfig :\n{}", String::from_utf8_lossy(&v.bootconfig));
    } else {
        let b = android::parse_boot(&data)?;
        println!("boot image v{} (Android {})", b.header_version, b.os_version_string());
        println!("  kernel     : {} bytes ({:?})", b.kernel.len(), apex_arm64::boot::image::detect_compression(&b.kernel));
        if !b.kernel.is_empty() {
            match apex_arm64::boot::image::prepare_kernel(&b.kernel) {
                Ok((raw, h)) => println!(
                    "               {} bytes decompressed, text_offset {:#x}, image_size {:#x}",
                    raw.len(),
                    h.text_offset,
                    h.image_size
                ),
                Err(e) => println!("               cannot decode: {e}"),
            }
        }
        println!("  ramdisk    : {} bytes", b.ramdisk.len());
        println!("  cmdline    : {}", b.cmdline);
    }
    Ok(())
}

/// Exit code 77 = skipped (the host cannot run VMs), as in automake.
fn cmd_selftest() -> ExitCode {
    use apex_vmm::selftest;
    println!("{}", apex_hvf::host_capabilities());
    let (mut failed, mut ran) = (0, 0);
    for gic in [GicRequest::Emulated, GicRequest::Hardware] {
        let label = format!("{gic:?} GIC");
        match selftest::run(gic, std::time::Duration::from_secs(15)) {
            Ok(o) => {
                ran += 1;
                let verdict = if o.passed() { "PASS" } else { "FAIL" };
                if !o.passed() {
                    failed += 1;
                }
                println!("[{verdict}] {label:<14} {:>7.1} ms  stop={:?}  console={:?}", o.elapsed.as_secs_f64() * 1e3, o.stop, o.console);
            }
            Err(e) if selftest::is_unavailable(&e) => println!("[SKIP] {label:<14} {e}"),
            Err(e) => {
                failed += 1;
                println!("[FAIL] {label:<14} {e}");
            }
        }
    }
    if failed > 0 {
        ExitCode::FAILURE
    } else if ran == 0 {
        ExitCode::from(77)
    } else {
        ExitCode::SUCCESS
    }
}

fn main() -> ExitCode {
    let mut args = Args { rest: std::env::args().skip(1).collect() };
    if let Some(l) = args.take_opt("--log") {
        match Level::parse(&l) {
            Some(l) => log::set_max_level(l),
            None => {
                eprintln!("unknown log level `{l}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(cmd) = args.positional() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let result: Result<ExitCode> = match cmd.as_str() {
        "run" => match args.positional() {
            Some(p) => VmConfig::from_file(&PathBuf::from(p)).and_then(run_config).map(|r| match r {
                StopReason::Error(_) => ExitCode::FAILURE,
                _ => ExitCode::SUCCESS,
            }),
            None => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        },
        "boot" => {
            let Some(kernel) = args.take_opt("--kernel") else {
                eprintln!("--kernel is required");
                return ExitCode::from(2);
            };
            let initrd = args.take_opt("--initrd").map(PathBuf::from);
            let cmdline = args.take_opt("--cmdline").unwrap_or_else(|| "console=ttyAMA0 earlycon=pl011,mmio32,0x9000000".into());
            let mut cfg = VmConfig::for_kernel(PathBuf::from(kernel), initrd, &cmdline);
            if let Some(n) = args.take_opt("--cpus") {
                cfg.cpus = n.parse().unwrap_or(cfg.cpus);
            }
            if let Some(m) = args.take_opt("--memory") {
                match parse_size(&m) {
                    Ok(v) => cfg.memory = v,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                }
            }
            if let Some(g) = args.take_opt("--gic") {
                cfg.gic = match g.as_str() {
                    "hardware" => GicRequest::Hardware,
                    "emulated" => GicRequest::Emulated,
                    _ => GicRequest::Auto,
                };
            }
            run_config(cfg).map(|r| if matches!(r, StopReason::Error(_)) { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        "inspect" => {
            let dts = args.take_flag("--dts");
            match args.positional() {
                Some(p) => cmd_inspect(&p, dts).map(|_| ExitCode::SUCCESS),
                None => {
                    eprint!("{USAGE}");
                    return ExitCode::from(2);
                }
            }
        }
        "bootimg" => match args.positional() {
            Some(p) => cmd_bootimg(&p).map(|_| ExitCode::SUCCESS),
            None => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        },
        "selftest" => Ok(cmd_selftest()),
        "caps" => {
            println!("{}", apex_hvf::host_capabilities());
            Ok(ExitCode::SUCCESS)
        }
        "version" | "--version" | "-V" => {
            println!("apex {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("unknown command `{other}`\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
