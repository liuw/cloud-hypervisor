// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Copyright © 2026, Microsoft Corporation
//
// Windows Hypervisor Platform (WHP) backend for cloud-hypervisor.
//

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::anyhow;
use log::{debug, warn};
use windows::Win32::System::Hypervisor::*;

use crate::cpu::HypervisorCpuError;
use crate::vm::{self, InterruptSourceConfig, VmOps};
use crate::{HypervisorType, HypervisorVmConfig, cpu, hypervisor};

pub mod x86_64;

pub use x86_64::*;

use crate::arch::x86::{CpuIdEntry, FpuState, LapicState, MsrEntry, SpecialRegisters};
use crate::{ClockData, CpuState, IrqRoutingEntry, MpState, StandardRegisters};

// ─── Constants ───────────────────────────────────────────────────────────────

const MAX_WHP_VCPUS: u32 = 240;

/// Number of x86_64 standard registers we read/write in a batch.
const NUM_STANDARD_REGS: usize = 18;

/// WHP register names for the 18 standard GP registers + RIP + RFLAGS.
const STANDARD_REG_NAMES: [WHV_REGISTER_NAME; NUM_STANDARD_REGS] = [
    WHvX64RegisterRax,
    WHvX64RegisterRbx,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
    WHvX64RegisterRip,
    WHvX64RegisterRflags,
];

// ─── WhpHypervisor ───────────────────────────────────────────────────────────

pub struct WhpHypervisor;

impl WhpHypervisor {
    /// Check whether WHP is available on this system.
    pub fn is_available() -> hypervisor::Result<bool> {
        let mut capability = WHV_CAPABILITY::default();
        // SAFETY: WHvGetCapability with HypervisorPresent returns a BOOL.
        // We provide a correctly-sized output buffer.
        let written = unsafe {
            let mut written: u32 = 0;
            WHvGetCapability(
                WHvCapabilityCodeHypervisorPresent,
                &raw mut capability as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_CAPABILITY>() as u32,
                Some(&mut written),
            )
            .map_err(|e| {
                hypervisor::HypervisorError::HypervisorAvailableCheck(anyhow!(
                    "WHvGetCapability failed: {e}"
                ))
            })?;
            written
        };

        if written == 0 {
            return Ok(false);
        }

        // SAFETY: The union field HypervisorPresent is valid when the capability
        // code is WHvCapabilityCodeHypervisorPresent.
        let present = unsafe { capability.HypervisorPresent.as_bool() };
        Ok(present)
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> hypervisor::Result<Arc<dyn hypervisor::Hypervisor>> {
        Ok(Arc::new(WhpHypervisor))
    }
}

impl hypervisor::Hypervisor for WhpHypervisor {
    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::Whp
    }

    fn create_vm(
        &self,
        _config: HypervisorVmConfig,
    ) -> hypervisor::Result<Arc<dyn vm::Vm>> {
        // SAFETY: Creates a new WHP partition.
        let partition = unsafe {
            WHvCreatePartition().map_err(|e| {
                hypervisor::HypervisorError::VmCreate(anyhow!(
                    "WHvCreatePartition failed: {e}"
                ))
            })?
        };

        // Set the local APIC emulation mode to X2APIC (most flexible).
        set_partition_property(
            partition,
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            WHvX64LocalApicEmulationModeXApic.0 as u64,
        )
        .map_err(|e| {
            hypervisor::HypervisorError::VmSetup(anyhow!(
                "Failed to set APIC mode: {e}"
            ))
        })?;

        // NOTE: WHvSetupPartition is deferred until the first vCPU is created,
        // because WHP requires ProcessorCount to be set before setup.
        // We track setup state in WhpVm.
        Ok(Arc::new(WhpVm {
            partition,
            is_setup: RwLock::new(false),
            vcpu_count: RwLock::new(0),
            irq_routing: RwLock::new(HashMap::new()),
        }))
    }

    fn get_supported_cpuid(&self) -> hypervisor::Result<Vec<CpuIdEntry>> {
        // On WHP, we rely on the host CPU's CPUID values.
        // Use the __cpuid intrinsic to gather supported leaves.
        let mut entries = Vec::new();

        // Get basic CPUID leaves 0x0 through 0xD
        for function in 0..=0xD {
            let result = std::arch::x86_64::__cpuid_count(function, 0);
            entries.push(CpuIdEntry {
                function,
                index: 0,
                flags: 0,
                eax: result.eax,
                ebx: result.ebx,
                ecx: result.ecx,
                edx: result.edx,
            });
        }

        // Get extended CPUID leaves
        let ext_max = std::arch::x86_64::__cpuid(0x8000_0000).eax;
        for function in 0x8000_0000..=ext_max.min(0x8000_0020) {
            let result = std::arch::x86_64::__cpuid_count(function, 0);
            entries.push(CpuIdEntry {
                function,
                index: 0,
                flags: 0,
                eax: result.eax,
                ebx: result.ebx,
                ecx: result.ecx,
                edx: result.edx,
            });
        }

        Ok(entries)
    }

    fn get_max_vcpus(&self) -> u32 {
        MAX_WHP_VCPUS
    }
}

// ─── WhpVm ───────────────────────────────────────────────────────────────────

pub struct WhpVm {
    partition: WHV_PARTITION_HANDLE,
    is_setup: RwLock<bool>,
    vcpu_count: RwLock<u32>,
    irq_routing: RwLock<HashMap<u32, WhpIrqRoutingEntry>>,
}

// SAFETY: WHV_PARTITION_HANDLE is a Windows HANDLE which is safe to send/share
// across threads. WHP partition operations are thread-safe.
unsafe impl Send for WhpVm {}
// SAFETY: See above — WHP partition handles support concurrent access.
unsafe impl Sync for WhpVm {}

impl WhpVm {
    /// Returns the raw WHP partition handle for direct API access (e.g., interrupt injection).
    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition
    }

    /// Inject a fixed interrupt into the guest vCPU using WHvRegisterPendingEvent.
    /// Also kicks the vCPU out of HLT if needed.
    pub fn inject_interrupt(&self, vector: u8, vp_index: u32) -> std::result::Result<(), anyhow::Error> {
        // Use WHvRegisterPendingEvent (ExtInt) to inject the interrupt,
        // matching how QEMU's WHPX backend does it.
        let reg_names = [
            WHvRegisterPendingEvent,
            WHvRegisterInternalActivityState,
        ];
        let mut reg_values = [WHV_REGISTER_VALUE::default(); 2];

        // Set up the pending external interrupt event
        // WHV_X64_PENDING_EXT_INT_EVENT layout:
        // bit 0: EventPending = 1
        // bits 1-4: EventType = WHvX64PendingEventExtInt (5)
        // bits 5-15: Reserved
        // bits 16-23: Vector
        let event_val: u64 = 1 // EventPending
            | (5u64 << 1) // EventType = ExtInt
            | ((vector as u64) << 16); // Vector
        reg_values[0] = reg64_value(event_val);

        // Clear HLT suspend state so the vCPU wakes from HLT
        // WHV_INTERNAL_ACTIVITY_REGISTER: bit 0 = StartupSuspend, bit 1 = HaltSuspend
        // Set HaltSuspend = 0 (clear it)
        reg_values[1] = reg64_value(0);

        // SAFETY: Sets virtual processor registers for interrupt injection.
        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                vp_index,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
            .map_err(|e| anyhow!("WHvSetVirtualProcessorRegisters (interrupt inject) failed: {e}"))?;
        }
        Ok(())
    }

    /// Cancel the vCPU run to force it out of HLT or internal loops.
    pub fn cancel_run(&self, vp_index: u32) -> std::result::Result<(), anyhow::Error> {
        // SAFETY: Cancels the run of a virtual processor.
        unsafe {
            WHvCancelRunVirtualProcessor(self.partition, vp_index, 0)
                .map_err(|e| anyhow!("WHvCancelRunVirtualProcessor failed: {e}"))?;
        }
        Ok(())
    }

    /// Ensure the partition is set up. Must be called before creating vCPUs.
    fn ensure_setup(&self) -> vm::Result<()> {
        let mut is_setup = self.is_setup.write().unwrap();
        if *is_setup {
            return Ok(());
        }

        // Set processor count before setup.
        let count = *self.vcpu_count.read().unwrap();
        let proc_count = if count > 0 { count } else { 1 };

        set_partition_property(
            self.partition,
            WHvPartitionPropertyCodeProcessorCount,
            proc_count as u64,
        )
        .map_err(|e| {
            vm::HypervisorVmError::InitializeVm(anyhow!(
                "Failed to set processor count: {e}"
            ))
        })?;

        // No extended VM exits needed — WHP's APIC emulation handles timers internally.
        // HLT is handled by WHP internally (the LAPIC timer should wake the vCPU).

        // SAFETY: Completes the partition setup.
        unsafe {
            WHvSetupPartition(self.partition).map_err(|e| {
                vm::HypervisorVmError::InitializeVm(anyhow!(
                    "WHvSetupPartition failed: {e}"
                ))
            })?;
        }

        *is_setup = true;
        Ok(())
    }
}

impl vm::Vm for WhpVm {
    #[cfg(target_arch = "x86_64")]
    fn set_identity_map_address(&self, _address: u64) -> vm::Result<()> {
        // WHP manages identity mapping internally.
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn set_tss_address(&self, _offset: usize) -> vm::Result<()> {
        // WHP manages TSS internally.
        Ok(())
    }

    fn create_irq_chip(&self) -> vm::Result<()> {
        // WHP provides built-in APIC emulation configured via partition
        // properties. No separate irqchip creation is needed.
        Ok(())
    }

    fn create_vcpu(
        &self,
        id: u32,
        vm_ops: Option<Arc<dyn VmOps>>,
    ) -> vm::Result<Box<dyn cpu::Vcpu>> {
        // Track vCPU count and ensure partition is set up.
        {
            let mut count = self.vcpu_count.write().unwrap();
            *count = (*count).max(id + 1);
        }
        self.ensure_setup()?;

        // SAFETY: Creates a virtual processor within the partition.
        unsafe {
            WHvCreateVirtualProcessor(self.partition, id, 0).map_err(|e| {
                vm::HypervisorVmError::CreateVcpu(anyhow!(
                    "WHvCreateVirtualProcessor failed: {e}"
                ))
            })?;
        }

        Ok(Box::new(WhpVcpu {
            partition: self.partition,
            vp_index: id,
            vm_ops,
        }))
    }

    fn make_routing_entry(
        &self,
        gsi: u32,
        config: &InterruptSourceConfig,
    ) -> IrqRoutingEntry {
        match config {
            InterruptSourceConfig::MsiIrq(msi) => IrqRoutingEntry::Whp(WhpIrqRoutingEntry {
                gsi,
                address_hi: msi.high_addr,
                address_lo: msi.low_addr,
                data: msi.data,
            }),
            InterruptSourceConfig::LegacyIrq(_) => {
                IrqRoutingEntry::Whp(WhpIrqRoutingEntry {
                    gsi,
                    ..Default::default()
                })
            }
        }
    }

    fn set_gsi_routing(&self, entries: &[IrqRoutingEntry]) -> vm::Result<()> {
        let mut routing = self.irq_routing.write().unwrap();
        routing.clear();
        for entry in entries {
            let IrqRoutingEntry::Whp(whp_entry) = entry;
            routing.insert(whp_entry.gsi, *whp_entry);
        }
        Ok(())
    }

    unsafe fn create_user_memory_region(
        &self,
        _slot: u32,
        guest_phys_addr: u64,
        memory_size: usize,
        userspace_addr: *mut u8,
        readonly: bool,
        _log_dirty_pages: bool,
    ) -> vm::Result<()> {
        self.ensure_setup()?;

        let flags = if readonly {
            WHvMapGpaRangeFlagRead
        } else {
            WHV_MAP_GPA_RANGE_FLAGS(
                WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0,
            )
        };

        // SAFETY: The caller guarantees the memory region is valid.
        unsafe {
            WHvMapGpaRange(
                self.partition,
                userspace_addr as *const std::ffi::c_void,
                guest_phys_addr,
                memory_size as u64,
                flags,
            )
            .map_err(|e| {
                vm::HypervisorVmError::CreateUserMemory(anyhow!(
                    "WHvMapGpaRange failed: {e}"
                ))
            })?;
        }

        Ok(())
    }

    unsafe fn remove_user_memory_region(
        &self,
        _slot: u32,
        guest_phys_addr: u64,
        memory_size: usize,
        _userspace_addr: *mut u8,
        _readonly: bool,
        _log_dirty_pages: bool,
    ) -> vm::Result<()> {
        // SAFETY: Unmaps a previously mapped GPA range.
        unsafe {
            WHvUnmapGpaRange(self.partition, guest_phys_addr, memory_size as u64).map_err(|e| {
                vm::HypervisorVmError::RemoveUserMemory(anyhow!(
                    "WHvUnmapGpaRange failed: {e}"
                ))
            })?;
        }

        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn enable_split_irq(&self) -> vm::Result<()> {
        // WHP handles interrupt splitting through its built-in APIC.
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn get_clock(&self) -> vm::Result<ClockData> {
        // Read TSC from the partition. WHP doesn't have a direct clock API,
        // so we return a TSC-based clock.
        Ok(ClockData::Whp(WhpClockData::default()))
    }

    #[cfg(target_arch = "x86_64")]
    fn set_clock(&self, _data: &ClockData) -> vm::Result<()> {
        // WHP manages TSC internally.
        Ok(())
    }

    fn start_dirty_log(&self) -> vm::Result<()> {
        // Dirty page tracking via WHvQueryGpaRangeDirtyBitmap is available
        // in newer WHP versions. For now, return unsupported.
        warn!("WHP dirty page tracking not yet implemented");
        Ok(())
    }

    fn stop_dirty_log(&self) -> vm::Result<()> {
        Ok(())
    }

    fn get_dirty_log(&self, _slot: u32, _base_gpa: u64, _memory_size: u64) -> vm::Result<Vec<u64>> {
        warn!("WHP dirty page tracking not yet implemented");
        Ok(Vec::new())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for WhpVm {
    fn drop(&mut self) {
        // SAFETY: Cleans up the WHP partition.
        if let Err(e) = unsafe { WHvDeletePartition(self.partition) } {
            warn!("Failed to delete WHP partition: {e}");
        }
    }
}

// ─── WhpVcpu ─────────────────────────────────────────────────────────────────

pub struct WhpVcpu {
    partition: WHV_PARTITION_HANDLE,
    vp_index: u32,
    vm_ops: Option<Arc<dyn VmOps>>,
}

// SAFETY: WHV_PARTITION_HANDLE is thread-safe, and vp_index is just a u32.
// WHP virtual processor operations are safe for concurrent use.
unsafe impl Send for WhpVcpu {}
// SAFETY: See above — WHP VP operations support concurrent access.
unsafe impl Sync for WhpVcpu {}

impl WhpVcpu {
    /// Read a set of registers from the virtual processor.
    fn get_registers(
        &self,
        names: &[WHV_REGISTER_NAME],
    ) -> cpu::Result<Vec<WHV_REGISTER_VALUE>> {
        let mut values = vec![WHV_REGISTER_VALUE::default(); names.len()];

        // SAFETY: We provide correctly sized buffers.
        unsafe {
            WHvGetVirtualProcessorRegisters(
                self.partition,
                self.vp_index,
                names.as_ptr(),
                names.len() as u32,
                values.as_mut_ptr(),
            )
            .map_err(|e| {
                HypervisorCpuError::GetStandardRegs(anyhow!(
                    "WHvGetVirtualProcessorRegisters failed: {e}"
                ))
            })?;
        }

        Ok(values)
    }

    /// Write a set of registers to the virtual processor.
    fn set_registers(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> cpu::Result<()> {
        // SAFETY: We provide correctly sized buffers.
        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.vp_index,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
            .map_err(|e| {
                HypervisorCpuError::SetStandardRegs(anyhow!(
                    "WHvSetVirtualProcessorRegisters failed: {e}"
                ))
            })?;
        }

        Ok(())
    }

    /// Convert WHV_REGISTER_VALUE array to WhpStandardRegisters.
    fn values_to_standard_regs(values: &[WHV_REGISTER_VALUE]) -> WhpStandardRegisters {
        // SAFETY: Reg64 is valid for GP registers.
        unsafe {
            WhpStandardRegisters {
                rax: values[0].Reg64,
                rbx: values[1].Reg64,
                rcx: values[2].Reg64,
                rdx: values[3].Reg64,
                rsi: values[4].Reg64,
                rdi: values[5].Reg64,
                rsp: values[6].Reg64,
                rbp: values[7].Reg64,
                r8: values[8].Reg64,
                r9: values[9].Reg64,
                r10: values[10].Reg64,
                r11: values[11].Reg64,
                r12: values[12].Reg64,
                r13: values[13].Reg64,
                r14: values[14].Reg64,
                r15: values[15].Reg64,
                rip: values[16].Reg64,
                rflags: values[17].Reg64,
            }
        }
    }

    /// Convert WhpStandardRegisters to WHV_REGISTER_VALUE array.
    fn standard_regs_to_values(regs: &WhpStandardRegisters) -> [WHV_REGISTER_VALUE; NUM_STANDARD_REGS] {
        [
            reg64_value(regs.rax),
            reg64_value(regs.rbx),
            reg64_value(regs.rcx),
            reg64_value(regs.rdx),
            reg64_value(regs.rsi),
            reg64_value(regs.rdi),
            reg64_value(regs.rsp),
            reg64_value(regs.rbp),
            reg64_value(regs.r8),
            reg64_value(regs.r9),
            reg64_value(regs.r10),
            reg64_value(regs.r11),
            reg64_value(regs.r12),
            reg64_value(regs.r13),
            reg64_value(regs.r14),
            reg64_value(regs.r15),
            reg64_value(regs.rip),
            reg64_value(regs.rflags),
        ]
    }

    /// Handle an MMIO exit from WHvRunVirtualProcessor.
    fn handle_mmio(
        &self,
        context: &WHV_MEMORY_ACCESS_CONTEXT,
    ) -> std::result::Result<(), HypervisorCpuError> {
        let vm_ops = self.vm_ops.as_ref().ok_or_else(|| {
            HypervisorCpuError::RunVcpu(anyhow!("MMIO exit but no VmOps configured"))
        })?;

        let gpa = context.Gpa;
        // SAFETY: Access the union bitfield for access type info.
        let access_info = unsafe { context.AccessInfo.AsUINT32 };
        let is_write = (access_info & 1) != 0; // Bit 0 = AccessType (write=1)
        let access_size = ((access_info >> 4) & 0xF) as usize; // Bits 4-7 = AccessSize
        let instruction_length = context.InstructionByteCount as u64;

        if is_write {
            // For MMIO writes, we need to read the data from the instruction bytes.
            // The data is embedded in the instruction encoding.
            let data = vec![0u8; access_size.max(1)];
            vm_ops.mmio_write(gpa, &data).map_err(|e| {
                HypervisorCpuError::RunVcpu(anyhow!("MMIO write error: {e}"))
            })?;
        } else {
            let mut data = vec![0u8; access_size.max(1)];
            vm_ops.mmio_read(gpa, &mut data).map_err(|e| {
                HypervisorCpuError::RunVcpu(anyhow!("MMIO read error: {e}"))
            })?;
        }

        // Advance RIP past the instruction.
        let rip_name = [WHvX64RegisterRip];
        let values = self.get_registers(&rip_name)?;
        // SAFETY: Reg64 is valid for the RIP register value.
        let new_rip = unsafe { values[0].Reg64 } + instruction_length;
        let new_values = [reg64_value(new_rip)];
        self.set_registers(&rip_name, &new_values)?;

        Ok(())
    }

    /// Handle a PIO exit from WHvRunVirtualProcessor.
    fn handle_pio(
        &self,
        context: &WHV_X64_IO_PORT_ACCESS_CONTEXT,
        instr_len: u64,
    ) -> std::result::Result<(), HypervisorCpuError> {
        let vm_ops = self.vm_ops.as_ref().ok_or_else(|| {
            HypervisorCpuError::RunVcpu(anyhow!("PIO exit but no VmOps configured"))
        })?;

        let port = context.PortNumber as u64;
        // SAFETY: Access the union bitfield for access info.
        let access_info = unsafe { context.AccessInfo.AsUINT32 };
        let is_write = (access_info & 1) != 0; // Bit 0 = IsWrite
        let access_size = ((access_info >> 1) & 0x7) as usize; // Bits 1-3 = AccessSize
        let size = access_size.max(1);

        if is_write {
            let data = &context.Rax.to_ne_bytes()[..size];
            vm_ops.pio_write(port, data).map_err(|e| {
                HypervisorCpuError::RunVcpu(anyhow!("PIO write error: {e}"))
            })?;
        } else {
            let mut data = vec![0u8; size];
            vm_ops.pio_read(port, &mut data).map_err(|e| {
                HypervisorCpuError::RunVcpu(anyhow!("PIO read error: {e}"))
            })?;
            // Write the result back to RAX.
            let rax = u64::from_ne_bytes({
                let mut buf = [0u8; 8];
                buf[..size].copy_from_slice(&data);
                buf
            });
            let names = [WHvX64RegisterRax];
            let values = [reg64_value(rax)];
            self.set_registers(&names, &values)?;
        }

        // Advance RIP past the I/O instruction.
        let rip_name = [WHvX64RegisterRip];
        let rip_values = self.get_registers(&rip_name)?;
        // SAFETY: Reg64 is valid for the RIP register.
        let new_rip = unsafe { rip_values[0].Reg64 } + instr_len;
        let new_rip_val = [reg64_value(new_rip)];
        self.set_registers(&rip_name, &new_rip_val)?;

        Ok(())
    }
}

impl cpu::Vcpu for WhpVcpu {
    fn create_standard_regs(&self) -> StandardRegisters {
        StandardRegisters::Whp(WhpStandardRegisters::default())
    }

    fn get_regs(&self) -> cpu::Result<StandardRegisters> {
        let values = self.get_registers(&STANDARD_REG_NAMES)?;
        Ok(StandardRegisters::Whp(Self::values_to_standard_regs(&values)))
    }

    fn set_regs(&self, regs: &StandardRegisters) -> cpu::Result<()> {
        let StandardRegisters::Whp(whp_regs) = regs;
        let values = Self::standard_regs_to_values(whp_regs);
        self.set_registers(&STANDARD_REG_NAMES, &values)
    }

    fn get_sregs(&self) -> cpu::Result<SpecialRegisters> {
        let names = [
            WHvX64RegisterCr0,
            WHvX64RegisterCr2,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterCr8,
            WHvX64RegisterEfer,
            WHvX64RegisterApicBase,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterFs,
            WHvX64RegisterGs,
            WHvX64RegisterSs,
            WHvX64RegisterTr,
            WHvX64RegisterLdtr,
            WHvX64RegisterGdtr,
            WHvX64RegisterIdtr,
        ];
        let values = self.get_registers(&names)?;

        fn whv_to_seg(val: &WHV_REGISTER_VALUE) -> crate::arch::x86::SegmentRegister {
            // SAFETY: Segment field is valid for segment register names.
            let seg = unsafe { &val.Segment };
            let attr = unsafe { seg.Anonymous.Attributes };
            crate::arch::x86::SegmentRegister {
                base: seg.Base,
                limit: seg.Limit,
                selector: seg.Selector,
                type_: (attr & 0xF) as u8,
                s: ((attr >> 4) & 1) as u8,
                dpl: ((attr >> 5) & 3) as u8,
                present: ((attr >> 7) & 1) as u8,
                avl: ((attr >> 8) & 1) as u8,
                l: ((attr >> 9) & 1) as u8,
                db: ((attr >> 10) & 1) as u8,
                g: ((attr >> 11) & 1) as u8,
                unusable: 0,
            }
        }

        fn whv_to_table(val: &WHV_REGISTER_VALUE) -> crate::arch::x86::DescriptorTable {
            // SAFETY: Table field is valid for GDTR/IDTR.
            let table = unsafe { &val.Table };
            crate::arch::x86::DescriptorTable {
                base: table.Base,
                limit: table.Limit,
            }
        }

        // SAFETY: Reg64 is valid for control registers; Segment/Table for others.
        Ok(unsafe {
            SpecialRegisters {
                cr0: values[0].Reg64,
                cr2: values[1].Reg64,
                cr3: values[2].Reg64,
                cr4: values[3].Reg64,
                cr8: values[4].Reg64,
                efer: values[5].Reg64,
                apic_base: values[6].Reg64,
                cs: whv_to_seg(&values[7]),
                ds: whv_to_seg(&values[8]),
                es: whv_to_seg(&values[9]),
                fs: whv_to_seg(&values[10]),
                gs: whv_to_seg(&values[11]),
                ss: whv_to_seg(&values[12]),
                tr: whv_to_seg(&values[13]),
                ldt: whv_to_seg(&values[14]),
                gdt: whv_to_table(&values[15]),
                idt: whv_to_table(&values[16]),
                interrupt_bitmap: [0u64; 4],
            }
        })
    }

    fn set_sregs(&self, sregs: &SpecialRegisters) -> cpu::Result<()> {
        // Write ALL special registers in a single WHvSetVirtualProcessorRegisters
        // call so WHP validates them as a consistent state. Writing CR0.PE
        // separately from segment registers causes InvalidVpRegisterValue.

        fn seg_to_whv(seg: &crate::arch::x86::SegmentRegister) -> WHV_REGISTER_VALUE {
            let attributes: u16 = (seg.type_ as u16 & 0xF)
                | ((seg.s as u16 & 1) << 4)
                | ((seg.dpl as u16 & 3) << 5)
                | ((seg.present as u16 & 1) << 7)
                | ((seg.avl as u16 & 1) << 8)
                | ((seg.l as u16 & 1) << 9)
                | ((seg.db as u16 & 1) << 10)
                | ((seg.g as u16 & 1) << 11);

            let mut val = WHV_REGISTER_VALUE::default();
            // SAFETY: The Segment field is valid for segment register names.
            unsafe {
                val.Segment = WHV_X64_SEGMENT_REGISTER {
                    Base: seg.base,
                    Limit: seg.limit,
                    Selector: seg.selector,
                    Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
                        Attributes: attributes,
                    },
                };
            }
            val
        }

        fn table_to_whv(table: &crate::arch::x86::DescriptorTable) -> WHV_REGISTER_VALUE {
            let mut val = WHV_REGISTER_VALUE::default();
            // SAFETY: Table field is valid for GDTR/IDTR.
            unsafe {
                val.Table = WHV_X64_TABLE_REGISTER {
                    Pad: [0; 3],
                    Limit: table.limit,
                    Base: table.base,
                };
            }
            val
        }

        let names = vec![
            // Table registers first
            WHvX64RegisterGdtr,
            WHvX64RegisterIdtr,
            // Segment registers
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterFs,
            WHvX64RegisterGs,
            WHvX64RegisterSs,
            WHvX64RegisterTr,
            WHvX64RegisterLdtr,
            // Control registers last
            WHvX64RegisterCr0,
            WHvX64RegisterCr2,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
        ];

        let values = vec![
            table_to_whv(&sregs.gdt),
            table_to_whv(&sregs.idt),
            seg_to_whv(&sregs.cs),
            seg_to_whv(&sregs.ds),
            seg_to_whv(&sregs.es),
            seg_to_whv(&sregs.fs),
            seg_to_whv(&sregs.gs),
            seg_to_whv(&sregs.ss),
            seg_to_whv(&sregs.tr),
            seg_to_whv(&sregs.ldt),
            reg64_value(sregs.cr0),
            reg64_value(sregs.cr2),
            reg64_value(sregs.cr3),
            reg64_value(sregs.cr4),
            reg64_value(sregs.efer),
        ];

        self.set_registers(&names, &values).map_err(|e| {
            HypervisorCpuError::SetSpecialRegs(anyhow!("Failed to set special registers: {e}"))
        })
    }

    fn get_fpu(&self) -> cpu::Result<FpuState> {
        // TODO: Implement full FPU state retrieval via WHP registers
        debug!("WHP get_fpu: returning default FPU state");
        Ok(FpuState::default())
    }

    fn set_fpu(&self, _fpu: &FpuState) -> cpu::Result<()> {
        // TODO: Implement full FPU state setting via WHP registers
        debug!("WHP set_fpu: not yet implemented");
        Ok(())
    }

    fn set_cpuid2(&self, _cpuid: &[CpuIdEntry]) -> cpu::Result<()> {
        // WHP doesn't allow setting CPUID directly; it uses the host CPU's
        // CPUID values with optional filtering via partition properties.
        debug!("WHP set_cpuid2: CPUID filtering not yet implemented");
        Ok(())
    }

    fn enable_hyperv_synic(&self) -> cpu::Result<()> {
        // WHP runs on Hyper-V which inherently supports SynIC.
        Ok(())
    }

    fn get_cpuid2(&self, _num_entries: usize) -> cpu::Result<Vec<CpuIdEntry>> {
        // Return host CPUID values.
        let mut entries = Vec::new();
        for function in 0..=0xD {
            let result = std::arch::x86_64::__cpuid_count(function, 0);
            entries.push(CpuIdEntry {
                function,
                index: 0,
                flags: 0,
                eax: result.eax,
                ebx: result.ebx,
                ecx: result.ecx,
                edx: result.edx,
            });
        }
        Ok(entries)
    }

    fn get_lapic(&self) -> cpu::Result<LapicState> {
        // TODO: Use WHvGetVirtualProcessorInterruptControllerState
        debug!("WHP get_lapic: returning default LAPIC state");
        Ok(LapicState::default())
    }

    fn set_lapic(&self, _lapic: &LapicState) -> cpu::Result<()> {
        // TODO: Use WHvSetVirtualProcessorInterruptControllerState
        debug!("WHP set_lapic: not yet implemented");
        Ok(())
    }

    fn get_msrs(&self, msrs: &mut Vec<MsrEntry>) -> cpu::Result<usize> {
        // Read each MSR individually.
        // WHP doesn't have a batch MSR read; we use individual register reads.
        let count = msrs.len();
        for msr in msrs.iter_mut() {
            let name = WHV_REGISTER_NAME(msr.index as i32);
            match self.get_registers(&[name]) {
                Ok(values) => {
                    // SAFETY: Reg64 is valid for MSR values.
                    msr.data = unsafe { values[0].Reg64 };
                }
                Err(_) => {
                    // Skip unsupported MSRs silently.
                    msr.data = 0;
                }
            }
        }
        Ok(count)
    }

    fn set_msrs(&self, msrs: &[MsrEntry]) -> cpu::Result<usize> {
        let mut count = 0;
        for msr in msrs {
            let name = WHV_REGISTER_NAME(msr.index as i32);
            let value = reg64_value(msr.data);
            if self.set_registers(&[name], &[value]).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn get_mp_state(&self) -> cpu::Result<MpState> {
        Ok(MpState::Whp)
    }

    fn set_mp_state(&self, _mp_state: MpState) -> cpu::Result<()> {
        Ok(())
    }

    fn state(&self) -> cpu::Result<CpuState> {
        let StandardRegisters::Whp(regs) = self.get_regs()?;
        let sregs = self.get_sregs()?;
        let fpu = self.get_fpu()?;
        let lapic = self.get_lapic()?;

        Ok(CpuState::Whp(VcpuWhpState {
            regs,
            sregs: WhpSpecialRegisters {
                cr0: sregs.cr0,
                cr2: sregs.cr2,
                cr3: sregs.cr3,
                cr4: sregs.cr4,
                cr8: sregs.cr8,
                efer: sregs.efer,
                apic_base: sregs.apic_base,
            },
            fpu,
            lapic,
            msrs: Vec::new(),
            clock: WhpClockData::default(),
        }))
    }

    fn set_state(&self, state: &CpuState) -> cpu::Result<()> {
        let CpuState::Whp(whp_state) = state;
        self.set_regs(&StandardRegisters::Whp(whp_state.regs))?;
        let sregs = SpecialRegisters {
            cr0: whp_state.sregs.cr0,
            cr2: whp_state.sregs.cr2,
            cr3: whp_state.sregs.cr3,
            cr4: whp_state.sregs.cr4,
            cr8: whp_state.sregs.cr8,
            efer: whp_state.sregs.efer,
            apic_base: whp_state.sregs.apic_base,
            ..Default::default()
        };
        self.set_sregs(&sregs)?;
        self.set_fpu(&whp_state.fpu)?;
        self.set_lapic(&whp_state.lapic)?;
        if !whp_state.msrs.is_empty() {
            self.set_msrs(&whp_state.msrs)?;
        }
        Ok(())
    }

    fn run(&mut self) -> std::result::Result<cpu::VmExit, HypervisorCpuError> {
        let mut exit_context = WHV_RUN_VP_EXIT_CONTEXT::default();

        debug!("WHP: calling WHvRunVirtualProcessor for vp {}", self.vp_index);

        // SAFETY: Runs the virtual processor until an exit occurs.
        unsafe {
            WHvRunVirtualProcessor(
                self.partition,
                self.vp_index,
                &raw mut exit_context as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
            .map_err(|e| {
                HypervisorCpuError::RunVcpu(anyhow!("WHvRunVirtualProcessor failed: {e}"))
            })?;
        }

        debug!("WHP: exit reason = {:?}, RIP = {:#x}", 
               exit_context.ExitReason, exit_context.VpContext.Rip);

        #[allow(non_upper_case_globals)]
        match exit_context.ExitReason {
            WHvRunVpExitReasonMemoryAccess => {
                // SAFETY: The union field is valid for this exit reason.
                let mem_ctx = unsafe { &exit_context.Anonymous.MemoryAccess };
                self.handle_mmio(mem_ctx)?;
                Ok(cpu::VmExit::Ignore)
            }

            WHvRunVpExitReasonX64IoPortAccess => {
                // SAFETY: The union field is valid for this exit reason.
                let io_ctx = unsafe { &exit_context.Anonymous.IoPortAccess };
                let instr_len = exit_context.VpContext._bitfield as u64;
                self.handle_pio(io_ctx, instr_len)?;
                Ok(cpu::VmExit::Ignore)
            }

            WHvRunVpExitReasonX64Halt => {
                debug!("WHP: vCPU halted at RIP={:#x}", exit_context.VpContext.Rip);
                Ok(cpu::VmExit::Shutdown)
            }

            WHvRunVpExitReasonCanceled => {
                debug!("WHP: vCPU run canceled");
                Ok(cpu::VmExit::Ignore)
            }

            WHvRunVpExitReasonX64ApicEoi => {
                // SAFETY: The union field is valid for this exit reason.
                let eoi_ctx = unsafe { &exit_context.Anonymous.ApicEoi };
                Ok(cpu::VmExit::IoapicEoi(eoi_ctx.InterruptVector as u8))
            }

            WHvRunVpExitReasonUnrecoverableException => {
                warn!("WHP: unrecoverable exception at RIP={:#x}", exit_context.VpContext.Rip);
                Ok(cpu::VmExit::Shutdown)
            }

            WHvRunVpExitReasonInvalidVpRegisterValue => {
                warn!("WHP: invalid VP register value");
                Ok(cpu::VmExit::Shutdown)
            }

            WHvRunVpExitReasonUnsupportedFeature => {
                warn!("WHP: unsupported feature");
                Ok(cpu::VmExit::Shutdown)
            }

            other => {
                warn!("WHP: unhandled exit reason: {other:?}");
                Ok(cpu::VmExit::Ignore)
            }
        }
    }

    fn translate_gva(&self, gva: u64, _flags: u64) -> cpu::Result<(u64, u32)> {
        let mut result = WHV_TRANSLATE_GVA_RESULT::default();
        let mut gpa: u64 = 0;

        // SAFETY: Translates a guest virtual address to a guest physical address.
        unsafe {
            WHvTranslateGva(
                self.partition,
                self.vp_index,
                gva,
                WHvTranslateGvaFlagNone,
                &mut result,
                &mut gpa,
            )
            .map_err(|e| {
                HypervisorCpuError::TranslateVirtualAddress(anyhow!(
                    "WHvTranslateGva failed: {e}"
                ))
            })?;
        }

        Ok((gpa, result.ResultCode.0 as u32))
    }

    fn set_immediate_exit(&mut self, exit: bool) {
        if exit {
            // SAFETY: Cancels a pending WHvRunVirtualProcessor call from
            // another thread, causing it to return with WHvRunVpExitReasonCanceled.
            let result =
                unsafe { WHvCancelRunVirtualProcessor(self.partition, self.vp_index, 0) };
            if let Err(e) = result {
                warn!("WHvCancelRunVirtualProcessor failed: {e}");
            }
        }
    }

    fn boot_msr_entries(&self) -> &'static [MsrEntry] {
        // Minimal set of MSRs needed for boot.
        &[]
    }

    fn nmi(&self) -> cpu::Result<()> {
        // WHV_INTERRUPT_CONTROL uses a bitfield:
        // Bits 0-3: InterruptType, Bits 4: DestinationMode, Bits 5: TriggerMode
        // NMI type = 4, Physical destination = 0, Edge trigger = 0
        let interrupt = WHV_INTERRUPT_CONTROL {
            _bitfield: WHvX64InterruptTypeNmi.0 as u64, // Type in bits 0-3
            Destination: self.vp_index,
            Vector: 2,
        };

        // SAFETY: Requests an NMI interrupt delivery to the virtual processor.
        unsafe {
            WHvRequestInterrupt(
                self.partition,
                &interrupt,
                std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
            )
            .map_err(|e| {
                HypervisorCpuError::Nmi(anyhow!("WHvRequestInterrupt (NMI) failed: {e}"))
            })?;
        }

        Ok(())
    }

    fn notify_guest_clock_paused(&self) -> cpu::Result<()> {
        Ok(())
    }
}

impl Drop for WhpVcpu {
    fn drop(&mut self) {
        // SAFETY: Cleans up the virtual processor. The partition handle is still
        // valid because WhpVm owns it and outlives the vCPU.
        let result = unsafe { WHvDeleteVirtualProcessor(self.partition, self.vp_index) };
        if let Err(e) = result {
            warn!("Failed to delete WHP vCPU {}: {e}", self.vp_index);
        }
    }
}

// ─── Helper functions ────────────────────────────────────────────────────────

/// Create a zero-initialized WHV_REGISTER_VALUE with Reg64 set.
/// 
/// SAFETY: WHV_REGISTER_VALUE is a union. Partial initialization via
/// `WHV_REGISTER_VALUE { Reg64: v }` leaves the upper 8 bytes undefined,
/// causing access violations in WHvSetVirtualProcessorRegisters.
/// This helper zeros the entire 16-byte union first.
fn reg64_value(v: u64) -> WHV_REGISTER_VALUE {
    let mut val = WHV_REGISTER_VALUE::default();
    // SAFETY: Reg64 is a valid union field.
    unsafe { val.Reg64 = v };
    val
}

/// Set a partition property from raw bytes.
fn set_partition_property(
    partition: WHV_PARTITION_HANDLE,
    code: WHV_PARTITION_PROPERTY_CODE,
    value: u64,
) -> std::result::Result<(), windows::core::Error> {
    // WHV_PARTITION_PROPERTY is a large union. Zero it, then write the value
    // into the first 8 bytes, which covers both u32 (ProcessorCount) and
    // u64 (ExtendedVmExits) property types.
    let mut property = WHV_PARTITION_PROPERTY::default();
    // SAFETY: Writing the value into the union's raw bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &value as *const u64 as *const u8,
            &raw mut property as *mut u8,
            std::mem::size_of::<u64>(),
        );
    }

    // SAFETY: Sets a partition property value.
    unsafe {
        WHvSetPartitionProperty(
            partition,
            code,
            &raw const property as *const std::ffi::c_void,
            std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
        )
    }
}
