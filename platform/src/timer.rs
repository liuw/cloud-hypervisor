// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform timer abstraction.
//
// On Unix: wraps vmm_sys_util::timerfd::TimerFd.
// On Windows: uses waitable timer objects.

use std::io;
use std::time::Duration;

/// A repeating or one-shot timer.
///
/// On Unix this wraps a Linux timerfd. On Windows it wraps a waitable timer.
/// The timer can be armed with `reset()` and fires by becoming "readable"
/// (on Unix the fd is readable; on Windows the event is signaled).

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::time::Duration;

    use vmm_sys_util::timerfd::TimerFd;

    pub struct Timer {
        inner: TimerFd,
    }

    impl Timer {
        pub fn new() -> io::Result<Self> {
            Ok(Timer {
                inner: TimerFd::new().map_err(|e| io::Error::other(format!("{e}")))?,
            })
        }

        /// Arm the timer to fire after `duration`, optionally repeating.
        pub fn reset(&mut self, duration: Duration, interval: Duration) -> io::Result<()> {
            let dur_spec = vmm_sys_util::timerfd::duration_to_timespec(duration);
            let int_spec = vmm_sys_util::timerfd::duration_to_timespec(interval);

            let tspec = libc::itimerspec {
                it_interval: int_spec,
                it_value: dur_spec,
            };
            self.inner
                .set_state(tspec, vmm_sys_util::timerfd::SetTimeFlags::Default);
            Ok(())
        }

        /// Wait for the timer to fire. Returns the number of expirations.
        pub fn wait(&mut self) -> io::Result<u64> {
            self.inner
                .wait()
                .map(|v| v.map_or(1, |n| n.get()))
                .map_err(|e| io::Error::other(format!("{e}")))
        }

        pub fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }

    impl AsRawFd for Timer {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }
}

// ─── Windows implementation ──────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod imp {
    use std::io;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::time::Duration;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        CreateWaitableTimerW, SetWaitableTimer, WaitForSingleObject, INFINITE,
    };

    pub struct Timer {
        handle: HANDLE,
    }

    impl Timer {
        pub fn new() -> io::Result<Self> {
            // SAFETY: Creating a waitable timer with no security attributes.
            let handle = unsafe { CreateWaitableTimerW(None, false, None) }
                .map_err(|e| io::Error::other(format!("{e}")))?;
            Ok(Timer { handle })
        }

        /// Arm the timer to fire after `duration`, repeating every `interval`.
        /// If `interval` is zero, it's a one-shot timer.
        pub fn reset(&mut self, duration: Duration, interval: Duration) -> io::Result<()> {
            // due_time is in 100-nanosecond intervals, negative = relative
            let due_100ns = -((duration.as_nanos() / 100) as i64);
            let period_ms = interval.as_millis() as i32;

            // SAFETY: handle is valid, we pass valid timer parameters.
            unsafe {
                SetWaitableTimer(self.handle, &due_100ns, period_ms, None, None, false)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
            Ok(())
        }

        /// Wait for the timer to fire. Returns 1 (Windows timers don't
        /// accumulate expirations like Linux timerfd).
        pub fn wait(&mut self) -> io::Result<u64> {
            // SAFETY: handle is valid.
            unsafe {
                WaitForSingleObject(self.handle, INFINITE);
            }
            Ok(1)
        }

        pub fn as_raw_handle(&self) -> RawHandle {
            self.handle.0 as RawHandle
        }
    }

    impl AsRawHandle for Timer {
        fn as_raw_handle(&self) -> RawHandle {
            self.handle.0 as RawHandle
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            // SAFETY: We own the handle.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    // SAFETY: Win32 waitable timer handles are thread-safe.
    unsafe impl Send for Timer {}
    // SAFETY: See above.
    unsafe impl Sync for Timer {}
}

pub use imp::Timer;
