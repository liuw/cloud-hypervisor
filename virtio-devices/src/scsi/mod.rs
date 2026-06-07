// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Virtio SCSI device implementation.
//!
//! This module implements a virtio-scsi host bus adapter (HBA) device as specified
//! in the VIRTIO specification (Section 5.6). The virtio-scsi device provides a
//! SCSI transport layer, allowing VMs to access SCSI devices with full SCSI
//! semantics including multiple LUNs, SCSI commands passthrough, and advanced
//! features like persistent reservations.

mod commands;
mod device;
mod handler;
mod protocol;
mod target;
#[cfg(test)]
mod tests;
pub use commands::{DiskOps, ScsiCommandError, ScsiCommandProcessor, ScsiCommandResult};
pub use device::{Error, Scsi, ScsiState};
pub use handler::{ScsiCtrlHandler, ScsiDisk, ScsiEventHandler, ScsiRequestHandler};
pub use protocol::*;
pub use target::*;
