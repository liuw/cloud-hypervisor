// Copyright © 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! VHDX adapter.

use std::fs::File;
use std::path::Path;

use block::disk_file::AsyncFullDiskFile;
use block::error::BlockResult;
use block::formats::vhdx::VhdxDisk;

use crate::disk_engine::format::{DiskFormat, OpenConfig};

/// Dynamic VHDX images, as opened by [`VhdxDisk`].
///
/// There is no template image: building a valid VHDX means writing the file
/// identifier, both headers with their checksums, the region table, the BAT
/// and a metadata region, and the `block` crate has no writer for any of
/// that. The parser is fuzzed through the image target instead.
pub struct Vhdx;

impl DiskFormat for Vhdx {
    const NAME: &'static str = "vhdx";

    // VHDX places its BAT region at 2 MiB and its metadata region at 3 MiB,
    // so a parseable image is several MiB before it holds any data: an empty
    // one is exactly 8 MiB and a populated one runs to 9 or 10 MiB. The
    // default budget would reject those.
    const MAX_IMAGE_LEN: usize = 16 << 20;

    // A VHDX read is all or error. `io::read` loops until every requested
    // sector is accounted for and returns early with an error on any entry
    // it cannot serve, so the `Ok(read_count)` it reaches
    // (block/src/formats/vhdx/io.rs:141) always covers the whole request; an
    // unallocated block is left as the zeroes the buffer already holds. A
    // length that is not a multiple of the sector size is rejected up front
    // (block/src/formats/vhdx/parser.rs:102), which is a rejection rather
    // than a short read.
    //
    // The check is on reads only, but the write path is built the same way:
    // it serves each sector run with `write_all_at` and propagates any
    // failure with `?` (block/src/formats/vhdx/io.rs:202 and 213), so it too
    // returns either the full count or an error.
    //
    // The capacity is not file backed, so `CAPACITY_FILE_TAIL` stays
    // `None`: a dynamic VHDX allocates payload blocks on demand, so its file
    // is routinely smaller than the virtual disk size.
    const NO_SHORT_READS: bool = true;

    fn open(
        file: File,
        _path: Option<&Path>,
        config: &OpenConfig,
    ) -> BlockResult<Box<dyn AsyncFullDiskFile>> {
        let disk = VhdxDisk::new(file, config.direct)?;
        Ok(Box::new(disk))
    }
}
