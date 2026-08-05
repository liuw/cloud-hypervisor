// Copyright © 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Flat VMDK adapter.

use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path};

use block::disk_file::AsyncFullDiskFile;
use block::error::{BlockError, BlockErrorKind, BlockResult};
use block::formats::vmdk::VmdkDisk;

use crate::disk_engine::format::{DiskFormat, OpenConfig};
use crate::disk_engine::image::scratch_dir;

/// Size of each extent file the harness provides.
const EXTENT_LEN: u64 = 1 << 20;

/// The first line of every descriptor, from `VMDK_DESCRIPTOR_HEADER` in the
/// parser under test (block/src/formats/vmdk/descriptor.rs:19).
const DESCRIPTOR_HEADER: &str = "# Disk DescriptorFile";

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

/// Placeholder a descriptor uses to name the scratch directory.
///
/// An extent name may be absolute, and the engine treats an absolute name
/// very differently from a relative one: it anchors resolution at the
/// filesystem root and deliberately does not apply `RESOLVE_BENEATH`
/// (block/src/formats/vmdk/flat.rs:141 and :190). That arm cannot be fuzzed
/// by naming a real absolute path, because the only absolute path that is
/// safe to open here is one inside the scratch directory, and the scratch
/// directory carries the process id so no fixed corpus entry can name it.
///
/// A descriptor therefore writes the placeholder, and the harness expands it
/// into the scratch directory before the parser reads the file. The
/// expansion is a pure function of the input, so an input still reproduces
/// on its own: what varies between processes is the directory the harness
/// itself creates, exactly as it already does for relative names.
const SCRATCH_TOKEN: &str = "/@fuzz-scratch@";

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

    /// Decides from the input whether this iteration resolves its extents
    /// with the `openat2` fallback walk.
    ///
    /// One input in two, by a hash of the descriptor, so both resolution
    /// paths keep a share of the corpus and neither depends on anything
    /// outside the input.
    ///
    /// A descriptor naming an absolute extent is the exception: it is the
    /// only thing that reaches the `openat2` arm which deliberately leaves
    /// `RESOLVE_BENEATH` off, so sending it down the walk would spend the
    /// one input that can cover it.
    #[cfg_attr(not(fuzzing), allow(dead_code))]
    fn force_walk(bytes: &[u8]) -> bool {
        if bytes
            .windows(SCRATCH_TOKEN.len())
            .any(|window| window == SCRATCH_TOKEN.as_bytes())
        {
            return false;
        }

        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        !hasher.finish().is_multiple_of(2)
    }

    /// Expands [`SCRATCH_TOKEN`] in `descriptor` into the scratch directory.
    ///
    /// Returns `None` when there is nothing to expand, so the common case
    /// pays nothing.
    fn expand_scratch_token(descriptor: &str, dir: &Path) -> Option<String> {
        if !descriptor.contains(SCRATCH_TOKEN) {
            return None;
        }
        Some(descriptor.replace(SCRATCH_TOKEN, &dir.to_string_lossy()))
    }

    /// Rejects a descriptor that names an extent outside the scratch
    /// directory.
    ///
    /// The engine resolves an extent name against the descriptor's directory
    /// with `openat2`, and for an absolute name it anchors at the filesystem
    /// root without `RESOLVE_BENEATH`, and it opens the extent writable. A
    /// corpus entry could therefore reach and overwrite any file on the
    /// host, which is unacceptable in a fuzzer.
    ///
    /// A relative name is accepted when every component is
    /// [`Component::Normal`]: that rejects root and prefix components, `..`
    /// and `.` alike. An absolute name is accepted only when it starts with
    /// the scratch directory and every component after it is `Normal`, so
    /// the engine's absolute arm is fuzzed with a name that still cannot
    /// leave the directory the harness owns.
    fn names_escaping_extent(descriptor: &str, dir: &Path) -> bool {
        descriptor.lines().any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if !matches!(parts.len(), 4 | 5) {
                return false;
            }
            let name = Path::new(parts[3].trim_matches('"'));
            let rest = if name.is_absolute() {
                match name.strip_prefix(dir) {
                    Ok(rest) => rest,
                    Err(_) => return true,
                }
            } else {
                name
            };
            // An empty remainder is the scratch directory itself, which is
            // not an extent.
            rest.components().next().is_none()
                || !rest.components().all(|c| matches!(c, Component::Normal(_)))
        })
    }
}

impl DiskFormat for Vmdk {
    const NAME: &'static str = "vmdk";

    // The descriptor names its extents relative to its own directory.
    const NEEDS_PATH: bool = true;

    // A descriptor is a small text file; the data lives in the extents.
    const MAX_IMAGE_LEN: usize = 64 << 10;

    // `parse_header` rejects a descriptor whose first line is not exactly
    // "# Disk DescriptorFile" (block/src/formats/vmdk/descriptor.rs:133,
    // VMDK_DESCRIPTOR_HEADER at line 19), and `read_descriptor` rejects one
    // that is not UTF-8 (line 118). Both run before anything else is parsed.
    //
    // The parser compares the line with its trailing whitespace stripped, so
    // this does too: a check the parser does not make would drop inputs it
    // would have accepted.
    fn magic_ok(bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        text.lines()
            .next()
            .is_some_and(|line| line.trim_end() == DESCRIPTOR_HEADER)
    }

    // Neither new invariant holds for a flat VMDK, so both keep their
    // conservative default.
    //
    // The capacity is the sum of the extent sizes the descriptor declares,
    // and the data lives in those separate files rather than in the image
    // file, so `physical_size` says nothing about it.
    //
    // A read can legitimately come up short: an extent declared longer than
    // the file behind it makes the buffered path stop at end of file
    // (block/src/formats/vmdk/engine_sync.rs:100) and the spanning path
    // break out of its loop (block/src/formats/vmdk/engine_sync.rs:180),
    // both reporting the partial count as a success. A fuzzed descriptor
    // declares extent sizes freely, so this is reachable by construction.

    fn open(
        file: File,
        path: Option<&Path>,
        config: &OpenConfig,
    ) -> BlockResult<Box<dyn AsyncFullDiskFile>> {
        let path = path.expect("vmdk images are path backed");
        let dir = path
            .parent()
            .unwrap_or_else(|| scratch_dir(Self::NAME).expect("scratch directory"));

        // Read positionally: `try_clone` is dup(2) and shares the file
        // cursor with the engine, so a probe that moved it would leave the
        // guard and the parser looking at different bytes.
        let mut raw = vec![0u8; Self::MAX_IMAGE_LEN];
        let len = file
            .read_at(&mut raw, 0)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
        let mut descriptor = String::from_utf8_lossy(&raw[..len]).into_owned();

        // The placeholder is expanded in the file itself, because the parser
        // reads the descriptor from there and the expanded name has to be
        // the one it resolves.
        if let Some(expanded) = Self::expand_scratch_token(&descriptor, dir) {
            file.write_all_at(expanded.as_bytes(), 0)
                .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
            file.set_len(expanded.len() as u64)
                .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
            descriptor = expanded;
        }

        if Self::names_escaping_extent(&descriptor, dir) {
            return Err(BlockError::from_kind(BlockErrorKind::UnsupportedFeature));
        }

        Self::reset_extents(dir)?;

        // Which resolution path the engine takes is derived from the input
        // rather than from the environment, so a crash found on the fallback
        // walk reproduces from its bytes alone. Without this the walk is
        // dead code on every kernel a fuzzer runs on, because `openat2`
        // always succeeds there.
        #[cfg(fuzzing)]
        block::formats::vmdk::set_force_extent_walk(Self::force_walk(&raw[..len]));

        // The fuzzer drives the writable path, so it asks for a writable
        // open and lets the descriptor's own access field decide per extent.
        let disk = VmdkDisk::new(file, path, false, config.direct)?;
        Ok(Box::new(disk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_engine::image::{image_file, scratch_dir};

    fn descriptor(name: &str) -> String {
        format!("# Disk DescriptorFile\nversion=1\ncreateType=\"monolithicFlat\"\nRW 2048 FLAT \"{name}\" 0\n")
    }

    fn scratch() -> &'static Path {
        scratch_dir(Vmdk::NAME).expect("scratch directory")
    }

    // The engine opens extents writable, and for an absolute name it
    // resolves from the filesystem root without RESOLVE_BENEATH, so a
    // descriptor that escapes the scratch directory must never reach it.
    #[test]
    fn traversing_extent_names_are_rejected() {
        let dir = scratch();
        for name in [
            "../../../../tmp/victim".to_string(),
            "..".to_string(),
            "./image-flat.vmdk".to_string(),
            "sub/../../tmp/victim".to_string(),
            "/tmp/victim".to_string(),
            "/etc/passwd".to_string(),
            // Absolute, and inside the scratch directory only until the
            // components after it are followed.
            format!("{}/../../tmp/victim", dir.display()),
            // The scratch directory itself is not an extent.
            dir.display().to_string(),
            // A directory whose name merely starts with the scratch path.
            format!("{}-evil/image-flat.vmdk", dir.display()),
        ] {
            assert!(
                Vmdk::names_escaping_extent(&descriptor(&name), dir),
                "{name} must be rejected"
            );
        }
    }

    // The absolute arm of the engine's extent opener is only reachable with
    // an absolute name, and the only absolute name that is safe to give it
    // is one inside the scratch directory the harness owns.
    #[test]
    fn absolute_names_inside_the_scratch_directory_are_accepted() {
        let dir = scratch();
        for name in [
            format!("{}/image-flat.vmdk", dir.display()),
            format!("{}/extents/s001.vmdk", dir.display()),
        ] {
            assert!(
                !Vmdk::names_escaping_extent(&descriptor(&name), dir),
                "{name} must be accepted"
            );
        }
    }

    // A corpus entry cannot know the scratch path, so it writes the
    // placeholder and the harness expands it. The expansion has to produce a
    // name the guard then accepts, or the arm stays unreachable.
    #[test]
    fn the_scratch_token_expands_to_an_accepted_absolute_name() {
        let dir = scratch();
        let raw = descriptor(&format!("{SCRATCH_TOKEN}/image-flat.vmdk"));
        assert!(
            Vmdk::names_escaping_extent(&raw, dir),
            "the unexpanded placeholder is not a valid name"
        );

        let expanded = Vmdk::expand_scratch_token(&raw, dir).expect("the token must expand");
        assert!(expanded.contains(&dir.display().to_string()));
        assert!(!Vmdk::names_escaping_extent(&expanded, dir));
        assert!(Vmdk::expand_scratch_token("no token here", dir).is_none());
    }

    // The resolution path must depend on the input alone, and both paths
    // have to keep a share of it.
    #[test]
    fn the_walk_choice_is_a_function_of_the_input() {
        let a = descriptor("image-flat.vmdk");
        assert_eq!(
            Vmdk::force_walk(a.as_bytes()),
            Vmdk::force_walk(a.as_bytes())
        );

        let mixed = (0..64u8)
            .map(|i| Vmdk::force_walk(format!("{a}{i}").as_bytes()))
            .filter(|walk| *walk)
            .count();
        assert!(
            (8..56).contains(&mixed),
            "{mixed} of 64 inputs took the walk, expected a rough split"
        );

        // An absolute name has to reach openat2, which is the only arm that
        // can cover the unconfined resolve.
        let absolute = descriptor(&format!("{SCRATCH_TOKEN}/image-flat.vmdk"));
        assert!(!Vmdk::force_walk(absolute.as_bytes()));
    }

    // `magic_ok` decides whether a corpus entry is a descriptor at all, so it
    // has to agree with the header line test the parser makes.
    #[test]
    fn magic_ok_tracks_the_descriptor_header() {
        assert!(Vmdk::magic_ok(descriptor("image-flat.vmdk").as_bytes()));
        // The parser trims trailing whitespace off the header line.
        assert!(Vmdk::magic_ok(b"# Disk DescriptorFile \nversion=1\n"));
        assert!(!Vmdk::magic_ok(b"# Disk Descriptor\n"));
        assert!(!Vmdk::magic_ok(b"version=1\n# Disk DescriptorFile\n"));
        assert!(!Vmdk::magic_ok(b""));
        assert!(!Vmdk::magic_ok(&[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn plain_extent_names_are_accepted() {
        for name in ["image-flat.vmdk", "two-f001.vmdk"] {
            assert!(
                !Vmdk::names_escaping_extent(&descriptor(name), scratch()),
                "{name} must be accepted"
            );
        }
    }

    // The guard has to hold at the harness entry point, not just in
    // isolation: `open` reads the descriptor positionally, so it sees the
    // same bytes the engine parses.
    #[test]
    fn open_refuses_a_traversing_descriptor() {
        let bytes = descriptor("../../../../tmp/victim");
        let (file, path) = image_file("vmdk", bytes.as_bytes()).expect("scratch image");
        let err = Vmdk::open(file, Some(&path), &OpenConfig::default())
            .err()
            .expect("a descriptor escaping the scratch directory must be refused");
        assert_eq!(err.kind(), BlockErrorKind::UnsupportedFeature);
        assert!(
            !Path::new("/tmp/victim").exists(),
            "the harness must not have created /tmp/victim"
        );
    }
}
