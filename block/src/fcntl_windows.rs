// SPDX-License-Identifier: Apache-2.0
//
// Windows stubs for file locking types.
// File locking is not implemented on Windows yet.

use std::fmt::Debug;
use std::io;
use std::str::FromStr;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum LockError {
    #[error("The file is already locked")]
    AlreadyLocked,
    #[error("Setting file lock failed")]
    SetLock(#[source] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    Read,
    Write,
    Unlock,
}

#[derive(Debug, Clone, Copy)]
pub enum LockGranularity {
    WholeFile,
    ByteRange { start: u64, len: u64 },
}

// Needed for pattern matching in block.rs
impl LockGranularity {
    pub fn byte_range(start: u64, len: u64) -> Self {
        Self::ByteRange { start, len }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LockGranularityChoice {
    Full,
    ByteRange,
}

impl Default for LockGranularityChoice {
    fn default() -> Self {
        Self::Full
    }
}

impl FromStr for LockGranularityChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "byte-range" | "byterange" => Ok(Self::ByteRange),
            _ => Err(format!("Unknown lock granularity: {s}")),
        }
    }
}

/// Stub: file locking not implemented on Windows.
pub fn get_lock_state(_file: &std::fs::File) -> Result<Option<LockType>, LockError> {
    Ok(None)
}

/// Stub: acquiring file locks not implemented on Windows.
pub fn try_acquire_lock(
    _file: &std::fs::File,
    _lock_type: LockType,
    _granularity: LockGranularity,
) -> Result<(), LockError> {
    Ok(())
}

/// Stub: clearing file locks not implemented on Windows.
pub fn clear_lock(
    _file: &std::fs::File,
    _granularity: LockGranularity,
) -> Result<(), LockError> {
    Ok(())
}
