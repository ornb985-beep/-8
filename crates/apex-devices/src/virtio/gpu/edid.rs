//! EDID 1.4 generator. The guest DRM driver builds its mode list from this
//! block (VIRTIO_GPU_F_EDID), so this is where "the panel is 120 Hz" is
//! declared to Linux, drm_hwcomposer and SurfaceFlinger.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub hactive: u32,
    pub vactive: u32,
    pub refresh_hz: u32,
    pub hfront: u32,
    pub hsync: u32,
    pub hback: u32,
    pub vfront: u32,
    pub vsync: u32,
    pub vback: u32,
}

impl Timing {
    /// Reduced-blanking timing (CVT-RB style) for a virtual panel.
    pub fn reduced_blanking(w: u32, h: u32, hz: u32) -> Timing {
        Timing { hactive: w, vactive: h, refresh_hz: hz, hfront: 48, hsync: 32, hback: 80, vfront: 3, vsync: 5, vback: 32 }
    }
    pub fn htotal(&self) -> u32 {
        self.hactive + self.hfront + self.hsync + self.hback
    }
    pub fn vtotal(&self) -> u32 {
        self.vactive + self.vfront + self.vsync + self.vback
    }
    /// Pixel clock in 10 kHz units, as stored in a detailed timing descriptor.
    pub fn clock_10khz(&self) -> u64 {
        (self.htotal() as u64 * self.vtotal() as u64 * self.refresh_hz as u64 + 5_000) / 10_000
    }
    /// Refresh the guest will compute back from the (rounded) pixel clock.
    pub fn effective_refresh_mhz(&self) -> u64 {
        self.clock_10khz() * 10_000 * 1000 / (self.htotal() as u64 * self.vtotal() as u64)
    }
}

fn manufacturer(id: &str) -> [u8; 2] {
    let b = id.as_bytes();
    let c = |i: usize| (b.get(i).copied().unwrap_or(b'A').to_ascii_uppercase().saturating_sub(b'A') + 1) as u16 & 0x1f;
    let v = (c(0) << 10) | (c(1) << 5) | c(2);
    v.to_be_bytes()
}

fn dtd(t: &Timing, width_mm: u32, height_mm: u32) -> Option<[u8; 18]> {
    let clk = t.clock_10khz();
    if clk == 0 || clk > 0xffff || t.hactive > 4095 || t.vactive > 4095 || width_mm > 4095 || height_mm > 4095 {
        return None;
    }
    let hblank = t.htotal() - t.hactive;
    let vblank = t.vtotal() - t.vactive;
    let mut d = [0u8; 18];
    d[0..2].copy_from_slice(&(clk as u16).to_le_bytes());
    d[2] = t.hactive as u8;
    d[3] = hblank as u8;
    d[4] = (((t.hactive >> 8) & 0xf) << 4) as u8 | ((hblank >> 8) & 0xf) as u8;
    d[5] = t.vactive as u8;
    d[6] = vblank as u8;
    d[7] = (((t.vactive >> 8) & 0xf) << 4) as u8 | ((vblank >> 8) & 0xf) as u8;
    d[8] = t.hfront as u8;
    d[9] = t.hsync as u8;
    d[10] = (((t.vfront & 0xf) << 4) | (t.vsync & 0xf)) as u8;
    d[11] = ((((t.hfront >> 8) & 3) << 6) | (((t.hsync >> 8) & 3) << 4) | (((t.vfront >> 4) & 3) << 2) | ((t.vsync >> 4) & 3)) as u8;
    d[12] = width_mm as u8;
    d[13] = height_mm as u8;
    d[14] = ((((width_mm >> 8) & 0xf) << 4) | ((height_mm >> 8) & 0xf)) as u8;
    d[17] = 0x1e; // digital separate sync, +hsync +vsync
    Some(d)
}

fn text_descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = tag;
    let bytes: Vec<u8> = text.bytes().filter(|b| b.is_ascii_graphic() || *b == b' ').take(13).collect();
    d[5..5 + bytes.len()].copy_from_slice(&bytes);
    if bytes.len() < 13 {
        d[5 + bytes.len()] = 0x0a;
        for b in d.iter_mut().skip(6 + bytes.len()) {
            *b = 0x20;
        }
    }
    d
}

/// Build a 128-byte EDID for a single-mode panel. Returns None if the mode
/// cannot be expressed in a detailed timing descriptor.
pub fn build(name: &str, serial: u32, timing: &Timing, width_mm: u32, height_mm: u32) -> Option<[u8; 128]> {
    let mut e = [0u8; 128];
    e[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    e[8..10].copy_from_slice(&manufacturer("APX"));
    e[10..12].copy_from_slice(&0xa120u16.to_le_bytes());
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[16] = 1; // week
    e[17] = (2026 - 1990) as u8;
    e[18] = 1;
    e[19] = 4; // EDID 1.4
    e[20] = 0xa5; // digital, 8 bpc, DisplayPort
    e[21] = (width_mm.div_ceil(10)).min(255) as u8;
    e[22] = (height_mm.div_ceil(10)).min(255) as u8;
    e[23] = 120; // gamma 2.2
    e[24] = 0x06; // RGB 4:4:4, sRGB default, preferred timing is native
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]); // sRGB primaries
    for i in 0..8 {
        e[38 + 2 * i] = 0x01;
        e[39 + 2 * i] = 0x01;
    }
    e[54..72].copy_from_slice(&dtd(timing, width_mm, height_mm)?);
    e[72..90].copy_from_slice(&text_descriptor(0xfc, name));
    e[90..108].copy_from_slice(&text_descriptor(0xff, &format!("APEX{serial:08X}")));
    e[108..126].copy_from_slice(&{
        let mut d = [0u8; 18];
        d[3] = 0x10; // dummy descriptor
        d
    });
    e[126] = 0;
    let sum: u32 = e[..127].iter().map(|&b| b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_120hz_edid() {
        let t = Timing::reduced_blanking(1080, 2400, 120);
        let e = build("Apex Display", 1, &t, 65, 145).unwrap();
        assert_eq!(&e[0..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
        assert_eq!(e.iter().map(|&b| b as u32).sum::<u32>() % 256, 0, "checksum");
        // Decode the DTD the way Linux drm_edid.c does.
        let d = &e[54..72];
        let clock_khz = u16::from_le_bytes([d[0], d[1]]) as u64 * 10;
        let hactive = d[2] as u32 | ((d[4] as u32 >> 4) << 8);
        let hblank = d[3] as u32 | ((d[4] as u32 & 0xf) << 8);
        let vactive = d[5] as u32 | ((d[7] as u32 >> 4) << 8);
        let vblank = d[6] as u32 | ((d[7] as u32 & 0xf) << 8);
        assert_eq!((hactive, vactive), (1080, 2400));
        let vrefresh =
            (clock_khz * 1000 + ((hactive + hblank) * (vactive + vblank)) as u64 / 2) / ((hactive + hblank) * (vactive + vblank)) as u64;
        assert_eq!(vrefresh, 120);
        assert_eq!(&e[77..89], b"Apex Display");
        assert_eq!(e[89], 0x0a);
        // Manufacturer "APX"
        let m = u16::from_be_bytes([e[8], e[9]]);
        assert_eq!(((m >> 10) & 31, (m >> 5) & 31, m & 31), (1, 16, 24));
    }

    #[test]
    fn effective_refresh_is_close() {
        for (w, h, hz) in [(1080, 2400, 120), (1440, 3200, 120), (2560, 1600, 144), (1080, 2340, 90)] {
            let t = Timing::reduced_blanking(w, h, hz);
            let eff = t.effective_refresh_mhz();
            assert!((eff as i64 - hz as i64 * 1000).abs() < 50, "{w}x{h}@{hz}: {eff}");
            assert!(build("x", 0, &t, 70, 150).is_some());
        }
    }

    #[test]
    fn rejects_unrepresentable_modes() {
        assert!(build("x", 0, &Timing::reduced_blanking(3840, 2160, 240), 600, 340).is_none()); // > 655 MHz
        assert!(build("x", 0, &Timing::reduced_blanking(5120, 1440, 60), 600, 340).is_none());
        // > 4095 px
    }
}
