// SPDX-License-Identifier: Apache-2.0
//
// Windows VMM implementation.
//
// This module provides a minimal VMM for Windows, sharing cross-platform
// submodules (api, config, vm_config) with the Unix implementation.

// ── Cross-platform submodules (shared with vmm_impl.rs) ─────────────────────
pub mod api;
pub mod config;
pub mod device_tree;
pub mod vm_config;

// ── Windows-specific submodules ─────────────────────────────────────────────
#[path = "serial_manager_windows.rs"]
pub mod serial_manager;
