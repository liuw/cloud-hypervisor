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

    // Override the bootstrap's jump target to the ELF entry
    // The simple bootstrap jumps to KERNEL_LOAD_ADDR (0x100000) by default.
    // For ELF, we need to jump to the actual entry point.
    // Patch the far jump offset in the bootstrap code.
    // The far jump is at offset 14 in the bootstrap: 66 EA <4-byte offset> <2-byte selector>
    // Offset of the jump target = BOOTSTRAP_ADDR + 16 (14 for instructions before + 2 for 66 EA)
    // SAFETY: Patching within guest memory.
    unsafe {
        *(host_mem.add(BOOTSTRAP_ADDR as usize + 16) as *mut u32) = entry as u32;
    }
    println!("  Patched bootstrap jmp target to {entry:#x}");

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
    code.extend_from_slice(&0x10u16.to_le_bytes()); // CS = 32-bit code segment

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

    // 16-bit bootstrap code at BOOTSTRAP_ADDR
    let mut code = Vec::new();
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
    // jmp far 0x10:KERNEL_LOAD_ADDR (32-bit code segment, offset to kernel)
    code.extend_from_slice(&[0x66, 0xEA]);
    code.extend_from_slice(&(KERNEL_LOAD_ADDR as u32).to_le_bytes());
    code.extend_from_slice(&0x10u16.to_le_bytes()); // CS selector = GDT entry 2

    // SAFETY: Writing within guest memory bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), host_mem.add(BOOTSTRAP_ADDR as usize), code.len());
    }
    println!("  Bootstrap ({} bytes) at {BOOTSTRAP_ADDR:#X} → far jmp to {KERNEL_LOAD_ADDR:#X}", code.len());
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

/// Set up Linux boot parameters (zero page) at BOOT_PARAMS_ADDR.
fn setup_linux_boot_params(host_mem: *mut u8) {
    // SAFETY: Writing boot_params fields within allocated guest memory.
    unsafe {
        let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);

        // Zero the entire 4K boot_params page
        std::ptr::write_bytes(bp, 0, 4096);

        // Boot protocol header signature at offset 0x202 = "HdrS"
        std::ptr::copy_nonoverlapping(b"HdrS".as_ptr(), bp.add(0x202), 4);

        // Boot protocol version at offset 0x206 = 0x020F (2.15)
        *(bp.add(0x206) as *mut u16) = 0x020F;

        // vid_mode at offset 0x1FA = 0xFFFF (normal)
        *(bp.add(0x1FA) as *mut u16) = 0xFFFF;

        // type_of_loader at offset 0x210 = 0xFF
        *bp.add(0x210) = 0xFF;

        // loadflags at offset 0x211 = LOADED_HIGH | KEEP_SEGMENTS | CAN_USE_HEAP
        *bp.add(0x211) = 0xC1;

        // cmd_line_ptr at offset 0x228
        *(bp.add(0x228) as *mut u32) = CMDLINE_ADDR as u32;

        // Write command line
        let cmdline = b"console=ttyS0 earlyprintk=serial noapic noacpi pci=off nomodules\0";
        std::ptr::copy_nonoverlapping(
            cmdline.as_ptr(),
            host_mem.add(CMDLINE_ADDR as usize),
            cmdline.len(),
        );

        // E820 memory map — one entry: 0 to GUEST_MEM_SIZE, type=RAM(1)
        // e820_table starts at offset 0x2D0, each entry is 20 bytes
        let e820 = bp.add(0x2D0);
        *(e820 as *mut u64) = 0;                      // addr
        *(e820.add(8) as *mut u64) = GUEST_MEM_SIZE as u64; // size
        *(e820.add(16) as *mut u32) = 1;               // type = RAM

        // e820_entries at offset 0x1E8
        *bp.add(0x1E8) = 1;

        // init_size at offset 0x260 (required for recent kernels)
        *(bp.add(0x260) as *mut u32) = GUEST_MEM_SIZE as u32;
    }

    println!("  Boot params at {BOOT_PARAMS_ADDR:#X}, cmdline at {CMDLINE_ADDR:#X}");
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
    regs.set_rsi(BOOT_PARAMS_ADDR);
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
}

impl SerialVmOps {
    fn new() -> Self {
        SerialVmOps {
            input: std::sync::Mutex::new(std::collections::VecDeque::new()),
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
    fn mmio_read(&self, _gpa: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
        data.fill(0xFF);
        Ok(())
    }
    fn mmio_write(&self, _gpa: u64, _data: &[u8]) -> Result<(), HypervisorVmError> {
        Ok(())
    }
    fn pio_read(&self, port: u64, data: &mut [u8]) -> Result<(), HypervisorVmError> {
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
            use std::io::Write;
            let _ = std::io::stdout().flush();
        } else if port == DEBUG_EXIT_PORT {
            println!();
            println!("--- Guest requested shutdown ---");
            std::process::exit(0);
        }
        Ok(())
    }
}
