// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "main_unix.rs"]
mod main_impl;

#[cfg(not(unix))]
fn main() {
    env_logger::init();

    println!("cloud-hypervisor (Windows / WHP backend)");
    println!();

    // Probe for WHP availability
    match hypervisor::new() {
        Ok(hv) => {
            println!("Hypervisor: {:?}", hv.hypervisor_type());
            println!("Max vCPUs:  {}", hv.get_max_vcpus());

            // Create a VM partition
            let config = hypervisor::HypervisorVmConfig::default();
            match hv.create_vm(config) {
                Ok(vm) => {
                    println!("VM created successfully!");

                    // Create a vCPU
                    match vm.create_vcpu(0, None) {
                        Ok(vcpu) => {
                            let regs = vcpu.get_regs().unwrap();
                            println!("vCPU 0 created — RIP: {:#x}", regs.get_rip());
                            println!();
                            println!("WHP backend is functional.");
                            println!("Full VMM (event loop, devices, kernel loading) not yet ported.");
                        }
                        Err(e) => eprintln!("Failed to create vCPU: {e}"),
                    }
                }
                Err(e) => eprintln!("Failed to create VM: {e}"),
            }
        }
        Err(e) => {
            eprintln!("No supported hypervisor found: {e}");
            eprintln!("Ensure Hyper-V / Windows Hypervisor Platform is enabled.");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn main() {
    main_impl::main_impl();
}