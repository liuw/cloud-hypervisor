// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[path = "main_unix.rs"]
mod main_impl;

#[cfg(not(unix))]
fn main() {
    env_logger::init();

    println!("cloud-hypervisor (Windows / WHP backend)");
    println!();

    if let Err(e) = run_whp_demo() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run_whp_demo() -> anyhow::Result<()> {
    use std::sync::Arc;

    use anyhow::{Context, anyhow};
    use hypervisor::{Vm, VmOps};

    // ── Probe hypervisor ────────────────────────────────────────────────
    let hv = hypervisor::new().context("No hypervisor found. Is WHP enabled?")?;
    println!("Hypervisor: {:?}  (max vCPUs: {})", hv.hypervisor_type(), hv.get_max_vcpus());

    // ── Create VM ───────────────────────────────────────────────────────
    let config = hypervisor::HypervisorVmConfig::default();
    let vm = hv.create_vm(config).context("Failed to create VM")?;

    // ── Allocate guest RAM ────────────────────────────────────────────────
    const MEM_SIZE: usize = 4 << 20; // 4 MiB

    let layout = std::alloc::Layout::from_size_align(MEM_SIZE, 4096).unwrap();
    // SAFETY: Allocating page-aligned zeroed memory.
    let host_mem = unsafe { std::alloc::alloc_zeroed(layout) };
    if host_mem.is_null() {
        return Err(anyhow!("Failed to allocate guest memory"));
    }

    // SAFETY: Memory is valid and will remain valid for the VM's lifetime.
    unsafe {
        vm.create_user_memory_region(0, 0, MEM_SIZE, host_mem, false, false)
            .context("Failed to map guest memory")?;
    }
    println!("Mapped {MEM_SIZE} bytes of guest RAM at GPA 0x0");

    // ── Write a tiny real-mode payload at 0x1000 ────────────────────────
    //
    // We'll place code at 0x1000 and set RIP there directly, avoiding
    // the need to handle the reset vector correctly.
    //
    // Payload: write "Hi!\n" to serial port 0x3F8, then halt.
    let payload: &[u8] = &[
        0xBA, 0xF8, 0x03, // mov dx, 0x3F8
        0xB0, b'H',       // mov al, 'H'
        0xEE,             // out dx, al
        0xB0, b'i',       // mov al, 'i'
        0xEE,             // out dx, al
        0xB0, b'!',       // mov al, '!'
        0xEE,             // out dx, al
        0xB0, b'\n',      // mov al, '\n'
        0xEE,             // out dx, al
        // Shutdown: write 0x34 to port 0x501 (QEMU debug exit)
        0xBA, 0x01, 0x05, // mov dx, 0x501
        0xB0, 0x34,       // mov al, 0x34
        0xEE,             // out dx, al
        0xFA,             // cli
        0xF4,             // hlt
    ];

    let code_addr = 0x1000_usize;
    // SAFETY: Writing within our allocated memory region.
    unsafe {
        std::ptr::copy_nonoverlapping(
            payload.as_ptr(),
            host_mem.add(code_addr),
            payload.len(),
        );
    }
    println!("Loaded {} byte payload at GPA {code_addr:#X}", payload.len());

    // ── Create vCPU with VmOps for I/O handling ─────────────────────────
    struct SimpleVmOps;

    impl VmOps for SimpleVmOps {
        fn guest_mem_write(&self, _gpa: u64, _buf: &[u8]) -> Result<usize, hypervisor::HypervisorVmError> {
            Ok(0)
        }
        fn guest_mem_read(&self, _gpa: u64, _buf: &mut [u8]) -> Result<usize, hypervisor::HypervisorVmError> {
            Ok(0)
        }
        fn mmio_read(&self, _gpa: u64, _data: &mut [u8]) -> Result<(), hypervisor::HypervisorVmError> {
            Ok(())
        }
        fn mmio_write(&self, _gpa: u64, _data: &[u8]) -> Result<(), hypervisor::HypervisorVmError> {
            Ok(())
        }
        fn pio_read(&self, _port: u64, data: &mut [u8]) -> Result<(), hypervisor::HypervisorVmError> {
            data.fill(0xFF);
            Ok(())
        }
        fn pio_write(&self, port: u64, data: &[u8]) -> Result<(), hypervisor::HypervisorVmError> {
            if port == 0x3F8 && !data.is_empty() {
                // Serial port output — print to host console
                let ch = data[0];
                if ch.is_ascii() {
                    print!("{}", ch as char);
                }
            } else if port == 0x501 {
                // Debug exit port — signal shutdown
                println!();
                println!("--- Guest requested shutdown ---");
                std::process::exit(0);
            }
            Ok(())
        }
    }

    let vm_ops: Arc<dyn VmOps> = Arc::new(SimpleVmOps);
    let mut vcpu = vm.create_vcpu(0, Some(vm_ops))
        .context("Failed to create vCPU")?;

    // Set RIP to our code at 0x1000. Also set CS base and selector to 0
    // so RIP maps directly to physical address.
    {
        let mut regs = vcpu.get_regs().context("get_regs")?;
        regs.set_rip(code_addr as u64);
        regs.set_rflags(0x2); // Bit 1 always set
        vcpu.set_regs(&regs).context("set_regs")?;

        // Set CS base to 0 so CS:RIP = 0:0x1000 = physical 0x1000
        let mut sregs = vcpu.get_sregs().context("get_sregs")?;
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        sregs.cs.limit = 0xFFFF;
        sregs.cs.type_ = 0xB; // Execute/Read/Accessed code segment
        sregs.cs.s = 1;       // Code/data segment
        sregs.cs.dpl = 0;
        sregs.cs.present = 1;
        sregs.cs.db = 0;      // 16-bit default
        sregs.cs.g = 0;       // Byte granularity
        sregs.cs.l = 0;       // Not 64-bit
        // Also set DS/ES/SS for data access
        sregs.ds = sregs.cs;
        sregs.ds.type_ = 0x3; // Read/Write/Accessed data segment
        sregs.es = sregs.ds;
        sregs.ss = sregs.ds;
        vcpu.set_sregs(&sregs).context("set_sregs")?;
    }

    let regs = vcpu.get_regs().unwrap();
    println!("vCPU 0 created, RIP={:#x}. Running...", regs.get_rip());

    println!("vCPU 0 created. Running payload...");
    println!("--- Guest serial output ---");

    // ── Run the vCPU ────────────────────────────────────────────────────
    loop {
        match vcpu.run() {
            Ok(hypervisor::VmExit::Ignore) => continue,
            Ok(hypervisor::VmExit::Shutdown) => {
                println!("--- Guest halted ---");
                break;
            }
            Ok(hypervisor::VmExit::Reset) => {
                println!("--- Guest reset ---");
                break;
            }
            Ok(exit) => {
                println!("--- VM exit: {exit:?} ---");
                break;
            }
            Err(e) => {
                eprintln!("vCPU run error: {e}");
                break;
            }
        }
    }

    // Cleanup
    // SAFETY: We allocated this memory and it hasn't been freed.
    unsafe {
        std::alloc::dealloc(host_mem, layout);
    };

    println!("\nWHP demo complete.");
    Ok(())
}

#[cfg(unix)]
fn main() {
    main_impl::main_impl();
}