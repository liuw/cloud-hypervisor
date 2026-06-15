// Copyright © 2025 Demi Marie Obenour
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! Helpers for `mmap()`

use core::ffi::{c_int, c_void};
use core::ptr::null_mut;
use std::io::{self, Error, ErrorKind};
use std::os::fd::{AsRawFd as _, BorrowedFd};

use libc::size_t;

const TWO_MIB: usize = 2 * 1024 * 1024;

/// Round `addr` up to the next 2 MiB boundary.
#[inline]
fn align_up_to_2mib(addr: usize) -> usize {
    addr.next_multiple_of(TWO_MIB)
}

/// A region of `mmap()`-allocated memory that calls `munmap()` when dropped.
/// This guarantees that the buffer is valid and that its address space
/// will be reserved.  The address space is not guaranteed to be accessible.
/// Atomic access to the data will not cause undefined behavior but might
/// cause SIGSEGV or SIGBUS.  Non-atomic access will generally cause data
/// races and thus Undefined Behavior.
#[derive(Debug)]
pub struct MmapRegion {
    addr: *mut u8,
    len: size_t,
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: guaranteed by type validity invariant
        unsafe { assert_eq!(libc::munmap(self.addr.cast(), self.len), 0) }
    }
}
// SAFETY: the caller is responsible for avoiding data races
unsafe impl Send for MmapRegion {}
// SAFETY: the caller is responsible for avoiding data races
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    #[inline]
    pub fn addr(&self) -> *mut u8 {
        self.addr
    }

    /// Return the length of the region.
    /// This function promises that the return value fits in [`libc::size_t`]
    /// and in [`isize`] and `unsafe` code can rely on this.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Create an [`MmapRegion`] using `mmap` of a file descriptor.
    pub fn mmap(
        len: u64,
        prot: c_int,
        fd: BorrowedFd,
        offset1: u64,
        offset2: u64,
    ) -> io::Result<Self> {
        const BAD_LENGTH: &str = "Offsets must fit in libc::off_t";
        const BAD_OFFSET: &str = "Mapping length must fit \
in both isize and libc::size_t";
        let Some(offset) = offset1.checked_add(offset2) else {
            return Err(Error::new(ErrorKind::InvalidInput, BAD_OFFSET));
        };
        let Ok(offset) = libc::off_t::try_from(offset) else {
            return Err(Error::new(ErrorKind::InvalidInput, BAD_OFFSET));
        };
        if isize::try_from(len).is_err() {
            return Err(Error::new(ErrorKind::InvalidInput, BAD_LENGTH));
        }
        let Ok(len) = libc::size_t::try_from(len) else {
            return Err(Error::new(ErrorKind::InvalidInput, BAD_LENGTH));
        };

        assert!(
            (prot & !(libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC)) == 0,
            "bad protection"
        );

        // A 2 MiB-aligned address only lets the kernel back the mapping with
        // huge pages when the mapping length is itself a multiple of 2 MiB. For
        // other lengths the alignment is pointless, so create a plain
        // page-aligned mapping instead.
        if !len.is_multiple_of(TWO_MIB) {
            // SAFETY: FFI call with correct parameters.
            let addr = unsafe {
                libc::mmap(
                    null_mut(),
                    len,
                    prot,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    offset,
                )
            };
            if addr == libc::MAP_FAILED {
                return Err(Error::last_os_error());
            }
            return Ok(Self {
                addr: addr.cast(),
                len,
            });
        }

        // Align the mapping to a 2 MiB boundary so the kernel can back it with
        // huge pages. `mmap(NULL, ...)` only guarantees page-sized alignment, so
        // reserve an anonymous region big enough to contain a 2 MiB-aligned run
        // of `len` bytes, map the file at the aligned address, and trim the
        // excess on both sides.
        let Some(reserve) = len.checked_add(TWO_MIB) else {
            return Err(Error::new(ErrorKind::InvalidInput, BAD_LENGTH));
        };

        // SAFETY: FFI call. Reserving address space with a NULL hint and no fd.
        let base = unsafe {
            libc::mmap(
                null_mut(),
                reserve,
                prot,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }

        let base_addr = base as usize;
        let aligned_addr = align_up_to_2mib(base_addr);
        let aligned = aligned_addr as *mut c_void;

        // SAFETY: FFI call. MAP_FIXED is safe here because it only replaces the
        // anonymous reservation we just created.
        let addr = unsafe {
            libc::mmap(
                aligned,
                len,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd.as_raw_fd(),
                offset,
            )
        };
        if addr == libc::MAP_FAILED {
            let err = Error::last_os_error();
            // SAFETY: release the whole reservation we created above.
            unsafe { assert_eq!(libc::munmap(base, reserve), 0) };
            return Err(err);
        }

        // Trim the head and tail of the reservation that surround the mapping.
        let head_len = aligned_addr - base_addr;
        if head_len > 0 {
            // SAFETY: this range is still the anonymous reservation.
            unsafe { assert_eq!(libc::munmap(base, head_len), 0) };
        }
        let tail_addr = aligned_addr + len;
        let tail_len = (base_addr + reserve) - tail_addr;
        if tail_len > 0 {
            // SAFETY: this range is still the anonymous reservation.
            unsafe { assert_eq!(libc::munmap(tail_addr as *mut c_void, tail_len), 0) };
        }

        Ok(Self {
            addr: addr.cast(),
            len,
        })
    }
}
