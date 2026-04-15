// SPDX-License-Identifier: Apache-2.0

mod cli;

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
        use vmm::config::VmParams;

        // Parse CLI using shared arg definitions
        let (default_vcpus, default_memory, default_rng) = cli::prepare_default_values();
        let app = cli::create_app(default_vcpus, default_memory, default_rng);
        let cmd_arguments = app.get_matches();

        if cmd_arguments.get_flag("version") {
            println!("cloud-hypervisor v{}", env!("CARGO_PKG_VERSION"));
            return;
        }

        // Build VmConfig from parsed args
        let vm_params = VmParams::from_arg_matches(&cmd_arguments);
        let vm_config = vmm::vm_config::VmConfig::parse(vm_params)
            .expect("Failed to parse VM configuration");

        let payload = vm_config.payload.clone();
        let disk_path = vm_config.disks.as_ref()
            .and_then(|d| d.first())
            .and_then(|d| d.path.as_ref())
            .map(|p| p.to_string_lossy().into_owned());

        // Create hypervisor and VMM thread
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

        let vmm_thread = vmm::start_vmm_thread(
            vmm::VmmVersionInfo::new(env!("BUILD_VERSION"), env!("CARGO_PKG_VERSION")),
            api_evt.try_clone().unwrap(),
            api_receiver,
            exit_evt.try_clone().unwrap(),
            hypervisor,
        )
        .expect("Failed to start VMM thread");

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
                let result = vmm.vm_boot(|exit_evt, vm, mm, vm_ops, serial, io_bus| {
                    let p = payload.unwrap_or(vmm::vm_config::PayloadConfig {
                        firmware: None, kernel: None, cmdline: None, initramfs: None,
                        #[cfg(feature = "igvm")] igvm: None,
                        #[cfg(feature = "sev_snp")] host_data: None,
                        #[cfg(feature = "fw_cfg")] fw_cfg_config: None,
                    });
                    whp_demo::boot(exit_evt, p, disk_path, vm, mm, vm_ops, serial, io_bus)
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