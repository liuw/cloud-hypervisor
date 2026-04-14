// Copyright © 2024 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//
// Windows raw synchronous block I/O backend.
//
// Implements the AsyncIo trait using synchronous ReadFile/WriteFile,
// mirroring the Unix raw_sync.rs pattern. Completions are queued
// immediately and signaled via EventFd.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::io::AsRawHandle;

use log::warn;
use platform::EventFd;

use crate::async_io::{AsyncIo, AsyncIoError, AsyncIoResult, DiskFileError};
use crate::error::{BlockError, BlockErrorKind, BlockResult};
use crate::{DiskTopology, SECTOR_SIZE, disk_file, probe_sparse_support, query_device_size};

#[derive(Debug)]
pub struct RawFileDiskSync {
    file: File,
}

impl RawFileDiskSync {
    pub fn new(file: File) -> Self {
        RawFileDiskSync { file }
    }
}

impl disk_file::DiskSize for RawFileDiskSync {
    fn logical_size(&self) -> BlockResult<u64> {
        query_device_size(&self.file)
            .map(|(logical_size, _)| logical_size)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::Size(e)))
    }
}

impl disk_file::PhysicalSize for RawFileDiskSync {
    fn physical_size(&self) -> BlockResult<u64> {
        query_device_size(&self.file)
            .map(|(_, physical_size)| physical_size)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::Size(e)))
    }
}

impl disk_file::Geometry for RawFileDiskSync {
    fn topology(&self) -> DiskTopology {
        DiskTopology::probe(&self.file).unwrap_or_else(|_| {
            warn!("Unable to get device topology. Using default topology");
            DiskTopology::default()
        })
    }
}

impl disk_file::SparseCapable for RawFileDiskSync {
    fn supports_sparse_operations(&self) -> bool {
        probe_sparse_support(&self.file)
    }
}

impl disk_file::Resizable for RawFileDiskSync {
    fn resize(&mut self, size: u64) -> BlockResult<()> {
        self.file
            .set_len(size)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::ResizeError(e)))
    }
}

impl disk_file::DiskFile for RawFileDiskSync {}

impl disk_file::AsyncDiskFile for RawFileDiskSync {
    fn try_clone(&self) -> BlockResult<Box<dyn disk_file::AsyncDiskFile>> {
        let file = self
            .file
            .try_clone()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::Clone(e)))?;
        Ok(Box::new(RawFileDiskSync { file }))
    }

    fn new_async_io(&self, _ring_depth: u32) -> BlockResult<Box<dyn AsyncIo>> {
        let file = self
            .file
            .try_clone()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::NewAsyncIo(e)))?;
        Ok(Box::new(RawFileSync::new(file)))
    }
}

/// Synchronous I/O backend for Windows.
///
/// Reads and writes are performed synchronously using Seek + Read/Write.
/// Completions are queued immediately and signaled via EventFd.
pub struct RawFileSync {
    file: File,
    eventfd: EventFd,
    completion_list: VecDeque<(u64, i32)>,
}

impl RawFileSync {
    pub fn new(file: File) -> Self {
        RawFileSync {
            file,
            eventfd: EventFd::new(platform::EFD_NONBLOCK)
                .expect("Failed creating EventFd for RawFile"),
            completion_list: VecDeque::new(),
        }
    }
}

impl AsyncIo for RawFileSync {
    fn notifier(&self) -> &EventFd {
        &self.eventfd
    }

    fn alignment(&self) -> u64 {
        SECTOR_SIZE
    }

    fn read_vectored(
        &mut self,
        offset: i64,
        bufs: &[(u64, u64)],
        user_data: u64,
    ) -> AsyncIoResult<()> {
        self.file
            .seek(SeekFrom::Start(offset as u64))
            .map_err(AsyncIoError::ReadVectored)?;

        let mut total = 0i32;
        for &(ptr, len) in bufs {
            // SAFETY: ptr points to valid guest memory of length len.
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
            let n = self.file.read(buf).map_err(AsyncIoError::ReadVectored)?;
            total += n as i32;
        }

        self.completion_list.push_back((user_data, total));
        self.eventfd.write(1).unwrap();
        Ok(())
    }

    fn write_vectored(
        &mut self,
        offset: i64,
        bufs: &[(u64, u64)],
        user_data: u64,
    ) -> AsyncIoResult<()> {
        self.file
            .seek(SeekFrom::Start(offset as u64))
            .map_err(AsyncIoError::WriteVectored)?;

        let mut total = 0i32;
        for &(ptr, len) in bufs {
            // SAFETY: ptr points to valid guest memory of length len.
            let buf = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
            let n = self.file.write(buf).map_err(AsyncIoError::WriteVectored)?;
            total += n as i32;
        }

        self.completion_list.push_back((user_data, total));
        self.eventfd.write(1).unwrap();
        Ok(())
    }

    fn fsync(&mut self, user_data: Option<u64>) -> AsyncIoResult<()> {
        self.file.sync_all().map_err(AsyncIoError::Fsync)?;

        if let Some(user_data) = user_data {
            self.completion_list.push_back((user_data, 0));
            self.eventfd.write(1).unwrap();
        }
        Ok(())
    }

    fn next_completed_request(&mut self) -> Option<(u64, i32)> {
        self.completion_list.pop_front()
    }

    fn punch_hole(&mut self, _offset: u64, _length: u64, user_data: u64) -> AsyncIoResult<()> {
        // Not supported on Windows yet
        self.completion_list.push_back((user_data, 0));
        self.eventfd.write(1).unwrap();
        Ok(())
    }

    fn write_zeroes(&mut self, offset: u64, length: u64, user_data: u64) -> AsyncIoResult<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(AsyncIoError::WriteZeroes)?;

        let zeros = vec![0u8; length.min(1 << 20) as usize]; // Max 1 MiB at a time
        let mut remaining = length;
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64) as usize;
            self.file
                .write_all(&zeros[..chunk])
                .map_err(AsyncIoError::WriteZeroes)?;
            remaining -= chunk as u64;
        }

        self.completion_list.push_back((user_data, 0));
        self.eventfd.write(1).unwrap();
        Ok(())
    }
}
