//! Device tree for the `apex-virt` machine.

use apex_arm64::cpu::{affinity, mpidr_for};
use apex_arm64::layout::*;
use apex_core::fdt::FdtWriter;
use apex_core::hv::{GicGeometry, GicMode};
use apex_core::Result;

const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;

pub struct VirtioNode {
    pub base: u64,
    pub size: u64,
    pub spi: u32,
}

pub struct FdtParams<'a> {
    pub cpus: usize,
    pub ram_base: u64,
    pub ram_size: u64,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    pub gic_mode: GicMode,
    pub gic: GicGeometry,
    pub virtio: &'a [VirtioNode],
    pub timer_freq: u64,
    pub model: &'a str,
    pub serial_console: bool,
}

/// Node name as Linux will name the platform device: `a000000.virtio_mmio`.
pub fn virtio_platform_name(base: u64) -> String {
    format!("{base:x}.virtio_mmio")
}

pub fn build(p: &FdtParams) -> Result<Vec<u8>> {
    let mut w = FdtWriter::new();
    let gic_ph = w.alloc_phandle();
    let clk_ph = w.alloc_phandle();

    w.begin_node("")?;
    w.prop_strs("compatible", &["apex,virt-phone", "apex,virt"])?;
    w.prop_str("model", p.model)?;
    w.prop_u32("#address-cells", 2)?;
    w.prop_u32("#size-cells", 2)?;
    w.prop_u32("interrupt-parent", gic_ph)?;

    // /chosen
    w.begin_node("chosen")?;
    w.prop_str("bootargs", p.cmdline)?;
    if p.serial_console {
        w.prop_str("stdout-path", &format!("/pl011@{UART_BASE:x}"))?;
    }
    if let Some((start, len)) = p.initrd {
        w.prop_u64("linux,initrd-start", start)?;
        w.prop_u64("linux,initrd-end", start + len)?;
    }
    let mut seed = [0u8; 64];
    let _ = apex_core::sys::fill_random(&mut seed);
    w.prop_bytes("rng-seed", &seed)?;
    let mut k = [0u8; 8];
    let _ = apex_core::sys::fill_random(&mut k);
    w.prop_u64("kaslr-seed", u64::from_le_bytes(k))?;
    w.end_node()?;

    // memory
    w.begin_node(&format!("memory@{:x}", p.ram_base))?;
    w.prop_str("device_type", "memory")?;
    w.prop_u64s("reg", &[p.ram_base, p.ram_size])?;
    w.end_node()?;

    // cpus
    w.begin_node("cpus")?;
    w.prop_u32("#address-cells", 1)?;
    w.prop_u32("#size-cells", 0)?;
    for i in 0..p.cpus {
        let aff = affinity(mpidr_for(i)) as u32;
        w.begin_node(&format!("cpu@{aff:x}"))?;
        w.prop_str("device_type", "cpu")?;
        w.prop_str("compatible", "arm,arm-v8")?;
        w.prop_u32("reg", aff)?;
        w.prop_str("enable-method", "psci")?;
        w.end_node()?;
    }
    w.end_node()?;

    // psci
    w.begin_node("psci")?;
    w.prop_strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"])?;
    w.prop_str("method", "hvc")?;
    w.end_node()?;

    // Generic timer: secure phys, non-secure phys, virtual, hypervisor PPIs.
    w.begin_node("timer")?;
    w.prop_str("compatible", "arm,armv8-timer")?;
    let ppi = |intid: u32| [GIC_PPI, intid - 16, IRQ_TYPE_LEVEL_HIGH];
    let mut cells = Vec::new();
    for intid in [29, p.gic.ptimer_ppi, p.gic.vtimer_ppi, 26] {
        cells.extend_from_slice(&ppi(intid));
    }
    w.prop_cells("interrupts", &cells)?;
    w.prop_u32("clock-frequency", p.timer_freq as u32)?;
    w.prop_empty("always-on")?;
    w.end_node()?;

    // GICv3
    w.begin_node(&format!("intc@{GIC_DIST_BASE:x}"))?;
    w.prop_str("compatible", "arm,gic-v3")?;
    w.prop_u32("#interrupt-cells", 3)?;
    w.prop_empty("interrupt-controller")?;
    w.prop_u32("#address-cells", 2)?;
    w.prop_u32("#size-cells", 2)?;
    w.prop_empty("ranges")?;
    w.prop_u32("#redistributor-regions", 1)?;
    w.prop_u64s("reg", &[GIC_DIST_BASE, p.gic.dist_size, GIC_REDIST_BASE, p.gic.redist_stride * p.cpus as u64])?;
    if p.gic.redist_stride != 0x2_0000 {
        w.prop_u64("redistributor-stride", p.gic.redist_stride)?;
    }
    w.prop_u32("phandle", gic_ph)?;
    w.end_node()?;

    if p.gic_mode == GicMode::Hardware {
        // The in-kernel vGIC exposes a PMU PPI; advertise the architectural PMU.
        w.begin_node("pmu")?;
        w.prop_str("compatible", "arm,armv8-pmuv3")?;
        w.prop_cells("interrupts", &ppi(p.gic.pmu_ppi))?;
        w.end_node()?;
    }

    // 24 MHz APB clock for the PrimeCells.
    w.begin_node("apb-pclk")?;
    w.prop_str("compatible", "fixed-clock")?;
    w.prop_u32("#clock-cells", 0)?;
    w.prop_u32("clock-frequency", 24_000_000)?;
    w.prop_str("clock-output-names", "clk24mhz")?;
    w.prop_u32("phandle", clk_ph)?;
    w.end_node()?;

    w.begin_node(&format!("pl011@{UART_BASE:x}"))?;
    w.prop_strs("compatible", &["arm,pl011", "arm,primecell"])?;
    w.prop_u64s("reg", &[UART_BASE, UART_SIZE])?;
    w.prop_cells("interrupts", &[GIC_SPI, UART_SPI, IRQ_TYPE_LEVEL_HIGH])?;
    w.prop_cells("clocks", &[clk_ph, clk_ph])?;
    w.prop_strs("clock-names", &["uartclk", "apb_pclk"])?;
    w.end_node()?;

    w.begin_node(&format!("pl031@{RTC_BASE:x}"))?;
    w.prop_strs("compatible", &["arm,pl031", "arm,primecell"])?;
    w.prop_u64s("reg", &[RTC_BASE, RTC_SIZE])?;
    w.prop_cells("interrupts", &[GIC_SPI, RTC_SPI, IRQ_TYPE_LEVEL_HIGH])?;
    w.prop_cells("clocks", &[clk_ph])?;
    w.prop_str("clock-names", "apb_pclk")?;
    w.end_node()?;

    w.begin_node(&format!("goldfish_battery@{BATTERY_BASE:x}"))?;
    w.prop_str("compatible", "google,goldfish-battery")?;
    w.prop_u64s("reg", &[BATTERY_BASE, BATTERY_SIZE])?;
    w.prop_cells("interrupts", &[GIC_SPI, BATTERY_SPI, IRQ_TYPE_LEVEL_HIGH])?;
    w.end_node()?;

    for v in p.virtio {
        w.begin_node(&format!("virtio_mmio@{:x}", v.base))?;
        w.prop_str("compatible", "virtio,mmio")?;
        w.prop_u64s("reg", &[v.base, v.size])?;
        w.prop_cells("interrupts", &[GIC_SPI, v.spi, IRQ_TYPE_LEVEL_HIGH])?;
        w.prop_empty("dma-coherent")?;
        w.end_node()?;
    }

    w.end_node()?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::fdt;

    #[test]
    fn tree_describes_machine() {
        let virtio = [VirtioNode { base: 0x0a00_0000, size: 0x1000, spi: 16 }, VirtioNode { base: 0x0a00_1000, size: 0x1000, spi: 17 }];
        let blob = build(&FdtParams {
            cpus: 18,
            ram_base: RAM_BASE,
            ram_size: 0x2_0000_0000,
            cmdline: "console=hvc0 bootconfig",
            initrd: Some((0x9000_0000, 0x10_0000)),
            gic_mode: GicMode::Emulated,
            gic: GicGeometry::default(),
            virtio: &virtio,
            timer_freq: 24_000_000,
            model: "Apex One",
            serial_console: true,
        })
        .unwrap();
        let (root, _) = fdt::parse(&blob).unwrap();
        assert_eq!(root.prop_str("model"), Some("Apex One"));
        let mem = root.find("/memory@80000000").unwrap();
        assert_eq!(mem.prop_cells("reg").unwrap(), vec![0, 0x8000_0000, 2, 0]);
        let cpus = root.find("/cpus").unwrap();
        assert_eq!(cpus.children.len(), 18);
        assert_eq!(root.find("/cpus/cpu@101").unwrap().prop_u32("reg"), Some(0x101)); // cpu 17 -> aff1=1 aff0=1
        let gic = root.find("/intc@8000000").unwrap();
        let reg = gic.prop_cells("reg").unwrap();
        assert_eq!(reg[6..8], [0, 0x2_0000 * 18]);
        let timer = root.find("/timer").unwrap();
        assert_eq!(timer.prop_cells("interrupts").unwrap(), vec![1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]);
        let chosen = root.find("/chosen").unwrap();
        assert_eq!(chosen.prop_str("stdout-path"), Some("/pl011@9000000"));
        assert_eq!(chosen.prop("linux,initrd-end").unwrap(), &0x9010_0000u64.to_be_bytes());
        assert_eq!(chosen.prop("rng-seed").unwrap().len(), 64);
        let v1 = root.find("/virtio_mmio@a001000").unwrap();
        assert_eq!(v1.prop_cells("interrupts").unwrap(), vec![0, 17, 4]);
        assert!(root.find("/pmu").is_none());
        assert_eq!(virtio_platform_name(0x0a00_1000), "a001000.virtio_mmio");
        // The phandle referenced by interrupt-parent exists.
        assert_eq!(root.prop_u32("interrupt-parent"), gic.prop_u32("phandle"));
    }
}
