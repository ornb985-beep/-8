//! Flattened Device Tree (DTB v17) writer and reader.
//!
//! The VMM acts as the bootloader, so the machine description handed to the
//! Linux kernel is generated here byte by byte. The reader half is used by
//! tests and by `apex dtb` to dump what was generated.

use std::collections::HashMap;

use crate::error::{Error, Result};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_NOP: u32 = 0x4;
const FDT_END: u32 = 0x9;
const HEADER_SIZE: usize = 40;

pub struct FdtWriter {
    structure: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: HashMap<String, u32>,
    reserve: Vec<(u64, u64)>,
    depth: usize,
    next_phandle: u32,
    boot_cpuid: u32,
}

impl Default for FdtWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FdtWriter {
    pub fn new() -> Self {
        FdtWriter {
            structure: Vec::with_capacity(8192),
            strings: Vec::with_capacity(1024),
            string_offsets: HashMap::new(),
            reserve: Vec::new(),
            depth: 0,
            next_phandle: 1,
            boot_cpuid: 0,
        }
    }

    pub fn add_reservation(&mut self, addr: u64, size: u64) {
        self.reserve.push((addr, size));
    }

    pub fn alloc_phandle(&mut self) -> u32 {
        let p = self.next_phandle;
        self.next_phandle += 1;
        p
    }

    fn put_u32(&mut self, v: u32) {
        self.structure.extend_from_slice(&v.to_be_bytes());
    }

    fn pad(&mut self) {
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }

    pub fn begin_node(&mut self, name: &str) -> Result<()> {
        if name.contains('\0') || (self.depth > 0 && name.is_empty()) {
            return Err(Error::Boot(format!("invalid FDT node name `{name}`")));
        }
        self.put_u32(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
        self.depth += 1;
        Ok(())
    }

    pub fn end_node(&mut self) -> Result<()> {
        if self.depth == 0 {
            return Err(Error::Boot("unbalanced FDT end_node".into()));
        }
        self.put_u32(FDT_END_NODE);
        self.depth -= 1;
        Ok(())
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        if let Some(&o) = self.string_offsets.get(name) {
            return o;
        }
        let o = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.string_offsets.insert(name.to_string(), o);
        o
    }

    pub fn prop_bytes(&mut self, name: &str, val: &[u8]) -> Result<()> {
        if self.depth == 0 {
            return Err(Error::Boot(format!("property `{name}` outside of a node")));
        }
        let off = self.string_offset(name);
        self.put_u32(FDT_PROP);
        self.put_u32(val.len() as u32);
        self.put_u32(off);
        self.structure.extend_from_slice(val);
        self.pad();
        Ok(())
    }

    pub fn prop_empty(&mut self, name: &str) -> Result<()> {
        self.prop_bytes(name, &[])
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) -> Result<()> {
        self.prop_bytes(name, &v.to_be_bytes())
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) -> Result<()> {
        self.prop_bytes(name, &v.to_be_bytes())
    }

    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) -> Result<()> {
        let mut b = Vec::with_capacity(cells.len() * 4);
        for c in cells {
            b.extend_from_slice(&c.to_be_bytes());
        }
        self.prop_bytes(name, &b)
    }

    pub fn prop_u64s(&mut self, name: &str, vals: &[u64]) -> Result<()> {
        let mut b = Vec::with_capacity(vals.len() * 8);
        for v in vals {
            b.extend_from_slice(&v.to_be_bytes());
        }
        self.prop_bytes(name, &b)
    }

    pub fn prop_str(&mut self, name: &str, s: &str) -> Result<()> {
        if s.contains('\0') {
            return Err(Error::Boot(format!("NUL in FDT string property `{name}`")));
        }
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        self.prop_bytes(name, &b)
    }

    pub fn prop_strs(&mut self, name: &str, list: &[&str]) -> Result<()> {
        let mut b = Vec::new();
        for s in list {
            if s.contains('\0') {
                return Err(Error::Boot(format!("NUL in FDT string list `{name}`")));
            }
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        self.prop_bytes(name, &b)
    }

    /// Serialize. Fails if nodes are unbalanced.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        if self.depth != 0 {
            return Err(Error::Boot(format!("FDT has {} unclosed nodes", self.depth)));
        }
        self.put_u32(FDT_END);

        let rsv_off = HEADER_SIZE; // 8-byte aligned
        let rsv_len = (self.reserve.len() + 1) * 16;
        let struct_off = rsv_off + rsv_len;
        let strings_off = struct_off + self.structure.len();
        let total = strings_off + self.strings.len();

        let mut out = Vec::with_capacity(total);
        for v in [
            FDT_MAGIC,
            total as u32,
            struct_off as u32,
            strings_off as u32,
            rsv_off as u32,
            17,
            16,
            self.boot_cpuid,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for (a, s) in &self.reserve {
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&s.to_be_bytes());
        }
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        Ok(out)
    }
}

/// Parsed device tree node (reader side).
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub props: Vec<(String, Vec<u8>)>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn prop(&self, name: &str) -> Option<&[u8]> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    }

    pub fn prop_u32(&self, name: &str) -> Option<u32> {
        let v = self.prop(name)?;
        (v.len() == 4).then(|| u32::from_be_bytes(v.try_into().unwrap()))
    }

    pub fn prop_cells(&self, name: &str) -> Option<Vec<u32>> {
        let v = self.prop(name)?;
        if v.len() % 4 != 0 {
            return None;
        }
        Some(v.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect())
    }

    pub fn prop_str(&self, name: &str) -> Option<&str> {
        let v = self.prop(name)?;
        std::str::from_utf8(v.strip_suffix(&[0])?).ok()
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Look up by absolute path such as `/soc/serial@9000000`.
    pub fn find(&self, path: &str) -> Option<&Node> {
        let mut n = self;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            n = n.child(part)?;
        }
        Some(n)
    }

    /// Render as DTS-like text for debugging.
    pub fn to_dts(&self) -> String {
        let mut s = String::from("/dts-v1/;\n\n");
        self.write_dts(&mut s, 0);
        s
    }

    fn write_dts(&self, s: &mut String, depth: usize) {
        let ind = "\t".repeat(depth);
        let name = if self.name.is_empty() { "/" } else { &self.name };
        s.push_str(&format!("{ind}{name} {{\n"));
        for (k, v) in &self.props {
            s.push_str(&format!("{ind}\t{k}{};\n", fmt_prop(v)));
        }
        for c in &self.children {
            s.push('\n');
            c.write_dts(s, depth + 1);
        }
        s.push_str(&format!("{ind}}};\n"));
    }
}

fn fmt_prop(v: &[u8]) -> String {
    if v.is_empty() {
        return String::new();
    }
    let printable = v.last() == Some(&0)
        && v.len() > 1
        && v[..v.len() - 1].iter().all(|&b| b == 0 || (0x20..0x7f).contains(&b))
        && v[0] != 0
        && !v.windows(2).any(|w| w == [0, 0]);
    if printable {
        let parts: Vec<String> = v[..v.len() - 1].split(|&b| b == 0).map(|p| format!("\"{}\"", String::from_utf8_lossy(p))).collect();
        return format!(" = {}", parts.join(", "));
    }
    if v.len() % 4 == 0 {
        let cells: Vec<String> = v.chunks(4).map(|c| format!("{:#x}", u32::from_be_bytes(c.try_into().unwrap()))).collect();
        return format!(" = <{}>", cells.join(" "));
    }
    let bytes: Vec<String> = v.iter().map(|b| format!("{b:02x}")).collect();
    format!(" = [{}]", bytes.join(" "))
}

fn be32(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap())).ok_or_else(|| Error::Boot("truncated FDT".into()))
}

/// Parse a DTB blob. Returns the root node and the memory reservation list.
pub fn parse(blob: &[u8]) -> Result<(Node, Vec<(u64, u64)>)> {
    if be32(blob, 0)? != FDT_MAGIC {
        return Err(Error::Boot("bad FDT magic".into()));
    }
    let total = be32(blob, 4)? as usize;
    if total > blob.len() {
        return Err(Error::Boot("FDT totalsize exceeds blob".into()));
    }
    let off_struct = be32(blob, 8)? as usize;
    let off_strings = be32(blob, 12)? as usize;
    let off_rsv = be32(blob, 16)? as usize;
    let mut rsv = Vec::new();
    let mut p = off_rsv;
    loop {
        let a = u64::from_be_bytes(blob.get(p..p + 8).ok_or_else(|| Error::Boot("rsv".into()))?.try_into().unwrap());
        let s = u64::from_be_bytes(blob.get(p + 8..p + 16).ok_or_else(|| Error::Boot("rsv".into()))?.try_into().unwrap());
        p += 16;
        if a == 0 && s == 0 {
            break;
        }
        rsv.push((a, s));
    }
    let strings = &blob[off_strings..total];
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut p = off_struct;
    loop {
        let tok = be32(blob, p)?;
        p += 4;
        match tok {
            FDT_BEGIN_NODE => {
                let end = blob[p..].iter().position(|&b| b == 0).ok_or_else(|| Error::Boot("node name".into()))?;
                let name = String::from_utf8_lossy(&blob[p..p + end]).into_owned();
                p = (p + end + 1 + 3) & !3;
                stack.push(Node { name, ..Default::default() });
            }
            FDT_END_NODE => {
                let n = stack.pop().ok_or_else(|| Error::Boot("unbalanced FDT".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => root = Some(n),
                }
            }
            FDT_PROP => {
                let len = be32(blob, p)? as usize;
                let nameoff = be32(blob, p + 4)? as usize;
                p += 8;
                let val = blob.get(p..p + len).ok_or_else(|| Error::Boot("prop value".into()))?.to_vec();
                p = (p + len + 3) & !3;
                let s = strings.get(nameoff..).ok_or_else(|| Error::Boot("prop name".into()))?;
                let e = s.iter().position(|&b| b == 0).ok_or_else(|| Error::Boot("prop name".into()))?;
                let name = String::from_utf8_lossy(&s[..e]).into_owned();
                stack.last_mut().ok_or_else(|| Error::Boot("prop outside node".into()))?.props.push((name, val));
            }
            FDT_NOP => {}
            FDT_END => break,
            t => return Err(Error::Boot(format!("bad FDT token {t:#x}"))),
        }
    }
    Ok((root.ok_or_else(|| Error::Boot("empty FDT".into()))?, rsv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut w = FdtWriter::new();
        w.add_reservation(0x8000_0000, 0x1000);
        w.begin_node("").unwrap();
        w.prop_u32("#address-cells", 2).unwrap();
        w.prop_str("compatible", "apex,virt").unwrap();
        let ph = w.alloc_phandle();
        w.begin_node("intc@8000000").unwrap();
        w.prop_strs("compatible", &["arm,gic-v3"]).unwrap();
        w.prop_u64s("reg", &[0x0800_0000, 0x10000]).unwrap();
        w.prop_empty("interrupt-controller").unwrap();
        w.prop_u32("phandle", ph).unwrap();
        w.end_node().unwrap();
        w.begin_node("chosen").unwrap();
        w.prop_str("bootargs", "console=hvc0").unwrap();
        w.end_node().unwrap();
        w.end_node().unwrap();
        let blob = w.finish().unwrap();
        assert_eq!(blob.len() % 4, 0);

        let (root, rsv) = parse(&blob).unwrap();
        assert_eq!(rsv, vec![(0x8000_0000, 0x1000)]);
        assert_eq!(root.prop_u32("#address-cells"), Some(2));
        assert_eq!(root.prop_str("compatible"), Some("apex,virt"));
        let gic = root.find("/intc@8000000").unwrap();
        assert_eq!(gic.prop_cells("reg").unwrap(), vec![0, 0x0800_0000, 0, 0x10000]);
        assert_eq!(gic.prop("interrupt-controller"), Some(&[][..]));
        assert_eq!(gic.prop_u32("phandle"), Some(1));
        assert_eq!(root.find("/chosen").unwrap().prop_str("bootargs"), Some("console=hvc0"));
        let dts = root.to_dts();
        assert!(dts.contains("compatible = \"arm,gic-v3\""), "{dts}");
    }

    #[test]
    fn unbalanced_is_error() {
        let mut w = FdtWriter::new();
        w.begin_node("").unwrap();
        assert!(w.finish().is_err());
        let mut w = FdtWriter::new();
        assert!(w.end_node().is_err());
        assert!(w.prop_u32("x", 1).is_err());
    }

    #[test]
    fn strings_are_deduplicated() {
        let mut w = FdtWriter::new();
        w.begin_node("").unwrap();
        for i in 0..10 {
            w.begin_node(&format!("n{i}")).unwrap();
            w.prop_str("compatible", "x").unwrap();
            w.end_node().unwrap();
        }
        w.end_node().unwrap();
        let blob = w.finish().unwrap();
        let size_strings = u32::from_be_bytes(blob[32..36].try_into().unwrap());
        assert_eq!(size_strings as usize, "compatible\0".len());
    }
}
