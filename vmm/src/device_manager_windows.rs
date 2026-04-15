// SPDX-License-Identifier: Apache-2.0
//
// Windows device manager: creates and wires devices using the bus architecture.
//
// Replaces the ad-hoc PIO/MMIO dispatch in whp_demo with proper Bus-based
// routing, matching the Unix device_manager pattern.

use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use devices::interrupt_controller::InterruptController;
use hypervisor::{HypervisorVmError, VmOps};
use log::info;
use vm_device::BusDevice;
use vm_device::Bus;
use vm_memory::GuestAddress;

use crate::memory_manager::MemoryManager;

// Standard x86 device addresses
const SERIAL_PORT_BASE: u64 = 0x3F8;
const SERIAL_PORT_SIZE: u64 = 0x8;
const IOAPIC_BASE: u64 = 0xFEC0_0000;
const IOAPIC_SIZE: u64 = 0x100;

/// Manages device creation and bus routing for Windows/WHP VMs.
pub struct DeviceManager {
    /// Port I/O bus (serial, PCI config, PIT, etc.)
    io_bus: Arc<Bus>,
    /// Memory-mapped I/O bus (IOAPIC, device BARs, etc.)
    mmio_bus: Arc<Bus>,
    /// Serial device
    serial: Arc<Mutex<devices::legacy::serial::Serial>>,
    /// IOAPIC (if kernel mode)
    ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>>,
}

impl DeviceManager {
    /// Create a new device manager with serial console and optional IOAPIC.
    pub fn new(
        vm: &Arc<dyn hypervisor::Vm>,
        _memory_manager: &Arc<Mutex<MemoryManager>>,
        has_kernel: bool,
    ) -> anyhow::Result<Self> {
        let io_bus = Arc::new(Bus::new());
        let mmio_bus = Arc::new(Bus::new());

        // ── Serial device (16550 UART at 0x3F8) ─────────────────────────
        let serial_irq = Arc::new(WhpSerialInterrupt {
            vm: vm.clone(),
            vector: 0x34,
        });
        let serial = Arc::new(Mutex::new(
            devices::legacy::serial::Serial::new_out(
                "serial0".to_string(),
                serial_irq,
                Box::new(std::io::stdout()),
                None,
            ),
        ));
        io_bus
            .insert(serial.clone(), SERIAL_PORT_BASE, SERIAL_PORT_SIZE)
            .map_err(|e| anyhow!("Failed to register serial on I/O bus: {e:?}"))?;
        info!("Registered serial device at I/O {SERIAL_PORT_BASE:#X}");

        // ── IOAPIC (for kernel mode) ─────────────────────────────────────
        let ioapic = if has_kernel {
            let interrupt_manager = WhpInterruptManager { vm: vm.clone() };
            let ioapic_dev = devices::ioapic::Ioapic::new(
                "ioapic".to_string(),
                GuestAddress(0xFEE0_0000),
                &interrupt_manager,
                None,
            )
            .context("Failed to create IOAPIC")?;
            let ioapic = Arc::new(Mutex::new(ioapic_dev));

            mmio_bus
                .insert(ioapic.clone(), IOAPIC_BASE, IOAPIC_SIZE)
                .map_err(|e| anyhow!("Failed to register IOAPIC on MMIO bus: {e:?}"))?;

            // Pre-program IOAPIC redirect entries
            {
                let mut guard = ioapic.lock().unwrap();
                Self::ioapic_write_reg(&mut guard, 0x10, 0x0000_0030); // IRQ0→vec 0x30
                Self::ioapic_write_reg(&mut guard, 0x11, 0x0000_0000);
                Self::ioapic_write_reg(&mut guard, 0x18, 0x0000_0034); // IRQ4→vec 0x34
                Self::ioapic_write_reg(&mut guard, 0x19, 0x0000_0000);
            }
            info!("Registered IOAPIC at MMIO {IOAPIC_BASE:#X}, pre-programmed IRQ0→0x30, IRQ4→0x34");

            Some(ioapic)
        } else {
            None
        };

        // ── PIT timer stub (0x40-0x43) ──────────────────────────────────
        let pit = Arc::new(Mutex::new(PitStub::new()));
        io_bus.insert(pit, 0x40, 0x4)
            .map_err(|e| anyhow!("Failed to register PIT: {e:?}"))?;

        // ── Port 0x61 (NMI/PIT gate) ────────────────────────────────────
        let port61 = Arc::new(Mutex::new(Port61Stub::default()));
        io_bus.insert(port61, 0x61, 0x1)
            .map_err(|e| anyhow!("Failed to register port 0x61: {e:?}"))?;

        // ── PIC stubs (0x20-0x21, 0xA0-0xA1) ────────────────────────────
        let pic_master = Arc::new(Mutex::new(PicStub::default()));
        let pic_slave = Arc::new(Mutex::new(PicStub::default()));
        io_bus.insert(pic_master, 0x20, 0x2)
            .map_err(|e| anyhow!("Failed to register PIC master: {e:?}"))?;
        io_bus.insert(pic_slave, 0xA0, 0x2)
            .map_err(|e| anyhow!("Failed to register PIC slave: {e:?}"))?;

        // ── CMOS/RTC stub (0x70-0x71) ───────────────────────────────────
        let cmos = Arc::new(Mutex::new(CmosStub::default()));
        io_bus.insert(cmos, 0x70, 0x2)
            .map_err(|e| anyhow!("Failed to register CMOS: {e:?}"))?;

        info!("Registered PIT, port 0x61, PIC, CMOS on I/O bus");

        Ok(DeviceManager {
            io_bus,
            mmio_bus,
            serial,
            ioapic,
        })
    }

    fn ioapic_write_reg(ioapic: &mut devices::ioapic::Ioapic, reg: u32, val: u32) {
        let reg_bytes = reg.to_le_bytes();
        let val_bytes = val.to_le_bytes();
        ioapic.write(IOAPIC_BASE, 0x00, &reg_bytes);
        ioapic.write(IOAPIC_BASE, 0x10, &val_bytes);
    }

    /// Get the serial device for the serial manager.
    pub fn serial(&self) -> &Arc<Mutex<devices::legacy::serial::Serial>> {
        &self.serial
    }

    /// Create a VmOps handler that dispatches to buses.
    pub fn vm_ops(&self) -> Arc<dyn VmOps> {
        Arc::new(BusVmOps {
            io_bus: self.io_bus.clone(),
            mmio_bus: self.mmio_bus.clone(),
        })
    }

    /// Get the I/O bus (for additional device registration).
    pub fn io_bus(&self) -> &Arc<Bus> {
        &self.io_bus
    }

    /// Get the MMIO bus (for additional device registration).
    pub fn mmio_bus(&self) -> &Arc<Bus> {
        &self.mmio_bus
    }

    /// Register PCI config I/O on a bus (static helper for external callers).
    pub fn register_pci_config_static<R, W>(
        io_bus: &Arc<Bus>,
        read_fn: R,
        write_fn: W,
    ) -> anyhow::Result<()>
    where
        R: Fn(u32, usize) -> u32 + Send + 'static,
        W: Fn(u32, usize, u64, &[u8]) + Send + 'static,
    {
        let bridge = Arc::new(Mutex::new(PciConfigBridge {
            config_address: 0,
            read_fn: Box::new(read_fn),
            write_fn: Box::new(write_fn),
        }));
        io_bus
            .insert(bridge, 0xCF8, 0x8)
            .map_err(|e| anyhow!("Failed to register PCI config I/O: {e:?}"))?;
        info!("Registered PCI config I/O at 0xCF8");
        Ok(())
    }
}

// ── VmOps implementation using Bus dispatch ──────────────────────────────────

struct BusVmOps {
    io_bus: Arc<Bus>,
    mmio_bus: Arc<Bus>,
}

impl VmOps for BusVmOps {
    fn guest_mem_write(&self, _gpa: u64, _buf: &[u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }

    fn guest_mem_read(&self, _gpa: u64, _buf: &mut [u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }

    fn mmio_read(&self, gpa: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        self.mmio_bus.read(gpa, data).ok();
        Ok(())
    }

    fn mmio_write(&self, gpa: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        self.mmio_bus.write(gpa, data).ok();
        Ok(())
    }

    fn pio_read(&self, port: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        self.io_bus.read(port, data).ok();
        Ok(())
    }

    fn pio_write(&self, port: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        self.io_bus.write(port, data).ok();
        Ok(())
    }
}

// ── WHP interrupt types ──────────────────────────────────────────────────────

use hypervisor::{InterruptSourceConfig, MsiIrqSourceConfig};
use vm_device::interrupt::{
    InterruptIndex, InterruptManager, InterruptSourceGroup, MsiIrqGroupConfig,
};

/// Fixed-vector interrupt for the serial device.
struct WhpSerialInterrupt {
    vm: Arc<dyn hypervisor::Vm>,
    vector: u8,
}

impl InterruptSourceGroup for WhpSerialInterrupt {
    fn trigger(&self, _index: InterruptIndex) -> Result<(), std::io::Error> {
        use hypervisor::whp::WhpVm;
        if let Some(whp) = self.vm.as_any().downcast_ref::<WhpVm>() {
            let _ = whp.request_interrupt(self.vector, 0);
        }
        Ok(())
    }

    fn update(
        &self,
        _index: InterruptIndex,
        _config: InterruptSourceConfig,
        _masked: bool,
        _set_gsi: bool,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn set_gsi(&self) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn notifier(&self, _index: InterruptIndex) -> Option<platform::EventFd> {
        None
    }
}

/// MSI interrupt manager for WHP (used by IOAPIC).
struct WhpInterruptManager {
    vm: Arc<dyn hypervisor::Vm>,
}

impl InterruptManager for WhpInterruptManager {
    type GroupConfig = MsiIrqGroupConfig;

    fn create_group(
        &self,
        config: Self::GroupConfig,
    ) -> Result<Arc<dyn InterruptSourceGroup>, std::io::Error> {
        Ok(Arc::new(WhpMsiInterrupt {
            vm: self.vm.clone(),
            base: config.base as u8,
        }))
    }

    fn destroy_group(&self, _group: Arc<dyn InterruptSourceGroup>) -> Result<(), std::io::Error> {
        Ok(())
    }
}

struct WhpMsiInterrupt {
    vm: Arc<dyn hypervisor::Vm>,
    base: u8,
}

impl InterruptSourceGroup for WhpMsiInterrupt {
    fn trigger(&self, index: InterruptIndex) -> Result<(), std::io::Error> {
        use hypervisor::whp::WhpVm;
        if let Some(whp) = self.vm.as_any().downcast_ref::<WhpVm>() {
            let _ = whp.request_interrupt(self.base + index as u8, 0);
        }
        Ok(())
    }

    fn update(
        &self,
        _index: InterruptIndex,
        _config: InterruptSourceConfig,
        _masked: bool,
        _set_gsi: bool,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn set_gsi(&self) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn notifier(&self, _index: InterruptIndex) -> Option<platform::EventFd> {
        None
    }
}

// ── Stub bus devices ─────────────────────────────────────────────────────────

use std::sync::Barrier;

struct PitStub {
    start: std::time::Instant,
    reload: u16,
}

impl PitStub {
    fn new() -> Self {
        PitStub { start: std::time::Instant::now(), reload: 0 }
    }
}

impl BusDevice for PitStub {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if offset == 0x02 && !data.is_empty() {
            let elapsed_us = self.start.elapsed().as_micros() as u64;
            let ticks = (elapsed_us * 1193) / 1000;
            let reload = self.reload as u64;
            let counter = if reload > 0 { reload.saturating_sub(ticks % (reload + 1)) } else { 0 };
            data[0] = counter as u8;
        }
    }
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if offset == 0x02 && !data.is_empty() { self.reload = data[0] as u16; }
        None
    }
}

#[derive(Default)]
struct Port61Stub { value: u8 }

impl BusDevice for Port61Stub {
    fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) {
        if !data.is_empty() { self.value ^= 0x20; data[0] = self.value; }
    }
    fn write(&mut self, _base: u64, _offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if !data.is_empty() { self.value = data[0]; }
        None
    }
}

#[derive(Default)]
struct PicStub { imr: u8 }

impl BusDevice for PicStub {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if !data.is_empty() { data[0] = if offset == 1 { self.imr } else { 0 }; }
    }
    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if offset == 1 && !data.is_empty() { self.imr = data[0]; }
        None
    }
}

#[derive(Default)]
struct CmosStub;

impl BusDevice for CmosStub {
    fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) { data.fill(0); }
}

/// PCI config I/O bridge — handles 0xCF8 (address) and 0xCFC (data).
struct PciConfigBridge {
    config_address: u32,
    read_fn: Box<dyn Fn(u32, usize) -> u32 + Send>,
    write_fn: Box<dyn Fn(u32, usize, u64, &[u8]) + Send>,
}

impl BusDevice for PciConfigBridge {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        match offset {
            0..=3 => {
                // Read config address register
                let start = offset as usize;
                for (i, d) in data.iter_mut().enumerate() {
                    *d = (self.config_address >> ((start + i) * 8)) as u8;
                }
            }
            4..=7 => {
                // Read config data — dispatch to registered devices
                let enabled = (self.config_address & 0x8000_0000) != 0;
                if !enabled {
                    data.fill(0xFF);
                    return;
                }
                let reg = ((self.config_address >> 2) & 0x3F) as usize;
                let val = (self.read_fn)(self.config_address, reg);
                let start = (offset - 4) as usize;
                for (i, d) in data.iter_mut().enumerate() {
                    *d = (val >> ((start + i) * 8)) as u8;
                }
            }
            _ => data.fill(0xFF),
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        match offset {
            0..=3 => {
                // Write config address register
                let start = offset as usize;
                for (i, d) in data.iter().enumerate() {
                    let shift = (start + i) * 8;
                    self.config_address = (self.config_address & !(0xFF << shift)) | ((*d as u32) << shift);
                }
            }
            4..=7 => {
                // Write config data — dispatch to registered devices
                let enabled = (self.config_address & 0x8000_0000) != 0;
                if !enabled {
                    return None;
                }
                let reg = ((self.config_address >> 2) & 0x3F) as usize;
                (self.write_fn)(self.config_address, reg, offset - 4, data);
            }
            _ => {}
        }
        None
    }
}
