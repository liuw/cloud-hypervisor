// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! SCSI queue handler implementation.
//!
//! This module implements the epoll-based queue handlers for the virtio-scsi
//! device, processing control, event, and request queues.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Barrier, Mutex};

use anyhow::anyhow;
use log::{debug, error, warn};
use virtio_queue::{Queue, QueueT};
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard,
};
use vmm_sys_util::eventfd::EventFd;

use super::commands::{DiskOps, ScsiCommandProcessor, ScsiCommandResult};
use super::protocol::*;
use super::target::ScsiLunId;
use crate::{
    EpollHelper, EpollHelperError, EpollHelperHandler, GuestMemoryMmap, VirtioInterrupt,
    VirtioInterruptType,
};

// Epoll events
const CTRL_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 1;
const EVENT_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 2;
const REQUEST_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 3;
const EVENT_NOTIFY_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 4;

/// Descriptor info for writing response back
#[derive(Clone, Copy)]
struct WriteDescriptor {
    addr: vm_memory::GuestAddress,
    len: u32,
}

#[derive(Default)]
pub struct ScsiEventState {
    pending_events: VecDeque<VirtioScsiEvent>,
    async_subscriptions: HashMap<ScsiLunId, u32>,
}

impl ScsiEventState {
    pub fn query_async_events(&self, event_requested: u32) -> u32 {
        event_requested & VIRTIO_SCSI_EVT_ASYNC_SUPPORTED
    }

    pub fn subscribe_async_events(&mut self, lun_id: ScsiLunId, event_requested: u32) -> u32 {
        let event_actual = self.query_async_events(event_requested);
        if event_actual == 0 {
            self.async_subscriptions.remove(&lun_id);
        } else {
            self.async_subscriptions.insert(lun_id, event_actual);
        }
        event_actual
    }

    pub fn queue_transport_reset(&mut self, lun_id: ScsiLunId, reason: u32) {
        self.queue_event(VirtioScsiEvent {
            event: VIRTIO_SCSI_T_TRANSPORT_RESET,
            lun: encode_lun(lun_id.target, lun_id.lun),
            reason,
        });
    }

    pub fn queue_async_notify(&mut self, lun_id: ScsiLunId, reason: u32) -> bool {
        let subscribed = self
            .async_subscriptions
            .get(&lun_id)
            .copied()
            .unwrap_or_default();
        let reason = reason & subscribed & VIRTIO_SCSI_EVT_ASYNC_SUPPORTED;
        if reason == 0 {
            return false;
        }

        self.queue_event(VirtioScsiEvent {
            event: VIRTIO_SCSI_T_ASYNC_NOTIFY,
            lun: encode_lun(lun_id.target, lun_id.lun),
            reason,
        });
        true
    }

    pub fn queue_param_change(&mut self, lun_id: ScsiLunId, asc: u8, ascq: u8) {
        self.queue_event(VirtioScsiEvent {
            event: VIRTIO_SCSI_T_PARAM_CHANGE,
            lun: encode_lun(lun_id.target, lun_id.lun),
            reason: u32::from(asc) | (u32::from(ascq) << 8),
        });
    }

    fn queue_event(&mut self, event: VirtioScsiEvent) {
        self.pending_events.push_back(event);
    }

    pub(crate) fn pop_event(&mut self) -> Option<VirtioScsiEvent> {
        self.pending_events.pop_front()
    }

    fn push_front_event(&mut self, event: VirtioScsiEvent) {
        self.pending_events.push_front(event);
    }
}

/// Handler for the control queue (queue 0).
pub struct ScsiCtrlHandler {
    pub queue: Queue,
    pub mem: GuestMemoryAtomic<GuestMemoryMmap>,
    pub interrupt_cb: Arc<dyn VirtioInterrupt>,
    pub queue_evt: EventFd,
    pub kill_evt: EventFd,
    pub pause_evt: EventFd,
    pub event_state: Arc<Mutex<ScsiEventState>>,
    pub event_notify: EventFd,
}

impl ScsiCtrlHandler {
    /// Process a single control queue request.
    fn process_ctrl_request(&mut self) -> Result<(), EpollHelperError> {
        let mem = self.mem.memory();

        while let Some(mut desc_chain) = self.queue.pop_descriptor_chain(mem.clone()) {
            let head_index = desc_chain.head_index();

            // Read the request header to determine the type
            let desc = match desc_chain.next() {
                Some(d) => d,
                None => {
                    warn!("Control queue: missing descriptor");
                    continue;
                }
            };

            if desc.len() < 4 {
                warn!("Control request too short");
                continue;
            }

            let request_type: u32 = desc_chain.memory().read_obj(desc.addr()).map_err(|e| {
                EpollHelperError::HandleEvent(anyhow!("Failed to read request type: {e}"))
            })?;

            let (response, used_len) = match request_type {
                VIRTIO_SCSI_T_TMF => {
                    // Task Management Function - return success
                    debug!("TMF request received");
                    if desc.len() >= std::mem::size_of::<VirtioScsiCtrlTmfReq>() as u32 {
                        let request: VirtioScsiCtrlTmfReq =
                            desc_chain.memory().read_obj(desc.addr()).map_err(|e| {
                                EpollHelperError::HandleEvent(anyhow!(
                                    "Failed to read TMF request: {e}"
                                ))
                            })?;
                        let subtype = request.subtype;
                        if matches!(
                            subtype,
                            VIRTIO_SCSI_T_TMF_LOGICAL_UNIT_RESET
                                | VIRTIO_SCSI_T_TMF_I_T_NEXUS_RESET
                        ) {
                            let lun_bytes = request.lun;
                            if let Some((target, lun)) = parse_lun(&lun_bytes) {
                                self.event_state.lock().unwrap().queue_transport_reset(
                                    ScsiLunId::new(target, lun),
                                    VIRTIO_SCSI_EVT_RESET_HARD,
                                );
                                if let Err(e) = self.event_notify.write(1) {
                                    warn!("Failed to notify virtio-scsi event handler: {e}");
                                }
                            }
                        }
                    }
                    (
                        CtrlResponse::Tmf(VirtioScsiCtrlTmfResp {
                            response: VIRTIO_SCSI_S_FUNCTION_SUCCEEDED,
                        }),
                        std::mem::size_of::<VirtioScsiCtrlTmfResp>() as u32,
                    )
                }
                VIRTIO_SCSI_T_AN_QUERY | VIRTIO_SCSI_T_AN_SUBSCRIBE => {
                    debug!("Async notification request: type={request_type}");
                    let request: VirtioScsiCtrlAnReq =
                        desc_chain.memory().read_obj(desc.addr()).map_err(|e| {
                            EpollHelperError::HandleEvent(anyhow!(
                                "Failed to read async notification request: {e}"
                            ))
                        })?;
                    let lun_bytes = request.lun;
                    let event_requested = request.event_requested;
                    let event_actual = match parse_lun(&lun_bytes) {
                        Some((target, lun)) => {
                            let lun_id = ScsiLunId::new(target, lun);
                            let mut state = self.event_state.lock().unwrap();
                            if request_type == VIRTIO_SCSI_T_AN_SUBSCRIBE {
                                state.subscribe_async_events(lun_id, event_requested)
                            } else {
                                state.query_async_events(event_requested)
                            }
                        }
                        None => 0,
                    };
                    (
                        CtrlResponse::Async(VirtioScsiCtrlAnResp {
                            event_actual,
                            response: VIRTIO_SCSI_S_OK,
                        }),
                        std::mem::size_of::<VirtioScsiCtrlAnResp>() as u32,
                    )
                }
                _ => {
                    warn!("Unknown control request type: {request_type}");
                    (
                        CtrlResponse::Tmf(VirtioScsiCtrlTmfResp {
                            response: VIRTIO_SCSI_S_FAILURE,
                        }),
                        std::mem::size_of::<VirtioScsiCtrlTmfResp>() as u32,
                    )
                }
            };

            // Find the response descriptor and write the response
            let mut resp_written = false;
            for desc in desc_chain.by_ref() {
                if desc.is_write_only() {
                    response.write(&desc_chain.memory(), desc.addr())?;
                    resp_written = true;
                    break;
                }
            }

            if !resp_written {
                warn!("No write descriptor for control response");
            }

            self.queue
                .add_used(desc_chain.memory(), head_index, used_len)
                .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to add used: {e}")))?;
        }

        self.signal_used_queue()?;
        Ok(())
    }

    fn signal_used_queue(&self) -> Result<(), EpollHelperError> {
        self.interrupt_cb
            .trigger(VirtioInterruptType::Queue(0))
            .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to signal: {e}")))
    }

    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
    ) -> Result<(), EpollHelperError> {
        let mut helper = EpollHelper::new(&self.kill_evt, &self.pause_evt)?;
        helper.add_event(self.queue_evt.as_raw_fd(), CTRL_QUEUE_EVENT)?;
        helper.run(paused, paused_sync, self)?;
        Ok(())
    }
}

enum CtrlResponse {
    Tmf(VirtioScsiCtrlTmfResp),
    Async(VirtioScsiCtrlAnResp),
}

impl CtrlResponse {
    fn write(
        &self,
        mem: &GuestMemoryMmap,
        addr: vm_memory::GuestAddress,
    ) -> Result<(), EpollHelperError> {
        match self {
            Self::Tmf(resp) => mem.write_obj(*resp, addr),
            Self::Async(resp) => mem.write_obj(*resp, addr),
        }
        .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write response: {e}")))
    }
}

impl EpollHelperHandler for ScsiCtrlHandler {
    fn handle_event(
        &mut self,
        _helper: &mut EpollHelper,
        event: &epoll::Event,
    ) -> Result<(), EpollHelperError> {
        let ev_type = event.data as u16;
        match ev_type {
            CTRL_QUEUE_EVENT => {
                self.queue_evt.read().map_err(|e| {
                    EpollHelperError::HandleEvent(anyhow!("Failed to read queue event: {e}"))
                })?;
                self.process_ctrl_request()?;
            }
            _ => {
                return Err(EpollHelperError::HandleEvent(anyhow!(
                    "Unexpected event: {ev_type}"
                )));
            }
        }
        Ok(())
    }
}

/// Handler for the event queue (queue 1).
pub struct ScsiEventHandler {
    pub queue: Queue,
    pub mem: GuestMemoryAtomic<GuestMemoryMmap>,
    pub interrupt_cb: Arc<dyn VirtioInterrupt>,
    pub queue_evt: EventFd,
    pub kill_evt: EventFd,
    pub pause_evt: EventFd,
    pub notify_evt: EventFd,
    pub event_state: Arc<Mutex<ScsiEventState>>,
}

impl ScsiEventHandler {
    fn process_event_queue(&mut self) -> Result<(), EpollHelperError> {
        let mem = self.mem.memory();
        let mut delivered = false;

        loop {
            let event = {
                let mut state = self.event_state.lock().unwrap();
                state.pop_event()
            };
            let Some(event) = event else {
                break;
            };

            let Some(mut desc_chain) = self.queue.pop_descriptor_chain(mem.clone()) else {
                self.event_state.lock().unwrap().push_front_event(event);
                break;
            };

            let head_index = desc_chain.head_index();
            let desc = match desc_chain.next() {
                Some(desc) if desc.is_write_only() => desc,
                _ => {
                    warn!("Event queue descriptor is not device-writable");
                    self.queue
                        .add_used(desc_chain.memory(), head_index, 0)
                        .map_err(|e| {
                            EpollHelperError::HandleEvent(anyhow!("Failed to add used: {e}"))
                        })?;
                    continue;
                }
            };

            let used_len = Self::write_event(&desc_chain.memory(), desc.addr(), desc.len(), event)?;
            self.queue
                .add_used(desc_chain.memory(), head_index, used_len)
                .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to add used: {e}")))?;
            delivered = true;
        }

        if delivered {
            self.signal_used_queue()?;
        }

        Ok(())
    }

    fn write_event(
        mem: &GuestMemoryMmap,
        addr: vm_memory::GuestAddress,
        len: u32,
        event: VirtioScsiEvent,
    ) -> Result<u32, EpollHelperError> {
        let event_size = std::mem::size_of::<VirtioScsiEvent>() as u32;
        if len < event_size {
            return Ok(0);
        }

        mem.write_obj(event, addr)
            .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write event: {e}")))?;
        Ok(event_size)
    }

    fn signal_used_queue(&self) -> Result<(), EpollHelperError> {
        self.interrupt_cb
            .trigger(VirtioInterruptType::Queue(1))
            .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to signal: {e}")))
    }

    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
    ) -> Result<(), EpollHelperError> {
        let mut helper = EpollHelper::new(&self.kill_evt, &self.pause_evt)?;
        helper.add_event(self.queue_evt.as_raw_fd(), EVENT_QUEUE_EVENT)?;
        helper.add_event(self.notify_evt.as_raw_fd(), EVENT_NOTIFY_EVENT)?;
        helper.run(paused, paused_sync, self)?;
        Ok(())
    }
}

impl EpollHelperHandler for ScsiEventHandler {
    fn handle_event(
        &mut self,
        _helper: &mut EpollHelper,
        event: &epoll::Event,
    ) -> Result<(), EpollHelperError> {
        let ev_type = event.data as u16;
        match ev_type {
            EVENT_QUEUE_EVENT => {
                self.queue_evt.read().map_err(|e| {
                    EpollHelperError::HandleEvent(anyhow!("Failed to read queue event: {e}"))
                })?;
                self.process_event_queue()?;
            }
            EVENT_NOTIFY_EVENT => {
                self.notify_evt.read().map_err(|e| {
                    EpollHelperError::HandleEvent(anyhow!("Failed to read notify event: {e}"))
                })?;
                self.process_event_queue()?;
            }
            _ => {
                return Err(EpollHelperError::HandleEvent(anyhow!(
                    "Unexpected event: {ev_type}"
                )));
            }
        }
        Ok(())
    }
}

/// A disk file that can be used by the SCSI handler.
pub trait ScsiDisk: DiskOps + Send {
    /// Clone the disk for use in another thread.
    fn try_clone(&self) -> std::io::Result<Box<dyn ScsiDisk>>;
}

impl ScsiDisk for File {
    fn try_clone(&self) -> std::io::Result<Box<dyn ScsiDisk>> {
        Ok(Box::new(self.try_clone()?))
    }
}

/// Handler for request queues (queues 2+).
pub struct ScsiRequestHandler {
    pub queue_index: u16,
    pub queue: Queue,
    pub mem: GuestMemoryAtomic<GuestMemoryMmap>,
    pub interrupt_cb: Arc<dyn VirtioInterrupt>,
    pub queue_evt: EventFd,
    pub kill_evt: EventFd,
    pub pause_evt: EventFd,
    /// Map from LUN ID to command processor
    pub processors: Arc<Mutex<HashMap<ScsiLunId, ScsiCommandProcessor>>>,
    /// Map from LUN ID to disk file
    pub disks: HashMap<ScsiLunId, Box<dyn ScsiDisk>>,
}

impl ScsiRequestHandler {
    /// Process requests from the virtqueue.
    fn process_requests(&mut self) -> Result<(), EpollHelperError> {
        let mem = self.mem.memory();

        while let Some(mut desc_chain) = self.queue.pop_descriptor_chain(mem.clone()) {
            let head_index = desc_chain.head_index();

            // Parse the request and execute it, collecting write descriptors
            let (result, write_descs) = self.process_single_request(&mut desc_chain);

            // Build the response
            let cmd_result = match result {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to process SCSI request: {e}");
                    ScsiCommandResult::check_condition(sense_key::ABORTED_COMMAND, 0x00, 0x00)
                }
            };

            // Write the response to the write-only descriptors
            let mem = desc_chain.memory();
            let bytes_written = Self::write_response(&mem, &cmd_result, &write_descs)?;

            self.queue
                .add_used(desc_chain.memory(), head_index, bytes_written)
                .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to add used: {e}")))?;
        }

        self.signal_used_queue()?;
        Ok(())
    }

    fn process_single_request(
        &mut self,
        desc_chain: &mut virtio_queue::DescriptorChain<GuestMemoryLoadGuard<GuestMemoryMmap>>,
    ) -> (
        Result<ScsiCommandResult, EpollHelperError>,
        Vec<WriteDescriptor>,
    ) {
        let mut write_descs = Vec::new();

        // First descriptor should contain the request header
        let req_desc = match desc_chain.next() {
            Some(d) => d,
            None => {
                return (
                    Err(EpollHelperError::HandleEvent(anyhow!(
                        "Missing request descriptor"
                    ))),
                    write_descs,
                );
            }
        };

        if req_desc.len() < std::mem::size_of::<VirtioScsiCmdReq>() as u32 {
            return (
                Err(EpollHelperError::HandleEvent(anyhow!(
                    "Request descriptor too small: {} < {}",
                    req_desc.len(),
                    std::mem::size_of::<VirtioScsiCmdReq>()
                ))),
                write_descs,
            );
        }

        let req: VirtioScsiCmdReq = match desc_chain.memory().read_obj(req_desc.addr()) {
            Ok(r) => r,
            Err(e) => {
                return (
                    Err(EpollHelperError::HandleEvent(anyhow!(
                        "Failed to read request: {e}"
                    ))),
                    write_descs,
                );
            }
        };

        // Collect all remaining descriptors first to avoid borrow conflicts
        let descriptors: Vec<_> = desc_chain.by_ref().collect();
        let mem = desc_chain.memory();

        // Collect data-out buffers (for write commands) and track write descriptors
        let mut data_out = Vec::new();
        let mut allocation_length = 0u32;

        for desc in &descriptors {
            if !desc.is_write_only() {
                // Data-out (from guest to device)
                let mut buf = vec![0u8; desc.len() as usize];
                if let Err(e) = mem.read_slice(&mut buf, desc.addr()) {
                    return (
                        Err(EpollHelperError::HandleEvent(anyhow!(
                            "Failed to read data-out: {e}"
                        ))),
                        write_descs,
                    );
                }
                data_out.extend(buf);
            } else {
                // Data-in buffer - record for writing response
                write_descs.push(WriteDescriptor {
                    addr: desc.addr(),
                    len: desc.len(),
                });
                allocation_length += desc.len();
            }
        }

        // Check for REPORT LUNS well-known logical unit
        // According to virtio-scsi spec 5.6.6.1, REPORT LUNS to the well-known LUN
        // [0xC1, 0x01, 0, 0, 0, 0, 0, 0] should report all available LUNs.
        if is_report_luns_wlun(&req.lun) {
            let opcode = req.cdb[0];
            debug!("SCSI REPORT LUNS WLUN request: opcode={:#04x}", opcode);
            if opcode == scsi_opcode::REPORT_LUNS {
                let result = self.handle_report_luns_wlun(allocation_length);
                debug!(
                    "SCSI REPORT LUNS returning {} bytes of data",
                    result.data_in.len()
                );
                return (Ok(result), write_descs);
            } else {
                // Other commands to well-known LUN should return ILLEGAL REQUEST
                return (
                    Ok(ScsiCommandResult::check_condition(
                        sense_key::ILLEGAL_REQUEST,
                        0x20, // Invalid command operation code
                        0x00,
                    )),
                    write_descs,
                );
            }
        }

        // Parse the LUN
        let (target, lun) = match parse_lun(&req.lun) {
            Some(v) => v,
            None => {
                debug!(
                    "SCSI: Failed to parse LUN: {:02x?}, returning BAD_TARGET",
                    req.lun
                );
                return (Ok(ScsiCommandResult::bad_target()), write_descs);
            }
        };

        let lun_id = ScsiLunId::new(target, lun);

        // Check if LUN exists
        {
            let processors = self.processors.lock().unwrap();
            if !processors.contains_key(&lun_id) {
                debug!(
                    "SCSI: LUN {} not found (available: {:?})",
                    lun_id,
                    processors.keys().collect::<Vec<_>>()
                );
                return (Ok(ScsiCommandResult::incorrect_lun()), write_descs);
            }
        }

        // Get the disk for this LUN
        let disk = match self.disks.get_mut(&lun_id) {
            Some(d) => d,
            None => {
                return (Ok(ScsiCommandResult::incorrect_lun()), write_descs);
            }
        };

        // Process the command
        let mut processors = self.processors.lock().unwrap();
        let processor = processors.get_mut(&lun_id).unwrap();
        let result =
            processor.process_command(&req.cdb[..], &data_out, allocation_length, disk.as_mut());

        (Ok(result), write_descs)
    }

    /// Handle REPORT LUNS command sent to the well-known logical unit.
    /// This returns all available LUNs for all targets.
    fn handle_report_luns_wlun(&self, allocation_length: u32) -> ScsiCommandResult {
        let processors = self.processors.lock().unwrap();
        let lun_count = processors.len();
        debug!("SCSI REPORT LUNS: {} LUNs available", lun_count);

        // REPORT LUNS response format:
        // Bytes 0-3: LUN list length (number of LUNs * 8)
        // Bytes 4-7: Reserved
        // Bytes 8+: LUN list (8 bytes per LUN)
        let response_len = 8 + lun_count * 8;
        let mut data = vec![0u8; response_len];

        // LUN list length (in bytes, not including the header)
        let lun_list_len = (lun_count * 8) as u32;
        data[0..4].copy_from_slice(&lun_list_len.to_be_bytes());

        // Add each LUN to the list
        for (i, lun_id) in processors.keys().enumerate() {
            let offset = 8 + i * 8;
            let lun_bytes = encode_lun(lun_id.target, lun_id.lun);
            data[offset..offset + 8].copy_from_slice(&lun_bytes);
        }

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    fn write_to_descriptors(
        mem: &GuestMemoryMmap,
        write_descs: &[WriteDescriptor],
        offset: usize,
        data: &[u8],
    ) -> Result<usize, EpollHelperError> {
        let mut written = 0usize;
        let mut skip = offset;

        for desc in write_descs {
            let desc_len = desc.len as usize;
            if skip >= desc_len {
                skip -= desc_len;
                continue;
            }

            let desc_offset = skip;
            let remaining = data.len() - written;
            let to_write = std::cmp::min(desc_len - desc_offset, remaining);
            let write_addr = desc.addr.checked_add(desc_offset as u64).ok_or_else(|| {
                EpollHelperError::HandleEvent(anyhow!("Response address overflow"))
            })?;

            mem.write_slice(&data[written..written + to_write], write_addr)
                .map_err(|e| {
                    EpollHelperError::HandleEvent(anyhow!("Failed to write response: {e}"))
                })?;

            written += to_write;
            skip = 0;

            if written == data.len() {
                break;
            }
        }

        Ok(written)
    }

    fn write_response(
        mem: &GuestMemoryMmap,
        result: &ScsiCommandResult,
        write_descs: &[WriteDescriptor],
    ) -> Result<u32, EpollHelperError> {
        if write_descs.is_empty() {
            warn!("No write descriptors for SCSI response");
            return Ok(0);
        }

        // Build response header
        let mut resp = VirtioScsiCmdResp::default();
        resp.response = result.response;
        resp.status = result.status;
        resp.resid = result.resid;
        resp.sense_len = result.sense.len() as u32;

        // Copy sense data
        let sense_len = std::cmp::min(result.sense.len(), VIRTIO_SCSI_SENSE_SIZE);
        resp.sense[..sense_len].copy_from_slice(&result.sense[..sense_len]);

        let resp_size = std::mem::size_of::<VirtioScsiCmdResp>() as u32;
        let write_capacity: u64 = write_descs.iter().map(|desc| u64::from(desc.len)).sum();
        if write_capacity < u64::from(resp_size) {
            return Err(EpollHelperError::HandleEvent(anyhow!(
                "Response descriptors too small: {} < {}",
                write_capacity,
                resp_size
            )));
        }

        // The writable chain contains the response header followed by data-in;
        // descriptor boundaries do not have to match this layout.
        let header_written = Self::write_to_descriptors(mem, write_descs, 0, resp.as_slice())?;
        if header_written != resp_size as usize {
            return Err(EpollHelperError::HandleEvent(anyhow!(
                "Failed to write complete response header"
            )));
        }

        let mut total_written = resp_size;

        if !result.data_in.is_empty() {
            let data_written =
                Self::write_to_descriptors(mem, write_descs, resp_size as usize, &result.data_in)?;
            total_written += data_written as u32;
        }

        Ok(total_written)
    }

    fn signal_used_queue(&self) -> Result<(), EpollHelperError> {
        self.interrupt_cb
            .trigger(VirtioInterruptType::Queue(self.queue_index))
            .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to signal: {e}")))
    }

    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
    ) -> Result<(), EpollHelperError> {
        let mut helper = EpollHelper::new(&self.kill_evt, &self.pause_evt)?;
        helper.add_event(self.queue_evt.as_raw_fd(), REQUEST_QUEUE_EVENT)?;
        helper.run(paused, paused_sync, self)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{Bytes, GuestAddress, GuestMemory};

    fn guest_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap()
    }

    fn read_descriptors(
        mem: &GuestMemoryMmap,
        write_descs: &[WriteDescriptor],
        len: usize,
    ) -> Vec<u8> {
        let mut data = Vec::new();
        let mut remaining = len;

        for desc in write_descs {
            let to_read = std::cmp::min(desc.len as usize, remaining);
            let mut buf = vec![0; to_read];
            mem.read_slice(&mut buf, desc.addr).unwrap();
            data.extend_from_slice(&buf);
            remaining -= to_read;

            if remaining == 0 {
                break;
            }
        }

        data
    }

    fn expected_header(result: &ScsiCommandResult) -> Vec<u8> {
        let mut resp = VirtioScsiCmdResp {
            response: result.response,
            status: result.status,
            resid: result.resid,
            sense_len: result.sense.len() as u32,
            ..Default::default()
        };
        let sense_len = std::cmp::min(result.sense.len(), VIRTIO_SCSI_SENSE_SIZE);
        resp.sense[..sense_len].copy_from_slice(&result.sense[..sense_len]);

        resp.as_slice().to_vec()
    }

    #[test]
    fn test_async_notification_query_and_subscription() {
        let lun_id = ScsiLunId::new(2, 3);
        let mut state = ScsiEventState::default();
        let requested = VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE | 0x1;

        assert_eq!(
            state.query_async_events(requested),
            VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE
        );
        assert!(!state.queue_async_notify(lun_id, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE));

        assert_eq!(
            state.subscribe_async_events(lun_id, requested),
            VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE
        );
        assert!(state.queue_async_notify(
            lun_id,
            VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE | VIRTIO_SCSI_EVT_ASYNC_POWER_MGMT
        ));
        let event = state.pop_event().unwrap();
        let event_type = event.event;
        let event_lun = event.lun;
        let event_reason = event.reason;
        assert_eq!(event_type, VIRTIO_SCSI_T_ASYNC_NOTIFY);
        assert_eq!(event_lun, encode_lun(2, 3));
        assert_eq!(event_reason, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE);

        assert_eq!(state.subscribe_async_events(lun_id, 0), 0);
        assert!(!state.queue_async_notify(lun_id, VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE));
    }

    #[test]
    fn test_event_write_delivers_event_payload() {
        let mem = guest_mem();
        let event = VirtioScsiEvent {
            event: VIRTIO_SCSI_T_PARAM_CHANGE,
            lun: encode_lun(1, 2),
            reason: 0x092a,
        };

        let written = ScsiEventHandler::write_event(
            &mem,
            GuestAddress(0x100),
            std::mem::size_of::<VirtioScsiEvent>() as u32,
            event,
        )
        .unwrap();
        let actual: VirtioScsiEvent = mem.read_obj(GuestAddress(0x100)).unwrap();
        let actual_event = actual.event;
        let actual_lun = actual.lun;
        let actual_reason = actual.reason;

        assert_eq!(written as usize, std::mem::size_of::<VirtioScsiEvent>());
        assert_eq!(actual_event, VIRTIO_SCSI_T_PARAM_CHANGE);
        assert_eq!(actual_lun, encode_lun(1, 2));
        assert_eq!(actual_reason, 0x092a);
    }

    #[test]
    fn test_write_response_spans_write_descriptors() {
        let mem = guest_mem();
        let resp_size = std::mem::size_of::<VirtioScsiCmdResp>();
        let first_len = 5;
        let result = ScsiCommandResult {
            response: VIRTIO_SCSI_S_OK,
            status: scsi_status::CHECK_CONDITION,
            data_in: vec![0xaa, 0xbb, 0xcc],
            sense: vec![0x70, 0x00, sense_key::ILLEGAL_REQUEST, 0x00],
            resid: 0x0102_0304,
        };
        let write_descs = [
            WriteDescriptor {
                addr: GuestAddress(0x100),
                len: first_len,
            },
            WriteDescriptor {
                addr: GuestAddress(0x200),
                len: (resp_size - first_len as usize + 1) as u32,
            },
            WriteDescriptor {
                addr: GuestAddress(0x300),
                len: 2,
            },
        ];

        let written = ScsiRequestHandler::write_response(&mem, &result, &write_descs).unwrap();

        let mut expected = expected_header(&result);
        expected.extend_from_slice(&result.data_in);
        assert_eq!(written as usize, expected.len());
        assert_eq!(
            read_descriptors(&mem, &write_descs, expected.len()),
            expected
        );
    }

    #[test]
    fn test_write_response_rejects_short_response_header_chain() {
        let mem = guest_mem();
        let resp_size = std::mem::size_of::<VirtioScsiCmdResp>();
        let write_descs = [WriteDescriptor {
            addr: GuestAddress(0x100),
            len: (resp_size - 1) as u32,
        }];

        assert!(
            ScsiRequestHandler::write_response(&mem, &ScsiCommandResult::default(), &write_descs)
                .is_err()
        );
    }

    #[test]
    fn test_write_response_truncates_data_in_to_write_capacity() {
        let mem = guest_mem();
        let resp_size = std::mem::size_of::<VirtioScsiCmdResp>();
        let result = ScsiCommandResult::ok_with_data(vec![1, 2, 3, 4]);
        let write_descs = [WriteDescriptor {
            addr: GuestAddress(0x100),
            len: (resp_size + 2) as u32,
        }];

        let written = ScsiRequestHandler::write_response(&mem, &result, &write_descs).unwrap();

        let mut expected = expected_header(&result);
        expected.extend_from_slice(&result.data_in[..2]);
        assert_eq!(written as usize, expected.len());
        assert_eq!(
            read_descriptors(&mem, &write_descs, expected.len()),
            expected
        );
    }
}

impl EpollHelperHandler for ScsiRequestHandler {
    fn handle_event(
        &mut self,
        _helper: &mut EpollHelper,
        event: &epoll::Event,
    ) -> Result<(), EpollHelperError> {
        let ev_type = event.data as u16;
        match ev_type {
            REQUEST_QUEUE_EVENT => {
                self.queue_evt.read().map_err(|e| {
                    EpollHelperError::HandleEvent(anyhow!("Failed to read queue event: {e}"))
                })?;
                self.process_requests()?;
            }
            _ => {
                return Err(EpollHelperError::HandleEvent(anyhow!(
                    "Unexpected event: {ev_type}"
                )));
            }
        }
        Ok(())
    }
}
