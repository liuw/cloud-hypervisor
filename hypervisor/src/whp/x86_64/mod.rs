// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Copyright © 2026, Microsoft Corporation
//

use serde::{Deserialize, Serialize};

use crate::arch::x86::{FpuState, LapicState, MsrEntry, SpecialRegisters};

/// Standard x86_64 general-purpose registers for WHP.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WhpStandardRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// Clock data for WHP (TSC-based).
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct WhpClockData {
    pub tsc_value: u64,
    pub tsc_frequency: u64,
}

/// Interrupt routing entry for WHP (software-maintained).
#[derive(Debug, Default, Clone, Copy)]
pub struct WhpIrqRoutingEntry {
    pub gsi: u32,
    pub address_hi: u32,
    pub address_lo: u32,
    pub data: u32,
}

/// Complete vCPU state for snapshot/restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuWhpState {
    pub regs: WhpStandardRegisters,
    pub sregs: SpecialRegisters,
    pub fpu: FpuState,
    pub lapic: LapicState,
    pub msrs: Vec<MsrEntry>,
    pub clock: WhpClockData,
}
