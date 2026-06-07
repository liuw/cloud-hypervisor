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
use std::os::unix::fs::OpenOptionsExt;
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
use vm_virtio::{AccessPlatform, VirtioDeviceType};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use super::commands::ScsiCommandProcessor;
use super::handler::{
    ScsiCtrlHandler, ScsiDisk, ScsiEventHandler, ScsiEventState, ScsiRequestHandler,
};
use super::protocol::*;
use super::target::{ScsiLunConfig, ScsiLunId, ScsiLunState};
use crate::seccomp_filters::Thread;
use crate::{
    ActivateError, ActivateResult, VIRTIO_F_ACCESS_PLATFORM, VIRTIO_F_VERSION_1, VirtioCommon,
    VirtioDevice,
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
    #[error("Failed to create eventfd: {0}")]
    CreateEventFd(#[source] io::Error),
}

pub type Result<T> = result::Result<T, Error>;

/// State for migration/snapshot
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScsiState {
    pub avail_features: u64,
    pub acked_features: u64,
    pub config: VirtioScsiConfig,
    pub luns: Vec<ScsiLunConfig>,
    #[serde(default)]
    pub lun_states: Vec<ScsiLunState>,
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
    event_state: Arc<Mutex<ScsiEventState>>,
    event_notify: EventFd,
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
        let (avail_features, acked_features, config, luns, lun_states, paused) =
            if let Some(state) = state {
                info!("Restoring virtio-scsi {id}");
                (
                    state.avail_features,
                    state.acked_features,
                    state.config,
                    state.luns,
                    state.lun_states,
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

                let mut config = VirtioScsiConfig::new(num_queues as u32);
                config.event_info_size = std::mem::size_of::<VirtioScsiEvent>() as u32;

                (avail_features, 0, config, luns, Vec::new(), false)
            };

        let mut config = config;
        if config.event_info_size == 0 {
            config.event_info_size = std::mem::size_of::<VirtioScsiEvent>() as u32;
        }

        let num_queues = config.num_queues as usize;
        if num_queues == 0 || queue_size == 0 {
            return Err(Error::InvalidQueueConfig);
        }

        // Build processor map and validate LUNs
        let mut processors = HashMap::new();
        let mut disks = HashMap::new();
        let lun_states: HashMap<ScsiLunId, ScsiLunState> =
            lun_states.into_iter().map(|s| (s.id, s)).collect();

        for lun_config in &luns {
            let lun_id = lun_config.id();

            if processors.contains_key(&lun_id) {
                return Err(Error::DuplicateLun(lun_id));
            }

            // Get disk size
            let mut opts = OpenOptions::new();
            opts.read(true).write(!lun_config.readonly);
            if lun_config.direct {
                opts.custom_flags(libc::O_DIRECT);
            }
            let file = opts.open(&lun_config.path).map_err(Error::OpenDisk)?;

            let disk_size = file.metadata().map_err(Error::GetDiskSize)?.len();
            let block_size = 512u32; // Standard block size

            let mut processor =
                ScsiCommandProcessor::new(lun_config.clone(), disk_size, block_size);
            if let Some(lun_state) = lun_states.get(&lun_id) {
                processor.set_ready(lun_state.online);
                processor
                    .set_persistent_reservation_state(lun_state.persistent_reservation.clone());
            }
            processors.insert(lun_id, processor);
            disks.insert(lun_id, lun_config.path.clone());
        }

        // Total queues: 1 control + 1 event + N request queues
        let total_queues = 2 + num_queues;
        let queue_sizes = vec![queue_size; total_queues];

        let event_notify = EventFd::new(EFD_NONBLOCK).map_err(Error::CreateEventFd)?;

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
            event_state: Arc::new(Mutex::new(ScsiEventState::default())),
            event_notify,
        })
    }

    fn state(&self) -> ScsiState {
        // No in-flight request list is needed here: requests are processed
        // synchronously and the common pause barrier waits for queue handlers
        // to stop before snapshotting.
        let processors = self.processors.lock().unwrap();
        let lun_states = self
            .luns
            .iter()
            .map(|lun| {
                let id = lun.id();
                ScsiLunState {
                    id,
                    online: processors.get(&id).map(|p| p.is_ready()).unwrap_or(true),
                    persistent_reservation: processors
                        .get(&id)
                        .map(|p| p.persistent_reservation_state())
                        .unwrap_or_default(),
                    unit_attention: false,
                    power_condition: 0,
                }
            })
            .collect();

        ScsiState {
            avail_features: self.common.avail_features,
            acked_features: self.common.acked_features,
            config: self.config,
            luns: self.luns.clone(),
            lun_states,
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
        let mut opts = OpenOptions::new();
        opts.read(true).write(!lun_config.readonly);
        if lun_config.direct {
            opts.custom_flags(libc::O_DIRECT);
        }
        let file = opts.open(&lun_config.path).map_err(Error::OpenDisk)?;

        let disk_size = file.metadata().map_err(Error::GetDiskSize)?.len();
        let block_size = 512u32;

        let processor = ScsiCommandProcessor::new(lun_config.clone(), disk_size, block_size);
        processors.insert(lun_id, processor);
        drop(processors);

        self.disks.insert(lun_id, lun_config.path.clone());
        self.luns.push(lun_config);
        self.event_state
            .lock()
            .unwrap()
            .queue_transport_reset(lun_id, VIRTIO_SCSI_EVT_RESET_RESCAN);
        self.notify_event_handler();

        Ok(())
    }

    /// Remove a LUN from the device.
    pub fn remove_lun(&mut self, lun_id: &ScsiLunId) -> bool {
        let mut processors = self.processors.lock().unwrap();
        if processors.remove(lun_id).is_some() {
            drop(processors);
            self.disks.remove(lun_id);
            self.luns.retain(|l| l.id() != *lun_id);
            self.event_state
                .lock()
                .unwrap()
                .queue_transport_reset(*lun_id, VIRTIO_SCSI_EVT_RESET_REMOVED);
            self.notify_event_handler();
            true
        } else {
            false
        }
    }

    /// Queue a hard reset event for a LUN that is still present.
    pub fn notify_lun_reset(&self, lun_id: &ScsiLunId) -> bool {
        if !self.processors.lock().unwrap().contains_key(lun_id) {
            return false;
        }

        self.event_state
            .lock()
            .unwrap()
            .queue_transport_reset(*lun_id, VIRTIO_SCSI_EVT_RESET_HARD);
        self.notify_event_handler();
        true
    }

    /// Queue a subscribed media-change asynchronous notification.
    pub fn notify_media_change(&self, lun_id: &ScsiLunId) -> bool {
        if !self.processors.lock().unwrap().contains_key(lun_id) {
            return false;
        }

        let queued = self
            .event_state
            .lock()
            .unwrap()
            .queue_async_notify(*lun_id, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE);
        if queued {
            self.notify_event_handler();
        }
        queued
    }

    /// Queue a LUN parameter change event.
    pub fn notify_parameter_change(&self, lun_id: &ScsiLunId, asc: u8, ascq: u8) -> bool {
        if !self.processors.lock().unwrap().contains_key(lun_id) {
            return false;
        }

        self.event_state
            .lock()
            .unwrap()
            .queue_param_change(*lun_id, asc, ascq);
        self.notify_event_handler();
        true
    }

    fn notify_event_handler(&self) {
        if let Err(e) = self.event_notify.write(1) {
            warn!("Failed to notify virtio-scsi event handler: {e}");
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
        let ctrl_event_notify = self.event_notify.try_clone().map_err(|e| {
            error!("Failed to clone virtio-scsi control event notification fd: {e}");
            ActivateError::BadActivate
        })?;

        let mut ctrl_handler = ScsiCtrlHandler {
            queue: ctrl_queue,
            mem: mem.clone(),
            interrupt_cb: interrupt_cb.clone(),
            queue_evt: ctrl_evt,
            kill_evt,
            pause_evt,
            event_state: self.event_state.clone(),
            event_notify: ctrl_event_notify,
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
        let event_notify = self.event_notify.try_clone().map_err(|e| {
            error!("Failed to clone virtio-scsi event notification fd: {e}");
            ActivateError::BadActivate
        })?;

        let mut event_handler = ScsiEventHandler {
            queue: event_queue,
            mem: mem.clone(),
            interrupt_cb: interrupt_cb.clone(),
            queue_evt: event_evt,
            kill_evt,
            pause_evt,
            notify_evt: event_notify,
            event_state: self.event_state.clone(),
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

        info!(
            "virtio-scsi {} activated with {} LUNs",
            self.id,
            self.luns.len()
        );

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

    fn set_access_platform(&mut self, access_platform: Arc<dyn AccessPlatform>) {
        self.common.set_access_platform(access_platform);
    }

    fn access_platform(&self) -> Option<Arc<dyn AccessPlatform>> {
        self.common.access_platform()
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use seccompiler::SeccompAction;
    use vm_migration::{Pausable, Snapshottable};
    use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

    use super::super::target::{
        ScsiDeviceType, ScsiPersistentReservation, ScsiPersistentReservationState,
    };
    use super::*;

    fn test_disk_path(name: &str) -> PathBuf {
        let dir = PathBuf::from("target/scsi-lifecycle-tests");
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}-{}.img", name, std::process::id()))
    }

    fn create_test_disk(name: &str) -> PathBuf {
        let path = test_disk_path(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(1024 * 1024).unwrap();
        path
    }

    fn test_lun(path: PathBuf, target: u8, lun: u16) -> ScsiLunConfig {
        ScsiLunConfig {
            target,
            lun,
            path,
            readonly: false,
            direct: false,
            device_type: ScsiDeviceType::DirectAccess,
            vendor_id: "CLOUD-HV".to_string(),
            product_id: "VIRTIO-SCSI".to_string(),
            product_rev: "0001".to_string(),
        }
    }

    fn test_state(luns: Vec<ScsiLunConfig>) -> ScsiState {
        ScsiState {
            avail_features: 0,
            acked_features: 0,
            config: VirtioScsiConfig::new(1),
            luns,
            lun_states: Vec::new(),
        }
    }

    fn new_scsi(luns: Vec<ScsiLunConfig>, state: Option<ScsiState>) -> Result<Scsi> {
        Scsi::new(
            "scsi-test".to_string(),
            luns,
            1,
            64,
            false,
            SeccompAction::Allow,
            EventFd::new(EFD_NONBLOCK).unwrap(),
            state,
        )
    }

    #[test]
    fn test_restore_uses_snapshot_luns_and_queue_count() {
        let snapshot_lun = test_lun(create_test_disk("restore-snapshot"), 3, 7);
        let snapshot_path = snapshot_lun.path.clone();
        let state = ScsiState {
            avail_features: 0x55,
            acked_features: 0x11,
            config: VirtioScsiConfig::new(2),
            luns: vec![snapshot_lun],
            lun_states: Vec::new(),
        };

        let scsi = new_scsi(Vec::new(), Some(state)).unwrap();

        assert!(scsi.common.paused.load(Ordering::SeqCst));
        assert_eq!(scsi.common.avail_features, 0x55);
        assert_eq!(scsi.common.acked_features, 0x11);
        assert_eq!(scsi.queue_max_sizes().len(), 4);
        assert_eq!(scsi.luns.len(), 1);
        assert_eq!(scsi.luns[0].id(), ScsiLunId::new(3, 7));
        assert_eq!(scsi.luns[0].path, snapshot_path);
    }

    #[test]
    fn test_restore_missing_snapshot_disk_fails() {
        let valid_config_lun = test_lun(create_test_disk("restore-config"), 0, 0);
        let missing_path = test_disk_path("missing-restore-disk");
        let _ = fs::remove_file(&missing_path);
        let state = test_state(vec![test_lun(missing_path, 1, 0)]);

        assert!(matches!(
            new_scsi(vec![valid_config_lun], Some(state)),
            Err(Error::OpenDisk(_))
        ));
    }

    #[test]
    fn test_restore_duplicate_snapshot_lun_fails() {
        let path = create_test_disk("duplicate-restore-lun");
        let state = test_state(vec![test_lun(path.clone(), 2, 0), test_lun(path, 2, 0)]);

        assert!(matches!(
            new_scsi(Vec::new(), Some(state)),
            Err(Error::DuplicateLun(id)) if id == ScsiLunId::new(2, 0)
        ));
    }

    #[test]
    fn test_restore_pause_resume_before_activate() {
        let state = test_state(vec![test_lun(
            create_test_disk("pause-resume-restore"),
            0,
            0,
        )]);
        let mut scsi = new_scsi(Vec::new(), Some(state)).unwrap();

        assert!(scsi.common.paused.load(Ordering::SeqCst));
        scsi.pause().unwrap();
        assert!(scsi.common.paused.load(Ordering::SeqCst));
        scsi.resume().unwrap();
        assert!(!scsi.common.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn test_invalid_queue_config_fails() {
        let mut state = test_state(vec![test_lun(create_test_disk("invalid-queues"), 0, 0)]);
        state.config = VirtioScsiConfig::new(0);

        assert!(matches!(
            new_scsi(Vec::new(), Some(state)),
            Err(Error::InvalidQueueConfig)
        ));
    }

    #[test]
    fn test_snapshot_round_trips_lifecycle_state() {
        let lun = test_lun(create_test_disk("snapshot-round-trip"), 4, 1);
        let lun_path = lun.path.clone();
        let mut scsi = new_scsi(vec![lun], None).unwrap();
        scsi.ack_features(1u64 << VIRTIO_SCSI_F_INOUT);
        scsi.processors
            .lock()
            .unwrap()
            .get_mut(&ScsiLunId::new(4, 1))
            .unwrap()
            .set_ready(false);
        scsi.processors
            .lock()
            .unwrap()
            .get_mut(&ScsiLunId::new(4, 1))
            .unwrap()
            .set_persistent_reservation_state(ScsiPersistentReservationState {
                generation: 7,
                registered_keys: vec![0x44],
                reservation: Some(ScsiPersistentReservation {
                    key: 0x44,
                    reservation_type: 1,
                }),
            });

        let snapshot = scsi.snapshot().unwrap();
        let state: ScsiState = snapshot.to_state().unwrap();

        assert_eq!(state.acked_features, 1u64 << VIRTIO_SCSI_F_INOUT);
        assert_eq!(state.config.num_queues, 1);
        assert_eq!(state.luns.len(), 1);
        assert_eq!(state.luns[0].id(), ScsiLunId::new(4, 1));
        assert_eq!(state.luns[0].path, lun_path);
        assert_eq!(state.lun_states.len(), 1);
        assert_eq!(state.lun_states[0].id, ScsiLunId::new(4, 1));
        assert!(!state.lun_states[0].online);
        assert_eq!(state.lun_states[0].persistent_reservation.generation, 7);
        assert_eq!(
            state.lun_states[0].persistent_reservation.registered_keys,
            vec![0x44]
        );

        let restored = new_scsi(Vec::new(), Some(state)).unwrap();
        let processors = restored.processors.lock().unwrap();
        let processor = processors.get(&ScsiLunId::new(4, 1)).unwrap();
        assert!(!processor.is_ready());
        assert_eq!(processor.persistent_reservation_state().generation, 7);
        assert_eq!(
            processor
                .persistent_reservation_state()
                .reservation
                .unwrap()
                .key,
            0x44
        );
    }

    #[test]
    fn test_add_lun_queues_rescan_event() {
        let lun = test_lun(create_test_disk("hotplug-add"), 1, 4);
        let mut scsi = new_scsi(Vec::new(), None).unwrap();

        scsi.add_lun(lun).unwrap();

        let event = scsi.event_state.lock().unwrap().pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_TRANSPORT_RESET);
        assert_eq!(event_lun, encode_lun(1, 4));
        assert_eq!(event_reason, VIRTIO_SCSI_EVT_RESET_RESCAN);
    }

    #[test]
    fn test_remove_lun_queues_removed_event() {
        let lun = test_lun(create_test_disk("hotplug-remove"), 2, 5);
        let mut scsi = new_scsi(vec![lun], None).unwrap();

        assert!(scsi.remove_lun(&ScsiLunId::new(2, 5)));

        let event = scsi.event_state.lock().unwrap().pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_TRANSPORT_RESET);
        assert_eq!(event_lun, encode_lun(2, 5));
        assert_eq!(event_reason, VIRTIO_SCSI_EVT_RESET_REMOVED);
    }

    #[test]
    fn test_lun_reset_queues_hard_reset_event() {
        let lun = test_lun(create_test_disk("lun-reset"), 3, 6);
        let scsi = new_scsi(vec![lun], None).unwrap();

        assert!(scsi.notify_lun_reset(&ScsiLunId::new(3, 6)));

        let event = scsi.event_state.lock().unwrap().pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_TRANSPORT_RESET);
        assert_eq!(event_lun, encode_lun(3, 6));
        assert_eq!(event_reason, VIRTIO_SCSI_EVT_RESET_HARD);
    }

    #[test]
    fn test_media_change_requires_subscription() {
        let lun = test_lun(create_test_disk("media-change"), 4, 7);
        let scsi = new_scsi(vec![lun], None).unwrap();
        let lun_id = ScsiLunId::new(4, 7);

        assert!(!scsi.notify_media_change(&lun_id));
        assert!(scsi.event_state.lock().unwrap().pop_event().is_none());

        scsi.event_state
            .lock()
            .unwrap()
            .subscribe_async_events(lun_id, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE);
        assert!(scsi.notify_media_change(&lun_id));

        let event = scsi.event_state.lock().unwrap().pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_ASYNC_NOTIFY);
        assert_eq!(event_lun, encode_lun(4, 7));
        assert_eq!(event_reason, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE);
    }

    #[test]
    fn test_parameter_change_queues_event() {
        let lun = test_lun(create_test_disk("param-change"), 5, 8);
        let scsi = new_scsi(vec![lun], None).unwrap();

        assert!(scsi.notify_parameter_change(&ScsiLunId::new(5, 8), 0x2a, 0x09));

        let event = scsi.event_state.lock().unwrap().pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_PARAM_CHANGE);
        assert_eq!(event_lun, encode_lun(5, 8));
        assert_eq!(event_reason, 0x092a);
    }
}
