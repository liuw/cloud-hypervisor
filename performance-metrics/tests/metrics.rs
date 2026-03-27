// Copyright © 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

//! Nextest-compatible performance metric tests.
//!
//! Each `#[test]` function wraps a single entry from the performance metrics
//! TEST_LIST so that `cargo nextest` can discover, filter, and run them
//! individually.
//!
//! The tests are gated behind `cfg(devcli_testenv)` because they require the
//! dev CLI environment (hugepages, workload images, etc.).

#![cfg(any(devcli_testenv, clippy))]
#![allow(non_snake_case)]

use performance_metrics::{PerformanceTestOverrides, run_test_by_name};

macro_rules! perf_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let overrides = PerformanceTestOverrides::default();
                run_test_by_name(stringify!($name), &overrides).unwrap();
            }
        )*
    };
}

perf_tests! {
    // Boot time
    boot_time_ms,
    boot_time_pmem_ms,
    boot_time_16_vcpus_ms,
    boot_time_16_vcpus_pmem_ms,

    // Restore latency
    restore_latency_time_ms,

    // Network throughput
    virtio_net_throughput_single_queue_rx_gbps,
    virtio_net_throughput_single_queue_tx_gbps,
    virtio_net_throughput_multi_queue_rx_gbps,
    virtio_net_throughput_multi_queue_tx_gbps,
    virtio_net_throughput_single_queue_rx_pps,
    virtio_net_throughput_single_queue_tx_pps,
    virtio_net_throughput_multi_queue_rx_pps,
    virtio_net_throughput_multi_queue_tx_pps,

    // Block I/O (raw) - bandwidth
    block_read_MiBps,
    block_write_MiBps,
    block_random_read_MiBps,
    block_random_write_MiBps,
    block_multi_queue_read_MiBps,
    block_multi_queue_write_MiBps,
    block_multi_queue_random_read_MiBps,
    block_multi_queue_random_write_MiBps,

    // Block I/O (raw) - IOPS
    block_read_IOPS,
    block_write_IOPS,
    block_random_read_IOPS,
    block_random_write_IOPS,
    block_multi_queue_read_IOPS,
    block_multi_queue_write_IOPS,
    block_multi_queue_random_read_IOPS,
    block_multi_queue_random_write_IOPS,

    // Block I/O (qcow2 uncompressed)
    block_qcow2_read_MiBps,
    block_qcow2_random_read_MiBps,
    block_qcow2_read_warm_MiBps,
    block_qcow2_multi_queue_read_MiBps,
    block_qcow2_multi_queue_random_read_MiBps,
    block_qcow2_multi_queue_read_warm_MiBps,

    // Block I/O (qcow2 zlib)
    block_qcow2_zlib_read_MiBps,
    block_qcow2_zlib_random_read_MiBps,
    block_qcow2_zlib_read_warm_MiBps,
    block_qcow2_zlib_multi_queue_read_MiBps,
    block_qcow2_zlib_multi_queue_random_read_MiBps,
    block_qcow2_zlib_multi_queue_read_warm_MiBps,

    // Block I/O (qcow2 zstd)
    block_qcow2_zstd_read_MiBps,
    block_qcow2_zstd_random_read_MiBps,
    block_qcow2_zstd_read_warm_MiBps,
    block_qcow2_zstd_multi_queue_read_MiBps,
    block_qcow2_zstd_multi_queue_random_read_MiBps,
    block_qcow2_zstd_multi_queue_read_warm_MiBps,

    // Block I/O (qcow2 with backing files)
    block_qcow2_backing_qcow2_read_MiBps,
    block_qcow2_backing_qcow2_random_read_MiBps,
    block_qcow2_backing_raw_read_MiBps,
    block_qcow2_backing_raw_random_read_MiBps,
    block_qcow2_backing_qcow2_read_warm_MiBps,
    block_qcow2_backing_raw_read_warm_MiBps,
    block_qcow2_multi_queue_backing_qcow2_read_MiBps,
    block_qcow2_multi_queue_backing_qcow2_random_read_MiBps,
    block_qcow2_multi_queue_backing_raw_read_MiBps,
    block_qcow2_multi_queue_backing_raw_random_read_MiBps,
    block_qcow2_multi_queue_backing_qcow2_read_warm_MiBps,
    block_qcow2_multi_queue_backing_raw_read_warm_MiBps,

    // Micro benchmarks
    micro_block_raw_aio_drain_128_us,
    micro_block_raw_aio_drain_256_us,
}

// virtio_net_latency_us is excluded on aarch64 (no ethr support)
#[cfg(not(target_arch = "aarch64"))]
#[test]
fn virtio_net_latency_us() {
    let overrides = PerformanceTestOverrides::default();
    run_test_by_name("virtio_net_latency_us", &overrides).unwrap();
}
