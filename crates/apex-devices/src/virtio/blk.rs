//! virtio-blk with multi-queue, FLUSH, DISCARD and WRITE_ZEROES.
//!
//! Requests are served on a dedicated I/O thread with `pread`/`pwrite`
//! straight into guest RAM (no bounce buffers).

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use apex_core::mem::GuestMemory;
use apex_core::sync::{Event, StopFlag};
use apex_core::{Error, Result};

use super::disk::{DiskBackend, SECTOR};
use super::{device_type, ActivateContext, DescriptorChain, Queue, VirtioDevice, VirtioInterrupt};

pub mod feature {
    pub const SIZE_MAX: u64 = 1 << 1;
    pub const SEG_MAX: u64 = 1 << 2;
    pub const RO: u64 = 1 << 5;
    pub const BLK_SIZE: u64 = 1 << 6;
    pub const FLUSH: u64 = 1 << 9;
    pub const TOPOLOGY: u64 = 1 << 10;
    pub const MQ: u64 = 1 << 12;
    pub const DISCARD: u64 = 1 << 13;
    pub const WRITE_ZEROES: u64 = 1 << 14;
}

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const QUEUE_SIZE: u16 = 256;
const SEG_MAX: u32 = 254;
const MAX_DISCARD_SECTORS: u32 = 1 << 22;

pub struct Block {
    disk: Arc<dyn DiskBackend>,
    serial: String,
    num_queues: u16,
    worker: Option<Worker>,
}

struct Worker {
    kick: Arc<Event>,
    stop: Arc<StopFlag>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.stop();
        self.kick.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Block {
    pub fn new(disk: Arc<dyn DiskBackend>, serial: &str, num_queues: u16) -> Block {
        Block { disk, serial: serial.chars().take(20).collect(), num_queues: num_queues.clamp(1, 16), worker: None }
    }

    fn config(&self) -> [u8; 64] {
        let mut c = [0u8; 64];
        let sectors = self.disk.size() / SECTOR;
        c[0..8].copy_from_slice(&sectors.to_le_bytes());
        c[8..12].copy_from_slice(&(1u32 << 20).to_le_bytes()); // size_max
        c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        c[20..24].copy_from_slice(&4096u32.to_le_bytes()); // blk_size
        c[24] = 3; // physical_block_exp: 4 KiB physical blocks
        c[26..28].copy_from_slice(&8u16.to_le_bytes()); // min_io_size (sectors)
        c[28..32].copy_from_slice(&256u32.to_le_bytes()); // opt_io_size
        c[34..36].copy_from_slice(&self.num_queues.to_le_bytes());
        c[36..40].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[40..44].copy_from_slice(&1u32.to_le_bytes()); // max_discard_seg
        c[44..48].copy_from_slice(&8u32.to_le_bytes()); // discard_sector_alignment
        c[48..52].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[52..56].copy_from_slice(&1u32.to_le_bytes());
        c[56] = 1; // write_zeroes_may_unmap
        c
    }
}

/// Process one request; returns bytes written into the chain.
fn handle_request(disk: &dyn DiskBackend, serial: &str, mem: &GuestMemory, chain: &DescriptorChain) -> Result<u32> {
    let mut r = chain.reader(mem);
    let mut w = chain.writer(mem);
    let mut status_w = w.split_tail(1).ok_or_else(|| Error::Device("virtio-blk request without status byte".into()))?;
    let rtype: u32 = u32::from_le(r.read_obj()?);
    let _reserved: u32 = r.read_obj()?;
    let sector: u64 = u64::from_le(r.read_obj()?);
    let offset = sector.checked_mul(SECTOR);

    let size = disk.size();
    let in_range = |off: Option<u64>, len: u64| off.is_some_and(|o| o.checked_add(len).is_some_and(|e| e <= size));

    let status = match rtype {
        T_IN => {
            let len = w.available() as u64;
            if !in_range(offset, len) {
                S_IOERR
            } else {
                let mut pos = offset.unwrap();
                let res = w.produce_with(len as usize, |buf| {
                    disk.read_at(buf, pos)?;
                    pos += buf.len() as u64;
                    Ok(buf.len())
                });
                if res.is_ok() {
                    S_OK
                } else {
                    S_IOERR
                }
            }
        }
        T_OUT => {
            let len = r.available() as u64;
            if disk.read_only() || !in_range(offset, len) {
                S_IOERR
            } else {
                let mut pos = offset.unwrap();
                let res = r.consume_with(len as usize, |buf| {
                    disk.write_at(buf, pos)?;
                    pos += buf.len() as u64;
                    Ok(buf.len())
                });
                if res.is_ok() {
                    S_OK
                } else {
                    S_IOERR
                }
            }
        }
        T_FLUSH => {
            if disk.flush().is_ok() {
                S_OK
            } else {
                S_IOERR
            }
        }
        T_GET_ID => {
            let mut id = [0u8; 20];
            id[..serial.len()].copy_from_slice(serial.as_bytes());
            let n = w.available().min(20);
            w.write(&id[..n]);
            S_OK
        }
        T_DISCARD | T_WRITE_ZEROES => {
            let mut st = S_OK;
            while r.available() >= 16 {
                let s: u64 = u64::from_le(r.read_obj()?);
                let n: u32 = u32::from_le(r.read_obj()?);
                let flags: u32 = u32::from_le(r.read_obj()?);
                let (off, len) = (s.checked_mul(SECTOR), n as u64 * SECTOR);
                if disk.read_only() || !in_range(off, len) || n > MAX_DISCARD_SECTORS || flags & !1 != 0 {
                    st = S_IOERR;
                    break;
                }
                let res = if rtype == T_DISCARD { disk.discard(off.unwrap(), len) } else { disk.write_zeroes(off.unwrap(), len) };
                if res.is_err() {
                    st = S_IOERR;
                    break;
                }
            }
            st
        }
        _ => S_UNSUPP,
    };
    status_w.write_all(&[status])?;
    Ok((w.bytes_written() + 1) as u32)
}

fn process_queue(disk: &dyn DiskBackend, serial: &str, mem: &GuestMemory, q: &mut Queue, irq: &VirtioInterrupt) {
    loop {
        let mut any = false;
        while let Some(chain) = q.pop(mem) {
            any = true;
            let len = match handle_request(disk, serial, mem, &chain) {
                Ok(n) => n,
                Err(e) => {
                    apex_core::warn!("virtio-blk: {e}");
                    0
                }
            };
            let _ = q.add_used(mem, chain.head, len);
        }
        if any && q.needs_notification(mem) {
            irq.signal_used_queue();
        }
        if !q.enable_notification(mem) {
            break;
        }
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        device_type::BLOCK
    }
    fn name(&self) -> &str {
        "blk"
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE; self.num_queues as usize]
    }
    fn device_features(&self) -> u64 {
        let mut f = feature::SIZE_MAX | feature::SEG_MAX | feature::BLK_SIZE | feature::FLUSH | feature::TOPOLOGY;
        if self.num_queues > 1 {
            f |= feature::MQ;
        }
        if self.disk.read_only() {
            f |= feature::RO;
        } else {
            f |= feature::DISCARD | feature::WRITE_ZEROES;
        }
        f
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::read_config_bytes(&self.config(), offset, data)
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let kick = Arc::new(Event::new());
        let stop = Arc::new(StopFlag::new());
        let disk = self.disk.clone();
        let serial = self.serial.clone();
        let queues = Arc::new(Mutex::new(ctx.queues));
        let (k, s) = (kick.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("virtio-blk-{}", self.serial))
            .spawn(move || {
                while !s.is_stopped() {
                    let mut qs = queues.lock().unwrap();
                    for q in qs.iter_mut().filter(|q| q.ready) {
                        process_queue(disk.as_ref(), &serial, &ctx.mem, q, &ctx.interrupt);
                    }
                    drop(qs);
                    k.wait();
                }
            })
            .map_err(Error::Io)?;
        self.worker = Some(Worker { kick, stop, thread: Some(thread) });
        Ok(())
    }
    fn queue_notify(&mut self, _index: u16) {
        if let Some(w) = &self.worker {
            w.kick.signal();
        }
    }
    fn reset(&mut self) {
        self.worker = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::disk::MemDisk;
    use crate::virtio::queue::test_driver::Driver;
    use apex_core::irq::{IrqLine, RecordingIrqChip};

    fn req(t: u32, sector: u64) -> Vec<u8> {
        let mut v = t.to_le_bytes().to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&sector.to_le_bytes());
        v
    }

    fn run(drv: &mut Driver, disk: &Arc<MemDisk>) {
        let chip = Arc::new(RecordingIrqChip::default());
        let irq = VirtioInterrupt::new(IrqLine::new(chip.clone(), 0));
        let mut q = drv.q.clone();
        process_queue(disk.as_ref(), "APEX-TEST", &drv.mem, &mut q, &irq);
        drv.q = q;
    }

    #[test]
    fn read_write_flush_getid() {
        let mut data = vec![0u8; 64 * 1024];
        data[512..1024].fill(0xab);
        let disk = Arc::new(MemDisk::new(data, false));
        let mut drv = Driver::new(16);
        let (_, rd) = drv.add_chain(&[&req(T_IN, 1)], &[512, 1]);
        let wr_data = vec![0xcd; 1024];
        let (_, ws) = drv.add_chain(&[&req(T_OUT, 4), &wr_data], &[1]);
        let (_, fs) = drv.add_chain(&[&req(T_FLUSH, 0)], &[1]);
        let (_, id) = drv.add_chain(&[&req(T_GET_ID, 0)], &[20, 1]);
        let (_, bad) = drv.add_chain(&[&req(T_IN, 127)], &[1024, 1]); // past end
        let (_, un) = drv.add_chain(&[&req(99, 0)], &[1]);
        run(&mut drv, &disk);
        let used = drv.take_used();
        assert_eq!(used.len(), 6);
        assert_eq!(used[0].1, 513);
        assert!(drv.read(rd[0], 512).iter().all(|&b| b == 0xab));
        assert_eq!(drv.read(rd[1], 1), vec![S_OK]);
        assert_eq!(drv.read(ws[0], 1), vec![S_OK]);
        assert!(disk.snapshot()[2048..3072].iter().all(|&b| b == 0xcd));
        assert_eq!(drv.read(fs[0], 1), vec![S_OK]);
        assert_eq!(&drv.read(id[0], 9), b"APEX-TEST");
        assert_eq!(drv.read(bad[1], 1), vec![S_IOERR]);
        assert_eq!(drv.read(un[0], 1), vec![S_UNSUPP]);
    }

    #[test]
    fn write_zeroes_and_readonly() {
        let disk = Arc::new(MemDisk::new(vec![0xff; 8192], false));
        let mut drv = Driver::new(8);
        let mut seg = 2u64.to_le_bytes().to_vec();
        seg.extend_from_slice(&3u32.to_le_bytes());
        seg.extend_from_slice(&0u32.to_le_bytes());
        let (_, st) = drv.add_chain(&[&req(T_WRITE_ZEROES, 0), &seg], &[1]);
        run(&mut drv, &disk);
        assert_eq!(drv.read(st[0], 1), vec![S_OK]);
        let snap = disk.snapshot();
        assert!(snap[1024..2560].iter().all(|&b| b == 0));
        assert_eq!(snap[2560], 0xff);

        let ro = Arc::new(MemDisk::new(vec![0; 4096], true));
        let mut drv = Driver::new(8);
        let (_, st) = drv.add_chain(&[&req(T_OUT, 0), &[1u8; 512]], &[1]);
        run(&mut drv, &ro);
        assert_eq!(drv.read(st[0], 1), vec![S_IOERR]);
    }

    #[test]
    fn config_space() {
        let b = Block::new(Arc::new(MemDisk::new(vec![0; 1 << 20], false)), "S", 4);
        let mut cap = [0u8; 8];
        b.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 2048);
        let mut nq = [0u8; 2];
        b.read_config(34, &mut nq);
        assert_eq!(u16::from_le_bytes(nq), 4);
        assert_ne!(b.device_features() & feature::MQ, 0);
        assert_eq!(b.queue_max_sizes().len(), 4);
    }
}
