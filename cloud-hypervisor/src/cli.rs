// SPDX-License-Identifier: Apache-2.0
//
// Shared CLI definition for cloud-hypervisor.
//
// This module defines the clap `Command` with all argument definitions,
// used by both the Unix and Windows binary entry points.

use clap::{Arg, ArgAction, ArgGroup, Command};
use vmm::config::RestoreConfig;
use vmm::vm_config;
use vmm::vm_config::{
    BalloonConfig, DeviceConfig, DiskConfig, FsConfig, GenericVhostUserConfig, LandlockConfig,
    NetConfig, NumaConfig, PciSegmentConfig, PlatformConfig, PmemConfig, RateLimiterGroupConfig,
    TpmConfig, UserDeviceConfig, VdpaConfig, VsockConfig,
};
#[cfg(feature = "fw_cfg")]
use vmm::vm_config::FwCfgConfig;
#[cfg(feature = "ivshmem")]
use vmm::vm_config::IvshmemConfig;

pub fn prepare_default_values() -> (String, String, String) {
    (default_vcpus(), default_memory(), default_rng())
}

fn default_vcpus() -> String {
    format!(
        "boot={},max_phys_bits={}",
        vm_config::DEFAULT_VCPUS,
        vm_config::DEFAULT_MAX_PHYS_BITS
    )
}

fn default_memory() -> String {
    format!("size={}M", vm_config::DEFAULT_MEMORY_MB)
}

fn default_rng() -> String {
    format!("src={}", vm_config::DEFAULT_RNG_SOURCE)
}

/// Creates the CLI definition of Cloud Hypervisor.
pub fn create_app(default_vcpus: String, default_memory: String, default_rng: String) -> Command {
    let groups = [
        ArgGroup::new("vm-config")
            .multiple(true)
            .requires("vm-payload"),
        ArgGroup::new("vmm-config").multiple(true),
        ArgGroup::new("logging").multiple(true),
        ArgGroup::new("vm-payload").multiple(true),
    ];

    let args = get_cli_options_sorted(default_vcpus, default_memory, default_rng);

    Command::new("cloud-hypervisor")
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("Launch a cloud-hypervisor VMM.")
        .arg_required_else_help(true)
        .groups(groups)
        .args(args)
}

/// Returns all [`Arg`]s in alphabetical order.
fn get_cli_options_sorted(
    default_vcpus: String,
    default_memory: String,
    default_rng: String,
) -> Box<[Arg]> {
    [
        Arg::new("api-socket")
            .long("api-socket")
            .help("HTTP API socket (UNIX domain socket): path=</path/to/a/file> or fd=<fd>.")
            .num_args(1)
            .group("vmm-config"),
        Arg::new("balloon")
            .long("balloon")
            .help(BalloonConfig::SYNTAX)
            .num_args(1)
            .group("vm-config"),
        Arg::new("cmdline")
            .long("cmdline")
            .help("Kernel command line")
            .num_args(1)
            .group("vm-config"),
        Arg::new("console")
            .long("console")
            .help("Control (virtio) console: \"off|null|pty|tty|file=</path/to/a/file>,iommu=on|off\"")
            .default_value("tty")
            .group("vm-config"),
        Arg::new("cpus")
            .long("cpus")
            .help(
                "boot=<boot_vcpus>,max=<max_vcpus>,\
                    topology=<threads_per_core>:<cores_per_die>:<dies_per_package>:<packages>,\
                    kvm_hyperv=on|off,max_phys_bits=<maximum_number_of_physical_bits>,\
                    affinity=<list_of_vcpus_with_their_associated_cpuset>,\
                    features=<list_of_features_to_enable>,\
                    nested=on|off,core_scheduling=vm|vcpu|off",
            )
            .default_value(default_vcpus)
            .group("vm-config"),
        #[cfg(feature = "dbus_api")]
        Arg::new("dbus-object-path")
            .long("dbus-object-path")
            .help("Object path to serve the dbus interface")
            .num_args(1)
            .group("vmm-config"),
        #[cfg(feature = "dbus_api")]
        Arg::new("dbus-service-name")
            .long("dbus-service-name")
            .help("Well known name of the device")
            .num_args(1)
            .group("vmm-config"),
        #[cfg(feature = "dbus_api")]
        Arg::new("dbus-system-bus")
            .long("dbus-system-bus")
            .action(ArgAction::SetTrue)
            .help("Use the system bus instead of a session bus")
            .num_args(0)
            .group("vmm-config"),
        #[cfg(target_arch = "x86_64")]
        Arg::new("debug-console")
            .long("debug-console")
            .help("Debug console: off|pty|tty|file=</path/to/a/file>,iobase=<port in hex>")
            .default_value("off,iobase=0xe9")
            .group("vm-config"),
        Arg::new("device")
            .long("device")
            .help(DeviceConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("disk")
            .long("disk")
            .help(DiskConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("event-monitor")
            .long("event-monitor")
            .help("File to report events on: path=</path/to/a/file> or fd=<fd>")
            .num_args(1)
            .group("vmm-config"),
        Arg::new("firmware")
            .long("firmware")
            .help("Path to firmware that is loaded in an architectural specific way")
            .num_args(1)
            .group("vm-payload"),
        Arg::new("fs")
            .long("fs")
            .help(FsConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        #[cfg(feature = "fw_cfg")]
        Arg::new("fw-cfg-config")
            .long("fw-cfg-config")
            .help(FwCfgConfig::SYNTAX)
            .num_args(1)
            .group("vm-payload"),
        #[cfg(feature = "guest_debug")]
        Arg::new("gdb")
            .long("gdb")
            .help("GDB socket (UNIX domain socket): path=</path/to/a/file>")
            .num_args(1)
            .group("vmm-config"),
        Arg::new("generic-vhost-user")
            .long("generic-vhost-user")
            .help(GenericVhostUserConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        #[cfg(feature = "igvm")]
        Arg::new("igvm")
            .long("igvm")
            .help("Path to IGVM file to load.")
            .num_args(1)
            .group("vm-payload"),
        #[cfg(feature = "sev_snp")]
        Arg::new("host-data")
            .long("host-data")
            .help("Host specific data to SEV SNP guest")
            .num_args(1)
            .group("vm-config"),
        Arg::new("initramfs")
            .long("initramfs")
            .help("Path to initramfs image")
            .num_args(1)
            .group("vm-config"),
        #[cfg(feature = "ivshmem")]
        Arg::new("ivshmem")
            .long("ivshmem")
            .help(IvshmemConfig::SYNTAX)
            .num_args(1)
            .group("vm-config"),
        Arg::new("kernel")
            .long("kernel")
            .help(
                "Path to kernel to load. This may be a kernel or firmware that supports a PVH \
                entry point (e.g. vmlinux) or architecture equivalent",
            )
            .num_args(1)
            .group("vm-payload"),
        Arg::new("landlock")
            .long("landlock")
            .num_args(0)
            .help("enable/disable Landlock.")
            .action(ArgAction::SetTrue)
            .default_value("false")
            .group("vm-config"),
        Arg::new("landlock-rules")
            .long("landlock-rules")
            .help(LandlockConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("log-file")
            .long("log-file")
            .help("Log file. Standard error is used if not specified")
            .num_args(1)
            .group("logging"),
        Arg::new("memory")
            .long("memory")
            .help(
                "Memory parameters \
                     \"size=<guest_memory_size>,mergeable=on|off,shared=on|off,\
                     hugepages=on|off,hugepage_size=<hugepage_size>,\
                     hotplug_method=acpi|virtio-mem,\
                     hotplug_size=<hotpluggable_memory_size>,\
                     hotplugged_size=<hotplugged_memory_size>,\
                     prefault=on|off,thp=on|off\"",
            )
            .default_value(default_memory)
            .group("vm-config"),
        Arg::new("memory-zone")
            .long("memory-zone")
            .help(
                "User defined memory zone parameters \
                     \"size=<guest_memory_region_size>,file=<backing_file>,\
                     shared=on|off,\
                     hugepages=on|off,hugepage_size=<hugepage_size>,\
                     host_numa_node=<node_id>,\
                     id=<zone_identifier>,hotplug_size=<hotpluggable_memory_size>,\
                     hotplugged_size=<hotplugged_memory_size>,\
                     prefault=on|off\"",
            )
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("net")
            .long("net")
            .help(NetConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("numa")
            .long("numa")
            .help(NumaConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("pci-segment")
            .long("pci-segment")
            .help(PciSegmentConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("platform")
            .long("platform")
            .help(PlatformConfig::syntax())
            .num_args(1)
            .group("vm-config"),
        Arg::new("pmem")
            .long("pmem")
            .help(PmemConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        #[cfg(feature = "pvmemcontrol")]
        Arg::new("pvmemcontrol")
            .long("pvmemcontrol")
            .help("Pvmemcontrol device")
            .num_args(0)
            .action(ArgAction::SetTrue)
            .group("vm-config"),
        Arg::new("pvpanic")
            .long("pvpanic")
            .help("Enable pvpanic device")
            .num_args(0)
            .action(ArgAction::SetTrue)
            .group("vm-config"),
        Arg::new("rate-limit-group")
            .long("rate-limit-group")
            .help(RateLimiterGroupConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("restore")
            .long("restore")
            .help(RestoreConfig::SYNTAX)
            .num_args(1)
            .group("vmm-config"),
        Arg::new("rng")
            .long("rng")
            .help(
                "Random number generator parameters \"src=<entropy_source_path>,iommu=on|off\"",
            )
            .default_value(default_rng)
            .group("vm-config"),
        Arg::new("seccomp")
            .long("seccomp")
            .num_args(1)
            .value_parser(["true", "false", "log"])
            .default_value("true"),
        Arg::new("serial")
            .long("serial")
            .help("Control serial port: off|null|pty|tty|file=</path/to/a/file>|socket=</path/to/a/file>")
            .default_value("null")
            .group("vm-config"),
        Arg::new("tpm")
            .long("tpm")
            .num_args(1)
            .help(TpmConfig::SYNTAX)
            .group("vm-config"),
        Arg::new("user-device")
            .long("user-device")
            .help(UserDeviceConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("v")
            .short('v')
            .action(ArgAction::Count)
            .help("Sets the level of debugging output")
            .group("logging"),
        Arg::new("vdpa")
            .long("vdpa")
            .help(VdpaConfig::SYNTAX)
            .num_args(1..)
            .action(ArgAction::Append)
            .group("vm-config"),
        Arg::new("version")
            .short('V')
            .long("version")
            .action(ArgAction::SetTrue)
            .help("Print version")
            .num_args(0),
        Arg::new("vsock")
            .long("vsock")
            .help(VsockConfig::SYNTAX)
            .num_args(1)
            .group("vm-config"),
        Arg::new("watchdog")
            .long("watchdog")
            .help("Enable virtio-watchdog")
            .num_args(0)
            .action(ArgAction::SetTrue)
            .group("vm-config"),
    ]
    .to_vec()
    .into_boxed_slice()
}
