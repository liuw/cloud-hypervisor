# Plan: Port Cloud-Hypervisor to Windows Hypervisor Platform (WHP)

## Problem Statement

Cloud-hypervisor currently supports two hypervisor backends — KVM (Linux) and MSHV (Microsoft Hypervisor on Linux). Both assume a Linux host. The goal is to add Windows Hypervisor Platform (WHP) as a third backend, and ultimately make the full program run on Windows.

**Target architecture:** x86_64 only  
**Binding approach:** `windows-rs` (official Microsoft Rust bindings)  
**Implementation scope:** Full phased plan; Phase 1 (hypervisor backend) implemented and verified.

## Current Status

**Phase 1: COMPLETE** ✓  
- WHP backend compiles and passes clippy on Windows x86_64  
- `cargo clippy -p hypervisor --features whp` — zero errors, zero warnings  
- Key changes across 2 commits, ~1100 lines of new WHP code

**Phase 2: IN PROGRESS** (event + signal + terminal)  
- Created `platform` crate with cross-platform EventFd, signal handler, and terminal I/O  
- Migrated all 66 source files from `vmm_sys_util::eventfd::EventFd` to `platform::EventFd`  
- Migrated 18 files from `libc::EFD_NONBLOCK` to `platform::EFD_NONBLOCK`  
- Added `platform` dependency to 12 crates  
- Remaining: IPC abstraction, memory abstraction (lower priority)

## Approach

Follow the existing pattern used by KVM and MSHV backends: create a new `whp` module in `hypervisor/src/` that implements the `Hypervisor`, `Vm`, and `Vcpu` traits using WHP APIs. Later phases address the platform-level abstractions (epoll→IOCP, EventFd→Windows Events, etc.) needed to compile and run the full VMM on Windows.

---

## Phase 1: WHP Hypervisor Backend (IMPLEMENT NOW)

Add a new `hypervisor/src/whp/` module implementing the three core traits.

### 1.1 — Crate setup and feature flags

- Add `whp` feature to `hypervisor/Cargo.toml` with `windows` crate dependency
- Add `windows` crate to workspace `Cargo.toml` with WHP-related feature flags:
  - `Win32_System_Hypervisor` (WHvCreatePartition, WHvRunVirtualProcessor, etc.)
- Add `HypervisorType::Whp` variant to `lib.rs`
- Add `CpuState::Whp(...)`, `ClockData::Whp(...)`, `MpState::Whp`, `StandardRegisters::Whp(...)`, `IrqRoutingEntry::Whp(...)` variants
- Update `hypervisor::new()` to probe WHP availability via `WHvGetCapability`
- Extend the `set_x86_64_reg!` / `get_x86_64_reg!` macros to handle `Whp` variant

### 1.2 — WhpHypervisor (implements `Hypervisor` trait)

Create `hypervisor/src/whp/mod.rs`:

- `WhpHypervisor::is_available()` — call `WHvGetCapability(WHvCapabilityCodeHypervisorPresent)`
- `WhpHypervisor::new()` — verify WHP is available, return `Arc<dyn Hypervisor>`
- `create_vm(config)` — call `WHvCreatePartition()`, set properties (processor count, extended VM exits, etc.), call `WHvSetupPartition()`
- `get_supported_cpuid()` — call `WHvGetCapability(WHvCapabilityCodeProcessorFeatures)` and synthesize `CpuIdEntry` list, or use `__cpuid` intrinsic
- `get_max_vcpus()` — query WHP capability or return a reasonable default (e.g., 240)
- `check_required_extensions()` — verify required WHP capabilities

### 1.3 — WhpVm (implements `Vm` trait)

Create `WhpVm` struct holding a `WHV_PARTITION_HANDLE`:

- **Memory management:**
  - `create_user_memory_region()` → `WHvMapGpaRange()`
  - `remove_user_memory_region()` → `WHvUnmapGpaRange()`
- **vCPU creation:**
  - `create_vcpu(id, vm_ops)` → `WHvCreateVirtualProcessor()`
- **Interrupt routing:**
  - `register_irqfd()` / `unregister_irqfd()` — WHP doesn't have irqfd; store mappings and deliver via `WHvRequestInterrupt()` from a signaling thread
  - `register_ioevent()` / `unregister_ioevent()` — WHP handles I/O intercepts via exit reasons; maintain a local dispatch table
  - `make_routing_entry()` / `set_gsi_routing()` — maintain a local GSI→MSI routing table
- **Interrupt controller:**
  - `create_irq_chip()` — WHP provides a built-in local APIC; configure via partition properties
  - `enable_split_irq()` — set `WHvPartitionPropertyCodeProcessorClFlushSize` or equivalent
- **Clock:**
  - `get_clock()` / `set_clock()` — use `WHvGetVirtualProcessorRegisters` with TSC register, or `QueryPerformanceCounter` for reference
- **Identity/TSS (x86):**
  - `set_identity_map_address()` / `set_tss_address()` — WHP manages these internally; implement as no-ops or configure via properties
- **Dirty page tracking:**
  - `start_dirty_log()` / `stop_dirty_log()` / `get_dirty_log()` — use `WHvQueryGpaRangeDirtyBitmap()` (available in newer WHP versions)
- **Passthrough:**
  - `create_passthrough_device()` — return error (VFIO not available on Windows); or stub

### 1.4 — WhpVcpu (implements `Vcpu` trait)

Create `WhpVcpu` struct:

- **Register access:**
  - `get_regs()` / `set_regs()` → `WHvGetVirtualProcessorRegisters()` / `WHvSetVirtualProcessorRegisters()` with standard register names (Rax, Rbx, ..., Rip, Rflags)
  - `get_sregs()` / `set_sregs()` → segment registers (Cs, Ds, Es, Fs, Gs, Ss), control registers (Cr0-Cr4), descriptor tables (Gdtr, Idtr, Ldtr, Tr), Efer
  - `get_fpu()` / `set_fpu()` → FP/SSE registers via WHv register names
  - `get_lapic()` / `set_lapic()` → `WHvGetVirtualProcessorInterruptControllerState()` / `WHvSetVirtualProcessorInterruptControllerState()`
  - `get_msrs()` / `set_msrs()` → map MSR indices to WHv register names
  - `get_cpuid2()` / `set_cpuid2()` → `WHvGetVirtualProcessorCpuidOutput()`
- **Execution:**
  - `run()` → `WHvRunVirtualProcessor()`, translate `WHV_RUN_VP_EXIT_CONTEXT` to `VmExit`:
    - `WHvRunVpExitReasonMemoryAccess` → MMIO (call `vm_ops.mmio_read/write`)
    - `WHvRunVpExitReasonX64IoPortAccess` → PIO (call `vm_ops.pio_read/write`)
    - `WHvRunVpExitReasonX64Halt` → `VmExit::Shutdown` or idle
    - `WHvRunVpExitReasonCanceled` → check for pending signals
    - `WHvRunVpExitReasonX64ApicEoi` → `VmExit::IoapicEoi`
    - `WHvRunVpExitReasonX64Cpuid` → handle CPUID intercepts
    - `WHvRunVpExitReasonX64MsrAccess` → handle MSR intercepts
    - `WHvRunVpExitReasonUnrecoverableException` → `VmExit::Shutdown`
- **State save/restore:**
  - `state()` / `set_state()` — serialize all registers into `CpuState::Whp(VcpuWhpState)`
- **Misc:**
  - `set_immediate_exit()` → `WHvCancelRunVirtualProcessor()`
  - `nmi()` → deliver NMI via `WHvRequestInterrupt()`
  - `boot_msr_entries()` → return static list of initial MSR values
  - `tsc_khz()` / `set_tsc_khz()` → read/write TSC frequency register

### 1.5 — WHP-specific types

Create `hypervisor/src/whp/x86_64/mod.rs`:

- `VcpuWhpState` — serializable struct holding all vCPU register state
- `WhpClockData` — TSC-based clock representation
- Conversion functions between WHP register values and generic types (e.g., `WHV_X64_SEGMENT_REGISTER` ↔ `SegmentRegister`)

### 1.6 — Integration and testing

- Conditionally compile with `#[cfg(feature = "whp")]` and `#[cfg(target_os = "windows")]`
- Add unit tests for WhpHypervisor::is_available(), register conversions
- Verify the hypervisor backend compiles on Windows with `cargo build --features whp`
- Test VM creation + vCPU register read/write on a Windows machine with Hyper-V enabled

---

## Phase 2: Platform Abstraction Layer

Abstract Linux-specific primitives so the VMM compiles on both platforms.

### 2.1 — Event notification abstraction

Replace direct epoll/EventFd usage with a cross-platform abstraction:
- Create `vmm/src/event/mod.rs` with traits:
  - `Event` trait (wrapping EventFd on Linux, Windows Event HANDLEs on Windows)
  - `EventLoop` trait (wrapping epoll on Linux, IOCP on Windows)
- ~40+ files use epoll/EventFd — each must be updated
- Key files: `virtio-devices/src/epoll_helper.rs`, `vmm/src/lib.rs`, `vmm/src/vm.rs`

### 2.2 — Memory mapping abstraction

- Abstract `mmap()` → `VirtualAlloc()`/`MapViewOfFile()` for guest memory
- Key file: `vmm/src/memory_manager.rs`
- The `vm-memory` crate may already have Windows support via `backend-mmap`

### 2.3 — Signal handling abstraction

- Replace Unix signals with Windows console control handlers
- `signal-hook` → `ctrlc` crate or `SetConsoleCtrlHandler`
- Key files: `cloud-hypervisor/src/main.rs`, `vmm/src/sigwinch_listener.rs`

### 2.4 — Terminal I/O abstraction

- Replace `termios`/`tcsetattr` with Windows Console API
- Key file: `cloud-hypervisor/src/main.rs`

### 2.5 — IPC abstraction (Unix sockets → Named pipes/TCP)

- API socket: replace Unix socket with named pipe or TCP
- Migration transport: abstract socket layer
- Key files: `vmm/src/migration_transport.rs`, `api_client/`

---

## Phase 3: Device and I/O Porting

Port device implementations to work on Windows.

### 3.1 — Block device I/O

- Replace Linux AIO / io_uring with Windows Overlapped I/O / IOCP
- Key files: `block/src/raw_async_aio.rs`, `block/src/raw_async.rs`

### 3.2 — Networking

- Replace Linux TAP (`/dev/net/tun`) with Windows networking:
  - Option A: Hyper-V Virtual Switch API
  - Option B: OpenVPN TAP-Windows driver
  - Option C: WinTun/WireGuard
- Key files: `net_util/src/tap.rs`, `virtio-devices/src/net.rs`

### 3.3 — vhost-user

- vhost-user protocol uses Unix sockets — requires named pipe transport or TCP
- May disable initially on Windows
- Key files: `virtio-devices/src/vhost_user/`

### 3.4 — Vsock

- Replace `AF_VSOCK` with Hyper-V sockets (`AF_HYPERV`)
- Key files: `virtio-devices/src/vsock/`

### 3.5 — Serial / Console

- Port serial device to use Windows COM port APIs or virtual console
- Key files: `vmm/src/serial_manager.rs`, `vmm/src/console_devices.rs`

---

## Phase 4: Security and Sandboxing

### 4.1 — Remove/abstract seccomp

- Seccomp is Linux-only; disable on Windows or replace with process mitigation policies
- Key file: `vmm/src/seccomp_filters.rs`

### 4.2 — Remove/abstract Landlock

- Landlock LSM is Linux-only; disable on Windows or replace with Windows SACLs
- Key file: `vmm/src/landlock.rs`

---

## Phase 5: Build System and CI

### 5.1 — Cargo configuration

- Add Windows target to workspace
- Conditional dependencies throughout (e.g., `epoll` only on Linux, `windows` only on Windows)
- Feature matrix: `kvm` (Linux-only), `mshv` (Linux-only), `whp` (Windows-only)

### 5.2 — CI pipeline

- Add Windows build job to GitHub Actions
- Cross-compilation testing
- Windows VM integration tests

### 5.3 — Documentation

- Update README with Windows build instructions
- Document WHP-specific requirements (Hyper-V enabled, admin privileges)
- API compatibility notes

---

## Phase 6: Integration Testing and Stabilization

### 6.1 — Boot a Linux guest on Windows

- End-to-end test: boot a minimal Linux kernel with virtio-console
- Verify vCPU execution, MMIO handling, interrupt delivery

### 6.2 — Performance benchmarking

- Compare WHP backend performance against KVM/MSHV baselines
- Optimize hot paths (vCPU run loop, register access)

### 6.3 — Feature parity assessment

- Document which features are available on Windows vs Linux
- Live migration, snapshot/restore, device hotplug — assess feasibility

---

## Key Risk Areas

1. **irqfd/ioevent gap**: WHP has no kernel-level irqfd/ioevent. Must implement in userspace with polling or callback threads. Performance impact likely.
2. **Dirty page tracking**: `WHvQueryGpaRangeDirtyBitmap` may not be available in all WHP versions.
3. **APIC emulation**: WHP provides built-in LAPIC but interrupt routing differs from KVM/MSHV.
4. **vm-memory / vmm-sys-util**: These rust-vmm crates may not compile on Windows. May need forks or conditional compilation.
5. **Networking**: No direct TAP equivalent on Windows. This is the hardest device to port.
6. **VFIO**: Device passthrough is not available via WHP. GPU passthrough requires different approach (GPU-PV).

---

## Dependencies Between Phases

```
Phase 1 (WHP backend) ─── standalone, no other phase needed
Phase 2 (Platform abstractions) ─── depends on Phase 1
Phase 3 (Device I/O) ─── depends on Phase 2
Phase 4 (Security) ─── depends on Phase 2
Phase 5 (Build/CI) ─── can start alongside Phase 2
Phase 6 (Testing) ─── depends on Phases 2+3
```
