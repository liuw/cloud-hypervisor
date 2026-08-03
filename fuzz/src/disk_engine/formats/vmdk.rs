// Copyright © 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Flat VMDK adapter.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use block::disk_file::AsyncFullDiskFile;
use block::error::{BlockError, BlockErrorKind, BlockResult};
use block::formats::vmdk::VmdkDisk;

use crate::disk_engine::format::{DiskFormat, OpenConfig};
use crate::disk_engine::image::scratch_dir;

/// Size of each extent file the harness provides.
const EXTENT_LEN: u64 = 1 << 20;

/// Extent names the harness creates in the scratch directory.
///
/// A descriptor referring to one of these opens successfully and reaches the
/// extent aware I/O engine; any other name fails at open, which is the same
/// path a missing extent takes in production. The names are the ones
/// `qemu-img` derives from an image called `image.vmdk`, plus the ones the
/// generated seeds use.
const EXTENTS: [&str; 6] = [
    "image-flat.vmdk",
    "image-f001.vmdk",
    "image-f002.vmdk",
    "flat-flat.vmdk",
    "two-f001.vmdk",
    "two-f002.vmdk",
];

/// Flat VMDK images, as opened by [`VmdkDisk`].
///
/// A VMDK image is a text descriptor that names its data extents as separate
/// files, so unlike every other format here it cannot be fuzzed from a memfd:
/// the engine resolves extent names against the descriptor's directory. The
/// harness therefore materializes the descriptor in a scratch directory that
/// already holds the extent files.
///
/// There is no template image: the `block` crate parses descriptors but never
/// writes one.
pub struct Vmdk;

impl Vmdk {
    /// Creates the extent files a descriptor may refer to, and resets them so
    /// that one iteration cannot observe what an earlier one wrote.
    fn reset_extents(dir: &Path) -> BlockResult<()> {
        for name in EXTENTS {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.join(name))
                .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
            file.set_len(EXTENT_LEN)
                .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
        }
        Ok(())
    }

    /// Rejects a descriptor that names an extent by absolute path.
    ///
    /// The engine anchors an absolute extent name at the filesystem root, so
    /// opening one would let a corpus entry reach any file on the host. That
    /// is unacceptable in a fuzzer, and it is the same reason backing files
    /// are not enabled for qcow2. Fuzzing that branch safely needs a
    /// filesystem sandbox rather than a scratch directory.
    fn names_absolute_extent(descriptor: &str) -> bool {
        descriptor.lines().any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            matches!(parts.len(), 4 | 5) && parts[3].trim_matches('"').starts_with('/')
        })
    }
}

impl DiskFormat for Vmdk {
    const NAME: &'static str = "vmdk";

    // The descriptor names its extents relative to its own directory.
    const NEEDS_PATH: bool = true;

    // A descriptor is a small text file; the data lives in the extents.
    const MAX_IMAGE_LEN: usize = 64 << 10;

    fn open(
        file: File,
        path: Option<&Path>,
        config: &OpenConfig,
    ) -> BlockResult<Box<dyn AsyncFullDiskFile>> {
        let path = path.expect("vmdk images are path backed");
        let dir = path
            .parent()
            .unwrap_or_else(|| scratch_dir(Self::NAME).expect("scratch directory"));

        let mut descriptor = String::new();
        let mut probe = file
            .try_clone()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
        probe
            .read_to_string(&mut descriptor)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
        probe
            .seek(SeekFrom::Start(0))
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;

        if Self::names_absolute_extent(&descriptor) {
            return Err(BlockError::from_kind(BlockErrorKind::UnsupportedFeature));
        }

        Self::reset_extents(dir)?;

        // The fuzzer drives the writable path, so it asks for a writable
        // open and lets the descriptor's own access field decide per extent.
        let disk = VmdkDisk::new(file, path, false, config.direct)?;
        Ok(Box::new(disk))
    }
}
