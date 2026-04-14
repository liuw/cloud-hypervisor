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

        // Send VmCreate API request to the VMM thread, get back VM + memory
        let (response_sender, response_receiver) = sync_channel::<
            vmm::Result<(
                std::sync::Arc<dyn hypervisor::Vm>,
                std::sync::Arc<std::sync::Mutex<vmm::memory_manager::MemoryManager>>,
            )>,
        >(1);
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

        api_sender
            .send(Box::new(move |vmm: &mut vmm::Vmm| {
                let result = vmm.vm_create(vm_config).map(|()| {
                    (
                        vmm.vm().unwrap().clone(),
                        vmm.memory_manager().unwrap().clone(),
                    )
                });
                response_sender.send(result).ok();
                Ok(false) // don't exit the control loop
            }))
            .expect("Failed to send VmCreate request");
        api_evt.write(1).unwrap();

        // Wait for VM creation to complete
        let (vm, memory_manager) = match response_receiver.recv() {
            Ok(Ok(handles)) => {
                println!("VM created via VMM control loop");
                handles
            }
            Ok(Err(e)) => {
                eprintln!("VmCreate failed: {e:#}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("VmCreate response error: {e}");
                std::process::exit(1);
            }
        };

        // Run the WHP demo with the VM and memory from the VMM.
        if let Err(e) = whp_demo::run(exit_evt, payload, disk_path, vm, memory_manager) {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }

        // Wait for VMM thread to finish
        if let Err(e) = vmm_thread.thread_handle.join() {
            eprintln!("VMM thread error: {e:?}");
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