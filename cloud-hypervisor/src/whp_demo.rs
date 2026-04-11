// SPDX-License-Identifier: Apache-2.0
//
// Windows Hypervisor Platform demo: load and execute a guest payload.
//
// Usage:
//   cloud-hypervisor.exe                       # run built-in "Hi!" payload
//   cloud-hypervisor.exe --kernel <bzImage>     # load a Linux bzImage

use std::fs::File;
use std::io::Read;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use hypervisor::{HypervisorVmError, VmOps};

// ── Constants ────────────────────────────────────────────────────────────────

const GUEST_MEM_SIZE: usize = 128 << 20; // 128 MiB
const SERIAL_PORT: u64 = 0x3F8;
const DEBUG_EXIT_PORT: u64 = 0x501;

// Linux boot protocol addresses
const BOOT_PARAMS_ADDR: u64 = 0x7000;
const CMDLINE_ADDR: u64 = 0x20000;
const KERNEL_LOAD_ADDR: u64 = 0x100000; // 1 MiB — protected-mode kernel

pub fn run() -> anyhow::Result<()> {
    // Parse minimal CLI
    let args: Vec<String> = std::env::args().collect();
    let kernel_path = args
        .windows(2)
        .find(|w| w[0] == "--kernel")
        .map(|w| w[1].clone());

    println!("cloud-hypervisor (Windows / WHP backend)");
    println!();

    // ── Create hypervisor and VM ─────────────────────────────────────────
    let hv = hypervisor::new().context("No hypervisor found. Is WHP enabled?")?;
    println!(
        "Hypervisor: {:?}  (max vCPUs: {})",
        hv.hypervisor_type(),
        hv.get_max_vcpus()
    );

    let config = hypervisor::HypervisorVmConfig::default();
    let vm = hv.create_vm(config).context("Failed to create VM")?;

    // ── Allocate guest memory ────────────────────────────────────────────
    let layout = std::alloc::Layout::from_size_align(GUEST_MEM_SIZE, 4096).unwrap();
    // SAFETY: Allocating page-aligned zeroed memory for the guest.
    let host_mem = unsafe { std::alloc::alloc_zeroed(layout) };
    if host_mem.is_null() {
        return Err(anyhow!("Failed to allocate {} MiB guest memory", GUEST_MEM_SIZE >> 20));
    }

    // SAFETY: host_mem is valid for GUEST_MEM_SIZE bytes.
    unsafe {
        vm.create_user_memory_region(0, 0, GUEST_MEM_SIZE, host_mem, false, false)
            .context("Failed to map guest memory")?;
    }
    println!("Mapped {} MiB guest RAM at GPA 0x0", GUEST_MEM_SIZE >> 20);

    // ── Load payload ─────────────────────────────────────────────────────
    let entry_point = if let Some(ref path) = kernel_path {
        load_kernel(host_mem, path)?
    } else {
        load_demo_payload(host_mem)
    };

    // ── Set up GDT for protected mode ────────────────────────────────────
    if entry_point >= KERNEL_LOAD_ADDR {
        setup_gdt(host_mem);
    }

    // ── Create vCPU ──────────────────────────────────────────────────────
    let vm_ops: Arc<dyn VmOps> = Arc::new(SerialVmOps);
    let mut vcpu = vm
        .create_vcpu(0, Some(vm_ops))
        .context("Failed to create vCPU")?;

    // Set initial register state
    setup_regs(&mut *vcpu, entry_point)?;

    println!("vCPU 0 ready, RIP={entry_point:#x}. Running...");
    println!("--- Guest serial output ---");

    // ── Run loop ─────────────────────────────────────────────────────────
    let mut exit_count = 0u64;
    loop {
        match vcpu.run() {
            Ok(hypervisor::VmExit::Ignore) => {}
            Ok(hypervisor::VmExit::Shutdown) => {
                println!("\n--- Guest halted (after {exit_count} exits) ---");
                break;
            }
            Ok(hypervisor::VmExit::Reset) => {
                println!("\n--- Guest reset ---");
                break;
            }
            Ok(exit) => {
                println!("\n--- Unhandled VM exit: {exit:?} ---");
                break;
            }
            Err(e) => {
                eprintln!("\nvCPU run error: {e}");
                break;
            }
        }
        exit_count += 1;
    }

    // SAFETY: We own this memory.
    unsafe { std::alloc::dealloc(host_mem, layout) };
    println!("WHP demo complete.");
    Ok(())
}

// ── Demo payload (no kernel) ─────────────────────────────────────────────────

fn load_demo_payload(host_mem: *mut u8) -> u64 {
    // Real-mode payload: print "Hi!" to serial, then shutdown.
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
        0xBA, 0x01, 0x05, // mov dx, 0x501
        0xB0, 0x34,       // mov al, 0x34
        0xEE,             // out dx, al
        0xFA,             // cli
        0xF4,             // hlt
    ];

    let addr = 0x1000_usize;
    // SAFETY: Writing within allocated guest memory.
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), host_mem.add(addr), payload.len());
    }
    println!("Loaded demo payload ({} bytes) at GPA {addr:#X}", payload.len());
    addr as u64
}

// ── Kernel loading ───────────────────────────────────────────────────────────

fn load_kernel(host_mem: *mut u8, path: &str) -> anyhow::Result<u64> {
    let mut file = File::open(path).context("Failed to open kernel image")?;
    let metadata = file.metadata()?;
    let file_size = metadata.len() as usize;
    println!("Loading kernel: {path} ({file_size} bytes)");

    // Read the entire kernel file into a buffer
    let mut kernel_data = vec![0u8; file_size];
    file.read_exact(&mut kernel_data)?;

    // Check for bzImage magic at offset 0x202 ("HdrS")
    if file_size > 0x206 && &kernel_data[0x202..0x206] == b"HdrS" {
        load_bzimage(host_mem, &kernel_data)
    } else {
        // Try loading as flat binary at 1 MiB
        load_flat_binary(host_mem, &kernel_data)
    }
}

fn load_bzimage(host_mem: *mut u8, data: &[u8]) -> anyhow::Result<u64> {
    // Parse bzImage header
    let setup_sects = if data[0x1F1] == 0 { 4 } else { data[0x1F1] as usize };
    let setup_size = (setup_sects + 1) * 512;
    let kernel_size = data.len() - setup_size;

    println!("  bzImage: setup={setup_size} bytes, kernel={kernel_size} bytes");

    if setup_size + kernel_size > data.len() {
        return Err(anyhow!("Invalid bzImage: sizes exceed file"));
    }

    // Copy protected-mode kernel to KERNEL_LOAD_ADDR (1 MiB)
    if KERNEL_LOAD_ADDR as usize + kernel_size > GUEST_MEM_SIZE {
        return Err(anyhow!("Kernel too large for guest memory"));
    }
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(
            data[setup_size..].as_ptr(),
            host_mem.add(KERNEL_LOAD_ADDR as usize),
            kernel_size,
        );
    }

    // Set up boot parameters at BOOT_PARAMS_ADDR
    // Copy the setup header (first setup_size bytes contain boot_params)
    let params_size = setup_size.min(4096);
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(
            data.as_ptr(),
            host_mem.add(BOOT_PARAMS_ADDR as usize),
            params_size,
        );
    }

    // Write command line
    let cmdline = b"console=ttyS0 earlyprintk=serial noapic noacpi pci=off\0";
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(
            cmdline.as_ptr(),
            host_mem.add(CMDLINE_ADDR as usize),
            cmdline.len(),
        );
    }

    // Patch boot_params fields
    // SAFETY: Writing specific fields within the boot_params struct.
    unsafe {
        let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);

        // vid_mode = normal (0xFFFF)
        *(bp.add(0x1FA) as *mut u16) = 0xFFFF;

        // type_of_loader = 0xFF (undefined)
        *bp.add(0x210) = 0xFF;

        // loadflags: set LOADED_HIGH (bit 0) + KEEP_SEGMENTS (bit 6) + CAN_USE_HEAP (bit 7)
        *bp.add(0x211) = 0xC1;

        // cmd_line_ptr
        *(bp.add(0x228) as *mut u32) = CMDLINE_ADDR as u32;

        // header sentinel for boot protocol version
        // (already copied from the bzImage header)
    }

    println!("  Boot params at {BOOT_PARAMS_ADDR:#X}, cmdline at {CMDLINE_ADDR:#X}");
    println!("  Protected-mode kernel at {KERNEL_LOAD_ADDR:#X}");

    // Entry point: 32-bit protected mode at KERNEL_LOAD_ADDR
    Ok(KERNEL_LOAD_ADDR)
}

fn load_flat_binary(host_mem: *mut u8, data: &[u8]) -> anyhow::Result<u64> {
    // Load flat binary at 0x1000 in real mode (like demo payload)
    let load_addr = 0x1000_usize;
    if load_addr + data.len() > GUEST_MEM_SIZE {
        return Err(anyhow!("Binary too large for guest memory"));
    }
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), host_mem.add(load_addr), data.len());
    }
    println!("  Loaded flat binary ({} bytes) at {load_addr:#X}", data.len());
    Ok(load_addr as u64)
}

// ── GDT setup ────────────────────────────────────────────────────────────────

const GDT_ADDR: u64 = 0x500;

/// Write a minimal GDT into guest memory at GDT_ADDR.
/// Entry 0: null, Entry 1 (0x08): 64-bit code (unused for now),
/// Entry 2 (0x10): 32-bit code, Entry 3 (0x18): 32-bit data.
fn setup_gdt(host_mem: *mut u8) {
    let gdt: [u64; 4] = [
        0,                      // 0x00: null descriptor
        0x00AF_9A00_0000_FFFF,  // 0x08: 64-bit code (L=1, D=0)
        0x00CF_9A00_0000_FFFF,  // 0x10: 32-bit code (G=1, D=1, P=1, S=1, Type=A)
        0x00CF_9200_0000_FFFF,  // 0x18: 32-bit data (G=1, D=1, P=1, S=1, Type=2)
    ];
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(
            gdt.as_ptr() as *const u8,
            host_mem.add(GDT_ADDR as usize),
            gdt.len() * 8,
        );
    }
}

// ── Register setup ───────────────────────────────────────────────────────────

fn setup_regs(vcpu: &mut dyn hypervisor::Vcpu, entry: u64) -> anyhow::Result<()> {
    use hypervisor::arch::x86::SegmentRegister;

    println!("  Setting GP registers (RIP={entry:#x})...");
    let mut regs = vcpu.get_regs().context("get_regs")?;
    regs.set_rip(entry);
    regs.set_rflags(0x2);
    regs.set_rsi(BOOT_PARAMS_ADDR); // Linux boot protocol: RSI = boot_params
    vcpu.set_regs(&regs).context("set_regs")?;
    println!("  GP registers set.");

    let mut sregs = vcpu.get_sregs().context("get_sregs")?;
    println!("  Current CR0={:#x}", sregs.cr0);

    if entry >= KERNEL_LOAD_ADDR {
        // 32-bit protected mode for Linux kernel entry
        let code_seg = SegmentRegister {
            base: 0,
            limit: 0xFFFF_FFFF,
            selector: 0x10,
            type_: 0xB, // Execute/Read/Accessed
            s: 1,
            dpl: 0,
            present: 1,
            db: 1, // 32-bit
            g: 1,  // 4K granularity
            l: 0,
            avl: 0,
            unusable: 0,
        };
        let data_seg = SegmentRegister {
            type_: 0x3, // Read/Write/Accessed
            selector: 0x18,
            ..code_seg
        };
        sregs.cs = code_seg;
        sregs.ds = data_seg;
        sregs.es = data_seg;
        sregs.ss = data_seg;
        sregs.cr0 |= 1; // PE = protected mode enable
        sregs.gdt.base = GDT_ADDR;
        sregs.gdt.limit = 31; // 4 entries * 8 bytes - 1
        println!("  Setting sregs (CR0.PE, GDT, segments)...");
    } else {
        // 16-bit real mode for demo payload
        let code_seg = SegmentRegister {
            base: 0,
            limit: 0xFFFF,
            selector: 0,
            type_: 0xB,
            s: 1,
            dpl: 0,
            present: 1,
            db: 0,
            g: 0,
            l: 0,
            avl: 0,
            unusable: 0,
        };
        let data_seg = SegmentRegister {
            type_: 0x3,
            ..code_seg
        };
        sregs.cs = code_seg;
        sregs.ds = data_seg;
        sregs.es = data_seg;
        sregs.ss = data_seg;
    }

    vcpu.set_sregs(&sregs).context("set_sregs")?;
    Ok(())
}

// ── VmOps: serial I/O handler ────────────────────────────────────────────────

struct SerialVmOps;

impl VmOps for SerialVmOps {
    fn guest_mem_write(&self, _gpa: u64, _buf: &[u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }
    fn guest_mem_read(&self, _gpa: u64, _buf: &mut [u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }
    fn mmio_read(&self, _gpa: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        data.fill(0xFF);
        Ok(())
    }
    fn mmio_write(&self, _gpa: u64, _data: &[u8]) -> Result<(), HypervisorVmError> {
        Ok(())
    }
    fn pio_read(&self, port: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        match port {
            // Serial Line Status Register: TX empty + TX holding empty
            0x3FD => data[0] = 0x60,
            _ => data.fill(0xFF),
        }
        Ok(())
    }
    fn pio_write(&self, port: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        if port == SERIAL_PORT && !data.is_empty() {
            let ch = data[0];
            if ch.is_ascii() {
                print!("{}", ch as char);
            }
        } else if port == DEBUG_EXIT_PORT {
            println!();
            println!("--- Guest requested shutdown ---");
            std::process::exit(0);
        }
        Ok(())
    }
}
