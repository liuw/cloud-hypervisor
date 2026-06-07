// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! SCSI queue handler implementation.
//!
//! This module implements the epoll-based queue handlers for the virtio-scsi
//! device, processing control, event, and request queues.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Barrier, Mutex};

use anyhow::anyhow;
use log::{debug, error, warn};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard};
use vmm_sys_util::eventfd::EventFd;

use super::commands::{DiskOps, ScsiCommandProcessor, ScsiCommandResult};
use super::protocol::*;
use super::target::ScsiLunId;
use crate::{
    EpollHelper, EpollHelperError, EpollHelperHandler, GuestMemoryMmap,
    VirtioInterrupt, VirtioInterruptType,
};

// Epoll events
const CTRL_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 1;
const EVENT_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 2;
const REQUEST_QUEUE_EVENT: u16 = crate::EPOLL_HELPER_EVENT_LAST + 3;

/// Handler for the control queue (queue 0).
pub struct ScsiCtrlHandler {
    pub queue: Queue,
    pub mem: GuestMemoryAtomic<GuestMemoryMmap>,
    pub interrupt_cb: Arc<dyn VirtioInterrupt>,
    pub queue_evt: EventFd,
    pub kill_evt: EventFd,
    pub pause_evt: EventFd,
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

            let request_type: u32 = desc_chain.memory()
                .read_obj(desc.addr())
                .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to read request type: {e}")))?;

            let response = match request_type {
                VIRTIO_SCSI_T_TMF => {
                    // Task Management Function - return success
                    debug!("TMF request received");
                    VIRTIO_SCSI_S_FUNCTION_SUCCEEDED
                }
                VIRTIO_SCSI_T_AN_QUERY | VIRTIO_SCSI_T_AN_SUBSCRIBE => {
                    // Async notification query/subscribe - return success with no events
                    debug!("Async notification request: type={}", request_type);
                    VIRTIO_SCSI_S_OK
                }
                _ => {
                    warn!("Unknown control request type: {}", request_type);
                    VIRTIO_SCSI_S_FAILURE
                }
            };

            // Find the response descriptor and write the response
            let mut resp_written = false;
            for desc in desc_chain.by_ref() {
                if desc.is_write_only() {
                    desc_chain.memory().write_obj(response, desc.addr())
                        .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write response: {e}")))?;
                    resp_written = true;
                    break;
                }
            }

            if !resp_written {
                warn!("No write descriptor for control response");
            }

            self.queue
                .add_used(desc_chain.memory(), head_index, 1)
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
}

impl ScsiEventHandler {
    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
    ) -> Result<(), EpollHelperError> {
        let mut helper = EpollHelper::new(&self.kill_evt, &self.pause_evt)?;
        helper.add_event(self.queue_evt.as_raw_fd(), EVENT_QUEUE_EVENT)?;
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
                // Event queue is passive - we queue events when hotplug/changes occur
                // For now, just consume the event
                debug!("Event queue notification received");
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

/// Descriptor info for writing response back
struct WriteDescriptor {
    addr: vm_memory::GuestAddress,
    len: u32,
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
                    ScsiCommandResult::check_condition(
                        sense_key::ABORTED_COMMAND,
                        0x00,
                        0x00,
                    )
                }
            };

            // Write the response to the write-only descriptors
            let mem = desc_chain.memory();
            let bytes_written = self.write_response(&mem, &cmd_result, &write_descs)?;

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
    ) -> (Result<ScsiCommandResult, EpollHelperError>, Vec<WriteDescriptor>) {
        let mut write_descs = Vec::new();
        
        // First descriptor should contain the request header
        let req_desc = match desc_chain.next() {
            Some(d) => d,
            None => {
                return (Err(EpollHelperError::HandleEvent(anyhow!("Missing request descriptor"))), write_descs);
            }
        };

        if req_desc.len() < std::mem::size_of::<VirtioScsiCmdReq>() as u32 {
            return (Err(EpollHelperError::HandleEvent(anyhow!(
                "Request descriptor too small: {} < {}",
                req_desc.len(),
                std::mem::size_of::<VirtioScsiCmdReq>()
            ))), write_descs);
        }

        let req: VirtioScsiCmdReq = match desc_chain.memory().read_obj(req_desc.addr()) {
            Ok(r) => r,
            Err(e) => {
                return (Err(EpollHelperError::HandleEvent(anyhow!("Failed to read request: {e}"))), write_descs);
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
                    return (Err(EpollHelperError::HandleEvent(anyhow!("Failed to read data-out: {e}"))), write_descs);
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
            if opcode == scsi_opcode::REPORT_LUNS {
                let result = self.handle_report_luns_wlun(allocation_length);
                return (Ok(result), write_descs);
            } else {
                // Other commands to well-known LUN should return ILLEGAL REQUEST
                return (Ok(ScsiCommandResult::check_condition(
                    sense_key::ILLEGAL_REQUEST,
                    0x20, // Invalid command operation code
                    0x00,
                )), write_descs);
            }
        }

        // Parse the LUN
        let (target, lun) = match parse_lun(&req.lun) {
            Some(v) => v,
            None => {
                return (Ok(ScsiCommandResult::bad_target()), write_descs);
            }
        };

        let lun_id = ScsiLunId::new(target, lun);

        // Check if LUN exists
        {
            let processors = self.processors.lock().unwrap();
            if !processors.contains_key(&lun_id) {
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
        let result = processor.process_command(
            &req.cdb[..],
            &data_out,
            allocation_length,
            disk.as_mut(),
        );

        (Ok(result), write_descs)
    }

    /// Handle REPORT LUNS command sent to the well-known logical unit.
    /// This returns all available LUNs for all targets.
    fn handle_report_luns_wlun(&self, allocation_length: u32) -> ScsiCommandResult {
        let processors = self.processors.lock().unwrap();
        let lun_count = processors.len();

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

    fn write_response(
        &self,
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

        // Write response header to first descriptor
        let first_desc = &write_descs[0];
        mem.write_obj(resp, first_desc.addr)
            .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write response header: {e}")))?;
        
        let resp_size = std::mem::size_of::<VirtioScsiCmdResp>() as u32;
        let mut total_written = resp_size;
        
        // Write data-in if any, starting after the response header in the first descriptor
        // or continuing to subsequent descriptors
        if !result.data_in.is_empty() {
            let mut data_offset = 0usize;
            let space_in_first = first_desc.len.saturating_sub(resp_size);
            
            // First, use remaining space in first descriptor
            if space_in_first > 0 && data_offset < result.data_in.len() {
                let to_write = std::cmp::min(space_in_first as usize, result.data_in.len());
                let write_addr = vm_memory::GuestAddress(first_desc.addr.raw_value() + resp_size as u64);
                mem.write_slice(&result.data_in[..to_write], write_addr)
                    .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write data-in: {e}")))?;
                data_offset += to_write;
                total_written += to_write as u32;
            }
            
            // Continue with subsequent descriptors
            let mut desc_idx = 1;
            while data_offset < result.data_in.len() && desc_idx < write_descs.len() {
                let desc = &write_descs[desc_idx];
                let remaining_data = result.data_in.len() - data_offset;
                let to_write = std::cmp::min(desc.len as usize, remaining_data);
                
                mem.write_slice(&result.data_in[data_offset..data_offset + to_write], desc.addr)
                    .map_err(|e| EpollHelperError::HandleEvent(anyhow!("Failed to write data-in: {e}")))?;
                data_offset += to_write;
                total_written += to_write as u32;
                desc_idx += 1;
            }
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
