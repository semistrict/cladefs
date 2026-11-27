// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Emulated devices.

pub(crate) mod bus;
#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
pub(crate) mod guest_tests;
pub(crate) mod pci;
#[cfg(target_arch = "aarch64")]
pub(crate) mod rtc;
pub(crate) mod serial;
pub(crate) mod virtio;

pub(crate) use pc::Pci;

mod pc {
    use std::sync::{Arc, Mutex};

    use vm_memory::GuestMemoryMmap;

    use super::pci::{BarAllocator, PciBus, PciHost};
    use super::virtio::VirtioDevice;
    use super::virtio::pci::VirtioPci;
    use super::virtio::worker::DeviceThread;
    use crate::arch::layout::PCI_MSI_GSI_BASE;
    use crate::error::{Error, Result};

    /// A PC's PCI bus with its virtio devices, and the thread that runs
    /// their queues.
    pub(crate) struct Pci {
        // Field order: the thread stops before the devices it uses drop.
        _thread: DeviceThread,
        bus: PciBus,
        transports: Vec<Arc<Mutex<VirtioPci>>>,
    }

    /// Every transport of a bus, held ([`Pci::quiesce`]).
    pub(crate) struct Quiesced<'a> {
        transports: Vec<std::sync::MutexGuard<'a, VirtioPci>>,
    }

    impl Quiesced<'_> {
        /// Has every device take what the guest gave it and hand over
        /// what its host side holds, as the device thread would: the type
        /// of one that had something for the guest, if any (it rang the
        /// doorbell, and may have interrupted the guest).
        pub(crate) fn settle(&mut self) -> Option<u16> {
            self.transports.iter_mut().find_map(|t| t.settle())
        }

        /// Every function's state, in bus order.
        pub(crate) fn save(&self, e: &mut crate::snapshot::codec::Encoder) -> Result<()> {
            e.count(self.transports.len());
            self.transports.iter().try_for_each(|t| t.save(e))
        }

        /// Readies every device for a snapshot of the machine, as `mode` says
        /// (`VirtioPci::prepare_snapshot`): the extent store devices' state, in bus order.
        /// [`snapshots_done`](Self::snapshots_done) follows, whether or not this succeeded.
        pub(crate) fn take_snapshots(
            &mut self,
            mode: virtio_extent::SnapshotMode,
        ) -> Result<Vec<virtio_extent::persist::State>> {
            let mut states = Vec::new();
            for t in &mut self.transports {
                states.extend(t.prepare_snapshot(mode)?);
            }
            Ok(states)
        }

        /// Has every device let go of what it serves the guest from (`VirtioPci::release_cell`):
        /// a live migration's hand-over of the extent store devices' cells. The first that can't
        /// refuses, those before it released.
        #[cfg_attr(
            target_os = "linux",
            allow(dead_code, reason = "KVM migrates no extent store device yet")
        )]
        pub(crate) fn release_cells(&mut self) -> Result<()> {
            self.transports.iter_mut().try_for_each(|t| t.release_cell())
        }

        /// Has every device take back what [`release_cells`](Self::release_cells) let go of: the
        /// migration aborted. The first that can't refuses, those before it reopened.
        #[cfg_attr(
            target_os = "linux",
            allow(dead_code, reason = "KVM migrates no extent store device yet")
        )]
        pub(crate) fn reopen_cells(&mut self) -> Result<()> {
            self.transports.iter_mut().try_for_each(|t| t.reopen_cell())
        }

        /// After a snapshot of the machine, `kept` or failed: every device carries on, what
        /// they took for a failed one dropped.
        pub(crate) fn snapshots_done(&mut self, kept: bool) {
            for t in &mut self.transports {
                t.snapshot_done(kept);
            }
        }

        /// Every function's state for a suspension, in bus order: each
        /// device hands over what must outlive its machine. On an error,
        /// what was handed over is taken back.
        pub(crate) fn hand_over(&mut self, e: &mut crate::snapshot::codec::Encoder) -> Result<()> {
            e.count(self.transports.len());
            let handed = self.transports.iter_mut().try_for_each(|t| t.hand_over(e));
            if let Err(e) = handed {
                return Err(match self.take_back() {
                    Ok(()) => e,
                    Err(back) => Error::Snapshot(format!("{e}; then taking back failed: {back}")),
                });
            }
            Ok(())
        }

        /// Undoes [`hand_over`](Self::hand_over), the suspension having
        /// failed: every device carries on as before it. The first error,
        /// after trying every device.
        pub(crate) fn take_back(&mut self) -> Result<()> {
            let mut first = Ok(());
            for t in &mut self.transports {
                if let Err(e) = t.take_back()
                    && first.is_ok()
                {
                    first = Err(e);
                }
            }
            first
        }
    }

    impl Pci {
        /// Puts `devices` on bus 0 at device numbers 1, 2, ... with their
        /// BARs assigned from `bars` and live, and starts the device thread.
        /// `bars` may have handed out addresses already: those of the
        /// shared memory devices placed themselves (`VirtioDevice::placed_shm`).
        pub(crate) fn new(
            devices: Vec<Box<dyn VirtioDevice>>,
            mem: &GuestMemoryMmap,
            host: &dyn PciHost,
            doorbell: &Arc<crate::suspend::Doorbell>,
            mut bars: BarAllocator,
        ) -> Result<Self> {
            if devices.len() > 31 {
                return Err(Error::InvalidConfig(format!(
                    "{} PCI devices; bus 0 holds 31",
                    devices.len()
                )));
            }
            let mut gsi = PCI_MSI_GSI_BASE;
            let mut bus = PciBus::default();
            let mut transports = Vec::new();
            for (i, device) in devices.into_iter().enumerate() {
                let mut transport = VirtioPci::new(device, mem.clone(), &mut bars, gsi, host)?;
                transport.set_doorbell(doorbell.clone());
                gsi += u32::from(transport.vector_count());
                let transport = Arc::new(Mutex::new(transport));
                bus.add(i as u8 + 1, transport.clone());
                transports.push(transport);
            }
            let thread = DeviceThread::spawn(transports.clone())?;
            Ok(Self { _thread: thread, bus, transports })
        }

        /// A guest read; false if the address is not PCI's.
        pub(crate) fn mmio_read(&self, addr: u64, data: &mut [u8]) -> bool {
            self.bus.mmio_read(addr, data)
        }

        /// A guest write; false if the address is not PCI's.
        pub(crate) fn mmio_write(
            &self,
            addr: u64,
            data: &[u8],
            host: &dyn PciHost,
        ) -> Result<bool> {
            self.bus.mmio_write(addr, data, host)
        }

        /// Holds every transport's lock until the returned value drops:
        /// the device thread waits, so nothing writes guest memory or
        /// changes a device meanwhile (with every vCPU stopped, nothing
        /// else does), and a snapshot sees one moment.
        pub(crate) fn quiesce(&self) -> Quiesced<'_> {
            Quiesced {
                transports: self
                    .transports
                    .iter()
                    .map(|t| t.lock().unwrap_or_else(|e| e.into_inner()))
                    .collect(),
            }
        }

        /// Puts back every function's state [`Quiesced::save`] wrote, into
        /// a bus built with the same devices, then has the bus decode the
        /// BARs where the guest had them.
        pub(crate) fn restore(
            &self,
            d: &mut crate::snapshot::codec::Decoder<'_>,
            host: &dyn PciHost,
        ) -> Result<()> {
            let n = d.count(1)?;
            if n != self.transports.len() {
                return Err(Error::Snapshot(format!(
                    "the snapshot has {n} PCI devices, this machine {}",
                    self.transports.len()
                )));
            }
            for t in &self.transports {
                t.lock().unwrap_or_else(|e| e.into_inner()).restore(d, host)?;
            }
            self.bus.refresh_bars();
            Ok(())
        }

        /// Interrupts every active queue's driver once: after a restore.
        pub(crate) fn signal_all_queues(&self) {
            for t in &self.transports {
                t.lock().unwrap_or_else(|e| e.into_inner()).signal_all_queues();
            }
        }

        /// The transport counters of the device at device number `device`.
        #[cfg(test)]
        pub(crate) fn stats(&self, device: u8) -> super::virtio::pci::Stats {
            let transport = &self.transports[usize::from(device) - 1];
            transport.lock().unwrap().stats()
        }
    }
}
