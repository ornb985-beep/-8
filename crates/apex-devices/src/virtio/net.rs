//! virtio-net with pluggable host back ends:
//!
//! * `unixgram:<path>` — one Ethernet frame per datagram (vfkit/gvproxy
//!   protocol; gvproxy provides user-mode NAT + DNS + port forwarding for
//!   `adb connect`).
//! * `unixstream:<path>` — 4-byte big-endian length prefix per frame
//!   (socket_vmnet protocol, bridged/shared vmnet networking).
//! * callback — frames handed to the embedding app (the macOS frontend uses
//!   this to drive vmnet.framework directly).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use apex_core::mem::GuestMemory;
use apex_core::sync::StopFlag;
use apex_core::{Error, Result};

use super::{device_type, ActivateContext, Queue, VirtioDevice, VirtioInterrupt};

pub mod feature {
    pub const CSUM: u64 = 1 << 0;
    pub const MTU: u64 = 1 << 3;
    pub const MAC: u64 = 1 << 5;
    pub const STATUS: u64 = 1 << 16;
}

const RXQ: usize = 0;
const TXQ: usize = 1;
const HDR_LEN: usize = 12; // virtio_net_hdr_v1
const MAX_FRAME: usize = 65550;
const RX_BACKLOG: usize = 256;

/// Where frames go and come from.
pub trait NetBackend: Send + Sync {
    fn send(&self, frame: &[u8]) -> io::Result<()>;
    /// Blocking receive with timeout; Ok(0) on timeout.
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
}

pub struct UnixDgramBackend {
    sock: UnixDatagram,
}

impl UnixDgramBackend {
    /// Bind a local socket next to `remote` and connect to it.
    pub fn connect(remote: &Path) -> io::Result<Self> {
        let local: PathBuf = std::env::temp_dir().join(format!("apex-net-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&local);
        let sock = UnixDatagram::bind(&local)?;
        sock.connect(remote)?;
        // vfkit protocol: announce ourselves with a magic datagram.
        let _ = sock.send(b"VFKT");
        Ok(UnixDgramBackend { sock })
    }
}

impl NetBackend for UnixDgramBackend {
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        self.sock.send(frame).map(|_| ())
    }
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.sock.set_read_timeout(Some(timeout))?;
        match self.sock.recv(buf) {
            Ok(n) => Ok(n),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(0),
            Err(e) => Err(e),
        }
    }
}

pub struct UnixStreamBackend {
    tx: Mutex<UnixStream>,
    rx: Mutex<UnixStream>,
}

impl UnixStreamBackend {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let s = UnixStream::connect(path)?;
        Ok(UnixStreamBackend { rx: Mutex::new(s.try_clone()?), tx: Mutex::new(s) })
    }
}

impl NetBackend for UnixStreamBackend {
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        let mut s = self.tx.lock().unwrap();
        s.write_all(&(frame.len() as u32).to_be_bytes())?;
        s.write_all(frame)
    }
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let mut s = self.rx.lock().unwrap();
        s.set_read_timeout(Some(timeout))?;
        let mut len = [0u8; 4];
        match s.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(0),
            Err(e) => return Err(e),
        }
        let n = u32::from_be_bytes(len) as usize;
        if n > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized frame"));
        }
        s.set_read_timeout(None)?;
        s.read_exact(&mut buf[..n])?;
        Ok(n)
    }
}

pub type FrameFn = Box<dyn Fn(&[u8]) + Send + Sync>;

/// Frames exchanged with the embedding application.
pub struct ChannelBackend {
    pub to_host: FrameFn,
    from_host: Mutex<VecDeque<Vec<u8>>>,
    cv: std::sync::Condvar,
}

impl ChannelBackend {
    pub fn new(to_host: FrameFn) -> Arc<ChannelBackend> {
        Arc::new(ChannelBackend { to_host, from_host: Mutex::new(VecDeque::new()), cv: std::sync::Condvar::new() })
    }
    /// Deliver a frame from the host network to the guest.
    pub fn inject(&self, frame: &[u8]) {
        let mut q = self.from_host.lock().unwrap();
        if q.len() < RX_BACKLOG {
            q.push_back(frame.to_vec());
            self.cv.notify_one();
        }
    }
}

impl NetBackend for Arc<ChannelBackend> {
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        (self.to_host)(frame);
        Ok(())
    }
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let q = self.from_host.lock().unwrap();
        let (mut q, _) = self.cv.wait_timeout_while(q, timeout, |q| q.is_empty()).unwrap();
        match q.pop_front() {
            Some(f) => {
                let n = f.len().min(buf.len());
                buf[..n].copy_from_slice(&f[..n]);
                Ok(n)
            }
            None => Ok(0),
        }
    }
}

struct Active {
    mem: GuestMemory,
    queues: Vec<Queue>,
    irq: VirtioInterrupt,
    backlog: VecDeque<Vec<u8>>,
}

pub struct Net {
    mac: [u8; 6],
    mtu: u16,
    backend: Arc<dyn NetBackend>,
    active: Arc<Mutex<Option<Active>>>,
    stop: Arc<StopFlag>,
    rx_thread: Option<JoinHandle<()>>,
}

impl Net {
    pub fn new(mac: [u8; 6], backend: Arc<dyn NetBackend>) -> Net {
        Net { mac, mtu: 1500, backend, active: Arc::new(Mutex::new(None)), stop: Arc::new(StopFlag::new()), rx_thread: None }
    }

    fn stop_rx(&mut self) {
        self.stop.stop();
        if let Some(t) = self.rx_thread.take() {
            let _ = t.join();
        }
        self.stop.reset();
    }
}

/// Deliver backlog frames into guest RX buffers. Returns true if all were
/// delivered.
fn deliver(act: &mut Active) -> bool {
    let q = &mut act.queues[RXQ];
    let mut any = false;
    while let Some(frame) = act.backlog.front() {
        let Some(chain) = q.pop(&act.mem) else { break };
        let mut w = chain.writer(&act.mem);
        let mut hdr = [0u8; HDR_LEN];
        hdr[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers
        let n = if w.available() >= HDR_LEN + frame.len() {
            w.write(&hdr) + w.write(frame)
        } else {
            apex_core::debug!("virtio-net: rx buffer too small for {} byte frame", frame.len());
            0
        };
        let _ = q.add_used(&act.mem, chain.head, n as u32);
        act.backlog.pop_front();
        any = true;
    }
    if any && q.needs_notification(&act.mem) {
        act.irq.signal_used_queue();
    }
    act.backlog.is_empty()
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        device_type::NET
    }
    fn name(&self) -> &str {
        "net"
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 256]
    }
    fn device_features(&self) -> u64 {
        feature::MAC | feature::STATUS | feature::MTU
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 12];
        cfg[..6].copy_from_slice(&self.mac);
        cfg[6..8].copy_from_slice(&1u16.to_le_bytes()); // VIRTIO_NET_S_LINK_UP
        cfg[8..10].copy_from_slice(&1u16.to_le_bytes()); // max_virtqueue_pairs
        cfg[10..12].copy_from_slice(&self.mtu.to_le_bytes());
        super::read_config_bytes(&cfg, offset, data)
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        *self.active.lock().unwrap() = Some(Active { mem: ctx.mem, queues: ctx.queues, irq: ctx.interrupt, backlog: VecDeque::new() });
        let (active, backend, stop) = (self.active.clone(), self.backend.clone(), self.stop.clone());
        let t = std::thread::Builder::new()
            .name("virtio-net-rx".into())
            .spawn(move || {
                let mut buf = vec![0u8; MAX_FRAME];
                while !stop.is_stopped() {
                    match backend.recv(&mut buf, Duration::from_millis(100)) {
                        Ok(0) => {}
                        Ok(n) => {
                            let mut a = active.lock().unwrap();
                            if let Some(act) = a.as_mut() {
                                if act.backlog.len() < RX_BACKLOG {
                                    act.backlog.push_back(buf[..n].to_vec());
                                }
                                deliver(act);
                            }
                        }
                        Err(e) => {
                            apex_core::warn!("virtio-net backend receive failed: {e}");
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            })
            .map_err(Error::Io)?;
        self.rx_thread = Some(t);
        Ok(())
    }
    fn queue_notify(&mut self, index: u16) {
        let mut a = self.active.lock().unwrap();
        let Some(act) = a.as_mut() else { return };
        match index as usize {
            RXQ => {
                deliver(act);
            }
            TXQ => {
                let mut frames = Vec::new();
                let q = &mut act.queues[TXQ];
                let mut any = false;
                while let Some(chain) = q.pop(&act.mem) {
                    let mut r = chain.reader(&act.mem);
                    r.skip(HDR_LEN);
                    frames.push(r.read_to_vec(MAX_FRAME));
                    let _ = q.add_used(&act.mem, chain.head, 0);
                    any = true;
                }
                if any && q.needs_notification(&act.mem) {
                    act.irq.signal_used_queue();
                }
                drop(a);
                for f in frames {
                    if let Err(e) = self.backend.send(&f) {
                        apex_core::debug!("virtio-net send failed: {e}");
                    }
                }
            }
            _ => {}
        }
    }
    fn reset(&mut self) {
        self.stop_rx();
        *self.active.lock().unwrap() = None;
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stop_rx();
    }
}

/// Locally administered, stable MAC derived from a seed.
pub fn mac_from_seed(seed: &str) -> [u8; 6] {
    let g = super::disk::guid_from(seed);
    [0x02, 0x41, 0x50, g[0], g[1], g[2]] // 02:41:50 = locally administered "AP"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::test_driver::Driver;
    use apex_core::irq::{IrqLine, RecordingIrqChip};

    #[test]
    fn tx_and_rx_through_channel() {
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let s2 = sent.clone();
        let chan = ChannelBackend::new(Box::new(move |f| s2.lock().unwrap().push(f.to_vec())));
        let backend: Arc<dyn NetBackend> = Arc::new(chan.clone());
        let mut net = Net::new(mac_from_seed("t"), backend);

        let mut rx = Driver::new(16);
        let mut tx = rx.sibling();
        let (_, bufs) = rx.add_chain(&[], &[2048]);
        let chip = Arc::new(RecordingIrqChip::default());
        net.activate(ActivateContext {
            mem: rx.mem.clone(),
            queues: vec![rx.q.clone(), tx.q.clone()],
            interrupt: VirtioInterrupt::new(IrqLine::new(chip.clone(), 2)),
            features: 0,
        })
        .unwrap();

        chan.inject(&[0xaa; 60]);
        for _ in 0..200 {
            if !rx.take_used().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let got = rx.read(bufs[0], HDR_LEN + 60);
        assert_eq!(u16::from_le_bytes([got[10], got[11]]), 1);
        assert!(got[HDR_LEN..].iter().all(|&b| b == 0xaa));
        assert!(chip.level(2));

        // TX: guest sends a frame with a virtio_net_hdr in front.
        let mut pkt = vec![0u8; HDR_LEN];
        pkt.extend_from_slice(&[0x55; 42]);
        tx.add_chain(&[&pkt], &[]);
        net.queue_notify(TXQ as u16);
        assert_eq!(tx.take_used().len(), 1);
        assert_eq!(sent.lock().unwrap().as_slice(), &[vec![0x55u8; 42]]);
        net.reset();
    }

    #[test]
    fn mac_is_locally_administered() {
        let m = mac_from_seed("apex");
        assert_eq!(m[0] & 0b11, 0b10);
        assert_eq!(m, mac_from_seed("apex"));
    }
}
