// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "main_unix.rs"]
mod main_impl;

#[cfg(not(unix))]
fn main() {
    eprintln!("cloud-hypervisor is not yet fully supported on this platform.");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    main_impl::main_impl();
}