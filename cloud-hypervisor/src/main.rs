// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "main_unix.rs"]
mod main_impl;

#[cfg(all(not(unix), feature = "whp"))]
mod whp_demo;

#[cfg(not(unix))]
fn main() {
    env_logger::init();

    #[cfg(feature = "whp")]
    {
        use std::sync::mpsc::{channel, sync_channel};
        use platform::{EFD_NONBLOCK, EventFd};

        // Create the exit event and hypervisor
        let exit_evt = EventFd::new(EFD_NONBLOCK).expect("Failed to create exit EventFd");
        let api_evt = EventFd::new(EFD_NONBLOCK).expect("Failed to create API EventFd");
        let hypervisor = hypervisor::new().expect("No hypervisor found. Is WHP enabled?");

        println!(
            "cloud-hypervisor (Windows / WHP backend) v{}",
            env!("CARGO_PKG_VERSION")
        );
        println!(
            "Hypervisor: {:?}  (max vCPUs: {})",
            hypervisor.hypervisor_type(),
            hypervisor.get_max_vcpus()
        );

        let (api_sender, api_receiver) = channel::<vmm::ApiRequest>();

        // Start the VMM control loop thread
        let vmm_thread = vmm::start_vmm_thread(
            vmm::VmmVersionInfo::new(env!("BUILD_VERSION"), env!("CARGO_PKG_VERSION")),
            api_evt.try_clone().unwrap(),
            api_receiver,
            exit_evt.try_clone().unwrap(),
            hypervisor,
        )
        .expect("Failed to start VMM thread");

        // Parse CLI and build VmConfig
        let args: Vec<String> = std::env::args().collect();
        let get_arg = |name: &str| -> Option<String> {
            args.windows(2)
                .find(|w| w[0] == format!("--{name}"))
                .map(|w| w[1].clone())
        };

        let payload = vmm::vm_config::PayloadConfig {
            kernel: get_arg("kernel").map(std::path::PathBuf::from),
            initramfs: get_arg("initramfs").map(std::path::PathBuf::from),
            cmdline: get_arg("cmdline"),
            firmware: None,
            #[cfg(feature = "igvm")]
            igvm: None,
            #[cfg(feature = "sev_snp")]
            host_data: None,
            #[cfg(feature = "fw_cfg")]
            fw_cfg_config: None,
        };
        let disk_path = get_arg("disk");
        let mem_size = get_arg("memory")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(512) << 20; // Default 512 MiB

        let vm_config = vmm::vm_config::VmConfig {
            cpus: vmm::vm_config::CpusConfig::default(),
            memory: vmm::vm_config::MemoryConfig {
                size: mem_size,
                ..Default::default()
            },
            payload: Some(payload.clone()),
            rate_limit_groups: None,
            disks: None,
            net: None,
            rng: vmm::vm_config::RngConfig::default(),
            balloon: None,
            generic_vhost_user: None,
            fs: None,
            pmem: None,
            serial: vmm::vm_config::default_serial(),
            console: vmm::vm_config::default_console(),
            #[cfg(target_arch = "x86_64")]
            debug_console: vmm::vm_config::DebugConsoleConfig::default(),
            devices: None,
            user_devices: None,
            vdpa: None,
            vsock: None,
            #[cfg(feature = "pvmemcontrol")]
            pvmemcontrol: None,
            pvpanic: false,
            iommu: false,
            numa: None,
            watchdog: false,
            #[cfg(feature = "guest_debug")]
            gdb: false,
            pci_segments: None,
            platform: None,
            tpm: None,
            preserved_fds: None,
            landlock_enable: false,
            landlock_rules: None,
            #[cfg(feature = "ivshmem")]
            ivshmem: None,
        };

        // ── VmCreate API request ─────────────────────────────────────────
        let (create_sender, create_receiver) = sync_channel::<vmm::Result<()>>(1);

        api_sender
            .send(Box::new(move |vmm: &mut vmm::Vmm| {
                create_sender.send(vmm.vm_create(vm_config)).ok();
                Ok(false)
            }))
            .expect("Failed to send VmCreate request");
        api_evt.write(1).unwrap();

        match create_receiver.recv() {
            Ok(Ok(())) => println!("VM created"),
            Ok(Err(e)) => {
                eprintln!("VmCreate failed: {e:#}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("VmCreate channel error: {e}");
                std::process::exit(1);
            }
        }

        // ── VmBoot API request ──────────────────────────────────────────
        let (boot_sender, boot_receiver) = sync_channel::<vmm::Result<()>>(1);

        api_sender
            .send(Box::new(move |vmm: &mut vmm::Vmm| {
                let result = vmm.vm_boot(|exit_evt, vm, mm| {
                    whp_demo::boot(exit_evt, payload, disk_path, vm, mm)
                });
                boot_sender.send(result).ok();
                Ok(false)
            }))
            .expect("Failed to send VmBoot request");
        api_evt.write(1).unwrap();

        match boot_receiver.recv() {
            Ok(Ok(())) => println!("VM booted — vCPU running"),
            Ok(Err(e)) => {
                eprintln!("VmBoot failed: {e:#}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("VmBoot channel error: {e}");
                std::process::exit(1);
            }
        }

        // ── Wait for VMM control loop to exit ───────────────────────────
        // The vCPU thread signals exit_evt on shutdown, which causes
        // the control loop to break.
        println!("--- Guest running, waiting for shutdown... ---");
        match vmm_thread.thread_handle.join() {
            Ok(Ok(())) => println!("VMM exited cleanly"),
            Ok(Err(e)) => eprintln!("VMM error: {e:#}"),
            Err(e) => eprintln!("VMM thread panic: {e:?}"),
        }
    }

    #[cfg(not(feature = "whp"))]
    {
        eprintln!("cloud-hypervisor requires the 'whp' feature on Windows.");
        std::process::exit(1);
    }
}

#[cfg(unix)]
fn main() {
    main_impl::main_impl();
}