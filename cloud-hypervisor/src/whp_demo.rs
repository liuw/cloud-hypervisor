// SPDX-License-Identifier: Apache-2.0
//
// Windows Hypervisor Platform demo: load and execute a guest payload.
//
// Usage:
//   cloud-hypervisor.exe                       # run built-in "Hi!" payload
//   cloud-hypervisor.exe --kernel <bzImage>     # load a Linux bzImage

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use devices::interrupt_controller::InterruptController;
use hypervisor::{HypervisorVmError, InterruptSourceConfig, MsiIrqSourceConfig, VmOps};
use vm_device::interrupt::{
    InterruptIndex, InterruptManager, InterruptSourceGroup, MsiIrqGroupConfig,
};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};
use vm_memory::bitmap::AtomicBitmap;

// ── Constants ────────────────────────────────────────────────────────────────

const GUEST_MEM_SIZE: usize = 256 << 20; // 256 MiB
const SERIAL_PORT: u64 = 0x3F8;
const DEBUG_EXIT_PORT: u64 = 0x501;

// Linux boot protocol addresses
const BOOT_PARAMS_ADDR: u64 = 0x7000;
const CMDLINE_ADDR: u64 = 0x20000;
const KERNEL_LOAD_ADDR: u64 = 0x100000; // 1 MiB — protected-mode kernel

type GuestMem = GuestMemoryMmap<AtomicBitmap>;

pub fn run() -> anyhow::Result<()> {
    // Parse minimal CLI
    let args: Vec<String> = std::env::args().collect();
    let kernel_path = args
        .windows(2)
        .find(|w| w[0] == "--kernel")
        .map(|w| w[1].clone());
    let initramfs_path = args
        .windows(2)
        .find(|w| w[0] == "--initramfs")
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

    // ── Allocate guest memory via GuestMemoryMmap ────────────────────────
    let guest_mem: GuestMem = GuestMemoryMmap::from_ranges(
        &[(GuestAddress(0), GUEST_MEM_SIZE)]
    ).context("Failed to allocate guest memory")?;

    // Get host pointer for WHP mapping
    let host_mem = guest_mem.get_host_address(GuestAddress(0))
        .map_err(|e| anyhow!("get_host_address: {e:?}"))?;
    // SAFETY: GuestMemoryMmap owns the memory and it remains valid.
    unsafe {
        vm.create_user_memory_region(0, 0, GUEST_MEM_SIZE, host_mem, false, false)
            .context("Failed to map guest memory")?;
    }
    println!("Mapped {} MiB guest RAM at GPA 0x0", GUEST_MEM_SIZE >> 20);

    // Create the I/O APIC using the existing devices crate implementation.
    // This provides functional interrupt routing (PIT IRQ 0 → LAPIC → timer calibration).
    let ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>> = if kernel_path.is_some() {
        let interrupt_manager = WhpInterruptManager { vm: vm.clone() };
        let ioapic_dev = devices::ioapic::Ioapic::new(
            "ioapic".to_string(),
            GuestAddress(0xFEE0_0000),
            &interrupt_manager,
            None,
        ).context("Failed to create IOAPIC")?;
        let ioapic = Arc::new(Mutex::new(ioapic_dev));

        // Pre-program IOAPIC redirect entries since WHP can't trap MMIO writes
        // to 0xFEC00000 (guest writes go to unmapped memory and return 0).
        // The kernel will try to program these via MMIO but the writes are lost.
        // We pre-configure IRQ 0 (PIT timer) and IRQ 4 (serial) with proper vectors.
        {
            use vm_device::BusDevice;
            let mut ioapic_guard = ioapic.lock().unwrap();

            // Helper: write IOREGSEL then IOWIN to program an IOAPIC register
            fn ioapic_write_reg(ioapic: &mut devices::ioapic::Ioapic, reg: u32, val: u32) {
                let reg_bytes = reg.to_le_bytes();
                let val_bytes = val.to_le_bytes();
                ioapic.write(0xFEC0_0000, 0x00, &reg_bytes); // IOREGSEL
                ioapic.write(0xFEC0_0000, 0x10, &val_bytes); // IOWIN
            }

            // IRQ 0 (PIT timer) → vector 0x30, physical destination 0, edge-triggered
            // Redirect entry 0: low dword at register 0x10
            ioapic_write_reg(&mut ioapic_guard, 0x10, 0x0000_0030); // vector=0x30, unmasked
            // Redirect entry 0: high dword at register 0x11
            ioapic_write_reg(&mut ioapic_guard, 0x11, 0x0000_0000); // destination=0

            // IRQ 4 (serial ttyS0) → vector 0x34, physical destination 0, edge-triggered
            // Redirect entry 4: low dword at register 0x18
            ioapic_write_reg(&mut ioapic_guard, 0x18, 0x0000_0034); // vector=0x34, unmasked
            // Redirect entry 4: high dword at register 0x19
            ioapic_write_reg(&mut ioapic_guard, 0x19, 0x0000_0000); // destination=0

            println!("  Pre-programmed IOAPIC: IRQ0→vec 0x30, IRQ4→vec 0x34");
        }

        println!("Created IOAPIC with WHP interrupt injection");
        Some(ioapic)
    } else {
        None
    };

    // ── Load payload ─────────────────────────────────────────────────────
    let entry_point = if let Some(ref path) = kernel_path {
        load_kernel(host_mem, path)?
    } else {
        load_demo_payload(host_mem)
    };

    // ── Load initramfs if provided ───────────────────────────────────────
    let mut initrd_addr: u64 = 0;
    let mut initrd_size: u64 = 0;
    if let Some(ref path) = initramfs_path {
        let mut f = File::open(path)
            .with_context(|| format!("Failed to open initramfs: {path}"))?;
        let fsize = f.metadata()?.len() as usize;
        // Place initramfs at end of guest RAM, page-aligned
        let addr = ((GUEST_MEM_SIZE - fsize) & !0xFFF) as u64;
        if addr < 0x200000 {
            return Err(anyhow!("Initramfs too large ({fsize} bytes)"));
        }
        // SAFETY: Reading into allocated guest memory bounds.
        unsafe {
            let buf = std::slice::from_raw_parts_mut(host_mem.add(addr as usize), fsize);
            f.read_exact(buf)?;
        }
        initrd_addr = addr;
        initrd_size = fsize as u64;
        println!("Loaded initramfs: {path} ({fsize} bytes at GPA {addr:#X})");
    }

    // ── Set up GDT and page tables for protected/long mode ─────────────
    if kernel_path.is_some() {
        setup_gdt(host_mem);
        setup_page_tables(host_mem);
        setup_acpi_tables(host_mem);

        // Set initrd fields in boot_params if initramfs was loaded
        if initrd_size > 0 {
            unsafe {
                let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);
                // ramdisk_image at offset 0x218 (u32)
                std::ptr::write_unaligned(bp.add(0x218) as *mut u32, initrd_addr as u32);
                // ramdisk_size at offset 0x21C (u32)
                std::ptr::write_unaligned(bp.add(0x21C) as *mut u32, initrd_size as u32);
            }
            println!("  Set boot_params initrd: addr={initrd_addr:#X} size={initrd_size}");
        }
    }

    // ── Create vCPU ──────────────────────────────────────────────────────
    let vm_ops = Arc::new(SerialVmOps::new(ioapic.clone()));
    let vm_ops_clone: Arc<dyn VmOps> = vm_ops.clone();
    let mut vcpu = vm
        .create_vcpu(0, Some(vm_ops_clone))
        .context("Failed to create vCPU")?;

    // Set initial register state
    setup_regs(&mut *vcpu, entry_point)?;

    println!("vCPU 0 ready, RIP={entry_point:#x}. Running...");
    println!("--- Guest serial output ---");

    // Start a timer interrupt injection thread for kernel boot.
    // The kernel needs periodic timer interrupts (IRQ 0) to run the scheduler.
    // We inject a fixed interrupt at vector 0x20 (standard PIT→PIC mapping) at ~100 Hz.
    // Timer interrupt injection is handled via a background thread.
    // After a delay (letting kernel boot), periodically inject timer interrupts
    // and cancel the vCPU run to wake it from WHP-internal HLT.
    // Timer interrupt injection thread.
    // Injects both LAPIC timer vector and IRQ 0 to advance jiffies and drive work queues.
    if kernel_path.is_some() {
        use hypervisor::whp::WhpVm;
        let vm_for_timer: Arc<dyn hypervisor::Vm> = vm.clone();
        let ioapic_for_timer = ioapic.clone();
        std::thread::Builder::new()
            .name("timer-inject".to_string())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let whp = vm_for_timer.as_any().downcast_ref::<WhpVm>().unwrap();
                eprintln!("[timer] Starting timer injection (0x20 PIC IRQ 0)");
                let mut tick = 0u64;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    // Inject PIC IRQ 0 at vector 0x20 (standard 8259 base vector)
                    let _ = whp.request_interrupt(0x20, 0);
                    tick += 1;
                    if tick == 1 { eprintln!("[timer] First tick OK"); }
                }
            })
            .context("Failed to spawn timer thread")?;
    }

    // Start a stdin reader thread that feeds input to the serial port
    let vm_ops_for_stdin = vm_ops.clone();
    std::thread::Builder::new()
        .name("stdin-reader".to_string())
        .spawn(move || {
            use std::io::Read;
            let stdin = std::io::stdin();
            let mut buf = [0u8; 1];
            loop {
                if stdin.lock().read(&mut buf).unwrap_or(0) > 0 {
                    vm_ops_for_stdin.feed_input(&buf);
                }
            }
        })
        .context("Failed to spawn stdin reader")?;

    // ── Run loop ─────────────────────────────────────────────────────────
    let mut exit_count = 0u64;
    let start = std::time::Instant::now();
    let mut last_rip_dump = std::time::Instant::now();
    let debug_kernel = std::env::var("CH_DEBUG").is_ok();
    loop {
        match vcpu.run() {
            Ok(hypervisor::VmExit::Ignore) => {}
            Ok(hypervisor::VmExit::Shutdown) => {
                println!("\n--- Guest halted (after {exit_count} exits, {:.1}s) ---",
                         start.elapsed().as_secs_f64());
                // Dump final RIP
                if let Ok(regs) = vcpu.get_regs() {
                    println!("  Final RIP = {:#X}", regs.get_rip());
                }
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

        // Periodic RIP dump for debugging
        if debug_kernel && last_rip_dump.elapsed().as_secs() >= 2 {
            if let Ok(regs) = vcpu.get_regs() {
                eprintln!("[{:.1}s] exits={exit_count} RIP={:#X}",
                          start.elapsed().as_secs_f64(), regs.get_rip());
            }
            last_rip_dump = std::time::Instant::now();
        }
    }

    println!("WHP demo complete.");
    Ok(())
}

// ── Demo payload (no kernel) ─────────────────────────────────────────────────

fn load_demo_payload(host_mem: *mut u8) -> u64 {
    // Real-mode payload: interactive serial echo loop.
    // Prints a greeting, then reads characters from serial and echos them.
    // Press Ctrl+C (0x03) to exit.
    //
    // Pseudocode:
    //   print "Hi! Type something (Ctrl+C to quit):\n"
    //   loop:
    //     wait for serial RX ready (LSR bit 0)
    //     read character from 0x3F8
    //     if char == 0x03 (Ctrl+C): shutdown
    //     write character to 0x3F8 (echo)
    //     if char == '\r': also write '\n'
    //     goto loop
    //
    let payload: &[u8] = &[
        // Print greeting
        0xBA, 0xF8, 0x03,       // mov dx, 0x3F8     ; serial data port
        0xB0, b'H', 0xEE,       // mov al,'H'; out dx,al
        0xB0, b'i', 0xEE,       // mov al,'i'; out dx,al
        0xB0, b'!', 0xEE,       // mov al,'!'; out dx,al
        0xB0, b' ', 0xEE,       // mov al,' '; out dx,al
        0xB0, b'T', 0xEE,       // mov al,'T'; out dx,al
        0xB0, b'y', 0xEE,       // mov al,'y'; out dx,al
        0xB0, b'p', 0xEE,       // mov al,'p'; out dx,al
        0xB0, b'e', 0xEE,       // mov al,'e'; out dx,al
        0xB0, b' ', 0xEE,       // mov al,' '; out dx,al
        0xB0, b's', 0xEE,       // mov al,'s'; out dx,al
        0xB0, b'o', 0xEE,       // mov al,'o'; out dx,al
        0xB0, b'm', 0xEE,       // mov al,'m'; out dx,al
        0xB0, b'e', 0xEE,       // mov al,'e'; out dx,al
        0xB0, b't', 0xEE,       // mov al,'t'; out dx,al
        0xB0, b'h', 0xEE,       // mov al,'h'; out dx,al
        0xB0, b'i', 0xEE,       // mov al,'i'; out dx,al
        0xB0, b'n', 0xEE,       // mov al,'n'; out dx,al
        0xB0, b'g', 0xEE,       // mov al,'g'; out dx,al
        0xB0, b'>', 0xEE,       // mov al,'>'; out dx,al
        0xB0, b' ', 0xEE,       // mov al,' '; out dx,al

        // offset = 60 bytes (0x3C) — echo loop starts here
        // Echo loop:
        //   wait for LSR bit 0 (data ready)
        0xBA, 0xFD, 0x03,       // mov dx, 0x3FD     ; LSR port
        0xEC,                   // in al, dx          ; read LSR
        0xA8, 0x01,             // test al, 1         ; bit 0 = data ready?
        0x74, 0xFA,             // jz -6              ; loop back to "in al, dx"

        //   read character
        0xBA, 0xF8, 0x03,       // mov dx, 0x3F8     ; data port
        0xEC,                   // in al, dx          ; read character

        //   check for Ctrl+C
        0x3C, 0x03,             // cmp al, 0x03
        0x74, 0x0C,             // je shutdown (12 bytes forward)

        //   echo character
        0xEE,                   // out dx, al

        //   if CR, also send LF
        0x3C, 0x0D,             // cmp al, 0x0D ('\r')
        0x75, 0xEB,             // jne loop (-21 = back to "mov dx, 0x3FD")
        0xB0, 0x0A,             // mov al, 0x0A ('\n')
        0xEE,                   // out dx, al
        0xEB, 0xE6,             // jmp loop (-26)

        // shutdown:
        0xBA, 0x01, 0x05,       // mov dx, 0x501
        0xB0, 0x34,             // mov al, 0x34
        0xEE,                   // out dx, al
        0xFA,                   // cli
        0xF4,                   // hlt
    ];

    let addr = 0x1000_usize;
    // SAFETY: Writing within allocated guest memory.
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), host_mem.add(addr), payload.len());
    }
    println!("Loaded interactive demo ({} bytes) at GPA {addr:#X}", payload.len());
    addr as u64
}

// ── Kernel loading ───────────────────────────────────────────────────────────

fn load_kernel(host_mem: *mut u8, path: &str) -> anyhow::Result<u64> {
    let mut file = File::open(path).context("Failed to open kernel image")?;
    let metadata = file.metadata()?;
    let file_size = metadata.len() as usize;
    println!("Loading kernel: {path} ({file_size} bytes)");

    // Read first bytes to detect format
    let mut header = [0u8; 0x210];
    let header_len = file.read(&mut header).context("Read header")?;
    file.seek(std::io::SeekFrom::Start(0)).context("Seek")?;

    if &header[0..4] == b"\x7FELF" {
        println!("  Format: ELF");
        load_elf(host_mem, &mut file)
    } else if header_len > 0x206 && &header[0x202..0x206] == b"HdrS" {
        println!("  Format: bzImage");
        let mut data = vec![0u8; file_size];
        file.read_exact(&mut data)?;
        load_bzimage(host_mem, &data)
    } else {
        println!("  Format: flat binary");
        let mut data = vec![0u8; file_size];
        file.read_exact(&mut data)?;
        load_flat_binary(host_mem, &data)
    }
}

fn load_elf(host_mem: *mut u8, file: &mut File) -> anyhow::Result<u64> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    // Read ELF header to find entry point and program headers
    let mut ehdr = [0u8; 64]; // ELF64 header
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut ehdr)?;

    let entry = u64::from_le_bytes(ehdr[24..32].try_into().unwrap());
    let phoff = u64::from_le_bytes(ehdr[32..40].try_into().unwrap());
    let phentsize = u16::from_le_bytes(ehdr[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(ehdr[56..58].try_into().unwrap()) as usize;

    println!("  ELF entry: {entry:#x}, {phnum} program headers");

    // Load each PT_LOAD segment
    for i in 0..phnum {
        let mut phdr = vec![0u8; phentsize];
        file.seek(SeekFrom::Start(phoff + (i * phentsize) as u64))?;
        file.read_exact(&mut phdr)?;

        let p_type = u32::from_le_bytes(phdr[0..4].try_into().unwrap());
        if p_type != 1 { continue; } // PT_LOAD = 1

        let p_offset = u64::from_le_bytes(phdr[8..16].try_into().unwrap());
        let p_paddr = u64::from_le_bytes(phdr[24..32].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(phdr[32..40].try_into().unwrap());
        let p_memsz = u64::from_le_bytes(phdr[40..48].try_into().unwrap());

        if p_paddr as usize + p_memsz as usize > GUEST_MEM_SIZE {
            return Err(anyhow!("ELF segment at {p_paddr:#x} + {p_memsz:#x} exceeds guest memory"));
        }

        // Read segment data from file into guest memory
        let mut data = vec![0u8; p_filesz as usize];
        file.seek(SeekFrom::Start(p_offset))?;
        file.read_exact(&mut data)?;
        // SAFETY: Writing within allocated guest memory bounds.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), host_mem.add(p_paddr as usize), data.len());
        }
        println!("  Loaded segment: {p_paddr:#010x} ({p_filesz} bytes, memsz {p_memsz})");
    }

    // Write bootstrap for mode transition, then jump to entry.
    // Use the simple bootstrap — far jump directly to 64-bit code segment.
    // GDT entry 1 (selector 0x08) has L=1 (long mode), and WHP handles
    // the mode transition when loading this selector.
    write_pm_bootstrap(host_mem);

    // Set up boot_params (zero page) for Linux boot protocol
    setup_linux_boot_params(host_mem);

    // Override the bootstrap's kernel address.
    // The bootstrap uses `mov rax, <8-byte addr>` to load the kernel entry.
    // The 8-byte immediate is at BOOTSTRAP_KERNEL_ADDR_OFFSET from BOOTSTRAP_ADDR.
    // We write the actual ELF entry point there.
    // SAFETY: Patching within guest memory.
    unsafe {
        std::ptr::write_unaligned(
            host_mem.add(BOOTSTRAP_ADDR as usize + BOOTSTRAP_KERNEL_ADDR_OFFSET) as *mut u64,
            entry,
        );
    }
    println!("  Patched bootstrap kernel addr to {entry:#x}");

    Ok(BOOTSTRAP_ADDR)
}

/// Bootstrap that transitions real mode → 32-bit PM → 64-bit long mode → entry.
fn write_pm_bootstrap_to_entry(host_mem: *mut u8, entry: u64) {
    // GDT descriptor at GDT_DESC_ADDR
    let gdt_desc: [u8; 6] = {
        let mut d = [0u8; 6];
        d[0..2].copy_from_slice(&39u16.to_le_bytes()); // 5 entries * 8 - 1
        d[2..6].copy_from_slice(&(GDT_ADDR as u32).to_le_bytes());
        d
    };
    // SAFETY: Writing within guest memory bounds.
    unsafe { std::ptr::copy_nonoverlapping(gdt_desc.as_ptr(), host_mem.add(GDT_DESC_ADDR as usize), 6) };

    // 64-bit entry address stored at 0x2200 (used by the 32-bit trampoline)
    // SAFETY: Writing within guest memory bounds.
    unsafe { *(host_mem.add(0x2200) as *mut u64) = entry };

    let mut code = Vec::new();

    // ── 16-bit real mode ──
    code.push(0xFA); // cli

    // lgdt [GDT_DESC_ADDR]
    code.extend_from_slice(&[0x0F, 0x01, 0x16]);
    code.extend_from_slice(&(GDT_DESC_ADDR as u16).to_le_bytes());

    // mov eax, cr0; or al, 1; mov cr0, eax  (enable PE)
    code.extend_from_slice(&[0x0F, 0x20, 0xC0]);
    code.extend_from_slice(&[0x0C, 0x01]);
    code.extend_from_slice(&[0x0F, 0x22, 0xC0]);

    // Far jump to 32-bit code segment (selector 0x10 = 32-bit code in our GDT)
    // Target: BOOTSTRAP_ADDR + pm32_offset (the next instruction after this jump)
    let pm32_offset = code.len() + 7; // 66 EA <4 bytes offset> <2 bytes selector>
    code.extend_from_slice(&[0x66, 0xEA]);
    code.extend_from_slice(&((BOOTSTRAP_ADDR as u32) + pm32_offset as u32).to_le_bytes());
    code.extend_from_slice(&0x10u16.to_le_bytes()); // CS = 32-bit code (GDT entry 2)

    // ── 32-bit protected mode ──
    // Set data segments
    code.extend_from_slice(&[0x66, 0xB8, 0x18, 0x00]); // mov ax, 0x18 (data selector)
    code.extend_from_slice(&[0x8E, 0xD8]); // mov ds, ax
    code.extend_from_slice(&[0x8E, 0xC0]); // mov es, ax
    code.extend_from_slice(&[0x8E, 0xD0]); // mov ss, ax

    // Enable PAE: mov eax, cr4; or eax, 0x20; mov cr4, eax
    code.extend_from_slice(&[0x0F, 0x20, 0xE0]); // mov eax, cr4
    code.extend_from_slice(&[0x83, 0xC8, 0x20]); // or eax, 0x20 (PAE)
    code.extend_from_slice(&[0x0F, 0x22, 0xE0]); // mov cr4, eax

    // Set CR3 to PML4: mov eax, PML4_ADDR; mov cr3, eax
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(PML4_ADDR as u32).to_le_bytes()); // mov eax, PML4_ADDR
    code.extend_from_slice(&[0x0F, 0x22, 0xD8]); // mov cr3, eax

    // Enable long mode in EFER MSR (0xC0000080): rdmsr; or eax, 0x100; wrmsr
    code.extend_from_slice(&[0xB9, 0x80, 0x00, 0x00, 0xC0]); // mov ecx, 0xC0000080
    code.extend_from_slice(&[0x0F, 0x32]); // rdmsr
    code.extend_from_slice(&[0x0F, 0xBA, 0xE8, 0x08]); // bts eax, 8 (LME bit)
    code.extend_from_slice(&[0x0F, 0x30]); // wrmsr

    // Enable paging: mov eax, cr0; or eax, 0x80000000; mov cr0, eax
    code.extend_from_slice(&[0x0F, 0x20, 0xC0]); // mov eax, cr0
    code.extend_from_slice(&[0x0D, 0x00, 0x00, 0x00, 0x80]); // or eax, 0x80000000
    code.extend_from_slice(&[0x0F, 0x22, 0xC0]); // mov cr0, eax

    // Far jump to 64-bit code (selector 0x08 = 64-bit code in our GDT)
    // We jump to a small 64-bit trampoline right after this instruction
    let lm64_offset = code.len() + 7;
    code.extend_from_slice(&[0xEA]); // jmp far (32-bit encoding in 32-bit mode)
    code.extend_from_slice(&((BOOTSTRAP_ADDR as u32) + lm64_offset as u32).to_le_bytes());
    code.extend_from_slice(&0x08u16.to_le_bytes()); // CS = 64-bit code

    // ── 64-bit long mode ──
    // Load the 64-bit entry address from 0x2200 and jump to it
    // mov rax, [0x2200]; jmp rax
    code.extend_from_slice(&[0x48, 0xA1]); // mov rax, [imm64]
    code.extend_from_slice(&0x2200u64.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xE0]); // jmp rax

    // SAFETY: Writing within guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), host_mem.add(BOOTSTRAP_ADDR as usize), code.len());
    }
    println!("  Bootstrap ({} bytes) at {BOOTSTRAP_ADDR:#X} → 64-bit jmp to {entry:#x}", code.len());
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
        std::ptr::write_unaligned(bp.add(0x1FA) as *mut u16, 0xFFFFu16);

        // type_of_loader = 0xFF (undefined)
        *bp.add(0x210) = 0xFF;

        // loadflags: set LOADED_HIGH (bit 0) + KEEP_SEGMENTS (bit 6) + CAN_USE_HEAP (bit 7)
        *bp.add(0x211) = 0xC1;

        // cmd_line_ptr
        std::ptr::write_unaligned(bp.add(0x228) as *mut u32, CMDLINE_ADDR as u32);

        // header sentinel for boot protocol version
        // (already copied from the bzImage header)
    }

    println!("  Boot params at {BOOT_PARAMS_ADDR:#X}, cmdline at {CMDLINE_ADDR:#X}");
    println!("  Protected-mode kernel at {KERNEL_LOAD_ADDR:#X}");

    // Entry point: 32-bit protected mode at KERNEL_LOAD_ADDR
    Ok(KERNEL_LOAD_ADDR)
}

fn load_flat_binary(host_mem: *mut u8, data: &[u8]) -> anyhow::Result<u64> {
    // Load flat binary at KERNEL_LOAD_ADDR (1 MiB)
    let load_addr = KERNEL_LOAD_ADDR as usize;
    if load_addr + data.len() > GUEST_MEM_SIZE {
        return Err(anyhow!("Binary too large for guest memory"));
    }
    // SAFETY: Writing within allocated guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), host_mem.add(load_addr), data.len());
    }
    println!("  Loaded flat binary ({} bytes) at {KERNEL_LOAD_ADDR:#X}", data.len());

    // Write a real-mode bootstrap at BOOTSTRAP_ADDR that transitions to
    // protected mode and far-jumps to the kernel at KERNEL_LOAD_ADDR.
    // This is needed because WHP requires the guest (not the host) to
    // perform the real→protected mode transition.
    write_pm_bootstrap(host_mem);

    // Return bootstrap address — the vCPU starts in real mode here
    Ok(BOOTSTRAP_ADDR)
}

const BOOTSTRAP_ADDR: u64 = 0x2000;
const GDT_DESC_ADDR: u64 = 0x2100;
/// Offset within the bootstrap code where the 8-byte kernel entry address is stored.
/// This is the immediate operand of `mov rax, <addr>` in the 64-bit section.
const BOOTSTRAP_KERNEL_ADDR_OFFSET: usize = 106;

/// Write a 16-bit real-mode bootstrap that transitions to 32-bit protected mode.
fn write_pm_bootstrap(host_mem: *mut u8) {
    // GDT at GDT_ADDR (already written by setup_gdt)
    // GDT descriptor at GDT_DESC_ADDR
    let gdt_desc: [u8; 6] = {
        let mut d = [0u8; 6];
        d[0..2].copy_from_slice(&39u16.to_le_bytes()); // limit = 5*8-1
        d[2..6].copy_from_slice(&(GDT_ADDR as u32).to_le_bytes());
        d
    };
    // SAFETY: Writing within guest memory bounds.
    unsafe { std::ptr::copy_nonoverlapping(gdt_desc.as_ptr(), host_mem.add(GDT_DESC_ADDR as usize), 6) };

    // 16-bit bootstrap: real mode → 32-bit PM → 64-bit long mode
    // GDT layout: 0x08=64-bit code(L=1), 0x10=32-bit code, 0x18=data
    let mut code = Vec::new();

    // === Phase 1: Real mode → 32-bit PM ===
    // cli
    code.push(0xFA);
    // lgdt [GDT_DESC_ADDR]
    code.extend_from_slice(&[0x0F, 0x01, 0x16]);
    code.extend_from_slice(&(GDT_DESC_ADDR as u16).to_le_bytes());
    // mov eax, cr0
    code.extend_from_slice(&[0x0F, 0x20, 0xC0]);
    // or al, 1 (set PE)
    code.extend_from_slice(&[0x0C, 0x01]);
    // mov cr0, eax
    code.extend_from_slice(&[0x0F, 0x22, 0xC0]);
    // jmp far 0x10:pm32_start (32-bit code segment)
    let pm32_start = BOOTSTRAP_ADDR as u32 + 22; // after this 8-byte far jmp
    code.extend_from_slice(&[0x66, 0xEA]);
    code.extend_from_slice(&pm32_start.to_le_bytes());
    code.extend_from_slice(&0x10u16.to_le_bytes()); // offset 20-21

    // === Phase 2: 32-bit PM — set up for long mode ===
    // offset 22: Now in 32-bit code
    // Reload data segments
    // mov ax, 0x18
    code.extend_from_slice(&[0x66, 0xB8]);
    code.extend_from_slice(&0x18u16.to_le_bytes());
    // mov ds, ax
    code.extend_from_slice(&[0x8E, 0xD8]);
    // mov es, ax
    code.extend_from_slice(&[0x8E, 0xC0]);
    // mov ss, ax
    code.extend_from_slice(&[0x8E, 0xD0]);

    // Enable PAE: mov eax, cr4; or eax, 0x20; mov cr4, eax
    code.extend_from_slice(&[0x0F, 0x20, 0xE0]); // mov eax, cr4
    code.extend_from_slice(&[0x0D, 0x20, 0x00, 0x00, 0x00]); // or eax, 0x20
    code.extend_from_slice(&[0x0F, 0x22, 0xE0]); // mov cr4, eax

    // Load page tables: mov eax, PML4_ADDR; mov cr3, eax
    code.extend_from_slice(&[0xB8]); // mov eax, imm32
    code.extend_from_slice(&(PML4_ADDR as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x22, 0xD8]); // mov cr3, eax

    // Enable long mode: rdmsr(EFER); or eax, 0x100; wrmsr(EFER)
    // mov ecx, 0xC0000080 (IA32_EFER)
    code.extend_from_slice(&[0xB9]);
    code.extend_from_slice(&0xC000_0080u32.to_le_bytes());
    // rdmsr
    code.extend_from_slice(&[0x0F, 0x32]);
    // or eax, 0x100 (LME bit)
    code.extend_from_slice(&[0x0D, 0x00, 0x01, 0x00, 0x00]);
    // wrmsr
    code.extend_from_slice(&[0x0F, 0x30]);

    // Enable paging: mov eax, cr0; or eax, 0x80000000; mov cr0, eax
    code.extend_from_slice(&[0x0F, 0x20, 0xC0]); // mov eax, cr0
    code.extend_from_slice(&[0x0D, 0x00, 0x00, 0x00, 0x80]); // or eax, 0x80000000
    code.extend_from_slice(&[0x0F, 0x22, 0xC0]); // mov cr0, eax

    // === Phase 3: Far jump to 64-bit code segment ===
    // jmp far 0x08:lm64_start
    // In 32-bit code, the encoding is: EA <4-byte offset> <2-byte selector>
    let lm64_offset = code.len() + 7; // after this 7-byte far jmp
    let lm64_addr = BOOTSTRAP_ADDR as u32 + lm64_offset as u32;
    code.push(0xEA);
    code.extend_from_slice(&lm64_addr.to_le_bytes());
    code.extend_from_slice(&0x08u16.to_le_bytes()); // CS = 64-bit code (GDT entry 1, L=1)

    // === Phase 4: 64-bit long mode ===
    // Now in 64-bit mode. Set up RSI and jump to kernel.
    let lm64_start = code.len();
    assert_eq!(lm64_start, lm64_offset, "64-bit code offset mismatch");

    // Set up stack: mov rsp, 0x80000  (REX.W + mov)
    // 48 C7 C4 00 00 08 00 = mov rsp, 0x80000
    code.extend_from_slice(&[0x48, 0xC7, 0xC4]);
    code.extend_from_slice(&0x80000u32.to_le_bytes());

    // mov rsi, BOOT_PARAMS_ADDR (for bzImage/zero page compat)
    // 48 C7 C6 <imm32>
    code.extend_from_slice(&[0x48, 0xC7, 0xC6]);
    code.extend_from_slice(&(BOOT_PARAMS_ADDR as u32).to_le_bytes());

    // mov rbx, PVH_INFO_START (for PVH boot)
    // 48 C7 C3 <imm32>
    code.extend_from_slice(&[0x48, 0xC7, 0xC3]);
    code.extend_from_slice(&0x6000u32.to_le_bytes());

    // Jump to kernel: mov rax, KERNEL_LOAD_ADDR; jmp rax
    // 48 B8 <8-byte imm> = mov rax, imm64
    code.extend_from_slice(&[0x48, 0xB8]);
    assert_eq!(code.len(), BOOTSTRAP_KERNEL_ADDR_OFFSET, "kernel addr offset mismatch");
    code.extend_from_slice(&(KERNEL_LOAD_ADDR as u64).to_le_bytes());
    // jmp rax = FF E0
    code.extend_from_slice(&[0xFF, 0xE0]);

    // SAFETY: Writing within guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), host_mem.add(BOOTSTRAP_ADDR as usize), code.len());
    }
    println!("  Bootstrap ({} bytes) at {BOOTSTRAP_ADDR:#X}, enters 64-bit long mode", code.len());
}

// ── GDT setup ────────────────────────────────────────────────────────────────

const GDT_ADDR: u64 = 0x500;

/// Write a minimal GDT into guest memory at GDT_ADDR.
/// Entry 0: null, Entry 1 (0x08): 64-bit code (unused for now),
/// Entry 2 (0x10): 32-bit code, Entry 3 (0x18): 32-bit data,
/// Entry 4 (0x20): TSS (for TR).
fn setup_gdt(host_mem: *mut u8) {
    let gdt: [u64; 5] = [
        0,                      // 0x00: null descriptor
        0x00AF_9A00_0000_FFFF,  // 0x08: 64-bit code (L=1, D=0)
        0x00CF_9A00_0000_FFFF,  // 0x10: 32-bit code (G=1, D=1, P=1, S=1, Type=A)
        0x00CF_9200_0000_FFFF,  // 0x18: 32-bit data (G=1, D=1, P=1, S=1, Type=2)
        0x0000_8B00_0000_FFFF,  // 0x20: 32-bit TSS (P=1, Type=0xB busy TSS)
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

const PML4_ADDR: u64 = 0xA000;

/// Set up Linux boot parameters for ELF vmlinux.
/// Creates a boot_params "zero page" at BOOT_PARAMS_ADDR and a PVH hvm_start_info at 0x6000.
fn setup_linux_boot_params(host_mem: *mut u8) {
    /// Write a value at an unaligned offset within guest memory.
    unsafe fn w<T: Copy>(base: *mut u8, offset: usize, val: T) {
        std::ptr::write_unaligned(base.add(offset) as *mut T, val);
    }

    // SAFETY: Writing boot structures within allocated guest memory.
    unsafe {
        // === Zero page (boot_params) at BOOT_PARAMS_ADDR (0x7000) ===
        let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);
        std::ptr::write_bytes(bp, 0, 4096);

        // Write command line
        let cmdline = b"console=ttyS0 earlyprintk=serial,ttyS0,115200 nomodules lpj=1000000 no_timer_check tsc=reliable keep_bootcon rdinit=/init\0";
        std::ptr::copy_nonoverlapping(
            cmdline.as_ptr(),
            host_mem.add(CMDLINE_ADDR as usize),
            cmdline.len(),
        );

        // Setup header signature "HdrS" at offset 0x202
        w::<u32>(bp, 0x202, 0x53726448);
        // Boot protocol version 2.14 at offset 0x206
        w::<u16>(bp, 0x206, 0x020E);
        // type_of_loader = 0xFF at offset 0x210
        *bp.add(0x210) = 0xFF;
        // loadflags: LOADED_HIGH(0x01) | KEEP_SEGMENTS(0x40) | CAN_USE_HEAP(0x80)
        *bp.add(0x211) = 0xC1;
        // cmd_line_ptr at offset 0x228
        w::<u32>(bp, 0x228, CMDLINE_ADDR as u32);

        // e820 memory map (e820_table at 0x2D0, 20 bytes per entry)
        // e820_entries count at offset 0x1E8
        *bp.add(0x1E8) = 3;

        // Entry 0: Low memory (0 - 0x9FC00) = usable
        w::<u64>(bp, 0x2D0, 0);          // addr
        w::<u64>(bp, 0x2D8, 0x9FC00);    // size
        w::<u32>(bp, 0x2E0, 1);          // type = RAM

        // Entry 1: Reserved (0x9FC00 - 0x100000)
        w::<u64>(bp, 0x2E4, 0x9FC00);                        // addr
        w::<u64>(bp, 0x2EC, 0x100000 - 0x9FC00);              // size
        w::<u32>(bp, 0x2F4, 2);                               // type = Reserved

        // Entry 2: Main memory (1MB - end of guest RAM)
        w::<u64>(bp, 0x2F8, 0x100000);                        // addr
        w::<u64>(bp, 0x300, GUEST_MEM_SIZE as u64 - 0x100000); // size
        w::<u32>(bp, 0x308, 1);                               // type = RAM

        // === PVH hvm_start_info at 0x6000 (for PVH boot compat) ===
        const PVH_INFO_START: u64 = 0x6000;
        const MEMMAP_START: u64 = 0x6100;
        const XEN_HVM_START_MAGIC: u32 = 0x336ec578;

        let info = host_mem.add(PVH_INFO_START as usize);
        std::ptr::write_bytes(info, 0, 256);

        w::<u32>(info, 0, XEN_HVM_START_MAGIC);    // magic
        w::<u32>(info, 4, 1);                       // version
        w::<u64>(info, 16, CMDLINE_ADDR);            // cmdline_paddr
        w::<u64>(info, 40, MEMMAP_START);            // memmap_paddr
        w::<u32>(info, 48, 1);                       // memmap_entries

        // Memory map entry
        let memmap = host_mem.add(MEMMAP_START as usize);
        w::<u64>(memmap, 0, 0);                     // addr
        w::<u64>(memmap, 8, GUEST_MEM_SIZE as u64); // size
        w::<u32>(memmap, 16, 1);                    // type = RAM
    }

    println!("  Boot params at {BOOT_PARAMS_ADDR:#X}, PVH info at 0x6000, cmdline at {CMDLINE_ADDR:#X}");
}

/// Set up MP (MultiProcessor) floating pointer and configuration tables.
/// The kernel scans for "_MP_" signature to find the APIC and I/O APIC.
fn setup_acpi_tables(host_mem: *mut u8) {
    /// Write a value at an unaligned offset within guest memory.
    unsafe fn w<T: Copy>(base: *mut u8, offset: usize, val: T) {
        std::ptr::write_unaligned(base.add(offset) as *mut T, val);
    }

    /// Compute MP table checksum (sum of all bytes must be 0).
    fn mp_checksum(data: &[u8]) -> u8 {
        let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        (!sum).wrapping_add(1)
    }

    const MP_FP_ADDR: u64 = 0x9_FC00; // MP Floating Pointer in EBDA
    const MP_TABLE_ADDR: u64 = 0x9_FD00; // MP Configuration Table

    // SAFETY: Writing MP structures within allocated guest memory.
    unsafe {
        // === MP Configuration Table at MP_TABLE_ADDR ===
        let mpc = host_mem.add(MP_TABLE_ADDR as usize);
        std::ptr::write_bytes(mpc, 0, 256);

        // MP Config Table Header (44 bytes)
        std::ptr::copy_nonoverlapping(b"PCMP".as_ptr(), mpc, 4); // Signature
        // Length filled later
        *mpc.add(4) = 0; // spec_rev placeholder
        // Checksum at offset 7 — computed later
        *mpc.add(6) = 4; // Spec revision (MP 1.4)
        std::ptr::copy_nonoverlapping(b"CLOUDH  ".as_ptr(), mpc.add(8), 8); // OEM ID
        std::ptr::copy_nonoverlapping(b"WHP DEMO    ".as_ptr(), mpc.add(16), 12); // Product ID
        w::<u32>(mpc, 28, 0); // OEM table pointer
        w::<u16>(mpc, 32, 0); // OEM table size
        w::<u16>(mpc, 34, 3 + 16); // Entry count (1 CPU + 1 bus + 1 IOAPIC + 16 IRQ entries)
        w::<u32>(mpc, 36, 0xFEE0_0000); // Local APIC address
        w::<u16>(mpc, 40, 0); // Extended table length
        *mpc.add(42) = 0; // Extended table checksum

        let mut offset = 44;

        // CPU entry (type 0, 20 bytes)
        let cpu = mpc.add(offset);
        *cpu = 0; // Entry type = Processor
        *cpu.add(1) = 0; // Local APIC ID
        *cpu.add(2) = 0x14; // Local APIC version
        *cpu.add(3) = 0x03; // CPU flags: enabled + bootstrap processor
        w::<u32>(cpu, 4, 0); // CPU signature
        w::<u32>(cpu, 8, 0); // Feature flags
        // Reserved (8 bytes at offset 12)
        offset += 20;

        // Bus entry (type 1, 8 bytes)
        let bus = mpc.add(offset);
        *bus = 1; // Entry type = Bus
        *bus.add(1) = 0; // Bus ID
        std::ptr::copy_nonoverlapping(b"ISA   ".as_ptr(), bus.add(2), 6); // Bus type
        offset += 8;

        // I/O APIC entry (type 2, 8 bytes)
        let ioapic = mpc.add(offset);
        *ioapic = 2; // Entry type = I/O APIC
        *ioapic.add(1) = 0; // I/O APIC ID
        *ioapic.add(2) = 0x11; // I/O APIC version
        *ioapic.add(3) = 0x01; // Flags: enabled
        w::<u32>(ioapic, 4, 0xFEC0_0000); // I/O APIC address
        offset += 8;

        // I/O Interrupt Assignment entries (type 3, 8 bytes each)
        // Map ISA IRQs 0-15 to I/O APIC inputs 0-15
        for irq in 0..16u8 {
            let entry = mpc.add(offset);
            *entry = 3; // Entry type = I/O Interrupt Assignment
            *entry.add(1) = 0; // Interrupt type: INT (vectored)
            w::<u16>(entry, 2, 0); // Flags: default (bus-type dependent)
            *entry.add(4) = 0; // Source bus ID
            *entry.add(5) = irq; // Source bus IRQ
            *entry.add(6) = 0; // Dest I/O APIC ID
            *entry.add(7) = irq; // Dest I/O APIC INTIN#
            offset += 8;
        }

        // Write length
        w::<u16>(mpc, 4, offset as u16);

        // Compute checksum
        let mpc_slice = std::slice::from_raw_parts(mpc, offset);
        let cksum = mp_checksum(mpc_slice);
        *mpc.add(7) = cksum;

        // === MP Floating Pointer Structure at MP_FP_ADDR ===
        // The kernel scans EBDA (from BDA pointer at 0x40E) and 0xE0000-0xFFFFF
        // for the "_MP_" signature on 16-byte boundaries.
        let mpfp = host_mem.add(MP_FP_ADDR as usize);
        std::ptr::write_bytes(mpfp, 0, 16);

        std::ptr::copy_nonoverlapping(b"_MP_".as_ptr(), mpfp, 4); // Signature
        w::<u32>(mpfp, 4, MP_TABLE_ADDR as u32); // Physical pointer to MP config table
        *mpfp.add(8) = 1; // Length (in 16-byte units)
        *mpfp.add(9) = 4; // Spec revision (MP 1.4)
        // Checksum at offset 10
        // Feature bytes at 11-15 = 0

        let mpfp_slice = std::slice::from_raw_parts(mpfp, 16);
        let cksum = mp_checksum(mpfp_slice);
        *mpfp.add(10) = cksum;

        // Write BDA EBDA pointer at 0x40E (segment of EBDA)
        // EBDA at 0x9FC00 → segment = 0x9FC0
        w::<u16>(host_mem, 0x40E, 0x9FC0u16);
    }

    println!("  MP tables: FP at {MP_FP_ADDR:#X}, config at {MP_TABLE_ADDR:#X}");
}
const PDPTE_ADDR: u64 = 0xB000;
const PDE_ADDR: u64 = 0xC000; // 4 pages: 0xC000, 0xD000, 0xE000, 0xF000

/// Set up identity-mapped page tables for the first 4 GiB using 2MB pages.
/// PML4[0] → PDPTE, PDPTE[0..3] → PDE tables, each with 512 × 2MB entries.
fn setup_page_tables(host_mem: *mut u8) {
    // SAFETY: All writes are within allocated guest memory bounds.
    unsafe {
        // PML4: one entry pointing to PDPTE
        let pml4 = host_mem.add(PML4_ADDR as usize) as *mut u64;
        *pml4 = PDPTE_ADDR | 0x3; // Present + Writable

        // PDPTE: 4 entries, each pointing to a PDE table
        let pdpte = host_mem.add(PDPTE_ADDR as usize) as *mut u64;
        for i in 0..4u64 {
            *pdpte.add(i as usize) = (PDE_ADDR + i * 0x1000) | 0x3; // Present + Writable
        }

        // PDE: 4 tables × 512 entries = 2048 × 2MB pages = 4GB
        for i in 0..2048u64 {
            let pde_table = i / 512;
            let pde_index = i % 512;
            let pde = host_mem.add((PDE_ADDR + pde_table * 0x1000) as usize) as *mut u64;
            // 2MB page: address | PS (bit 7) | Present + Writable
            *pde.add(pde_index as usize) = (i * 0x200000) | 0x83;
        }
    }
}

// ── Register setup ───────────────────────────────────────────────────────────

fn setup_regs(vcpu: &mut dyn hypervisor::Vcpu, entry: u64) -> anyhow::Result<()> {
    use hypervisor::arch::x86::SegmentRegister;

    // Always start in real mode. For protected-mode kernels, a bootstrap
    // at BOOTSTRAP_ADDR handles the real→protected transition via lgdt+CR0.PE.
    let mut regs = vcpu.get_regs().context("get_regs")?;
    regs.set_rip(entry);
    regs.set_rflags(0x2);
    vcpu.set_regs(&regs).context("set_regs")?;

    // Set CS.base=0 so RIP maps directly to physical address
    let mut sregs = vcpu.get_sregs().context("get_sregs")?;
    let code_seg = SegmentRegister {
        base: 0, limit: 0xFFFF, selector: 0,
        type_: 0xB, s: 1, dpl: 0, present: 1, db: 0, g: 0,
        l: 0, avl: 0, unusable: 0,
    };
    let data_seg = SegmentRegister { type_: 0x3, ..code_seg };
    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.ss = data_seg;
    vcpu.set_sregs(&sregs).context("set_sregs")?;

    Ok(())
}

// ── WHP Interrupt Manager for IOAPIC ─────────────────────────────────────────

/// Interrupt manager that delivers interrupts via WHvRequestInterrupt.
/// Used with the existing `devices::ioapic::Ioapic` to provide functional
/// I/O APIC emulation on WHP.
struct WhpInterruptManager {
    vm: Arc<dyn hypervisor::Vm>,
}

impl InterruptManager for WhpInterruptManager {
    type GroupConfig = MsiIrqGroupConfig;

    fn create_group(&self, config: Self::GroupConfig) -> std::io::Result<Arc<dyn InterruptSourceGroup>> {
        Ok(Arc::new(WhpInterruptSourceGroup {
            vm: self.vm.clone(),
            configs: Mutex::new(HashMap::new()),
            masked: Mutex::new(HashMap::new()),
            _base: config.base,
            _count: config.count,
        }))
    }

    fn destroy_group(&self, _group: Arc<dyn InterruptSourceGroup>) -> std::io::Result<()> {
        Ok(())
    }
}

/// Interrupt source group that injects interrupts via WHvRequestInterrupt.
struct WhpInterruptSourceGroup {
    vm: Arc<dyn hypervisor::Vm>,
    configs: Mutex<HashMap<InterruptIndex, MsiIrqSourceConfig>>,
    masked: Mutex<HashMap<InterruptIndex, bool>>,
    _base: InterruptIndex,
    _count: InterruptIndex,
}

impl InterruptSourceGroup for WhpInterruptSourceGroup {
    fn trigger(&self, index: InterruptIndex) -> std::io::Result<()> {
        // Check if masked
        if *self.masked.lock().unwrap().get(&index).unwrap_or(&false) {
            return Ok(());
        }

        // Get the MSI config for this interrupt
        let cfg = match self.configs.lock().unwrap().get(&index).copied() {
            Some(c) => c,
            None => return Ok(()), // No config yet, skip
        };

        // Extract vector and destination from MSI address/data
        let vector = (cfg.data & 0xFF) as u8;
        let destination = ((cfg.low_addr >> 12) & 0xFF) as u32;

        // Inject via WHP
        use hypervisor::whp::WhpVm;
        let whp_vm = self.vm.as_any().downcast_ref::<WhpVm>()
            .ok_or_else(|| std::io::Error::other("not WhpVm"))?;
        whp_vm.request_interrupt(vector, destination)
            .map_err(|e| std::io::Error::other(format!("WHvRequestInterrupt: {e}")))?;

        Ok(())
    }

    fn notifier(&self, _index: InterruptIndex) -> Option<platform::EventFd> {
        None
    }

    fn update(
        &self,
        index: InterruptIndex,
        config: InterruptSourceConfig,
        masked: bool,
        _set_gsi: bool,
    ) -> std::io::Result<()> {
        if let InterruptSourceConfig::MsiIrq(msi_cfg) = config {
            self.configs.lock().unwrap().insert(index, msi_cfg);
        }
        self.masked.lock().unwrap().insert(index, masked);
        Ok(())
    }

    fn set_gsi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── VmOps: serial I/O handler ────────────────────────────────────────────────

struct SerialVmOps {
    input: std::sync::Mutex<std::collections::VecDeque<u8>>,
    seen_ports: std::sync::Mutex<std::collections::BTreeSet<u64>>,
    debug: bool,
    /// Tracks the start time for PIT counter decrement emulation.
    pit_start: std::time::Instant,
    /// PIT counter 2 reload value.
    pit_reload: std::sync::atomic::AtomicU16,
    /// Port 0x61 (NMI Status and Control) register value.
    port61: std::sync::atomic::AtomicU8,
    /// Serial IER (Interrupt Enable Register) at port 0x3F9.
    serial_ier: std::sync::atomic::AtomicU8,
    /// Serial interrupt pending flag (set when THRE interrupt should fire).
    serial_thre_pending: std::sync::atomic::AtomicBool,
    /// PIC master IMR (port 0x21) — track writes for probe detection.
    pic_master_imr: std::sync::atomic::AtomicU8,
    /// PIC slave IMR (port 0xA1).
    pic_slave_imr: std::sync::atomic::AtomicU8,
    /// Real IOAPIC device for interrupt routing.
    ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>>,
}

impl SerialVmOps {
    fn new(ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>>) -> Self {
        SerialVmOps {
            input: std::sync::Mutex::new(std::collections::VecDeque::new()),
            seen_ports: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            debug: std::env::var("CH_DEBUG").is_ok(),
            pit_start: std::time::Instant::now(),
            pit_reload: std::sync::atomic::AtomicU16::new(0xFFFF),
            port61: std::sync::atomic::AtomicU8::new(0),
            serial_ier: std::sync::atomic::AtomicU8::new(0),
            serial_thre_pending: std::sync::atomic::AtomicBool::new(false),
            pic_master_imr: std::sync::atomic::AtomicU8::new(0xFF),
            pic_slave_imr: std::sync::atomic::AtomicU8::new(0xFF),
            ioapic,
        }
    }

    fn feed_input(&self, data: &[u8]) {
        let mut buf = self.input.lock().unwrap();
        buf.extend(data);
    }

    fn has_input(&self) -> bool {
        let buf = self.input.lock().unwrap();
        !buf.is_empty()
    }

    fn read_input(&self) -> Option<u8> {
        let mut buf = self.input.lock().unwrap();
        buf.pop_front()
    }
}

impl VmOps for SerialVmOps {
    fn guest_mem_write(&self, _gpa: u64, _buf: &[u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }
    fn guest_mem_read(&self, _gpa: u64, _buf: &mut [u8]) -> Result<usize, HypervisorVmError> {
        Ok(0)
    }
    fn mmio_read(&self, gpa: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        match gpa {
            0xFEC00000..=0xFEC000FF => {
                // Forward to real IOAPIC device
                if let Some(ref ioapic) = self.ioapic {
                    use vm_device::BusDevice;
                    ioapic.lock().unwrap().read(0xFEC0_0000, gpa - 0xFEC0_0000, data);
                } else {
                    data.fill(0);
                }
            }
            _ => {
                if self.debug {
                    eprintln!("[MMIO] read GPA={gpa:#X} len={}", data.len());
                }
                data.fill(0);
            }
        }
        Ok(())
    }
    fn mmio_write(&self, gpa: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        match gpa {
            0xFEC00000..=0xFEC000FF => {
                // Forward to real IOAPIC device
                if let Some(ref ioapic) = self.ioapic {
                    use vm_device::BusDevice;
                    ioapic.lock().unwrap().write(0xFEC0_0000, gpa - 0xFEC0_0000, data);
                }
            }
            _ => {
                if self.debug {
                    eprintln!("[MMIO] write GPA={gpa:#X} data={data:02X?}");
                }
            }
        }
        Ok(())
    }
    fn pio_read(&self, port: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        if self.debug {
            let mut ports = self.seen_ports.lock().unwrap();
            if ports.insert(port) && ports.len() <= 50 {
                eprintln!("[PIO] read port {port:#X}");
            }
        }
        match port {
            // Serial data register — return next input byte
            0x3F8 => {
                data[0] = self.read_input().unwrap_or(0);
            }
            // Serial IER (Interrupt Enable Register) — read back
            0x3F9 => {
                data[0] = self.serial_ier.load(std::sync::atomic::Ordering::Relaxed);
            }
            // Serial IIR (Interrupt Identification Register)
            0x3FA => {
                use std::sync::atomic::Ordering;
                let ier = self.serial_ier.load(Ordering::Relaxed);
                if self.serial_thre_pending.load(Ordering::Relaxed) && (ier & 0x02) != 0 {
                    // THRE interrupt pending
                    data[0] = 0x02; // IIR: THRE interrupt
                    self.serial_thre_pending.store(false, Ordering::Relaxed);
                } else if self.has_input() && (ier & 0x01) != 0 {
                    // Data available interrupt
                    data[0] = 0x04; // IIR: received data available
                } else {
                    data[0] = 0x01; // IIR: no interrupt pending
                }
                data[0] |= 0xC0; // FIFO enabled bits
            }
            // Serial MCR
            0x3FC => { data[0] = 0x08; /* MCR: AUX output 2 (interrupt enable) */ }
            // Serial Line Status Register
            0x3FD => {
                let mut lsr = 0x60; // THR empty + Transmitter idle
                if self.has_input() {
                    lsr |= 0x01; // Data ready
                }
                data[0] = lsr;
            }
            // PIT counter 2 data port — return simulated decrementing counter
            0x42 => {
                use std::sync::atomic::Ordering;
                // PIT runs at ~1.193182 MHz. Simulate counter decrement based on elapsed time.
                let elapsed_us = self.pit_start.elapsed().as_micros() as u64;
                // ~1.19 ticks per microsecond
                let ticks = (elapsed_us * 1193) / 1000;
                let reload = self.pit_reload.load(Ordering::Relaxed) as u64;
                let counter = if reload > 0 {
                    reload.saturating_sub(ticks % (reload + 1))
                } else {
                    0
                };
                data[0] = counter as u8;
            }
            // Port 0x61: NMI Status and Control Register
            // Bit 5 = Timer Counter 2 output (toggles based on PIT counter 2)
            0x61 => {
                use std::sync::atomic::Ordering;
                let val = self.port61.load(Ordering::Relaxed);
                let elapsed_us = self.pit_start.elapsed().as_micros() as u64;
                let reload = self.pit_reload.load(Ordering::Relaxed) as u64;
                let cycles = if reload > 0 { elapsed_us * 1193 / 1000 / (reload + 1) } else { 0 };
                // Toggle bit 5 based on elapsed PIT cycles
                let out_bit = if cycles % 2 == 0 { 0 } else { 0x20 };
                data[0] = (val & !0x20) | out_bit;
            }
            // CMOS/RTC
            0x71 => {
                // Return 0 for all CMOS reads (the kernel just needs something)
                data[0] = 0;
            }
            // PIC (8259) emulation — track IMR for probe detection
            0x20 => { data[0] = 0x00; } // Master PIC CMD: IRR = no pending
            0x21 => {
                data[0] = self.pic_master_imr.load(std::sync::atomic::Ordering::Relaxed);
            }
            0xA0 => { data[0] = 0x00; } // Slave PIC CMD
            0xA1 => {
                data[0] = self.pic_slave_imr.load(std::sync::atomic::Ordering::Relaxed);
            }
            _ => data.fill(0xFF),
        }
        Ok(())
    }
    fn pio_write(&self, port: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        if self.debug {
            let mut ports = self.seen_ports.lock().unwrap();
            if ports.insert(port | 0x10000) && ports.len() <= 50 {
                eprintln!("[PIO] write port {port:#X} data={data:02X?}");
            }
        }
        if port == SERIAL_PORT && !data.is_empty() {
            let ch = data[0];
            if ch.is_ascii() {
                print!("{}", ch as char);
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // After writing data, THRE becomes empty → set pending
            let ier = self.serial_ier.load(std::sync::atomic::Ordering::Relaxed);
            if (ier & 0x02) != 0 {
                self.serial_thre_pending.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else if port == 0x3F9 && !data.is_empty() {
            // IER write
            self.serial_ier.store(data[0], std::sync::atomic::Ordering::Relaxed);
        } else if port == DEBUG_EXIT_PORT {
            println!();
            println!("--- Guest requested shutdown ---");
            std::process::exit(0);
        } else if port == 0x42 && !data.is_empty() {
            // PIT counter 2 data — reload value
            use std::sync::atomic::Ordering;
            self.pit_reload.store(data[0] as u16, Ordering::Relaxed);
        } else if port == 0x61 && !data.is_empty() {
            // NMI Status and Control
            use std::sync::atomic::Ordering;
            self.port61.store(data[0], Ordering::Relaxed);
        } else if port == 0x21 && !data.is_empty() {
            // PIC master IMR write
            self.pic_master_imr.store(data[0], std::sync::atomic::Ordering::Relaxed);
        } else if port == 0xA1 && !data.is_empty() {
            // PIC slave IMR write
            self.pic_slave_imr.store(data[0], std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }
}
