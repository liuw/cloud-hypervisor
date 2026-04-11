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
    if let Err(e) = whp_demo::run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
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