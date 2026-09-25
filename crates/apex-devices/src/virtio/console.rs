//! virtio-console (single port). Used for `hvc0` (kernel console + logcat)
//! and for the host<->guest hardware bridge channel.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use apex_core::mem::GuestMemory;
use apex_core::Result;

use super::{device_type, ActivateContext, Queue, VirtioDevice, VirtioInterrupt};

const RX: usize = 0;
const TX: usize = 1;
const MAX_PENDING: usize = 1 << 20;

pub type OutputFn = Box<dyn Fn(&[u8]) + Send + Sync>;

struct Active {
    mem: GuestMemory,
    queues: Vec<Queue>,
    irq: VirtioInterrupt,
}

struct Shared {
    active: Mutex<Option<Active>>,
    pending_rx: Mutex<VecDeque<u8>>,
    output: OutputFn,
}

impl Shared {
    fn flush_rx(&self) {
        let mut a = self.active.lock().unwrap();
        let Some(act) = a.as_mut() else { return };
        let mut pending = self.pending_rx.lock().unwrap();
        let q = &mut act.queues[RX];
        let mut any = false;
        while !pending.is_empty() {
            let Some(chain) = q.pop(&act.mem) else { break };
            let mut w = chain.writer(&act.mem);
            let (a1, a2) = pending.as_slices();
            let mut n = w.write(a1);
            if n == a1.len() {
                n += w.write(a2);
            }
            pending.drain(..n);
            let _ = q.add_used(&act.mem, chain.head, n as u32);
            any = true;
        }
        if any && q.needs_notification(&act.mem) {
            act.irq.signal_used_queue();
        }
    }

    fn drain_tx(&self) {
        let mut out = Vec::new();
        {
            let mut a = self.active.lock().unwrap();
            let Some(act) = a.as_mut() else { return };
            let q = &mut act.queues[TX];
            let mut any = false;
            while let Some(chain) = q.pop(&act.mem) {
                out.extend(chain.reader(&act.mem).read_to_vec(1 << 20));
                let _ = q.add_used(&act.mem, chain.head, 0);
                any = true;
            }
            if any && q.needs_notification(&act.mem) {
                act.irq.signal_used_queue();
            }
        }
        if !out.is_empty() {
            (self.output)(&out);
        }
    }
}

/// Host side handle used to type into the guest console.
#[derive(Clone)]
pub struct ConsoleInput(Arc<Shared>);

impl ConsoleInput {
    pub fn send(&self, data: &[u8]) {
        {
            let mut p = self.0.pending_rx.lock().unwrap();
            if p.len() + data.len() > MAX_PENDING {
                return;
            }
            p.extend(data);
        }
        self.0.flush_rx();
    }
}

pub struct Console {
    shared: Arc<Shared>,
    name: String,
}

impl Console {
    pub fn new(name: &str, output: OutputFn) -> Console {
        Console {
            shared: Arc::new(Shared { active: Mutex::new(None), pending_rx: Mutex::new(VecDeque::new()), output }),
            name: name.to_string(),
        }
    }

    pub fn input(&self) -> ConsoleInput {
        ConsoleInput(self.shared.clone())
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        device_type::CONSOLE
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 256]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 12];
        cfg[0..2].copy_from_slice(&80u16.to_le_bytes());
        cfg[2..4].copy_from_slice(&25u16.to_le_bytes());
        cfg[4..8].copy_from_slice(&1u32.to_le_bytes());
        super::read_config_bytes(&cfg, offset, data)
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // emerg_wr: early emergency output.
        if offset == 8 && !data.is_empty() {
            (self.shared.output)(&data[..1]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        *self.shared.active.lock().unwrap() = Some(Active { mem: ctx.mem, queues: ctx.queues, irq: ctx.interrupt });
        self.shared.flush_rx();
        Ok(())
    }
    fn queue_notify(&mut self, index: u16) {
        match index as usize {
            RX => self.shared.flush_rx(),
            TX => self.shared.drain_tx(),
            _ => {}
        }
    }
    fn reset(&mut self) {
        *self.shared.active.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::test_driver::Driver;
    use apex_core::irq::{IrqLine, RecordingIrqChip};

    #[test]
    fn tx_and_rx() {
        let mut rx = Driver::new(8);
        let mut tx = rx.sibling();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let mut con = Console::new("console", Box::new(move |b| g.lock().unwrap().extend_from_slice(b)));
        let input = con.input();
        input.send(b"early"); // before activation: buffered
        let (_, outs) = rx.add_chain(&[], &[16]);
        let chip = Arc::new(RecordingIrqChip::default());
        con.activate(ActivateContext {
            mem: rx.mem.clone(),
            queues: vec![rx.q.clone(), tx.q.clone()],
            interrupt: VirtioInterrupt::new(IrqLine::new(chip.clone(), 1)),
            features: 0,
        })
        .unwrap();
        assert_eq!(rx.take_used(), vec![(0, 5)]);
        assert_eq!(rx.read(outs[0], 5), b"early");
        assert!(chip.level(1));
        tx.add_chain(&[b"hello ", b"host"], &[]);
        con.queue_notify(TX as u16);
        assert_eq!(got.lock().unwrap().as_slice(), b"hello host");
    }
}
