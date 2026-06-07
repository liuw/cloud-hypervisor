// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Virtio SCSI protocol definitions.
//!
//! This module contains data structures and constants as defined in the VIRTIO
//! specification (Section 5.6) for the SCSI Host device.

use serde::{Deserialize, Serialize};
use vm_memory::ByteValued;

/// Virtio SCSI Feature Flags
/// VIRTIO_SCSI_F_INOUT: A single request can include both read and write data buffers.
pub const VIRTIO_SCSI_F_INOUT: u64 = 0;
/// VIRTIO_SCSI_F_HOTPLUG: Supports hot-plug and hot-unplug of LUNs.
pub const VIRTIO_SCSI_F_HOTPLUG: u64 = 1;
/// VIRTIO_SCSI_F_CHANGE: Supports change notification for media and power management events.
pub const VIRTIO_SCSI_F_CHANGE: u64 = 2;
/// VIRTIO_SCSI_F_T10_PI: Supports T10 Protection Information (DIF/DIX).
pub const VIRTIO_SCSI_F_T10_PI: u64 = 3;

/// Control queue type: Task Management Function
pub const VIRTIO_SCSI_T_TMF: u32 = 0;
/// Control queue type: Asynchronous Notification Query
pub const VIRTIO_SCSI_T_AN_QUERY: u32 = 1;
/// Control queue type: Asynchronous Notification Subscribe
pub const VIRTIO_SCSI_T_AN_SUBSCRIBE: u32 = 2;

/// Task Management Function subcodes
pub const VIRTIO_SCSI_T_TMF_ABORT_TASK: u32 = 0;
pub const VIRTIO_SCSI_T_TMF_ABORT_TASK_SET: u32 = 1;
pub const VIRTIO_SCSI_T_TMF_CLEAR_ACA: u32 = 2;
pub const VIRTIO_SCSI_T_TMF_CLEAR_TASK_SET: u32 = 3;
pub const VIRTIO_SCSI_T_TMF_I_T_NEXUS_RESET: u32 = 4;
pub const VIRTIO_SCSI_T_TMF_LOGICAL_UNIT_RESET: u32 = 5;
pub const VIRTIO_SCSI_T_TMF_QUERY_TASK: u32 = 6;
pub const VIRTIO_SCSI_T_TMF_QUERY_TASK_SET: u32 = 7;

/// Response status codes
pub const VIRTIO_SCSI_S_OK: u8 = 0;
pub const VIRTIO_SCSI_S_OVERRUN: u8 = 1;
pub const VIRTIO_SCSI_S_ABORTED: u8 = 2;
pub const VIRTIO_SCSI_S_BAD_TARGET: u8 = 3;
pub const VIRTIO_SCSI_S_RESET: u8 = 4;
pub const VIRTIO_SCSI_S_BUSY: u8 = 5;
pub const VIRTIO_SCSI_S_TRANSPORT_FAILURE: u8 = 6;
pub const VIRTIO_SCSI_S_TARGET_FAILURE: u8 = 7;
pub const VIRTIO_SCSI_S_NEXUS_FAILURE: u8 = 8;
pub const VIRTIO_SCSI_S_FAILURE: u8 = 9;
pub const VIRTIO_SCSI_S_FUNCTION_SUCCEEDED: u8 = 10;
pub const VIRTIO_SCSI_S_FUNCTION_REJECTED: u8 = 11;
pub const VIRTIO_SCSI_S_INCORRECT_LUN: u8 = 12;

/// Event types for the event queue
pub const VIRTIO_SCSI_T_NO_EVENT: u32 = 0;
pub const VIRTIO_SCSI_T_TRANSPORT_RESET: u32 = 1;
pub const VIRTIO_SCSI_T_ASYNC_NOTIFY: u32 = 2;
pub const VIRTIO_SCSI_T_PARAM_CHANGE: u32 = 3;

/// Transport reset event reasons
pub const VIRTIO_SCSI_EVT_RESET_HARD: u32 = 0;
pub const VIRTIO_SCSI_EVT_RESET_RESCAN: u32 = 1;
pub const VIRTIO_SCSI_EVT_RESET_REMOVED: u32 = 2;

/// Asynchronous notification event flags
pub const VIRTIO_SCSI_EVT_ASYNC_OPERATIONAL_CHANGE: u32 = 2;
pub const VIRTIO_SCSI_EVT_ASYNC_POWER_MGMT: u32 = 4;
pub const VIRTIO_SCSI_EVT_ASYNC_EXTERNAL_REQUEST: u32 = 8;
pub const VIRTIO_SCSI_EVT_ASYNC_MEDIA_CHANGE: u32 = 16;
pub const VIRTIO_SCSI_EVT_ASYNC_MULTI_HOST: u32 = 32;
pub const VIRTIO_SCSI_EVT_ASYNC_DEVICE_BUSY: u32 = 64;

/// Maximum CDB (Command Descriptor Block) size
pub const VIRTIO_SCSI_CDB_SIZE: usize = 32;

/// Maximum sense data size
pub const VIRTIO_SCSI_SENSE_SIZE: usize = 96;

/// Default configuration values
pub const VIRTIO_SCSI_DEFAULT_NUM_QUEUES: usize = 1;
pub const VIRTIO_SCSI_DEFAULT_QUEUE_SIZE: u16 = 128;
pub const VIRTIO_SCSI_DEFAULT_SEG_MAX: u32 = 126;
pub const VIRTIO_SCSI_DEFAULT_MAX_SECTORS: u32 = 0xFFFF;
pub const VIRTIO_SCSI_DEFAULT_CMD_PER_LUN: u32 = 128;
pub const VIRTIO_SCSI_DEFAULT_MAX_CHANNEL: u16 = 0;
pub const VIRTIO_SCSI_DEFAULT_MAX_TARGET: u16 = 255;
pub const VIRTIO_SCSI_DEFAULT_MAX_LUN: u32 = 16383;

/// Virtio SCSI device configuration structure.
///
/// This structure represents the device configuration space as defined
/// in the VIRTIO specification.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
pub struct VirtioScsiConfig {
    /// Number of request queues (in addition to control and event queues)
    pub num_queues: u32,
    /// Maximum number of segments per request
    pub seg_max: u32,
    /// Maximum number of sectors per request
    pub max_sectors: u32,
    /// Maximum number of outstanding commands per LUN
    pub cmd_per_lun: u32,
    /// Event info size (for the event queue)
    pub event_info_size: u32,
    /// Sense data size
    pub sense_size: u32,
    /// CDB size
    pub cdb_size: u32,
    /// Maximum channel number
    pub max_channel: u16,
    /// Maximum target number
    pub max_target: u16,
    /// Maximum LUN number
    pub max_lun: u32,
}

// SAFETY: VirtioScsiConfig contains only primitive types with no padding issues
// when packed, and is safe to read/write as raw bytes for virtio config space.
unsafe impl ByteValued for VirtioScsiConfig {}

impl VirtioScsiConfig {
    /// Create a new configuration with default values
    pub fn new(num_queues: u32) -> Self {
        VirtioScsiConfig {
            num_queues,
            seg_max: VIRTIO_SCSI_DEFAULT_SEG_MAX,
            max_sectors: VIRTIO_SCSI_DEFAULT_MAX_SECTORS,
            cmd_per_lun: VIRTIO_SCSI_DEFAULT_CMD_PER_LUN,
            event_info_size: 0,
            sense_size: VIRTIO_SCSI_SENSE_SIZE as u32,
            cdb_size: VIRTIO_SCSI_CDB_SIZE as u32,
            max_channel: VIRTIO_SCSI_DEFAULT_MAX_CHANNEL,
            max_target: VIRTIO_SCSI_DEFAULT_MAX_TARGET,
            max_lun: VIRTIO_SCSI_DEFAULT_MAX_LUN,
        }
    }
}

/// SCSI command request header.
///
/// This structure is sent by the driver at the beginning of each request
/// on the request queues.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiCmdReq {
    /// Logical Unit Number (8 bytes as per SAM specification)
    pub lun: [u8; 8],
    /// Command identifier (for task management)
    pub tag: u64,
    /// Task attributes (SIMPLE, ORDERED, HEAD OF QUEUE, ACA)
    pub task_attr: u8,
    /// Priority (for some transport protocols)
    pub prio: u8,
    /// Command reference number
    pub crn: u8,
    /// Command Descriptor Block (the actual SCSI command)
    pub cdb: [u8; VIRTIO_SCSI_CDB_SIZE],
}

// SAFETY: VirtioScsiCmdReq is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCmdReq {}

/// SCSI command response header.
///
/// This structure is returned by the device at the beginning of the response
/// for each request on the request queues.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct VirtioScsiCmdResp {
    /// Sense data length
    pub sense_len: u32,
    /// Residual byte count (difference between expected and actual transfer)
    pub resid: u32,
    /// Status qualifier (additional status information)
    pub status_qualifier: u16,
    /// SCSI status byte (GOOD, CHECK CONDITION, etc.)
    pub status: u8,
    /// Virtio SCSI response code
    pub response: u8,
    /// Sense data
    pub sense: [u8; VIRTIO_SCSI_SENSE_SIZE],
}

impl Default for VirtioScsiCmdResp {
    fn default() -> Self {
        VirtioScsiCmdResp {
            sense_len: 0,
            resid: 0,
            status_qualifier: 0,
            status: 0,
            response: 0,
            sense: [0u8; VIRTIO_SCSI_SENSE_SIZE],
        }
    }
}

// SAFETY: VirtioScsiCmdResp is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCmdResp {}

/// Task Management Function request.
///
/// Used on the control queue for task management operations.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiCtrlTmfReq {
    /// Request type (must be VIRTIO_SCSI_T_TMF)
    pub request_type: u32,
    /// TMF subtype (ABORT_TASK, LUN_RESET, etc.)
    pub subtype: u32,
    /// Logical Unit Number
    pub lun: [u8; 8],
    /// Tag of the command to act upon (for ABORT_TASK)
    pub tag: u64,
}

// SAFETY: VirtioScsiCtrlTmfReq is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCtrlTmfReq {}

/// Task Management Function response.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiCtrlTmfResp {
    /// Response status code
    pub response: u8,
}

// SAFETY: VirtioScsiCtrlTmfResp is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCtrlTmfResp {}

/// Asynchronous Notification Query/Subscribe request.
///
/// Used on the control queue for querying or subscribing to async events.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiCtrlAnReq {
    /// Request type (VIRTIO_SCSI_T_AN_QUERY or VIRTIO_SCSI_T_AN_SUBSCRIBE)
    pub request_type: u32,
    /// Logical Unit Number
    pub lun: [u8; 8],
    /// Event types to query or subscribe to
    pub event_requested: u32,
}

// SAFETY: VirtioScsiCtrlAnReq is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCtrlAnReq {}

/// Asynchronous Notification Query/Subscribe response.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiCtrlAnResp {
    /// Event types supported or subscribed
    pub event_actual: u32,
    /// Response status code
    pub response: u8,
}

// SAFETY: VirtioScsiCtrlAnResp is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiCtrlAnResp {}

/// Event notification structure.
///
/// Used on the event queue to report events from the device to the driver.
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct VirtioScsiEvent {
    /// Event type
    pub event: u32,
    /// Logical Unit Number associated with the event
    pub lun: [u8; 8],
    /// Event-specific reason code
    pub reason: u32,
}

// SAFETY: VirtioScsiEvent is a packed struct of primitive types, safe for ByteValued.
unsafe impl ByteValued for VirtioScsiEvent {}

/// SCSI status codes (as per SAM specification)
pub mod scsi_status {
    pub const GOOD: u8 = 0x00;
    pub const CHECK_CONDITION: u8 = 0x02;
    pub const CONDITION_MET: u8 = 0x04;
    pub const BUSY: u8 = 0x08;
    pub const RESERVATION_CONFLICT: u8 = 0x18;
    pub const TASK_SET_FULL: u8 = 0x28;
    pub const ACA_ACTIVE: u8 = 0x30;
    pub const TASK_ABORTED: u8 = 0x40;
}

/// SCSI sense key codes
pub mod sense_key {
    pub const NO_SENSE: u8 = 0x00;
    pub const RECOVERED_ERROR: u8 = 0x01;
    pub const NOT_READY: u8 = 0x02;
    pub const MEDIUM_ERROR: u8 = 0x03;
    pub const HARDWARE_ERROR: u8 = 0x04;
    pub const ILLEGAL_REQUEST: u8 = 0x05;
    pub const UNIT_ATTENTION: u8 = 0x06;
    pub const DATA_PROTECT: u8 = 0x07;
    pub const BLANK_CHECK: u8 = 0x08;
    pub const VENDOR_SPECIFIC: u8 = 0x09;
    pub const COPY_ABORTED: u8 = 0x0A;
    pub const ABORTED_COMMAND: u8 = 0x0B;
    pub const VOLUME_OVERFLOW: u8 = 0x0D;
    pub const MISCOMPARE: u8 = 0x0E;
    pub const COMPLETED: u8 = 0x0F;
}

/// SCSI operation codes for common commands
pub mod scsi_opcode {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SELECT_6: u8 = 0x15;
    pub const MODE_SENSE_6: u8 = 0x1A;
    pub const START_STOP_UNIT: u8 = 0x1B;
    pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
    pub const UNMAP: u8 = 0x42;
    pub const MODE_SELECT_10: u8 = 0x55;
    pub const PERSISTENT_RESERVE_IN: u8 = 0x5E;
    pub const PERSISTENT_RESERVE_OUT: u8 = 0x5F;
    pub const MODE_SENSE_10: u8 = 0x5A;
    pub const READ_16: u8 = 0x88;
    pub const WRITE_16: u8 = 0x8A;
    pub const SYNCHRONIZE_CACHE_16: u8 = 0x91;
    pub const SERVICE_ACTION_IN_16: u8 = 0x9E;
    pub const REPORT_LUNS: u8 = 0xA0;

    /// Service action for READ CAPACITY (16)
    pub const SAI_READ_CAPACITY_16: u8 = 0x10;
}

/// Build a fixed-format sense data buffer.
///
/// This creates a sense data response in the fixed format as defined by SPC.
///
/// # Arguments
/// * `sense_key` - The sense key indicating the error category
/// * `asc` - Additional Sense Code
/// * `ascq` - Additional Sense Code Qualifier
///
/// # Returns
/// A 18-byte fixed format sense data buffer
pub fn build_sense_data(sense_key: u8, asc: u8, ascq: u8) -> [u8; 18] {
    let mut sense = [0u8; 18];
    // Response code: 0x70 = current errors, fixed format
    sense[0] = 0x70;
    // Sense key
    sense[2] = sense_key & 0x0F;
    // Additional sense length (number of bytes after byte 7)
    sense[7] = 10;
    // Additional Sense Code
    sense[12] = asc;
    // Additional Sense Code Qualifier
    sense[13] = ascq;
    sense
}

/// Check if a LUN field addresses the REPORT LUNS well-known logical unit.
///
/// According to virtio-scsi spec section 5.6.6.1, the REPORT LUNS well-known
/// logical unit is addressed as [0xC1, 0x01, 0, 0, 0, 0, 0, 0].
/// This is used by the driver to discover all available LUNs.
pub fn is_report_luns_wlun(lun: &[u8; 8]) -> bool {
    lun[0] == 0xC1 && lun[1] == 0x01 && lun[2..] == [0, 0, 0, 0, 0, 0]
}

/// Parse a LUN field from virtio-scsi format.
///
/// The virtio-scsi LUN format follows SAM-5 specification:
/// - Byte 0: Must be 1 for single-level logical unit addressing
/// - Byte 1: Target ID
/// - Bytes 2-3: Second level LUN with addressing method in top 2 bits
///   - Method 00 (peripheral device): bus ID in bits 13-8, LUN in bits 7-0
///   - Method 01 (flat space): 14-bit LUN in bits 13-0
///   - Method 10: Extended flat space (not supported)
///   - Method 11: Extended logical unit (not supported)
/// - Bytes 4-7: Reserved (must be 0)
///
/// # Returns
/// A tuple of (target, lun) or None if the LUN format is invalid.
pub fn parse_lun(lun: &[u8; 8]) -> Option<(u8, u16)> {
    // First byte must be 1 for single-level LUN structure
    if lun[0] != 1 {
        return None;
    }

    let target = lun[1];

    // Extract addressing method from top 2 bits of byte 2
    let address_method = (lun[2] >> 6) & 0x03;

    let lun_value = match address_method {
        0b00 => {
            // Peripheral device addressing: byte 2 bits 5-0 = bus, byte 3 = LUN
            // For simplicity, we only support bus 0 and use byte 3 as LUN
            lun[3] as u16
        }
        0b01 => {
            // Flat space addressing: 14-bit LUN in bits 13-0
            (((lun[2] & 0x3F) as u16) << 8) | (lun[3] as u16)
        }
        _ => {
            // Extended addressing methods not supported
            return None;
        }
    };

    // Bytes 4-7 must be zero
    if lun[4] != 0 || lun[5] != 0 || lun[6] != 0 || lun[7] != 0 {
        return None;
    }

    Some((target, lun_value))
}

/// Encode a target and LUN into virtio-scsi format using flat space addressing.
///
/// # Arguments
/// * `target` - The target ID (0-255)
/// * `lun` - The LUN number (0-16383 for flat addressing)
///
/// # Returns
/// An 8-byte LUN field in virtio-scsi format.
pub fn encode_lun(target: u8, lun: u16) -> [u8; 8] {
    let mut lun_bytes = [0u8; 8];
    lun_bytes[0] = 1; // Single-level LUN structure
    lun_bytes[1] = target;
    // Use flat space addressing (method 01) in top 2 bits
    // LUN value in lower 14 bits
    lun_bytes[2] = 0x40 | ((lun >> 8) & 0x3F) as u8;
    lun_bytes[3] = lun as u8;
    lun_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_size() {
        // Verify that the config struct has the expected size
        assert_eq!(std::mem::size_of::<VirtioScsiConfig>(), 36);
    }

    #[test]
    fn test_cmd_req_size() {
        // Request header should be 51 bytes (8 + 8 + 1 + 1 + 1 + 32)
        assert_eq!(std::mem::size_of::<VirtioScsiCmdReq>(), 51);
    }

    #[test]
    fn test_cmd_resp_size() {
        // Response header should be 108 bytes (4 + 4 + 2 + 1 + 1 + 96)
        assert_eq!(std::mem::size_of::<VirtioScsiCmdResp>(), 108);
    }

    #[test]
    fn test_lun_parsing() {
        // Test flat space addressing (method 01)
        let lun = encode_lun(5, 10);
        assert_eq!(lun[0], 1); // Single-level
        assert_eq!(lun[1], 5); // Target
        assert_eq!(lun[2], 0x40); // Method 01 + high bits of LUN 10 (0)
        assert_eq!(lun[3], 10); // Low bits of LUN
        let parsed = parse_lun(&lun);
        assert_eq!(parsed, Some((5, 10)));
    }

    #[test]
    fn test_lun_parsing_peripheral() {
        // Test peripheral device addressing (method 00) - what Linux uses
        let mut lun = [0u8; 8];
        lun[0] = 1; // Single-level
        lun[1] = 0; // Target 0
        lun[2] = 0x00; // Method 00, bus 0
        lun[3] = 0; // LUN 0
        let parsed = parse_lun(&lun);
        assert_eq!(parsed, Some((0, 0)));

        // Target 5, LUN 3 with peripheral addressing
        lun[1] = 5;
        lun[2] = 0x00;
        lun[3] = 3;
        let parsed = parse_lun(&lun);
        assert_eq!(parsed, Some((5, 3)));
    }

    #[test]
    fn test_lun_parsing_flat_space() {
        // Test flat space addressing (method 01)
        let mut lun = [0u8; 8];
        lun[0] = 1;
        lun[1] = 0; // Target 0
        lun[2] = 0x40; // Method 01, LUN 0
        lun[3] = 0;
        let parsed = parse_lun(&lun);
        assert_eq!(parsed, Some((0, 0)));

        // Target 240, LUN 0 with flat addressing
        lun[1] = 240;
        lun[2] = 0x40;
        lun[3] = 0;
        let parsed = parse_lun(&lun);
        assert_eq!(parsed, Some((240, 0)));
    }

    #[test]
    fn test_lun_parsing_invalid() {
        let mut lun = [0u8; 8];
        lun[0] = 0; // Invalid: must be 1
        assert_eq!(parse_lun(&lun), None);
    }

    #[test]
    fn test_sense_data() {
        let sense = build_sense_data(sense_key::ILLEGAL_REQUEST, 0x20, 0x00);
        assert_eq!(sense[0], 0x70); // Fixed format, current errors
        assert_eq!(sense[2], sense_key::ILLEGAL_REQUEST);
        assert_eq!(sense[12], 0x20); // ASC: Invalid command operation code
        assert_eq!(sense[13], 0x00); // ASCQ
    }

    #[test]
    fn test_config_default() {
        let config = VirtioScsiConfig::new(4);
        assert_eq!(config.num_queues, 4);
        assert_eq!(config.seg_max, VIRTIO_SCSI_DEFAULT_SEG_MAX);
        assert_eq!(config.max_target, VIRTIO_SCSI_DEFAULT_MAX_TARGET);
    }
}
