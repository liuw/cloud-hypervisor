// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! SCSI target and LUN abstraction.
//!
//! This module provides abstractions for SCSI targets and logical units (LUNs),
//! which are the addressable entities in a SCSI subsystem.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during SCSI target/LUN operations.
#[derive(Error, Debug)]
pub enum ScsiTargetError {
    #[error("Invalid target ID: {0}")]
    InvalidTargetId(u8),
    #[error("Invalid LUN: {0}")]
    InvalidLun(u16),
    #[error("Target not found: {0}")]
    TargetNotFound(u8),
    #[error("LUN not found: target={0}, lun={1}")]
    LunNotFound(u8, u16),
    #[error("Maximum targets exceeded")]
    MaxTargetsExceeded,
    #[error("Maximum LUNs exceeded")]
    MaxLunsExceeded,
}

/// Result type for SCSI target operations.
pub type ScsiTargetResult<T> = std::result::Result<T, ScsiTargetError>;

/// SCSI device type codes as defined in SPC specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ScsiDeviceType {
    /// Direct access block device (e.g., disk)
    DirectAccess = 0x00,
    /// Sequential access device (e.g., tape)
    SequentialAccess = 0x01,
    /// Printer device
    Printer = 0x02,
    /// Processor device
    Processor = 0x03,
    /// Write-once device
    WriteOnce = 0x04,
    /// CD/DVD device
    CdDvd = 0x05,
    /// Scanner device
    Scanner = 0x06,
    /// Optical memory device
    OpticalMemory = 0x07,
    /// Medium changer device
    MediumChanger = 0x08,
    /// Communications device
    Communications = 0x09,
    /// Storage array controller
    StorageArrayController = 0x0C,
    /// Enclosure services device
    EnclosureServices = 0x0D,
    /// Simplified direct access device
    SimplifiedDirectAccess = 0x0E,
    /// Optical card reader/writer
    OpticalCardRW = 0x0F,
    /// Bridge controller
    BridgeController = 0x10,
    /// Object-based storage device
    ObjectBasedStorage = 0x11,
    /// Automation/drive interface
    AutomationDriveInterface = 0x12,
    /// Well known logical unit
    WellKnownLu = 0x1E,
    /// Unknown or no device type
    Unknown = 0x1F,
}

impl Default for ScsiDeviceType {
    fn default() -> Self {
        ScsiDeviceType::DirectAccess
    }
}

impl From<u8> for ScsiDeviceType {
    fn from(val: u8) -> Self {
        match val {
            0x00 => ScsiDeviceType::DirectAccess,
            0x01 => ScsiDeviceType::SequentialAccess,
            0x02 => ScsiDeviceType::Printer,
            0x03 => ScsiDeviceType::Processor,
            0x04 => ScsiDeviceType::WriteOnce,
            0x05 => ScsiDeviceType::CdDvd,
            0x06 => ScsiDeviceType::Scanner,
            0x07 => ScsiDeviceType::OpticalMemory,
            0x08 => ScsiDeviceType::MediumChanger,
            0x09 => ScsiDeviceType::Communications,
            0x0C => ScsiDeviceType::StorageArrayController,
            0x0D => ScsiDeviceType::EnclosureServices,
            0x0E => ScsiDeviceType::SimplifiedDirectAccess,
            0x0F => ScsiDeviceType::OpticalCardRW,
            0x10 => ScsiDeviceType::BridgeController,
            0x11 => ScsiDeviceType::ObjectBasedStorage,
            0x12 => ScsiDeviceType::AutomationDriveInterface,
            0x1E => ScsiDeviceType::WellKnownLu,
            _ => ScsiDeviceType::Unknown,
        }
    }
}

/// Unique identifier for a SCSI LUN within a controller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScsiLunId {
    /// Target ID (0-255)
    pub target: u8,
    /// LUN number (0-16383 for virtio-scsi)
    pub lun: u16,
}

impl ScsiLunId {
    /// Create a new LUN identifier.
    pub fn new(target: u8, lun: u16) -> Self {
        ScsiLunId { target, lun }
    }
}

impl fmt::Display for ScsiLunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.target, self.lun)
    }
}

/// Configuration for a SCSI LUN backed by a file/disk image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScsiLunConfig {
    /// Target ID
    pub target: u8,
    /// LUN number
    pub lun: u16,
    /// Path to the disk image
    pub path: PathBuf,
    /// Read-only flag
    pub readonly: bool,
    /// Use direct I/O
    pub direct: bool,
    /// Device type
    pub device_type: ScsiDeviceType,
    /// Vendor identification (8 chars, space-padded)
    pub vendor_id: String,
    /// Product identification (16 chars, space-padded)
    pub product_id: String,
    /// Product revision (4 chars, space-padded)
    pub product_rev: String,
}

impl Default for ScsiLunConfig {
    fn default() -> Self {
        ScsiLunConfig {
            target: 0,
            lun: 0,
            path: PathBuf::new(),
            readonly: false,
            direct: false,
            device_type: ScsiDeviceType::DirectAccess,
            vendor_id: "CLOUD-HV".to_string(),
            product_id: "VIRTIO-SCSI".to_string(),
            product_rev: "0001".to_string(),
        }
    }
}

impl ScsiLunConfig {
    /// Create a new LUN configuration.
    pub fn new(target: u8, lun: u16, path: PathBuf) -> Self {
        ScsiLunConfig {
            target,
            lun,
            path,
            ..Default::default()
        }
    }

    /// Get the LUN identifier.
    pub fn id(&self) -> ScsiLunId {
        ScsiLunId::new(self.target, self.lun)
    }

    /// Get the vendor ID as a fixed-length byte array.
    pub fn vendor_id_bytes(&self) -> [u8; 8] {
        let mut bytes = [b' '; 8];
        let len = std::cmp::min(self.vendor_id.len(), 8);
        bytes[..len].copy_from_slice(&self.vendor_id.as_bytes()[..len]);
        bytes
    }

    /// Get the product ID as a fixed-length byte array.
    pub fn product_id_bytes(&self) -> [u8; 16] {
        let mut bytes = [b' '; 16];
        let len = std::cmp::min(self.product_id.len(), 16);
        bytes[..len].copy_from_slice(&self.product_id.as_bytes()[..len]);
        bytes
    }

    /// Get the product revision as a fixed-length byte array.
    pub fn product_rev_bytes(&self) -> [u8; 4] {
        let mut bytes = [b' '; 4];
        let len = std::cmp::min(self.product_rev.len(), 4);
        bytes[..len].copy_from_slice(&self.product_rev.as_bytes()[..len]);
        bytes
    }
}

/// Information about a SCSI target (a collection of LUNs).
#[derive(Debug, Clone, Default)]
pub struct ScsiTargetInfo {
    /// Target ID
    pub id: u8,
    /// Number of LUNs attached to this target
    pub lun_count: usize,
}

/// State of a SCSI LUN for serialization/migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScsiLunState {
    /// LUN identifier
    pub id: ScsiLunId,
    /// Whether the LUN is online
    pub online: bool,
    /// Persistent reservation state for this LUN.
    #[serde(default)]
    pub persistent_reservation: ScsiPersistentReservationState,
    /// Unit attention condition pending
    pub unit_attention: bool,
    /// Power condition
    pub power_condition: u8,
}

/// Active persistent reservation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScsiPersistentReservation {
    /// Registered reservation key that owns the reservation.
    pub key: u64,
    /// SCSI persistent reservation type.
    pub reservation_type: u8,
}

/// Per-LUN persistent reservation state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScsiPersistentReservationState {
    /// Generation counter returned by PR IN commands.
    pub generation: u32,
    /// Registered keys. virtio-scsi currently exposes one local initiator.
    pub registered_keys: Vec<u64>,
    /// Current active reservation, if any.
    pub reservation: Option<ScsiPersistentReservation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lun_id_display() {
        let id = ScsiLunId::new(5, 10);
        assert_eq!(format!("{}", id), "5:10");
    }

    #[test]
    fn test_device_type_from_u8() {
        assert_eq!(ScsiDeviceType::from(0x00), ScsiDeviceType::DirectAccess);
        assert_eq!(ScsiDeviceType::from(0x05), ScsiDeviceType::CdDvd);
        assert_eq!(ScsiDeviceType::from(0xFF), ScsiDeviceType::Unknown);
    }

    #[test]
    fn test_lun_config_vendor_bytes() {
        let config = ScsiLunConfig::default();
        let vendor = config.vendor_id_bytes();
        assert_eq!(&vendor, b"CLOUD-HV");
    }

    #[test]
    fn test_lun_config_product_bytes() {
        let config = ScsiLunConfig::default();
        let product = config.product_id_bytes();
        assert_eq!(&product[..11], b"VIRTIO-SCSI");
        assert_eq!(product[11], b' '); // Space-padded
    }

    #[test]
    fn test_lun_config_new() {
        let path = PathBuf::from("/path/to/disk.img");
        let config = ScsiLunConfig::new(1, 5, path.clone());
        assert_eq!(config.target, 1);
        assert_eq!(config.lun, 5);
        assert_eq!(config.path, path);
        assert!(!config.readonly);
    }
}
