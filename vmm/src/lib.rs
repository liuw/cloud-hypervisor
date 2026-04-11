// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "vmm_impl.rs"]
mod vmm_impl;

#[cfg(unix)]
pub use vmm_impl::*;