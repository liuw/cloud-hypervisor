// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! SCSI command processing.
//!
//! This module implements SCSI command parsing and execution for the
//! virtio-scsi device.

use std::io::{Read, Seek, SeekFrom, Write};

use log::{debug, error, warn};
use thiserror::Error;

use super::protocol::*;
use super::target::ScsiLunConfig;

/// Combined trait for disk operations (Read + Write + Seek).
/// This is needed because Rust doesn't allow multiple non-auto traits in dyn objects.
pub trait DiskOps: Read + Write + Seek {}

/// Blanket implementation for any type that implements Read + Write + Seek.
impl<T: Read + Write + Seek> DiskOps for T {}

/// Errors that can occur during SCSI command processing.
#[derive(Error, Debug)]
pub enum ScsiCommandError {
    #[error("Invalid CDB format")]
    InvalidCdb,
    #[error("Invalid LUN: {0:?}")]
    InvalidLun([u8; 8]),
    #[error("LUN not found: target={0}, lun={1}")]
    LunNotFound(u8, u16),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Guest memory error: {0}")]
    GuestMemoryError(#[from] vm_memory::GuestMemoryError),
    #[error("Unsupported command: opcode={0:#04x}")]
    UnsupportedCommand(u8),
    #[error("Medium not present")]
    MediumNotPresent,
    #[error("Write protected")]
    WriteProtected,
    #[error("Invalid field in CDB")]
    InvalidFieldInCdb,
    #[error("Logical block address out of range")]
    LbaOutOfRange,
}

/// Result of SCSI command execution.
pub struct ScsiCommandResult {
    /// Virtio SCSI response code
    pub response: u8,
    /// SCSI status byte
    pub status: u8,
    /// Data transferred to guest (for read operations)
    pub data_in: Vec<u8>,
    /// Sense data (if status is CHECK CONDITION)
    pub sense: Vec<u8>,
    /// Residual byte count
    pub resid: u32,
}

impl Default for ScsiCommandResult {
    fn default() -> Self {
        ScsiCommandResult {
            response: VIRTIO_SCSI_S_OK,
            status: scsi_status::GOOD,
            data_in: Vec::new(),
            sense: Vec::new(),
            resid: 0,
        }
    }
}

impl ScsiCommandResult {
    /// Create a successful result with data.
    pub fn ok_with_data(data: Vec<u8>) -> Self {
        ScsiCommandResult {
            response: VIRTIO_SCSI_S_OK,
            status: scsi_status::GOOD,
            data_in: data,
            sense: Vec::new(),
            resid: 0,
        }
    }

    /// Create an error result with sense data.
    pub fn check_condition(sense_key: u8, asc: u8, ascq: u8) -> Self {
        let sense = build_sense_data(sense_key, asc, ascq);
        ScsiCommandResult {
            response: VIRTIO_SCSI_S_OK,
            status: scsi_status::CHECK_CONDITION,
            data_in: Vec::new(),
            sense: sense.to_vec(),
            resid: 0,
        }
    }

    /// Create a bad target response.
    pub fn bad_target() -> Self {
        ScsiCommandResult {
            response: VIRTIO_SCSI_S_BAD_TARGET,
            status: 0,
            data_in: Vec::new(),
            sense: Vec::new(),
            resid: 0,
        }
    }

    /// Create a response for an incorrect LUN.
    ///
    /// For request queues, VIRTIO_SCSI_S_INCORRECT_LUN is not a valid response.
    /// Instead, we return VIRTIO_SCSI_S_OK with a CHECK CONDITION status and
    /// sense data indicating "LOGICAL UNIT NOT SUPPORTED" (ASC 0x25, ASCQ 0x00).
    pub fn incorrect_lun() -> Self {
        // Sense key: ILLEGAL_REQUEST (0x05)
        // ASC: 0x25 (LOGICAL UNIT NOT SUPPORTED)
        // ASCQ: 0x00
        let sense = build_sense_data(sense_key::ILLEGAL_REQUEST, 0x25, 0x00);
        ScsiCommandResult {
            response: VIRTIO_SCSI_S_OK,
            status: scsi_status::CHECK_CONDITION,
            data_in: Vec::new(),
            sense: sense.to_vec(),
            resid: 0,
        }
    }
}

/// SCSI command processor for a single LUN.
pub struct ScsiCommandProcessor {
    /// LUN configuration
    config: ScsiLunConfig,
    /// Disk size in bytes
    #[allow(dead_code)]
    disk_size: u64,
    /// Block size (typically 512)
    block_size: u32,
    /// Number of blocks
    num_blocks: u64,
    /// Whether the LUN is ready
    ready: bool,
}

impl ScsiCommandProcessor {
    /// Create a new command processor for a LUN.
    pub fn new(config: ScsiLunConfig, disk_size: u64, block_size: u32) -> Self {
        let num_blocks = disk_size / block_size as u64;
        ScsiCommandProcessor {
            config,
            disk_size,
            block_size,
            num_blocks,
            ready: true,
        }
    }

    /// Get the LUN configuration.
    pub fn config(&self) -> &ScsiLunConfig {
        &self.config
    }

    /// Check if the LUN is read-only.
    pub fn is_readonly(&self) -> bool {
        self.config.readonly
    }

    /// Process a SCSI command.
    ///
    /// # Arguments
    /// * `cdb` - Command Descriptor Block
    /// * `data_out` - Data from guest (for write commands)
    /// * `allocation_length` - Maximum data to return
    /// * `disk` - The disk file to operate on
    ///
    /// # Returns
    /// The result of the command execution.
    pub fn process_command(
        &mut self,
        cdb: &[u8],
        data_out: &[u8],
        allocation_length: u32,
        disk: &mut dyn DiskOps,
    ) -> ScsiCommandResult {
        if cdb.is_empty() {
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x20, // Invalid command operation code
                0x00,
            );
        }

        let opcode = cdb[0];
        debug!("Processing SCSI command: opcode={:#04x}", opcode);

        match opcode {
            scsi_opcode::TEST_UNIT_READY => self.cmd_test_unit_ready(),
            scsi_opcode::REQUEST_SENSE => self.cmd_request_sense(cdb, allocation_length),
            scsi_opcode::INQUIRY => self.cmd_inquiry(cdb, allocation_length),
            scsi_opcode::MODE_SENSE_6 => self.cmd_mode_sense_6(cdb, allocation_length),
            scsi_opcode::MODE_SENSE_10 => self.cmd_mode_sense_10(cdb, allocation_length),
            scsi_opcode::READ_CAPACITY_10 => self.cmd_read_capacity_10(),
            scsi_opcode::SERVICE_ACTION_IN_16 => {
                self.cmd_service_action_in_16(cdb, allocation_length)
            }
            scsi_opcode::READ_10 => self.cmd_read_10(cdb, allocation_length, disk),
            scsi_opcode::READ_16 => self.cmd_read_16(cdb, allocation_length, disk),
            scsi_opcode::WRITE_10 => self.cmd_write_10(cdb, data_out, disk),
            scsi_opcode::WRITE_16 => self.cmd_write_16(cdb, data_out, disk),
            scsi_opcode::SYNCHRONIZE_CACHE_10 | scsi_opcode::SYNCHRONIZE_CACHE_16 => {
                self.cmd_synchronize_cache(disk)
            }
            scsi_opcode::START_STOP_UNIT => self.cmd_start_stop_unit(cdb),
            scsi_opcode::REPORT_LUNS => self.cmd_report_luns(cdb, allocation_length),
            scsi_opcode::PREVENT_ALLOW_MEDIUM_REMOVAL => self.cmd_prevent_allow_medium_removal(cdb),
            _ => {
                warn!("Unsupported SCSI command: opcode={:#04x}", opcode);
                ScsiCommandResult::check_condition(
                    sense_key::ILLEGAL_REQUEST,
                    0x20, // Invalid command operation code
                    0x00,
                )
            }
        }
    }

    /// TEST UNIT READY command
    fn cmd_test_unit_ready(&self) -> ScsiCommandResult {
        if self.ready {
            ScsiCommandResult::default()
        } else {
            ScsiCommandResult::check_condition(
                sense_key::NOT_READY,
                0x04, // Logical unit not ready
                0x02, // Initializing command required
            )
        }
    }

    /// REQUEST SENSE command
    fn cmd_request_sense(&self, cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        let desc = (cdb[1] & 0x01) != 0;
        if desc {
            // Descriptor format not supported
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x24, // Invalid field in CDB
                0x00,
            );
        }

        // Return "no sense" data
        let sense = build_sense_data(sense_key::NO_SENSE, 0x00, 0x00);
        let len = std::cmp::min(sense.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(sense[..len].to_vec())
    }

    /// INQUIRY command
    fn cmd_inquiry(&self, cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        let evpd = (cdb[1] & 0x01) != 0;
        let page_code = cdb[2];

        if evpd {
            // VPD pages
            match page_code {
                0x00 => self.inquiry_vpd_supported_pages(allocation_length),
                0x80 => self.inquiry_vpd_unit_serial_number(allocation_length),
                0x83 => self.inquiry_vpd_device_identification(allocation_length),
                0xB0 => self.inquiry_vpd_block_limits(allocation_length),
                0xB2 => self.inquiry_vpd_logical_block_provisioning(allocation_length),
                _ => ScsiCommandResult::check_condition(
                    sense_key::ILLEGAL_REQUEST,
                    0x24, // Invalid field in CDB
                    0x00,
                ),
            }
        } else if page_code != 0 {
            ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x24, // Invalid field in CDB
                0x00,
            )
        } else {
            self.inquiry_standard(allocation_length)
        }
    }

    /// Standard INQUIRY response
    fn inquiry_standard(&self, allocation_length: u32) -> ScsiCommandResult {
        let mut data = vec![0u8; 96];

        // Peripheral qualifier (0) | Peripheral device type
        data[0] = self.config.device_type as u8;
        // RMB (removable media bit) = 0
        data[1] = 0x00;
        // Version: SPC-4
        data[2] = 0x06;
        // Response data format: 2, HiSup=1
        data[3] = 0x12;
        // Additional length
        data[4] = 91; // 96 - 5
        // SCCS, ACC, TPGS, 3PC, Protect
        data[5] = 0x00;
        // EncServ, VS, MultiP, Addr16
        data[6] = 0x00;
        // WBus16, Sync, CmdQue, VS
        data[7] = 0x02; // CmdQue=1 (command queuing supported)

        // Vendor identification (8 bytes)
        let vendor = self.config.vendor_id_bytes();
        data[8..16].copy_from_slice(&vendor);

        // Product identification (16 bytes)
        let product = self.config.product_id_bytes();
        data[16..32].copy_from_slice(&product);

        // Product revision level (4 bytes)
        let revision = self.config.product_rev_bytes();
        data[32..36].copy_from_slice(&revision);

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// VPD page 0x00: Supported VPD Pages
    fn inquiry_vpd_supported_pages(&self, allocation_length: u32) -> ScsiCommandResult {
        let mut data = vec![0u8; 8];
        data[0] = self.config.device_type as u8;
        data[1] = 0x00; // Page code
        data[3] = 4; // Page length
        data[4] = 0x00; // Supported VPD Pages
        data[5] = 0x80; // Unit Serial Number
        data[6] = 0x83; // Device Identification
        data[7] = 0xB0; // Block Limits

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// VPD page 0x80: Unit Serial Number
    fn inquiry_vpd_unit_serial_number(&self, allocation_length: u32) -> ScsiCommandResult {
        let serial = b"CLOUD-HV-SCSI001";
        let mut data = vec![0u8; 4 + serial.len()];
        data[0] = self.config.device_type as u8;
        data[1] = 0x80; // Page code
        data[3] = serial.len() as u8;
        data[4..].copy_from_slice(serial);

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// VPD page 0x83: Device Identification
    fn inquiry_vpd_device_identification(&self, allocation_length: u32) -> ScsiCommandResult {
        // Simple NAA identifier
        let mut data = vec![0u8; 16];
        data[0] = self.config.device_type as u8;
        data[1] = 0x83; // Page code
        data[3] = 12; // Page length

        // Designation descriptor
        data[4] = 0x01; // Protocol identifier (0) | Code set (binary)
        data[5] = 0x03; // PIV=0 | Association (LUN) | Designator type (NAA)
        data[7] = 8; // Designator length

        // NAA 5 identifier (8 bytes)
        data[8] = 0x50; // NAA (5) in high nibble
        // Rest is a pseudo-unique identifier
        data[9] = 0x01;
        data[10] = 0x02;
        data[11] = 0x03;
        data[12] = self.config.target;
        data[13] = (self.config.lun >> 8) as u8;
        data[14] = self.config.lun as u8;
        data[15] = 0x00;

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// VPD page 0xB0: Block Limits
    fn inquiry_vpd_block_limits(&self, allocation_length: u32) -> ScsiCommandResult {
        let mut data = vec![0u8; 64];
        data[0] = self.config.device_type as u8;
        data[1] = 0xB0; // Page code
        data[2] = 0x00;
        data[3] = 60; // Page length

        // Maximum transfer length (in blocks) - 0 means no limit reported
        // Optimal transfer length (in blocks)
        let optimal_transfer = 128u32; // 128 blocks = 64KB with 512-byte blocks
        data[12..16].copy_from_slice(&optimal_transfer.to_be_bytes());

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// VPD page 0xB2: Logical Block Provisioning
    fn inquiry_vpd_logical_block_provisioning(&self, allocation_length: u32) -> ScsiCommandResult {
        let mut data = vec![0u8; 8];
        data[0] = self.config.device_type as u8;
        data[1] = 0xB2; // Page code
        data[3] = 4; // Page length

        // Threshold exponent, LBPU, LBPWS, LBPRZ, ANC_SUP, DP
        data[4] = 0x00;
        data[5] = 0x00;
        data[6] = 0x00;
        data[7] = 0x00;

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// MODE SENSE (6) command
    fn cmd_mode_sense_6(&self, cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        let dbd = (cdb[1] & 0x08) != 0;
        let page_code = cdb[2] & 0x3F;
        let page_control = (cdb[2] >> 6) & 0x03;

        self.mode_sense_common(dbd, page_code, page_control, allocation_length, false)
    }

    /// MODE SENSE (10) command
    fn cmd_mode_sense_10(&self, cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        let dbd = (cdb[1] & 0x08) != 0;
        let page_code = cdb[2] & 0x3F;
        let page_control = (cdb[2] >> 6) & 0x03;

        self.mode_sense_common(dbd, page_code, page_control, allocation_length, true)
    }

    /// Common MODE SENSE implementation
    fn mode_sense_common(
        &self,
        dbd: bool,
        page_code: u8,
        _page_control: u8,
        allocation_length: u32,
        is_10: bool,
    ) -> ScsiCommandResult {
        let header_len = if is_10 { 8 } else { 4 };
        let block_desc_len = if dbd { 0 } else { 8 };

        let mut data = vec![0u8; header_len + block_desc_len + 12];

        if is_10 {
            // Mode data length (excluding itself)
            let len = (data.len() - 2) as u16;
            data[0] = (len >> 8) as u8;
            data[1] = len as u8;
            // Medium type
            data[2] = 0x00;
            // Device-specific parameter (WP bit if read-only)
            data[3] = if self.config.readonly { 0x80 } else { 0x00 };
            // Block descriptor length
            data[6] = (block_desc_len >> 8) as u8;
            data[7] = block_desc_len as u8;
        } else {
            // Mode data length (excluding itself)
            data[0] = (data.len() - 1) as u8;
            // Medium type
            data[1] = 0x00;
            // Device-specific parameter (WP bit if read-only)
            data[2] = if self.config.readonly { 0x80 } else { 0x00 };
            // Block descriptor length
            data[3] = block_desc_len as u8;
        }

        // Block descriptor (if not disabled)
        if !dbd {
            let offset = header_len;
            // Number of blocks (0 = all remaining)
            let num_blocks = if self.num_blocks > 0xFFFFFF {
                0xFFFFFF
            } else {
                self.num_blocks as u32
            };
            data[offset] = ((num_blocks >> 16) & 0xFF) as u8;
            data[offset + 1] = ((num_blocks >> 8) & 0xFF) as u8;
            data[offset + 2] = (num_blocks & 0xFF) as u8;
            // Block length
            data[offset + 4] = ((self.block_size >> 16) & 0xFF) as u8;
            data[offset + 5] = ((self.block_size >> 8) & 0xFF) as u8;
            data[offset + 6] = (self.block_size & 0xFF) as u8;
        }

        // Mode pages
        let page_offset = header_len + block_desc_len;
        match page_code {
            0x08 => {
                // Caching mode page
                data[page_offset] = 0x08; // Page code
                data[page_offset + 1] = 0x12; // Page length
                data[page_offset + 2] = 0x04; // WCE=1 (write cache enabled)
            }
            0x3F => {
                // Return all pages (just caching for now)
                data[page_offset] = 0x08;
                data[page_offset + 1] = 0x12;
                data[page_offset + 2] = 0x04;
            }
            _ => {
                // Unsupported page
                return ScsiCommandResult::check_condition(
                    sense_key::ILLEGAL_REQUEST,
                    0x24, // Invalid field in CDB
                    0x00,
                );
            }
        }

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// READ CAPACITY (10) command
    fn cmd_read_capacity_10(&self) -> ScsiCommandResult {
        let mut data = [0u8; 8];

        // Last LBA (saturate to 0xFFFFFFFF if larger)
        let last_lba = if self.num_blocks > 0 {
            std::cmp::min(self.num_blocks - 1, 0xFFFFFFFF) as u32
        } else {
            0
        };
        data[0..4].copy_from_slice(&last_lba.to_be_bytes());

        // Block size
        data[4..8].copy_from_slice(&self.block_size.to_be_bytes());

        ScsiCommandResult::ok_with_data(data.to_vec())
    }

    /// SERVICE ACTION IN (16) - READ CAPACITY (16)
    fn cmd_service_action_in_16(&self, cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        let service_action = cdb[1] & 0x1F;

        if service_action != scsi_opcode::SAI_READ_CAPACITY_16 {
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x20, // Invalid command operation code
                0x00,
            );
        }

        let mut data = vec![0u8; 32];

        // Last LBA (8 bytes)
        let last_lba = if self.num_blocks > 0 {
            self.num_blocks - 1
        } else {
            0
        };
        data[0..8].copy_from_slice(&last_lba.to_be_bytes());

        // Block size (4 bytes)
        data[8..12].copy_from_slice(&self.block_size.to_be_bytes());

        // RC BASIS, P_TYPE, PROT_EN
        data[12] = 0x00;
        // P_I_EXPONENT, LBPPBE
        data[13] = 0x00;
        // TPE, TPRZ, LALBA
        data[14] = 0x00;

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// READ (10) command
    fn cmd_read_10(
        &self,
        cdb: &[u8],
        allocation_length: u32,
        disk: &mut dyn DiskOps,
    ) -> ScsiCommandResult {
        if cdb.len() < 10 {
            return ScsiCommandResult::check_condition(sense_key::ILLEGAL_REQUEST, 0x24, 0x00);
        }

        let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
        let transfer_length = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;

        self.read_blocks(lba, transfer_length, allocation_length, disk)
    }

    /// READ (16) command
    fn cmd_read_16(
        &self,
        cdb: &[u8],
        allocation_length: u32,
        disk: &mut dyn DiskOps,
    ) -> ScsiCommandResult {
        if cdb.len() < 16 {
            return ScsiCommandResult::check_condition(sense_key::ILLEGAL_REQUEST, 0x24, 0x00);
        }

        let lba = u64::from_be_bytes([
            cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
        ]);
        let transfer_length = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);

        self.read_blocks(lba, transfer_length, allocation_length, disk)
    }

    /// Common read implementation
    fn read_blocks(
        &self,
        lba: u64,
        transfer_length: u32,
        _allocation_length: u32,
        disk: &mut dyn DiskOps,
    ) -> ScsiCommandResult {
        if transfer_length == 0 {
            return ScsiCommandResult::default();
        }

        // Check LBA range
        if lba >= self.num_blocks || lba + transfer_length as u64 > self.num_blocks {
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x21, // Logical block address out of range
                0x00,
            );
        }

        let byte_offset = lba * self.block_size as u64;
        let byte_length = transfer_length as usize * self.block_size as usize;

        if let Err(e) = disk.seek(SeekFrom::Start(byte_offset)) {
            error!("Failed to seek: {}", e);
            return ScsiCommandResult::check_condition(
                sense_key::MEDIUM_ERROR,
                0x11, // Unrecovered read error
                0x00,
            );
        }

        let mut data = vec![0u8; byte_length];
        match disk.read_exact(&mut data) {
            Ok(()) => ScsiCommandResult::ok_with_data(data),
            Err(e) => {
                error!("Failed to read: {}", e);
                ScsiCommandResult::check_condition(
                    sense_key::MEDIUM_ERROR,
                    0x11, // Unrecovered read error
                    0x00,
                )
            }
        }
    }

    /// WRITE (10) command
    fn cmd_write_10(&self, cdb: &[u8], data: &[u8], disk: &mut dyn DiskOps) -> ScsiCommandResult {
        if self.config.readonly {
            return ScsiCommandResult::check_condition(
                sense_key::DATA_PROTECT,
                0x27, // Write protected
                0x00,
            );
        }

        if cdb.len() < 10 {
            return ScsiCommandResult::check_condition(sense_key::ILLEGAL_REQUEST, 0x24, 0x00);
        }

        let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
        let transfer_length = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;

        self.write_blocks(lba, transfer_length, data, disk)
    }

    /// WRITE (16) command
    fn cmd_write_16(&self, cdb: &[u8], data: &[u8], disk: &mut dyn DiskOps) -> ScsiCommandResult {
        if self.config.readonly {
            return ScsiCommandResult::check_condition(
                sense_key::DATA_PROTECT,
                0x27, // Write protected
                0x00,
            );
        }

        if cdb.len() < 16 {
            return ScsiCommandResult::check_condition(sense_key::ILLEGAL_REQUEST, 0x24, 0x00);
        }

        let lba = u64::from_be_bytes([
            cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
        ]);
        let transfer_length = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);

        self.write_blocks(lba, transfer_length, data, disk)
    }

    /// Common write implementation
    fn write_blocks(
        &self,
        lba: u64,
        transfer_length: u32,
        data: &[u8],
        disk: &mut dyn DiskOps,
    ) -> ScsiCommandResult {
        if transfer_length == 0 {
            return ScsiCommandResult::default();
        }

        // Check LBA range
        if lba >= self.num_blocks || lba + transfer_length as u64 > self.num_blocks {
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x21, // Logical block address out of range
                0x00,
            );
        }

        let byte_offset = lba * self.block_size as u64;
        let expected_length = transfer_length as usize * self.block_size as usize;

        if data.len() < expected_length {
            warn!(
                "Write data too short: expected {}, got {}",
                expected_length,
                data.len()
            );
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x24, // Invalid field in CDB
                0x00,
            );
        }

        if let Err(e) = disk.seek(SeekFrom::Start(byte_offset)) {
            error!("Failed to seek: {}", e);
            return ScsiCommandResult::check_condition(
                sense_key::MEDIUM_ERROR,
                0x03, // Write fault
                0x00,
            );
        }

        match disk.write_all(&data[..expected_length]) {
            Ok(()) => ScsiCommandResult::default(),
            Err(e) => {
                error!("Failed to write: {}", e);
                ScsiCommandResult::check_condition(
                    sense_key::MEDIUM_ERROR,
                    0x03, // Write fault
                    0x00,
                )
            }
        }
    }

    /// SYNCHRONIZE CACHE command
    fn cmd_synchronize_cache(&self, disk: &mut dyn DiskOps) -> ScsiCommandResult {
        match disk.flush() {
            Ok(()) => ScsiCommandResult::default(),
            Err(e) => {
                error!("Failed to sync: {}", e);
                ScsiCommandResult::check_condition(
                    sense_key::MEDIUM_ERROR,
                    0x03, // Write fault
                    0x00,
                )
            }
        }
    }

    /// START STOP UNIT command
    fn cmd_start_stop_unit(&mut self, cdb: &[u8]) -> ScsiCommandResult {
        let start = (cdb[4] & 0x01) != 0;
        let loej = (cdb[4] & 0x02) != 0;

        if loej {
            // Load/eject not supported for virtual disks
            return ScsiCommandResult::check_condition(
                sense_key::ILLEGAL_REQUEST,
                0x24, // Invalid field in CDB
                0x00,
            );
        }

        self.ready = start;
        ScsiCommandResult::default()
    }

    /// REPORT LUNS command
    fn cmd_report_luns(&self, _cdb: &[u8], allocation_length: u32) -> ScsiCommandResult {
        // Report only this LUN
        let mut data = vec![0u8; 16];

        // LUN list length (8 bytes per LUN)
        data[0..4].copy_from_slice(&8u32.to_be_bytes());

        // LUN 0 in SAM-5 format
        data[8] = 0x00;
        data[9] = self.config.lun as u8;

        let len = std::cmp::min(data.len(), allocation_length as usize);
        ScsiCommandResult::ok_with_data(data[..len].to_vec())
    }

    /// PREVENT ALLOW MEDIUM REMOVAL command
    fn cmd_prevent_allow_medium_removal(&self, _cdb: &[u8]) -> ScsiCommandResult {
        // Always succeed (virtual disks can't be ejected)
        ScsiCommandResult::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::target::ScsiDeviceType;
    use super::*;
    use std::io::Cursor;

    fn create_test_processor() -> ScsiCommandProcessor {
        let config = ScsiLunConfig::default();
        ScsiCommandProcessor::new(config, 1024 * 1024, 512) // 1MB disk
    }

    #[test]
    fn test_test_unit_ready() {
        let processor = create_test_processor();
        let result = processor.cmd_test_unit_ready();
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::GOOD);
    }

    #[test]
    fn test_inquiry() {
        let processor = create_test_processor();
        let cdb = [scsi_opcode::INQUIRY, 0, 0, 0, 96, 0];
        let result = processor.cmd_inquiry(&cdb, 96);
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::GOOD);
        assert!(!result.data_in.is_empty());
        // Check device type
        assert_eq!(result.data_in[0], ScsiDeviceType::DirectAccess as u8);
    }

    #[test]
    fn test_read_capacity_10() {
        let processor = create_test_processor();
        let result = processor.cmd_read_capacity_10();
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.data_in.len(), 8);

        // Check last LBA (1MB / 512 - 1 = 2047)
        let last_lba = u32::from_be_bytes([
            result.data_in[0],
            result.data_in[1],
            result.data_in[2],
            result.data_in[3],
        ]);
        assert_eq!(last_lba, 2047);

        // Check block size
        let block_size = u32::from_be_bytes([
            result.data_in[4],
            result.data_in[5],
            result.data_in[6],
            result.data_in[7],
        ]);
        assert_eq!(block_size, 512);
    }

    #[test]
    fn test_read_write() {
        let mut processor = create_test_processor();
        let mut disk = Cursor::new(vec![0u8; 1024 * 1024]);

        // Write some data
        let write_data = vec![0xABu8; 512];
        let write_cdb = [scsi_opcode::WRITE_10, 0, 0, 0, 0, 1, 0, 0, 1, 0];
        processor.config.readonly = false;
        let result = processor.cmd_write_10(&write_cdb, &write_data, &mut disk);
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::GOOD);

        // Read it back
        let read_cdb = [scsi_opcode::READ_10, 0, 0, 0, 0, 1, 0, 0, 1, 0];
        let result = processor.cmd_read_10(&read_cdb, 512, &mut disk);
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.data_in, write_data);
    }

    #[test]
    fn test_unsupported_command() {
        let mut processor = create_test_processor();
        let mut disk = Cursor::new(vec![0u8; 1024]);

        let cdb = [0xFF, 0, 0, 0, 0, 0]; // Invalid opcode
        let result = processor.process_command(&cdb, &[], 0, &mut disk);
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::CHECK_CONDITION);
        assert!(!result.sense.is_empty());
    }
}
