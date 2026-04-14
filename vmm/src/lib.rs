// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "vmm_impl.rs"]
mod vmm_impl;

#[cfg(unix)]
pub use vmm_impl::*;

#[cfg(target_os = "windows")]
#[path = "vmm_windows.rs"]
mod vmm_impl;

#[cfg(target_os = "windows")]
pub use vmm_impl::*;