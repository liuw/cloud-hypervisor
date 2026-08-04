// Copyright © 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Fixed VHD adapter.

use std::fs::File;
use std::path::Path;

use block::disk_file::AsyncFullDiskFile;
use block::error::BlockResult;
use block::formats::vhd::VhdDisk;

use crate::disk_engine::format::{DiskFormat, OpenConfig};

/// Fixed VHD images, as opened by [`VhdDisk`].
///
/// The io_uring backend is not selected: the `block` crate only builds it
/// with its `io_uring` feature, which the fuzz crate does not enable, and a
/// kernel ring per iteration would dominate the run time anyway.
///
/// There is no template image because the `block` crate parses a VHD footer
/// but never writes one, and a footer built inside the fuzzer would encode
/// the harness author's reading of the format rather than the code under
/// test. The parser is fuzzed through the image target instead.
pub struct Vhd;

impl DiskFormat for Vhd {
    const NAME: &'static str = "vhd";

    // A fixed VHD is the disk data followed by the 512 byte hard disk
    // footer, so the capacity the footer states cannot exceed the file
    // length less that footer. `FixedVhd::new` rejects an image where it
    // does (block/src/formats/vhd/fixed.rs:27), and this is the harness
    // side guard on that check.
    const CAPACITY_FILE_TAIL: Option<u64> = Some(512);

    // Reads go through `RawSync`, which serves them with a single `preadv`
    // on the image file (block/src/formats/raw/engine_sync.rs:55), after
    // `FixedVhdSync` has bounded the request by the capacity
    // (block/src/formats/vhd/engine_sync.rs:35). A `preadv` only comes up
    // short at end of file, and the capacity check above keeps every byte
    // below the capacity inside the file, so it cannot.
    const NO_SHORT_READS: bool = true;

    fn open(
        file: File,
        _path: Option<&Path>,
        config: &OpenConfig,
    ) -> BlockResult<Box<dyn AsyncFullDiskFile>> {
        let disk = VhdDisk::new(file, false, config.direct)?;
        Ok(Box::new(disk))
    }
}
