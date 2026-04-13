// SPDX-License-Identifier: Apache-2.0
//
// Windows Hypervisor Platform demo: load and execute a guest payload.
//
// Usage:
//   cloud-hypervisor.exe                       # run built-in "Hi!" payload
//   cloud-hypervisor.exe --kernel <bzImage>     # load a Linux bzImage

use std::fs::File;
use std::io::{Read, Seek};
use std::sync::Arc;

use anyhow::{Context, anyhow};
use hypervisor::{HypervisorVmError, VmOps};
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

    // ── Load payload ─────────────────────────────────────────────────────
    let entry_point = if let Some(ref path) = kernel_path {
        load_kernel(host_mem, path)?
    } else {
        load_demo_payload(host_mem)
    };

    // ── Set up GDT and page tables for protected/long mode ─────────────
    if kernel_path.is_some() {
        setup_gdt(host_mem);
        setup_page_tables(host_mem);
        setup_acpi_tables(host_mem);
    }

    // ── Create vCPU ──────────────────────────────────────────────────────
    let vm_ops = Arc::new(SerialVmOps::new());
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
    // Timer interrupt injection is handled in the vCPU loop below.

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
        let cmdline = b"console=ttyS0 earlyprintk=serial,ttyS0,115200 nomodules tsc=reliable lpj=1000000 no_timer_check lapic\0";
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

        // acpi_rsdp_addr at offset 0x070 (boot_params.acpi_rsdp_addr)
        w::<u64>(bp, 0x070, ACPI_RSDP_ADDR);

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
        w::<u64>(info, 24, ACPI_RSDP_ADDR);           // rsdp_paddr

        // Memory map entry
        let memmap = host_mem.add(MEMMAP_START as usize);
        w::<u64>(memmap, 0, 0);                     // addr
        w::<u64>(memmap, 8, GUEST_MEM_SIZE as u64); // size
        w::<u32>(memmap, 16, 1);                    // type = RAM
    }

    println!("  Boot params at {BOOT_PARAMS_ADDR:#X}, PVH info at 0x6000, cmdline at {CMDLINE_ADDR:#X}");
}

/// ACPI table base addresses
const ACPI_RSDP_ADDR: u64 = 0x000E_0000; // RSDP in BIOS ROM scan range (0xE0000-0xFFFFF)
const ACPI_TABLES_ADDR: u64 = 0x00F0_0000; // XSDT + MADT at 15MB

/// Set up minimal ACPI tables (RSDP → XSDT → MADT) so the kernel finds the APIC.
fn setup_acpi_tables(host_mem: *mut u8) {
    /// Write a value at an unaligned offset within guest memory.
    unsafe fn w<T: Copy>(base: *mut u8, offset: usize, val: T) {
        std::ptr::write_unaligned(base.add(offset) as *mut T, val);
    }

    /// Compute ACPI table checksum (sum of all bytes must be 0).
    fn acpi_checksum(data: &[u8]) -> u8 {
        let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        (!sum).wrapping_add(1)
    }

    // SAFETY: Writing ACPI structures within allocated guest memory.
    unsafe {
        // === MADT (Multiple APIC Description Table) ===
        // Layout: header (44 bytes) + Local APIC entry (8 bytes) + I/O APIC entry (12 bytes)
        let madt_addr = ACPI_TABLES_ADDR + 64; // after XSDT
        let madt = host_mem.add(madt_addr as usize);
        std::ptr::write_bytes(madt, 0, 128);

        // MADT header (44 bytes)
        std::ptr::copy_nonoverlapping(b"APIC".as_ptr(), madt, 4); // Signature
        let madt_len: u32 = 44 + 8 + 12; // header + LAPIC + IOAPIC
        w::<u32>(madt, 4, madt_len); // Length
        *madt.add(8) = 5; // Revision
        // Checksum at offset 9 — computed later
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), madt.add(10), 6); // OEM ID
        std::ptr::copy_nonoverlapping(b"CHMADT  ".as_ptr(), madt.add(16), 8); // OEM Table ID
        w::<u32>(madt, 24, 1); // OEM Revision
        std::ptr::copy_nonoverlapping(b"CLHV".as_ptr(), madt.add(28), 4); // Creator ID
        w::<u32>(madt, 32, 1); // Creator Revision
        w::<u32>(madt, 36, 0xFEE0_0000); // Local APIC Address
        w::<u32>(madt, 40, 1); // Flags (PCAT_COMPAT)

        // Local APIC entry (type 0, length 8)
        let lapic = madt.add(44);
        *lapic = 0; // Type = Processor Local APIC
        *lapic.add(1) = 8; // Length
        *lapic.add(2) = 0; // ACPI Processor ID
        *lapic.add(3) = 0; // APIC ID
        w::<u32>(lapic, 4, 1); // Flags: Enabled

        // I/O APIC entry (type 1, length 12)
        let ioapic = madt.add(52);
        *ioapic = 1; // Type = I/O APIC
        *ioapic.add(1) = 12; // Length
        *ioapic.add(2) = 0; // I/O APIC ID
        *ioapic.add(3) = 0; // Reserved
        w::<u32>(ioapic, 4, 0xFEC0_0000); // I/O APIC Address
        w::<u32>(ioapic, 8, 0); // Global System Interrupt Base

        // Compute MADT checksum
        let madt_slice = std::slice::from_raw_parts(madt, madt_len as usize);
        let cksum = acpi_checksum(madt_slice);
        *madt.add(9) = cksum;

        // === XSDT (Extended System Description Table) ===
        let xsdt = host_mem.add(ACPI_TABLES_ADDR as usize);
        std::ptr::write_bytes(xsdt, 0, 64);

        // XSDT header (36 bytes) + one 8-byte pointer to MADT
        let xsdt_len: u32 = 36 + 8;
        std::ptr::copy_nonoverlapping(b"XSDT".as_ptr(), xsdt, 4);
        w::<u32>(xsdt, 4, xsdt_len);
        *xsdt.add(8) = 1; // Revision
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), xsdt.add(10), 6);
        std::ptr::copy_nonoverlapping(b"CHXSDT  ".as_ptr(), xsdt.add(16), 8);
        w::<u32>(xsdt, 24, 1);
        std::ptr::copy_nonoverlapping(b"CLHV".as_ptr(), xsdt.add(28), 4);
        w::<u32>(xsdt, 32, 1);
        // Pointer to MADT
        w::<u64>(xsdt, 36, madt_addr);
        // Checksum
        let xsdt_slice = std::slice::from_raw_parts(xsdt, xsdt_len as usize);
        let cksum = acpi_checksum(xsdt_slice);
        *xsdt.add(9) = cksum;

        // === RSDP (Root System Description Pointer) ===
        let rsdp = host_mem.add(ACPI_RSDP_ADDR as usize);
        std::ptr::write_bytes(rsdp, 0, 36);

        // RSDP v2 (36 bytes)
        std::ptr::copy_nonoverlapping(b"RSD PTR ".as_ptr(), rsdp, 8); // Signature
        // Checksum at offset 8 — computed later
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), rsdp.add(9), 6); // OEM ID
        *rsdp.add(15) = 2; // Revision (ACPI 2.0+)
        w::<u32>(rsdp, 16, ACPI_TABLES_ADDR as u32); // RSDT Address (same as XSDT for simplicity)
        w::<u32>(rsdp, 20, 36); // Length (RSDP v2 = 36 bytes)
        w::<u64>(rsdp, 24, ACPI_TABLES_ADDR); // XSDT Address
        // Extended checksum at offset 32
        let rsdp_slice = std::slice::from_raw_parts(rsdp, 36);
        // First compute checksum for bytes 0-19 (RSDP v1 portion)
        let cksum_v1 = acpi_checksum(&rsdp_slice[..20]);
        *rsdp.add(8) = cksum_v1;
        // Then extended checksum for all 36 bytes
        let rsdp_slice = std::slice::from_raw_parts(rsdp, 36);
        let cksum_ext = acpi_checksum(rsdp_slice);
        *rsdp.add(32) = cksum_ext;
    }

    println!("  ACPI tables: RSDP at {ACPI_RSDP_ADDR:#X}, XSDT+MADT at {ACPI_TABLES_ADDR:#X}");
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
}

impl SerialVmOps {
    fn new() -> Self {
        SerialVmOps {
            input: std::sync::Mutex::new(std::collections::VecDeque::new()),
            seen_ports: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            debug: std::env::var("CH_DEBUG").is_ok(),
            pit_start: std::time::Instant::now(),
            pit_reload: std::sync::atomic::AtomicU16::new(0xFFFF),
            port61: std::sync::atomic::AtomicU8::new(0),
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
        if self.debug {
            eprintln!("[MMIO] read GPA={gpa:#X} len={}", data.len());
        }
        // Return 0 for IOAPIC/LAPIC reads (WHP handles LAPIC internally)
        data.fill(0);
        Ok(())
    }
    fn mmio_write(&self, gpa: u64, data: &[u8]) -> Result<(), HypervisorVmError> {
        if self.debug {
            eprintln!("[MMIO] write GPA={gpa:#X} data={data:02X?}");
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
            // PIC (8259) — mask registers
            0x21 | 0xA1 => {
                data[0] = 0xFF; // All IRQs masked
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
        }
        Ok(())
    }
}
