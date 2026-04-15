// SPDX-License-Identifier: Apache-2.0
//
// Windows VMM implementation.
//
// This module provides a minimal VMM for Windows, sharing cross-platform
// submodules (api, config, vm_config) with the Unix implementation.

use std::io;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use log::{error, info, warn};
use thiserror::Error;
use platform::{EFD_NONBLOCK, EventFd, EventPoll, PollEvent};

// ── Cross-platform submodules (shared with vmm_impl.rs) ─────────────────────
pub mod api;
pub mod config;
pub mod device_tree;
pub mod vm_config;

// ── Windows-specific submodules ─────────────────────────────────────────────
#[path = "serial_manager_windows.rs"]
pub mod serial_manager;
#[path = "memory_manager_windows.rs"]
pub mod memory_manager;
#[path = "device_manager_windows.rs"]
pub mod device_manager;

// ── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum Error {
    #[error("Error creating EventFd")]
    EventFdCreate(#[source] io::Error),

    #[error("Error reading EventFd")]
    EventFdRead(#[source] io::Error),

    #[error("Error creating EventPoll")]
    Epoll(#[source] io::Error),

    #[error("Error receiving API request")]
    ApiRequestRecv(#[source] std::sync::mpsc::RecvError),

    #[error("Error spawning VMM thread")]
    VmmThreadSpawn(#[source] io::Error),

    #[error("Error creating VM")]
    VmCreate(#[source] hypervisor::HypervisorError),

    #[error("Error in memory manager")]
    MemoryManager(#[source] memory_manager::Error),

    #[error("Error booting VM")]
    VmBoot(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ── Version info ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct VmmVersionInfo {
    pub build_version: String,
    pub version: String,
}

impl VmmVersionInfo {
    pub fn new(build_version: &str, version: &str) -> Self {
        Self {
            build_version: build_version.to_owned(),
            version: version.to_owned(),
        }
    }
}

// ── Event dispatch ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum EpollDispatch {
    Exit = 0,
    Reset = 1,
    Api = 2,
    ActivateVirtioDevices = 3,
    Unknown,
}

impl From<u64> for EpollDispatch {
    fn from(v: u64) -> Self {
        match v {
            0 => EpollDispatch::Exit,
            1 => EpollDispatch::Reset,
            2 => EpollDispatch::Api,
            3 => EpollDispatch::ActivateVirtioDevices,
            _ => EpollDispatch::Unknown,
        }
    }
}

// ── API request type ────────────────────────────────────────────────────────

/// Placeholder API request type for the Windows VMM control loop.
///
/// On Unix, `ApiRequest` is a complex closure-based dispatch. On Windows,
/// we start with a simple enum that can be extended as needed.
pub type ApiRequest = Box<dyn FnOnce(&mut Vmm) -> Result<bool> + Send>;

// ── VMM struct ──────────────────────────────────────────────────────────────

pub struct Vmm {
    poll: EventPoll,
    exit_evt: EventFd,
    reset_evt: EventFd,
    api_evt: EventFd,
    version: VmmVersionInfo,
    hypervisor: Arc<dyn hypervisor::Hypervisor>,
    activate_evt: EventFd,
    vm: Option<Arc<dyn hypervisor::Vm>>,
    memory_manager: Option<Arc<Mutex<memory_manager::MemoryManager>>>,
    vm_config: Option<Arc<Mutex<vm_config::VmConfig>>>,
    device_manager: Option<device_manager::DeviceManager>,
}

pub struct VmmThreadHandle {
    pub thread_handle: thread::JoinHandle<Result<()>>,
}

impl Vmm {
    fn new(
        vmm_version: VmmVersionInfo,
        api_evt: EventFd,
        exit_evt: EventFd,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
    ) -> Result<Self> {
        let mut poll = EventPoll::new().map_err(Error::Epoll)?;
        let reset_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFdCreate)?;
        let activate_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFdCreate)?;

        poll.add_event(&exit_evt, EpollDispatch::Exit as u64)
            .map_err(Error::Epoll)?;
        poll.add_event(&reset_evt, EpollDispatch::Reset as u64)
            .map_err(Error::Epoll)?;
        poll.add_event(&activate_evt, EpollDispatch::ActivateVirtioDevices as u64)
            .map_err(Error::Epoll)?;
        poll.add_event(&api_evt, EpollDispatch::Api as u64)
            .map_err(Error::Epoll)?;

        Ok(Vmm {
            poll,
            exit_evt,
            reset_evt,
            api_evt,
            version: vmm_version,
            hypervisor,
            activate_evt,
            vm: None,
            memory_manager: None,
            vm_config: None,
            device_manager: None,
        })
    }

    /// Create a VM with the given configuration.
    ///
    /// Allocates guest memory and maps it to the hypervisor.
    pub fn vm_create(&mut self, config: vm_config::VmConfig) -> Result<()> {
        let hv_config = hypervisor::HypervisorVmConfig::default();
        let vm = self.hypervisor.create_vm(hv_config).map_err(Error::VmCreate)?;

        let mm = memory_manager::MemoryManager::new(vm.clone(), &config.memory)
            .map_err(Error::MemoryManager)?;

        let has_kernel = config.payload.as_ref().map_or(false, |p| p.kernel.is_some());
        let dm = device_manager::DeviceManager::new(&vm, &mm, has_kernel)
            .map_err(|e| Error::VmBoot(format!("Device manager: {e:#}")))?;

        self.vm = Some(vm);
        self.memory_manager = Some(mm);
        self.vm_config = Some(Arc::new(Mutex::new(config)));
        self.device_manager = Some(dm);

        info!("VM created with device manager");
        Ok(())
    }

    /// Get the VM handle (if created).
    pub fn vm(&self) -> Option<&Arc<dyn hypervisor::Vm>> {
        self.vm.as_ref()
    }

    /// Get the memory manager (if created).
    pub fn memory_manager(&self) -> Option<&Arc<Mutex<memory_manager::MemoryManager>>> {
        self.memory_manager.as_ref()
    }

    /// Get the device manager (if created).
    pub fn device_manager(&self) -> Option<&device_manager::DeviceManager> {
        self.device_manager.as_ref()
    }

    /// Get the exit event (for signaling shutdown from vCPU threads).
    pub fn exit_evt(&self) -> &EventFd {
        &self.exit_evt
    }

    /// Boot the VM using the provided boot function.
    ///
    /// The boot function receives the exit_evt, VM handle, memory manager,
    /// and a VmOps handler for bus-based device dispatch.
    pub fn vm_boot<F>(&self, boot_fn: F) -> Result<()>
    where
        F: FnOnce(
            EventFd,
            Arc<dyn hypervisor::Vm>,
            Arc<Mutex<memory_manager::MemoryManager>>,
            Arc<dyn hypervisor::VmOps>,
            Arc<Mutex<devices::legacy::serial::Serial>>,
        ) -> std::result::Result<(), anyhow::Error>,
    {
        let vm = self.vm.as_ref().ok_or_else(|| {
            Error::VmBoot("VM not created — call vm_create first".into())
        })?;
        let mm = self.memory_manager.as_ref().ok_or_else(|| {
            Error::VmBoot("Memory manager not available".into())
        })?;
        let dm = self.device_manager.as_ref().ok_or_else(|| {
            Error::VmBoot("Device manager not available".into())
        })?;

        boot_fn(
            self.exit_evt.try_clone().map_err(Error::EventFdCreate)?,
            vm.clone(),
            mm.clone(),
            dm.vm_ops(),
            dm.serial().clone(),
        )
        .map_err(|e| Error::VmBoot(format!("{e:#}")))?;

        info!("VM booted");
        Ok(())
    }

    fn control_loop(&mut self, api_receiver: &Receiver<ApiRequest>) -> Result<()> {
        const POLL_EVENTS_LEN: usize = 16;
        let mut events = vec![PollEvent { data: 0, raw_events: 0 }; POLL_EVENTS_LEN];

        info!(
            "VMM control loop started (version: {} {})",
            self.version.build_version, self.version.version
        );

        'outer: loop {
            let num_events = match self.poll.wait(-1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(Error::Epoll(e));
                }
            };

            for event in events.iter().take(num_events) {
                let dispatch_event: EpollDispatch = event.data.into();
                match dispatch_event {
                    EpollDispatch::Unknown => {
                        warn!("Unknown VMM loop event: {}", event.data);
                    }
                    EpollDispatch::Exit => {
                        info!("VM exit event");
                        self.exit_evt.read().map_err(Error::EventFdRead)?;
                        break 'outer;
                    }
                    EpollDispatch::Reset => {
                        info!("VM reset event");
                        self.reset_evt.read().map_err(Error::EventFdRead)?;
                        // TODO: implement VM reboot on Windows
                        warn!("VM reset not yet implemented on Windows");
                    }
                    EpollDispatch::ActivateVirtioDevices => {
                        let count = self.activate_evt.read().map_err(Error::EventFdRead)?;
                        info!("Activate virtio devices: count = {count}");
                        // TODO: activate virtio devices
                    }
                    EpollDispatch::Api => {
                        for _ in 0..self.api_evt.read().map_err(Error::EventFdRead)? {
                            let api_request =
                                api_receiver.recv().map_err(Error::ApiRequestRecv)?;
                            if api_request(self)? {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        info!("VMM control loop exited");
        Ok(())
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Start the VMM thread with the Windows control loop.
pub fn start_vmm_thread(
    vmm_version: VmmVersionInfo,
    api_event: EventFd,
    api_receiver: Receiver<ApiRequest>,
    exit_event: EventFd,
    hypervisor: Arc<dyn hypervisor::Hypervisor>,
) -> Result<VmmThreadHandle> {
    let thread = thread::Builder::new()
        .name("vmm".to_string())
        .spawn(move || {
            let mut vmm = Vmm::new(vmm_version, api_event, exit_event, hypervisor)?;
            vmm.control_loop(&api_receiver)
        })
        .map_err(Error::VmmThreadSpawn)?;

    Ok(VmmThreadHandle {
        thread_handle: thread,
    })
}

/// Return the list of enabled features (for display purposes).
pub fn feature_list() -> Vec<String> {
    let mut features = Vec::new();
    #[cfg(feature = "whp")]
    features.push("whp".to_string());
    features
}
