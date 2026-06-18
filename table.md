# Log severity mismatches

## Clear `error!()` mismatches

| Site | Why it does not fit `error!` | Suggested |
|---|---|---|
| `vmm/src/lib.rs:2248, 2283, 2299, 2321, 2352, 2371, 2402, 2430, 2466, 2494, 2522, 2550, 2583, 2602` | VM API resize/hotplug/counters failures are returned to the caller; docs cite failed API requests as `warn!` examples. | `warn!` |
| `vmm/src/device_manager.rs:6050, 6070` | Guest/device register access and ejection failures are handled in an I/O path; execution continues. | `warn!` |
| `vmm/src/device_manager.rs:760, 802` | BAR move rollback failure is returned from `move_bar()`, not an imminent process exit. | `warn!` |
| `vmm/src/memory_manager.rs:2559, 2566, 2576, 2612, 2627` | Resize/config/utility failures return errors or fallback values; not immediate unrecoverable process failures. | `warn!` |

## Clear `info!()` mismatches

| Site | Why it does not fit `info!` | Suggested |
|---|---|---|
| `arch/src/x86_64/mod.rs:1173, 1235` | Per-memory-range boot messages in loops. | `debug!` |
| `net_util/src/ctrl_queue.rs:145` | Per-TAP offload reprogramming inside a loop. | `debug!` |
| `net_util/src/queue_pair.rs:513, 523` | TAP EAGAIN/recovery state changes can occur under network pressure. | `debug!` |
| `vmm/src/vm.rs:465, 474, 477, 479, 489, 499, 502, 504` | Guest MMIO/PIO missing-address accesses and barrier waits are guest/device-path events, potentially repeated. | `debug!` |
| `vmm/src/cpu.rs:2101, 2113` | Guest VA translation states can be hit repeatedly by guest behavior. | `debug!` |
| `virtio-devices/src/vsock/csm/connection.rs:341` | Empty guest vsock packets are consumed and may repeat. | `debug!` |

## Likely `warn!()` mismatches/noisy candidates

| Site | Why | Suggested |
|---|---|---|
| `vmm/src/cpu.rs:1416, 1418` | Unsupported TDX VMCALL notices are in the vCPU run path and may repeat. | `info!` or `debug!` |
| `hypervisor/src/mshv/mod.rs:710` | Attribute intercept with full host visibility is explicitly ignored. | `debug!` |
| `virtio-devices/src/vsock/csm/connection.rs:284` | `EWOULDBLOCK` after readiness is handled gracefully. | `debug!` |
