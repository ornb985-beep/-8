//! virtio-rng: entropy for /dev/hwrng straight from the host CSPRNG.

use apex_core::Result;

use super::{device_type, ActivateContext, VirtioDevice};

#[derive(Default)]
pub struct Rng {
    ctx: Option<ActivateContext>,
}

impl Rng {
    pub fn new() -> Rng {
        Rng::default()
    }
}

impl VirtioDevice for Rng {
    fn device_type(&self) -> u32 {
        device_type::RNG
    }
    fn name(&self) -> &str {
        "rng"
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.ctx = Some(ctx);
        Ok(())
    }
    fn queue_notify(&mut self, _index: u16) {
        let Some(ctx) = self.ctx.as_mut() else { return };
        let q = &mut ctx.queues[0];
        let mut any = false;
        while let Some(chain) = q.pop(&ctx.mem) {
            let mut w = chain.writer(&ctx.mem);
            let n = w
                .produce_with(w.available().min(64 * 1024), |buf| {
                    apex_core::sys::fill_random(buf)?;
                    Ok(buf.len())
                })
                .unwrap_or(0);
            let _ = q.add_used(&ctx.mem, chain.head, n as u32);
            any = true;
        }
        if any && q.needs_notification(&ctx.mem) {
            ctx.interrupt.signal_used_queue();
        }
    }
    fn reset(&mut self) {
        self.ctx = None;
    }
}
