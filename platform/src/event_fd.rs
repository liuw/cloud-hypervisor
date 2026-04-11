// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform EventFd implementation.
//
// On Unix: thin re-export of vmm_sys_util::eventfd::EventFd.
// On Windows: counting wake primitive backed by a Windows Event + AtomicU64.

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
pub use vmm_sys_util::eventfd::EventFd;

// ─── Windows implementation ──────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win {
    use std::io;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        CreateEventA, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
    };

    use crate::EFD_NONBLOCK;

    /// A cross-platform EventFd equivalent using a counting semaphore model.
    ///
    /// On Windows, this is backed by a Win32 manual-reset event object plus an
    /// atomic counter. `write()` increments the counter and signals the event.
    /// `read()` waits for the event and atomically drains the counter, returning
    /// the accumulated count.
    #[derive(Debug)]
    pub struct EventFd {
        inner: Arc<EventFdInner>,
    }

    #[derive(Debug)]
    struct EventFdInner {
        handle: HANDLE,
        counter: AtomicU64,
        nonblock: bool,
    }

    impl EventFd {
        /// Create a new EventFd.
        ///
        /// `flags` mirrors the Linux API: `EFD_NONBLOCK` (0x800) controls
        /// whether `read()` blocks.
        pub fn new(flags: i32) -> io::Result<Self> {
            let nonblock = (flags & EFD_NONBLOCK) != 0;

            // SAFETY: Creating a Win32 manual-reset event with no security attrs.
            let handle = unsafe { CreateEventA(None, true, false, None) }
                .map_err(|e| io::Error::other(format!("{e}")))?;

            Ok(EventFd {
                inner: Arc::new(EventFdInner {
                    handle,
                    counter: AtomicU64::new(0),
                    nonblock,
                }),
            })
        }

        /// Increment the counter by `v` and signal the event.
        pub fn write(&self, v: u64) -> io::Result<usize> {
            self.inner.counter.fetch_add(v, Ordering::SeqCst);
            // SAFETY: handle is valid for the lifetime of EventFdInner.
            unsafe {
                SetEvent(self.inner.handle)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
            Ok(std::mem::size_of::<u64>())
        }

        /// Read (drain) the counter, blocking until non-zero if not in
        /// non-blocking mode.
        pub fn read(&self) -> io::Result<u64> {
            loop {
                let val = self.inner.counter.swap(0, Ordering::SeqCst);
                if val > 0 {
                    // Reset the event since we drained the counter. If another
                    // write() races in, the counter will be >0 and the event
                    // will be re-signaled by that write().
                    // SAFETY: handle is valid.
                    let _ = unsafe { ResetEvent(self.inner.handle) };
                    return Ok(val);
                }

                if self.inner.nonblock {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "no data"));
                }

                // SAFETY: Wait on the event handle until signaled.
                unsafe {
                    WaitForSingleObject(self.inner.handle, INFINITE);
                }
            }
        }

        /// Clone the EventFd, sharing the underlying event object.
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(EventFd {
                inner: Arc::clone(&self.inner),
            })
        }
    }

    impl AsRawHandle for EventFd {
        fn as_raw_handle(&self) -> RawHandle {
            self.inner.handle.0 as RawHandle
        }
    }

    impl Drop for EventFdInner {
        fn drop(&mut self) {
            // SAFETY: We own the handle and it has not been closed.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    // SAFETY: The inner Arc<EventFdInner> handles synchronization, and Win32
    // event handles are safe to use from any thread.
    unsafe impl Send for EventFdInner {}
    // SAFETY: See above — Win32 event handles support concurrent access.
    unsafe impl Sync for EventFdInner {}
}

#[cfg(target_os = "windows")]
pub use win::EventFd;
