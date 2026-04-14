// Copyright © 2024 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//
// Windows memory manager: allocates and maps guest memory for WHP VMs.
//
// This is a minimal implementation that handles the core operations:
// - Allocate guest memory via GuestMemoryMmap (VirtualAlloc on Windows)
// - Map memory regions to the hypervisor VM (WHvMapGpaRange)
// - Track memory slots

use std::io;
use std::result;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use log::info;
use thiserror::Error;
use vm_memory::bitmap::AtomicBitmap;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap};

use crate::vm_config::MemoryConfig;

type GuestMem = GuestMemoryMmap<AtomicBitmap>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to allocate guest memory")]
    AllocateGuestMemory(#[source] vm_memory::mmap::FromRangesError),

    #[error("Failed to get host address for guest memory")]
    GetHostAddress,

    #[error("Failed to map memory to VM")]
    MapMemoryToVm(#[source] hypervisor::HypervisorVmError),

    #[error("Invalid memory configuration")]
    InvalidConfig(String),
}

pub type Result<T> = result::Result<T, Error>;

/// Minimal Windows memory manager.
///
/// Allocates guest memory using `GuestMemoryMmap` (backed by `VirtualAlloc`
/// on Windows) and maps it to the hypervisor VM via `create_user_memory_region`.
pub struct MemoryManager {
    guest_memory: GuestMem,
    vm: Arc<dyn hypervisor::Vm>,
    next_slot: AtomicU32,
    ram_size: usize,
}

impl MemoryManager {
    /// Create a new memory manager with the given configuration.
    ///
    /// Allocates guest RAM and maps it to the hypervisor VM.
    pub fn new(
        vm: Arc<dyn hypervisor::Vm>,
        config: &MemoryConfig,
    ) -> Result<Arc<Mutex<Self>>> {
        let ram_size = config.size as usize;

        if ram_size == 0 {
            return Err(Error::InvalidConfig("Memory size must be > 0".into()));
        }

        // Allocate guest memory as a single contiguous region at GPA 0.
        // GuestMemoryMmap uses VirtualAlloc on Windows.
        let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), ram_size)])
            .map_err(Error::AllocateGuestMemory)?;

        let host_ptr = guest_memory
            .get_host_address(GuestAddress(0))
            .map_err(|_| Error::GetHostAddress)?;

        // Map the guest memory region to the VM (slot 0).
        // SAFETY: host_ptr is a valid pointer to guest_memory's backing allocation.
        unsafe {
            vm.create_user_memory_region(0, 0, ram_size, host_ptr, false, false)
                .map_err(Error::MapMemoryToVm)?;
        }

        info!("Mapped {} MiB guest RAM at GPA 0x0", ram_size >> 20);

        Ok(Arc::new(Mutex::new(MemoryManager {
            guest_memory,
            vm,
            next_slot: AtomicU32::new(1),
            ram_size,
        })))
    }

    /// Get a reference to the guest memory.
    pub fn guest_memory(&self) -> &GuestMem {
        &self.guest_memory
    }

    /// Get the host pointer for the given guest address.
    pub fn host_address(&self, gpa: GuestAddress) -> Result<*mut u8> {
        self.guest_memory
            .get_host_address(gpa)
            .map_err(|_| Error::GetHostAddress)
    }

    /// Get the total RAM size in bytes.
    pub fn ram_size(&self) -> usize {
        self.ram_size
    }

    /// Map an additional memory region to the VM (e.g., device MMIO).
    pub fn map_region(
        &self,
        gpa: u64,
        size: usize,
        host_ptr: *mut u8,
        readonly: bool,
    ) -> Result<u32> {
        let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
        // SAFETY: host_ptr is a valid pointer provided by the caller.
        unsafe {
            self.vm
                .create_user_memory_region(slot, gpa, size, host_ptr, readonly, false)
                .map_err(Error::MapMemoryToVm)?;
        }
        Ok(slot)
    }
}
