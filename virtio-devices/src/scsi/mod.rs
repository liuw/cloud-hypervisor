// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Virtio SCSI device implementation.

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
