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
        use std::sync::mpsc::channel;
        use platform::{EFD_NONBLOCK, EventFd};

        // Create the exit event and hypervisor — shared between VMM thread and demo
        let exit_evt = EventFd::new(EFD_NONBLOCK).expect("Failed to create exit EventFd");
        let api_evt = EventFd::new(EFD_NONBLOCK).expect("Failed to create API EventFd");
        let hypervisor = hypervisor::new().expect("No hypervisor found. Is WHP enabled?");

        let (_api_sender, api_receiver) = channel::<vmm::ApiRequest>();

        // Start the VMM control loop thread
        let vmm_thread = vmm::start_vmm_thread(
            vmm::VmmVersionInfo::new(env!("BUILD_VERSION"), env!("CARGO_PKG_VERSION")),
            api_evt,
            api_receiver,
            exit_evt.try_clone().unwrap(),
            hypervisor,
        )
        .expect("Failed to start VMM thread");

        // Run the WHP demo (VM setup + vCPU execution) on the main thread.
        // The demo signals exit_evt when the guest shuts down, which causes
        // the VMM control loop to exit.
        if let Err(e) = whp_demo::run(exit_evt) {
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