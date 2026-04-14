// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform event polling abstraction.
//
// On Unix: wraps the `epoll` crate (epoll_create/ctl/wait).
// On Windows: uses WaitForMultipleObjects on waitable HANDLEs.
//
// This module is designed for event loops that wait on a set of `EventFd`
// (or other waitable) objects. For complex I/O multiplexing (sockets, tap
// devices), use platform-specific abstractions instead.

use std::io;

/// An event returned by [`EventPoll::wait`].
#[derive(Debug, Clone, Copy)]
pub struct PollEvent {
    /// User-defined token associated with this event source.
    pub data: u64,
}

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs::File;
    use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

    /// Cross-platform event polling context.
    ///
    /// Wraps Linux `epoll` for efficient event notification.
    pub struct EventPoll {
        epoll_file: File,
    }

    impl EventPoll {
        /// Create a new event polling context.
        pub fn new() -> io::Result<Self> {
            let epoll_fd = epoll::create(true)?;
            // SAFETY: epoll_fd is a valid fd returned by epoll::create.
            let epoll_file = unsafe { File::from_raw_fd(epoll_fd) };
            Ok(EventPoll { epoll_file })
        }

        /// Register an event source with the given token.
        ///
        /// When the source becomes readable, `wait()` will return a
        /// `PollEvent` with `data` set to `token`.
        pub fn add_event<T: AsRawFd>(&self, source: &T, token: u64) -> io::Result<()> {
            epoll::ctl(
                self.epoll_file.as_raw_fd(),
                epoll::ControlOptions::EPOLL_CTL_ADD,
                source.as_raw_fd(),
                epoll::Event::new(epoll::Events::EPOLLIN, token),
            )
        }

        /// Remove a previously registered event source.
        pub fn del_event<T: AsRawFd>(&self, source: &T) -> io::Result<()> {
            epoll::ctl(
                self.epoll_file.as_raw_fd(),
                epoll::ControlOptions::EPOLL_CTL_DEL,
                source.as_raw_fd(),
                epoll::Event::new(epoll::Events::empty(), 0),
            )
        }

        /// Wait for events.
        ///
        /// Blocks until at least one registered source is ready, or the
        /// timeout (in milliseconds) expires. A timeout of `-1` blocks
        /// indefinitely; `0` returns immediately.
        ///
        /// Returns the number of events written to `events`.
        pub fn wait(&self, timeout_ms: i32, events: &mut [PollEvent]) -> io::Result<usize> {
            // Allocate temporary epoll::Event buffer matching the output slice.
            let mut epoll_events =
                vec![epoll::Event::new(epoll::Events::empty(), 0); events.len()];

            let n = epoll::wait(self.epoll_file.as_raw_fd(), timeout_ms, &mut epoll_events)?;

            for i in 0..n {
                events[i] = PollEvent {
                    data: epoll_events[i].data,
                };
            }

            Ok(n)
        }
    }

    impl AsRawFd for EventPoll {
        fn as_raw_fd(&self) -> RawFd {
            self.epoll_file.as_raw_fd()
        }
    }
}

// ─── Windows implementation ──────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use std::os::windows::io::{AsRawHandle, RawHandle};

    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        WaitForMultipleObjects, WaitForSingleObject,
    };

    /// Maximum number of event sources per poll context.
    ///
    /// Windows `WaitForMultipleObjects` supports at most 64 handles.
    const MAX_EVENTS: usize = 64;

    /// Cross-platform event polling context.
    ///
    /// On Windows, uses `WaitForMultipleObjects` on a set of waitable HANDLEs.
    pub struct EventPoll {
        /// Registered handles and their associated tokens.
        entries: Vec<PollEntry>,
    }

    struct PollEntry {
        handle: HANDLE,
        token: u64,
    }

    impl EventPoll {
        /// Create a new event polling context.
        pub fn new() -> io::Result<Self> {
            Ok(EventPoll {
                entries: Vec::with_capacity(8),
            })
        }

        /// Register an event source with the given token.
        ///
        /// The source must implement `AsRawHandle` and the handle must be a
        /// waitable object (e.g., Win32 Event, waitable timer).
        pub fn add_event<T: AsRawHandle>(&mut self, source: &T, token: u64) -> io::Result<()> {
            if self.entries.len() >= MAX_EVENTS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "EventPoll: too many events (max {})",
                        MAX_EVENTS
                    ),
                ));
            }

            let raw = source.as_raw_handle();
            self.entries.push(PollEntry {
                handle: HANDLE(raw),
                token,
            });

            Ok(())
        }

        /// Remove the event source with the given token.
        pub fn del_event_by_token(&mut self, token: u64) -> io::Result<()> {
            let pos = self.entries.iter().position(|e| e.token == token);
            match pos {
                Some(idx) => {
                    self.entries.remove(idx);
                    Ok(())
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("EventPoll: no event with token {token}"),
                )),
            }
        }

        /// Remove a previously registered event source by handle.
        pub fn del_event<T: AsRawHandle>(&mut self, source: &T) -> io::Result<()> {
            let raw = source.as_raw_handle();
            let handle = HANDLE(raw);
            let pos = self.entries.iter().position(|e| e.handle == handle);
            match pos {
                Some(idx) => {
                    self.entries.remove(idx);
                    Ok(())
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "EventPoll: handle not found",
                )),
            }
        }

        /// Wait for events.
        ///
        /// Blocks until at least one registered handle is signaled, or the
        /// timeout (in milliseconds) expires. A timeout of `-1` blocks
        /// indefinitely; `0` returns immediately.
        ///
        /// Returns the number of events written to `events`.
        pub fn wait(&self, timeout_ms: i32, events: &mut [PollEvent]) -> io::Result<usize> {
            if self.entries.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "EventPoll: no events registered",
                ));
            }

            let handles: Vec<HANDLE> = self.entries.iter().map(|e| e.handle).collect();
            let timeout = if timeout_ms < 0 { 0xFFFFFFFF_u32 } else { timeout_ms as u32 };

            // SAFETY: All handles are valid waitable objects registered via add_event.
            let result = unsafe {
                WaitForMultipleObjects(&handles, false, timeout)
            };

            if result == WAIT_TIMEOUT {
                return Ok(0);
            }

            let first_idx = (result.0 - WAIT_OBJECT_0.0) as usize;
            if first_idx >= self.entries.len() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("WaitForMultipleObjects failed: result={}", result.0),
                ));
            }

            // Collect the first signaled event.
            let mut count = 0;
            if count < events.len() {
                events[count] = PollEvent {
                    data: self.entries[first_idx].token,
                };
                count += 1;
            }

            // Check remaining handles (after first_idx) for additional signals.
            // Only check with zero timeout to avoid consuming auto-reset objects
            // that weren't part of the original wait result.
            for entry in self.entries.iter().skip(first_idx + 1) {
                if count >= events.len() {
                    break;
                }
                // SAFETY: handle is a valid waitable object.
                let r = unsafe { WaitForSingleObject(entry.handle, 0) };
                if r == WAIT_OBJECT_0 {
                    events[count] = PollEvent {
                        data: entry.token,
                    };
                    count += 1;
                }
            }

            Ok(count)
        }
    }
}

#[cfg(unix)]
pub use unix::EventPoll;

#[cfg(target_os = "windows")]
pub use win::EventPoll;
