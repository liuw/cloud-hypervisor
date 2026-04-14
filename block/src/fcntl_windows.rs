// SPDX-License-Identifier: Apache-2.0
//
// Windows stubs for file locking types.
// File locking is not implemented on Windows yet.

use std::io;
use std::str::FromStr;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum LockError {
    #[error("The file is already locked")]
    AlreadyLocked,
    #[error("Setting file lock failed")]
    Io(#[source] io::Error),
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
    ByteRange(u64, u64),
}

#[derive(Debug, Clone, Copy)]
pub enum LockState {
    Unlocked,
    Read,
    Write,
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

pub fn try_acquire_lock<F>(
    _file: &F, _lock_type: LockType, _granularity: LockGranularity,
) -> Result<(), LockError> {
    Ok(())
}

pub fn clear_lock<F>(
    _file: &F, _granularity: LockGranularity,
) -> Result<(), LockError> {
    Ok(())
}

pub fn get_lock_state<F>(
    _file: &F, _granularity: LockGranularity,
) -> Result<LockState, LockError> {
    Ok(LockState::Unlocked)
}
