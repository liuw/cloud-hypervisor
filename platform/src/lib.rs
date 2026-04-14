// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform abstractions for cloud-hypervisor.
//
// This crate provides platform-independent wrappers for OS-specific
// primitives used throughout the VMM:
//
// - `EventFd`: Counting wake primitive (eventfd on Unix, Event+AtomicU64 on Windows)
// - `signal`: Termination handler installation
// - `terminal`: Terminal state save/restore and raw mode

mod event_fd;
pub mod clock;
pub mod signal;
pub mod terminal;
pub mod timer;

pub use event_fd::EventFd;
pub use terminal::{TerminalState, is_terminal, restore_terminal_state, save_terminal_state, set_raw_mode};

/// Constant matching Linux `EFD_NONBLOCK` for use in `EventFd::new()`.
/// Value 0x800 mirrors the Linux definition; on Windows this is interpreted
/// by our EventFd implementation.
pub const EFD_NONBLOCK: i32 = 0x800;
