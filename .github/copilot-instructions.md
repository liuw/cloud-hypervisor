# Copilot Instructions for Cloud Hypervisor

## Build, Check, and Lint Commands

```sh
# Format (requires nightly)
cargo +nightly fmt --all

# Check all targets
cargo check --all-targets --tests

# Clippy (treats warnings as errors)
cargo clippy --all-targets --tests -- -D warnings

# Unit tests (does NOT include integration tests)
cargo test --all-targets --tests

# Check a single crate (useful for iterating on one module)
cargo check -p <crate-name>
cargo clippy -p <crate-name>

# Check with a specific feature
cargo check -p hypervisor --features whp

# Gitlint on recent commits
gitlint --commits "HEAD~3..HEAD"
```

## Architecture

### Hypervisor Abstraction

The `hypervisor` crate provides trait-based abstraction with three core traits in `hypervisor/src/`:
- **`Hypervisor`** (`hypervisor.rs`) — partition/VM creation, CPUID queries
- **`Vm`** (`vm.rs`) — memory mapping, vCPU creation, interrupt routing
- **`Vcpu`** (`cpu.rs`) — register access, vCPU execution, state save/restore

Backends are feature-gated modules:
- `kvm` — Linux KVM (feature `kvm`)
- `mshv` — Microsoft Hypervisor on Linux (feature `mshv`)
- `whp` — Windows Hypervisor Platform (feature `whp`, Windows-only)

Shared enums (`HypervisorType`, `CpuState`, `StandardRegisters`, `ClockData`, etc.) in `lib.rs` have per-backend variants gated by `#[cfg(feature = "...")]`.

### Platform Abstraction

The `platform` crate provides cross-platform wrappers:
- **`EventFd`** — counting wake primitive (re-exports `vmm_sys_util::eventfd::EventFd` on Unix; AtomicU64 + Win32 Event on Windows)
- **`signal`** — termination handler (`signal_hook` on Unix, `SetConsoleCtrlHandler` on Windows)
- **`terminal`** — opaque terminal state save/restore (`termios` on Unix, Console Mode on Windows)
- **`clock`** — UTC time queries (`clock_gettime` on Unix, `GetSystemTime` on Windows)

Import `EventFd` from `platform::EventFd`, not `vmm_sys_util::eventfd::EventFd`.

### vm-memory Portability

The workspace uses `vm-memory` with `default-features = false` to avoid the `rawfd` feature, which emits `compile_error!` on Windows. A local patch (`vm-memory-patch/`) makes `rawfd` a no-op on Windows instead. Crates that don't need raw fd I/O should not request the `rawfd` feature. The `rawfd` feature still gets unified across the workspace via `virtio-queue`'s defaults.

### Linux-Only APIs

Several `Vm` trait methods are `#[cfg(unix)]` because they use `EventFd` as a parameter type:
- `register_irqfd`, `unregister_irqfd`, `register_ioevent`, `unregister_ioevent`
- `create_passthrough_device` (returns `vfio_ioctls::VfioDeviceFd`)

Subsystems that are entirely Linux-specific are cfg-gated at the module level:
- `pci::vfio`, `pci::vfio_user`, `pci::mmap` — VFIO device passthrough
- `devices::tpm` — uses Unix sockets
- AMX support in `hypervisor::arch::x86` — uses `libc::syscall(SYS_arch_prctl, ...)`

## Commit Message Format

Follow the project convention from CONTRIBUTING.md:

```
<component>: Change summary

More detailed explanation of your changes.
Wrap to 72 characters.

Signed-off-by: Name <email>
```

Valid components include each cargo workspace member name (e.g., `hypervisor`, `vmm`, `platform`, `devices`, `pci`) plus `build`, `ci`, `docs`, and `misc`. Use `*` when a change spans many crates.

## Windows-rs API Signatures

When using the `windows` crate (for WHP, Console, or other Win32 APIs), always check the actual Rust function signatures in the downloaded crate source before writing code. The `windows` crate wraps C APIs with Rust-idiomatic signatures that often differ significantly from the C originals:

- Functions may return `Result<T>` instead of taking out-parameters (e.g., `WHvCreatePartition()` returns `Result<WHV_PARTITION_HANDLE>`, not taking `*mut` param)
- Bitfield structs use `_bitfield: u64` instead of named fields
- Union types require `unsafe` field access and have `Anonymous` + raw value fields
- `BOOL` is `windows_core::BOOL`, not in `Win32::Foundation`
- Slice-taking functions often need explicit `.as_ptr()` + length args
