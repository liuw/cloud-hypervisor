// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Virtio SCSI device implementation.
//!
//! This module implements the virtio-scsi host bus adapter (HBA) device,
//! providing SCSI transport to virtual machines.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::num::Wrapping;
use std::path::PathBuf;
use std::result;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Barrier, Mutex};

use log::{error, info, warn};
use seccompiler::SeccompAction;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use vm_memory::ByteValued;
use vm_migration::{Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable};
use vm_virtio::VirtioDeviceType;
use vmm_sys_util::eventfd::EventFd;

use super::commands::ScsiCommandProcessor;
use super::handler::{ScsiCtrlHandler, ScsiDisk, ScsiEventHandler, ScsiRequestHandler};
use super::protocol::*;
use super::target::{ScsiLunConfig, ScsiLunId};
use crate::seccomp_filters::Thread;
use crate::{
    ActivateError, ActivateResult, VirtioCommon, VirtioDevice, VIRTIO_F_ACCESS_PLATFORM,
    VIRTIO_F_VERSION_1,
};

/// Minimum number of queues: 1 control + 1 event + 1 request
const MIN_NUM_QUEUES: usize = 3;

/// Default queue size
#[allow(dead_code)]
const DEFAULT_QUEUE_SIZE: u16 = 128;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to open disk: {0}")]
    OpenDisk(#[source] io::Error),
    #[error("Failed to get disk size: {0}")]
    GetDiskSize(#[source] io::Error),
    #[error("Invalid queue configuration")]
    InvalidQueueConfig,
    #[error("Duplicate LUN: {0}")]
    DuplicateLun(ScsiLunId),
}

pub type Result<T> = result::Result<T, Error>;

/// State for migration/snapshot
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScsiState {
    pub avail_features: u64,
    pub acked_features: u64,
    pub config: VirtioScsiConfig,
    pub luns: Vec<ScsiLunConfig>,
}

/// Virtio SCSI device.
pub struct Scsi {
    common: VirtioCommon,
    id: String,
    config: VirtioScsiConfig,
    seccomp_action: SeccompAction,
    exit_evt: EventFd,
    /// LUN configurations
    luns: Vec<ScsiLunConfig>,
    /// Command processors for each LUN (shared with handlers)
    processors: Arc<Mutex<HashMap<ScsiLunId, ScsiCommandProcessor>>>,
    /// Disk files for each LUN
    disks: HashMap<ScsiLunId, PathBuf>,
}

impl Scsi {
    /// Create a new virtio-scsi device.
    ///
    /// # Arguments
    /// * `id` - Device identifier
    /// * `luns` - List of LUN configurations
    /// * `num_queues` - Number of request queues (in addition to control and event queues)
    /// * `queue_size` - Size of each virtqueue
    /// * `iommu` - Whether to enable IOMMU support
    /// * `seccomp_action` - Seccomp action for worker threads
    /// * `exit_evt` - Event to signal VM exit
    /// * `state` - Optional state for restore
    pub fn new(
        id: String,
        luns: Vec<ScsiLunConfig>,
        num_queues: usize,
        queue_size: u16,
        iommu: bool,
        seccomp_action: SeccompAction,
        exit_evt: EventFd,
        state: Option<ScsiState>,
    ) -> Result<Self> {
        let (avail_features, acked_features, config, paused) = if let Some(state) = state {
            info!("Restoring virtio-scsi {id}");
            (
                state.avail_features,
                state.acked_features,
                state.config,
                true,
            )
        } else {
            let mut avail_features = (1u64 << VIRTIO_F_VERSION_1)
                | (1u64 << VIRTIO_SCSI_F_INOUT)
                | (1u64 << VIRTIO_SCSI_F_HOTPLUG)
                | (1u64 << VIRTIO_SCSI_F_CHANGE);

            if iommu {
                avail_features |= 1u64 << VIRTIO_F_ACCESS_PLATFORM;
            }

            let config = VirtioScsiConfig::new(num_queues as u32);

            (avail_features, 0, config, false)
        };

        // Build processor map and validate LUNs
        let mut processors = HashMap::new();
        let mut disks = HashMap::new();

        for lun_config in &luns {
            let lun_id = lun_config.id();

            if processors.contains_key(&lun_id) {
                return Err(Error::DuplicateLun(lun_id));
            }

            // Get disk size
            let file = OpenOptions::new()
                .read(true)
                .write(!lun_config.readonly)
                .open(&lun_config.path)
                .map_err(Error::OpenDisk)?;

            let disk_size = file.metadata().map_err(Error::GetDiskSize)?.len();
            let block_size = 512u32; // Standard block size

            let processor = ScsiCommandProcessor::new(lun_config.clone(), disk_size, block_size);
            processors.insert(lun_id, processor);
            disks.insert(lun_id, lun_config.path.clone());
        }

        // Total queues: 1 control + 1 event + N request queues
        let total_queues = 2 + num_queues;
        let queue_sizes = vec![queue_size; total_queues];

        Ok(Scsi {
            common: VirtioCommon {
                device_type: VirtioDeviceType::Scsi as u32,
                avail_features,
                acked_features,
                paused_sync: Some(Arc::new(Barrier::new(total_queues + 1))),
                queue_sizes,
                min_queues: MIN_NUM_QUEUES as u16,
                paused: Arc::new(AtomicBool::new(paused)),
                ..Default::default()
            },
            id,
            config,
            seccomp_action,
            exit_evt,
            luns,
            processors: Arc::new(Mutex::new(processors)),
            disks,
        })
    }

    fn state(&self) -> ScsiState {
        ScsiState {
            avail_features: self.common.avail_features,
            acked_features: self.common.acked_features,
            config: self.config,
            luns: self.luns.clone(),
        }
    }

    /// Add a new LUN to the device.
    pub fn add_lun(&mut self, lun_config: ScsiLunConfig) -> Result<()> {
        let lun_id = lun_config.id();

        let mut processors = self.processors.lock().unwrap();
        if processors.contains_key(&lun_id) {
            return Err(Error::DuplicateLun(lun_id));
        }

        // Get disk size
        let file = OpenOptions::new()
            .read(true)
            .write(!lun_config.readonly)
            .open(&lun_config.path)
            .map_err(Error::OpenDisk)?;

        let disk_size = file.metadata().map_err(Error::GetDiskSize)?.len();
        let block_size = 512u32;

        let processor = ScsiCommandProcessor::new(lun_config.clone(), disk_size, block_size);
        processors.insert(lun_id, processor);
        drop(processors);

        self.disks.insert(lun_id, lun_config.path.clone());
        self.luns.push(lun_config);

        Ok(())
    }

    /// Remove a LUN from the device.
    pub fn remove_lun(&mut self, lun_id: &ScsiLunId) -> bool {
        let mut processors = self.processors.lock().unwrap();
        if processors.remove(lun_id).is_some() {
            drop(processors);
            self.disks.remove(lun_id);
            self.luns.retain(|l| l.id() != *lun_id);
            true
        } else {
            false
        }
    }
}

impl VirtioDevice for Scsi {
    fn device_type(&self) -> u32 {
        self.common.device_type
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.common.queue_sizes
    }

    fn features(&self) -> u64 {
        self.common.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        self.common.ack_features(value);
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.read_config_from_slice(self.config.as_slice(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // According to virtio spec 5.6.4.1, only sense_size (offset 20) and
        // cdb_size (offset 24) are writable by the driver.
        const SENSE_SIZE_OFFSET: u64 = 20;
        const CDB_SIZE_OFFSET: u64 = 24;

        let config_slice = self.config.as_mut_slice();
        let config_len = config_slice.len() as u64;

        // Check if the write is within bounds
        if offset >= config_len || offset.saturating_add(data.len() as u64) > config_len {
            warn!(
                "virtio-scsi config write out of bounds: offset={}, len={}",
                offset,
                data.len()
            );
            return;
        }

        // Only allow writes to sense_size and cdb_size
        match offset {
            SENSE_SIZE_OFFSET if data.len() == 4 => {
                config_slice[offset as usize..offset as usize + 4].copy_from_slice(data);
            }
            CDB_SIZE_OFFSET if data.len() == 4 => {
                config_slice[offset as usize..offset as usize + 4].copy_from_slice(data);
            }
            _ => {
                warn!(
                    "Attempt to write to read-only virtio-scsi config at offset {}",
                    offset
                );
            }
        }
    }

    fn activate(&mut self, context: crate::device::ActivationContext) -> ActivateResult {
        let crate::device::ActivationContext {
            mem,
            interrupt_cb,
            mut queues,
            device_status,
        } = context;
        self.common.activate(&queues, interrupt_cb.clone())?;

        if queues.len() < MIN_NUM_QUEUES {
            error!(
                "virtio-scsi requires at least {} queues, got {}",
                MIN_NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        // Queue 0: Control queue
        let (_, ctrl_queue, ctrl_evt) = queues.remove(0);
        let (kill_evt, pause_evt) = self.common.dup_eventfds()?;

        let mut ctrl_handler = ScsiCtrlHandler {
            queue: ctrl_queue,
            mem: mem.clone(),
            interrupt_cb: interrupt_cb.clone(),
            queue_evt: ctrl_evt,
            kill_evt,
            pause_evt,
        };

        let paused = self.common.paused.clone();
        let paused_sync = self.common.paused_sync.clone();

        self.common.spawn_worker(
            &format!("{}_ctrl", self.id),
            &self.seccomp_action,
            Thread::VirtioScsi,
            &self.exit_evt,
            device_status.clone(),
            interrupt_cb.clone(),
            move || ctrl_handler.run(&paused, paused_sync.as_ref().unwrap()),
        )?;

        // Queue 1: Event queue
        let (_, event_queue, event_evt) = queues.remove(0);
        let (kill_evt, pause_evt) = self.common.dup_eventfds()?;

        let mut event_handler = ScsiEventHandler {
            queue: event_queue,
            mem: mem.clone(),
            interrupt_cb: interrupt_cb.clone(),
            queue_evt: event_evt,
            kill_evt,
            pause_evt,
        };

        let paused = self.common.paused.clone();
        let paused_sync = self.common.paused_sync.clone();

        self.common.spawn_worker(
            &format!("{}_event", self.id),
            &self.seccomp_action,
            Thread::VirtioScsi,
            &self.exit_evt,
            device_status.clone(),
            interrupt_cb.clone(),
            move || event_handler.run(&paused, paused_sync.as_ref().unwrap()),
        )?;

        // Remaining queues: Request queues
        for (i, (_, req_queue, req_evt)) in queues.into_iter().enumerate() {
            let queue_index = (i + 2) as u16;
            let (kill_evt, pause_evt) = self.common.dup_eventfds()?;

            // Open disk files for this handler
            let mut disks: HashMap<ScsiLunId, Box<dyn ScsiDisk>> = HashMap::new();
            for (lun_id, path) in &self.disks {
                let processors = self.processors.lock().unwrap();
                let readonly = processors
                    .get(lun_id)
                    .map(|p| p.is_readonly())
                    .unwrap_or(true);
                drop(processors);

                let mut opts = OpenOptions::new();
                opts.read(true);
                if !readonly {
                    opts.write(true);
                }

                match opts.open(path) {
                    Ok(file) => {
                        disks.insert(*lun_id, Box::new(file));
                    }
                    Err(e) => {
                        error!("Failed to open disk for LUN {}: {}", lun_id, e);
                        return Err(ActivateError::BadActivate);
                    }
                }
            }

            let mut req_handler = ScsiRequestHandler {
                queue_index,
                queue: req_queue,
                mem: mem.clone(),
                interrupt_cb: interrupt_cb.clone(),
                queue_evt: req_evt,
                kill_evt,
                pause_evt,
                processors: self.processors.clone(),
                disks,
            };

            let paused = self.common.paused.clone();
            let paused_sync = self.common.paused_sync.clone();

            self.common.spawn_worker(
                &format!("{}_req{}", self.id, i),
                &self.seccomp_action,
                Thread::VirtioScsi,
                &self.exit_evt,
                device_status.clone(),
                interrupt_cb.clone(),
                move || req_handler.run(&paused, paused_sync.as_ref().unwrap()),
            )?;
        }

        info!("virtio-scsi {} activated with {} LUNs", self.id, self.luns.len());

        Ok(())
    }

    fn reset(&mut self) {
        self.common.reset();
        info!("virtio-scsi {} reset", self.id);
    }

    fn counters(&self) -> Option<HashMap<&'static str, Wrapping<u64>>> {
        // TODO: Implement counters for read/write bytes and ops
        None
    }
}

impl Drop for Scsi {
    fn drop(&mut self) {
        self.common.wait_for_epoll_threads();
    }
}

impl Pausable for Scsi {
    fn pause(&mut self) -> result::Result<(), MigratableError> {
        self.common.pause()
    }

    fn resume(&mut self) -> result::Result<(), MigratableError> {
        self.common.resume()
    }
}

impl Snapshottable for Scsi {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn snapshot(&mut self) -> result::Result<Snapshot, MigratableError> {
        Snapshot::new_from_state(&self.state())
    }
}

impl Transportable for Scsi {}
impl Migratable for Scsi {}
