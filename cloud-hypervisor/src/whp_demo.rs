// SPDX-License-Identifier: Apache-2.0
//
// Windows Hypervisor Platform demo: load and execute a guest payload.
//
// Usage:
//   cloud-hypervisor.exe                       # run built-in "Hi!" payload
//   cloud-hypervisor.exe --kernel <bzImage>     # load a Linux bzImage

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};
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

const GUEST_MEM_SIZE: usize = 512 << 20; // 512 MiB
const DEBUG_EXIT_PORT: u64 = 0x501;

// PCI configuration I/O ports
const PCI_CONFIG_ADDR_PORT: u64 = 0xCF8;
const PCI_CONFIG_DATA_PORT: u64 = 0xCFC;

// PCI device slots
const PCI_HOST_BRIDGE_SLOT: u32 = 0;
const PCI_BLK_SLOT: u32 = 1;

// Default I/O BAR address for virtio-blk (may be reprogrammed by kernel)
const VIRTIO_BLK_IO_BAR_DEFAULT: u32 = 0xC000;
const VIRTIO_BLK_IO_BAR_SIZE: u32 = 64;

// MSI-X table MMIO BAR
const MSIX_BAR_GPA: u64 = 0xFEA0_0000;
const MSIX_BAR_SIZE: u32 = 4096;
const MSIX_TABLE_ENTRIES: u16 = 2; // config + requestq

// Legacy virtio register offsets (within I/O BAR)
const VIRTIO_PCI_HOST_FEATURES: u16 = 0;
const VIRTIO_PCI_GUEST_FEATURES: u16 = 4;
const VIRTIO_PCI_QUEUE_PFN: u16 = 8;
const VIRTIO_PCI_QUEUE_NUM: u16 = 12;
const VIRTIO_PCI_QUEUE_SEL: u16 = 14;
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 16;
const VIRTIO_PCI_STATUS: u16 = 18;
const VIRTIO_PCI_ISR: u16 = 19;
const VIRTIO_PCI_MSIX_CONFIG_VECTOR: u16 = 20;
const VIRTIO_PCI_MSIX_QUEUE_VECTOR: u16 = 22;
const VIRTIO_PCI_CONFIG_OFF: u16 = 24; // device config starts here (with MSI-X)

// Virtio device status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// Virtio block request types
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

// Virtio block status
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// Virtio descriptor flags
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

// Virtio MSI-X no vector
const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

const BLK_QUEUE_SIZE: u16 = 128;

// Linux boot protocol addresses
const BOOT_PARAMS_ADDR: u64 = 0x7000;
const CMDLINE_ADDR: u64 = 0x20000;
const KERNEL_LOAD_ADDR: u64 = 0x100000; // 1 MiB — protected-mode kernel

type GuestMem = GuestMemoryMmap<AtomicBitmap>;

/// Boot the VM: load payload, create devices, start vCPU and helper threads.
///
/// Returns immediately after starting all threads. The vCPU thread will
/// signal `exit_evt` when the guest shuts down, which causes the VMM
/// control loop to exit.
pub fn boot(
    exit_evt: platform::EventFd,
    payload: vmm::vm_config::PayloadConfig,
    disk_path: Option<String>,
    vm: Arc<dyn hypervisor::Vm>,
    memory_manager: Arc<Mutex<vmm::memory_manager::MemoryManager>>,
    vm_ops: Arc<dyn hypervisor::VmOps>,
    serial: Arc<Mutex<devices::legacy::serial::Serial>>,
) -> anyhow::Result<()> {
    // ── Get guest memory from the VMM's memory manager ───────────────────
    let mm = memory_manager.lock().unwrap();
    let guest_mem = mm.guest_memory().clone();
    let host_mem = mm.host_address(GuestAddress(0))
        .map_err(|e| anyhow!("get_host_address: {e:?}"))?;
    let ram_size = mm.ram_size();
    drop(mm);

    println!("Using VMM-managed memory: {} MiB at GPA 0x0", ram_size >> 20);

    // Map IOAPIC version page for kernel mode (WHP needs physical backing)
    if payload.kernel.is_some() {
        let ioapic_page_layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
        let ioapic_host = unsafe { std::alloc::alloc_zeroed(ioapic_page_layout) };
        if !ioapic_host.is_null() {
            unsafe {
                std::ptr::write_unaligned(ioapic_host as *mut u32, 1);
                std::ptr::write_unaligned(ioapic_host.add(0x10) as *mut u32, 0x0017_0011u32);
                let _ = vm.create_user_memory_region(1, 0xFEC0_0000, 4096, ioapic_host, false, false);
            }
            println!("  Mapped IOAPIC version page at GPA 0xFEC00000");
        }
    }

    // ── Create PCI virtio-blk device if disk is provided ─────────────────
    let pci_blk: Option<Arc<Mutex<PciBlkDevice>>> = if let Some(ref path) = disk_path {
        // Allocate and map MSI-X table page in guest address space
        let msix_page_layout = std::alloc::Layout::from_size_align(MSIX_BAR_SIZE as usize, 4096).unwrap();
        let msix_host = unsafe { std::alloc::alloc_zeroed(msix_page_layout) };
        if msix_host.is_null() {
            return Err(anyhow!("Failed to allocate MSI-X table page"));
        }
        // Initialize all MSI-X entries as masked
        unsafe {
            for i in 0..MSIX_TABLE_ENTRIES as usize {
                let entry = msix_host.add(i * 16);
                // Vector Control: bit 0 = masked
                std::ptr::write_unaligned(entry.add(12) as *mut u32, 1);
            }
        }
        unsafe {
            vm.create_user_memory_region(2, MSIX_BAR_GPA, MSIX_BAR_SIZE as usize, msix_host, false, false)
                .context("Failed to map MSI-X BAR")?;
        }
        println!("Mapped MSI-X table at GPA {MSIX_BAR_GPA:#X}");

        let dev = PciBlkDevice::new(path, guest_mem.clone(), vm.clone(), msix_host)?;
        println!("Created virtio-blk PCI device: {} sectors ({:.1} MiB), disk={path}",
                 dev.capacity, (dev.capacity * 512) as f64 / (1024.0 * 1024.0));
        Some(Arc::new(Mutex::new(dev)))
    } else {
        None
    };

    // ── Load payload ─────────────────────────────────────────────────────
    let entry_point = if let Some(ref path) = payload.kernel {
        load_kernel(host_mem, path.to_str().unwrap(), disk_path.is_some())?
    } else {
        load_demo_payload(host_mem)
    };

    // ── Load initramfs if provided ───────────────────────────────────────
    let mut initrd_addr: u64 = 0;
    let mut initrd_size: u64 = 0;
    if let Some(ref path) = payload.initramfs {
        let mut f = File::open(path)
            .with_context(|| format!("Failed to open initramfs: {}", path.display()))?;
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
        println!("Loaded initramfs: {} ({fsize} bytes at GPA {addr:#X})", path.display());
    }

    // ── Set up GDT and page tables for protected/long mode ─────────────
    if payload.kernel.is_some() {
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

    // ── Create vCPU using bus-based VmOps from device manager ────────────
    // Serial and IOAPIC are already registered on the I/O and MMIO buses
    // by the device manager. The vm_ops dispatches PIO/MMIO to the buses.
    let mut vcpu = vm
        .create_vcpu(0, Some(vm_ops))
        .context("Failed to create vCPU")?;

    // Set initial register state
    setup_regs(&mut *vcpu, entry_point)?;

    println!("vCPU 0 ready, RIP={entry_point:#x}. Running...");
    println!("--- Guest serial output ---");

    // ── Set terminal to raw mode for interactive serial console ──────────
    // Save the terminal state so we can restore it on exit. In raw mode,
    // keystrokes are sent immediately (no line buffering, no echo).
    let terminal_state = if platform::is_terminal(0) {
        let state = platform::save_terminal_state(0)
            .context("Failed to save terminal state")?;
        platform::set_raw_mode(0).context("Failed to set raw mode")?;
        Some(state)
    } else {
        None
    };

    // Ensure terminal is restored even on panic
    let terminal_state_for_panic = terminal_state.clone();
    let prev_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(ref state) = terminal_state_for_panic {
            let _ = platform::restore_terminal_state(0, state);
        }
        prev_panic(info);
    }));

    // Start a timer interrupt injection thread for kernel boot.
    // The kernel needs periodic timer interrupts (IRQ 0) to run the scheduler.
    // We inject a fixed interrupt at vector 0x20 (standard PIT→PIC mapping) at ~100 Hz.
    // Timer interrupt injection is handled via a background thread.
    // After a delay (letting kernel boot), periodically inject timer interrupts
    // and cancel the vCPU run to wake it from WHP-internal HLT.
    // Timer interrupt injection thread.
    // Injects both LAPIC timer vector and IRQ 0 to advance jiffies and drive work queues.
    if payload.kernel.is_some() {
        use hypervisor::whp::WhpVm;
        let vm_for_timer: Arc<dyn hypervisor::Vm> = vm.clone();
        std::thread::Builder::new()
            .name("timer-inject".to_string())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let whp = vm_for_timer.as_any().downcast_ref::<WhpVm>().unwrap();
                eprintln!("[timer] Starting timer injection (0x20 PIC IRQ 0)");
                let mut tick = 0u64;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    // Inject timer vector only; serial interrupt is now handled
                    // by the Serial device directly via queue_input_bytes().
                    // 0x30 = PIT timer (IRQ 0 via IOAPIC), advances jiffies
                    let _ = whp.request_interrupt(0x30, 0);
                    tick += 1;
                    if tick == 1 { eprintln!("[timer] First tick OK"); }
                }
            })
            .context("Failed to spawn timer thread")?;
    }

    // Use the exit_evt passed from the VMM control loop.
    // Start the serial manager which reads from stdin and feeds input
    // to the serial device via queue_input_bytes().
    let mut serial_mgr = vmm::serial_manager::SerialManager::new(serial.clone())
        .context("Failed to create serial manager")?;
    if let Some(ref mut mgr) = serial_mgr {
        mgr.start_thread(exit_evt.try_clone().unwrap())
            .context("Failed to start serial manager")?;
    }

    // ── Run vCPU in a dedicated thread ─────────────────────────────────
    let vcpu_exit_evt = exit_evt.try_clone().unwrap();
    let terminal_state_for_vcpu = terminal_state.clone();
    let debug_kernel = std::env::var("CH_DEBUG").is_ok();
    let _vcpu_thread = std::thread::Builder::new()
        .name("vcpu-0".to_string())
        .spawn(move || {
            let mut exit_count = 0u64;
            let start = std::time::Instant::now();
            let mut last_rip_dump = std::time::Instant::now();
            loop {
                match vcpu.run() {
                    Ok(hypervisor::VmExit::Ignore) => {}
                    Ok(hypervisor::VmExit::Shutdown) => {
                        println!("\n--- Guest halted (after {exit_count} exits, {:.1}s) ---",
                                 start.elapsed().as_secs_f64());
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
                        let msg = format!("{e}");
                        if msg.contains("Guest requested shutdown") {
                            println!("\n--- Guest requested shutdown ---");
                        } else {
                            eprintln!("\nvCPU run error: {e}");
                        }
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
            // Signal the VMM control loop to exit
            vcpu_exit_evt.write(1).ok();

            // Restore terminal state from vCPU thread
            if let Some(ref state) = terminal_state_for_vcpu {
                let _ = platform::restore_terminal_state(0, state);
            }
            println!("WHP VM stopped.");
        })
        .context("Failed to spawn vCPU thread")?;

    // Don't join the vCPU thread — return immediately.
    // The VMM control loop will handle the exit event.
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

        // offset = 63 bytes (0x3F) — echo loop starts here
        // Echo loop:
        //   wait for LSR bit 0 (data ready)
        0xBA, 0xFD, 0x03,       // mov dx, 0x3FD     ; LSR port
        0xEC,                   // in al, dx          ; read LSR
        0xA8, 0x01,             // test al, 1         ; bit 0 = data ready?
        0x74, 0xFB,             // jz -5              ; loop back to "in al, dx"

        //   read character
        0xBA, 0xF8, 0x03,       // mov dx, 0x3F8     ; data port
        0xEC,                   // in al, dx          ; read character

        //   check for Ctrl+C
        0x3C, 0x03,             // cmp al, 0x03
        0x74, 0x0A,             // je shutdown (10 bytes forward)

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

fn load_kernel(host_mem: *mut u8, path: &str, has_disk: bool) -> anyhow::Result<u64> {
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
        load_elf(host_mem, &mut file, has_disk)
    } else if header_len > 0x206 && &header[0x202..0x206] == b"HdrS" {
        println!("  Format: bzImage");
        let mut data = vec![0u8; file_size];
        file.read_exact(&mut data)?;
        load_bzimage(host_mem, &data, has_disk)
    } else {
        println!("  Format: flat binary");
        let mut data = vec![0u8; file_size];
        file.read_exact(&mut data)?;
        load_flat_binary(host_mem, &data)
    }
}

fn load_elf(host_mem: *mut u8, file: &mut File, has_disk: bool) -> anyhow::Result<u64> {
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
    setup_linux_boot_params(host_mem, has_disk);

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

fn load_bzimage(host_mem: *mut u8, data: &[u8], has_disk: bool) -> anyhow::Result<u64> {
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

    // Write command line — adjust for PCI if disk is available
    let cmdline: Vec<u8> = if has_disk {
        b"console=ttyS0 earlyprintk=serial noapic noacpi root=/dev/vda rw rootwait\0".to_vec()
    } else {
        b"console=ttyS0 earlyprintk=serial noapic noacpi pci=off\0".to_vec()
    };
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
fn setup_linux_boot_params(host_mem: *mut u8, has_disk: bool) {
    /// Write a value at an unaligned offset within guest memory.
    unsafe fn w<T: Copy>(base: *mut u8, offset: usize, val: T) {
        std::ptr::write_unaligned(base.add(offset) as *mut T, val);
    }

    // SAFETY: Writing boot structures within allocated guest memory.
    unsafe {
        // === Zero page (boot_params) at BOOT_PARAMS_ADDR (0x7000) ===
        let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);
        std::ptr::write_bytes(bp, 0, 4096);

        // Write command line — adjust based on whether disk is available
        let cmdline: Vec<u8> = if has_disk {
            b"console=ttyS0,115200 earlyprintk=serial,ttyS0,115200 nomodules lpj=1000000 no_timer_check tsc=reliable idle=halt root=/dev/vda rw rootwait\0".to_vec()
        } else {
            b"console=ttyS0,115200 earlyprintk=serial,ttyS0,115200 nomodules lpj=1000000 no_timer_check tsc=reliable idle=halt rdinit=/init\0".to_vec()
        };
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
        w::<u16>(mpc, 34, 4 + 17); // Entry count (1 CPU + 2 bus + 1 IOAPIC + 16 ISA IRQ + 1 PCI IRQ)
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

        // Bus 0: PCI (type 1, 8 bytes)
        let bus0 = mpc.add(offset);
        *bus0 = 1; // Entry type = Bus
        *bus0.add(1) = 0; // Bus ID 0
        std::ptr::copy_nonoverlapping(b"PCI   ".as_ptr(), bus0.add(2), 6);
        offset += 8;

        // Bus 1: ISA (type 1, 8 bytes)
        let bus1 = mpc.add(offset);
        *bus1 = 1; // Entry type = Bus
        *bus1.add(1) = 1; // Bus ID 1
        std::ptr::copy_nonoverlapping(b"ISA   ".as_ptr(), bus1.add(2), 6);
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
        // Map ISA IRQs 0-15 to I/O APIC inputs 0-15 (source = bus 1 ISA)
        for irq in 0..16u8 {
            let entry = mpc.add(offset);
            *entry = 3; // Entry type = I/O Interrupt Assignment
            *entry.add(1) = 0; // Interrupt type: INT (vectored)
            w::<u16>(entry, 2, 0); // Flags: default (bus-type dependent)
            *entry.add(4) = 1; // Source bus ID = 1 (ISA)
            *entry.add(5) = irq; // Source bus IRQ
            *entry.add(6) = 0; // Dest I/O APIC ID
            *entry.add(7) = irq; // Dest I/O APIC INTIN#
            offset += 8;
        }

        // PCI interrupt routing: device 1 INTA# → IOAPIC pin 16
        // Source bus IRQ for PCI: (device << 2) | (pin - 1) = (1 << 2) | 0 = 4
        {
            let entry = mpc.add(offset);
            *entry = 3; // Entry type = I/O Interrupt Assignment
            *entry.add(1) = 0; // Interrupt type: INT
            w::<u16>(entry, 2, 0x000F); // Flags: active-low, level-triggered (PCI)
            *entry.add(4) = 0; // Source bus ID = 0 (PCI)
            *entry.add(5) = (PCI_BLK_SLOT as u8) << 2; // Source: device 1, pin A
            *entry.add(6) = 0; // Dest I/O APIC ID
            *entry.add(7) = 16; // Dest I/O APIC INTIN# 16
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

    // === ACPI tables: RSDP → XSDT → MADT ===
    // For kernels with ACPI support (e.g., CH's 6.x kernel).
    const ACPI_RSDP_ADDR: u64 = 0x000E_0000;
    const ACPI_TABLES_ADDR: u64 = 0x000E_1000;

    unsafe {
        // MADT at ACPI_TABLES_ADDR + 64
        let madt_addr = ACPI_TABLES_ADDR + 64;
        let madt = host_mem.add(madt_addr as usize);
        std::ptr::write_bytes(madt, 0, 128);
        let madt_len: u32 = 44 + 8 + 12; // header + LAPIC + IOAPIC

        std::ptr::copy_nonoverlapping(b"APIC".as_ptr(), madt, 4);
        w::<u32>(madt, 4, madt_len);
        *madt.add(8) = 5; // Revision
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), madt.add(10), 6);
        std::ptr::copy_nonoverlapping(b"CHMADT  ".as_ptr(), madt.add(16), 8);
        w::<u32>(madt, 24, 1);
        std::ptr::copy_nonoverlapping(b"CLHV".as_ptr(), madt.add(28), 4);
        w::<u32>(madt, 32, 1);
        w::<u32>(madt, 36, 0xFEE0_0000); // Local APIC Address
        w::<u32>(madt, 40, 1); // Flags: PCAT_COMPAT

        // Local APIC (type 0, 8 bytes)
        let lapic = madt.add(44);
        *lapic = 0; *lapic.add(1) = 8; *lapic.add(2) = 0; *lapic.add(3) = 0;
        w::<u32>(lapic, 4, 1); // Enabled

        // I/O APIC (type 1, 12 bytes)
        let ioapic_entry = madt.add(52);
        *ioapic_entry = 1; *ioapic_entry.add(1) = 12; *ioapic_entry.add(2) = 0;
        w::<u32>(ioapic_entry, 4, 0xFEC0_0000);
        w::<u32>(ioapic_entry, 8, 0);

        // Checksum
        let madt_slice = std::slice::from_raw_parts(madt, madt_len as usize);
        *madt.add(9) = cksum(madt_slice);

        // XSDT at ACPI_TABLES_ADDR
        let xsdt = host_mem.add(ACPI_TABLES_ADDR as usize);
        std::ptr::write_bytes(xsdt, 0, 64);
        let xsdt_len: u32 = 36 + 8;
        std::ptr::copy_nonoverlapping(b"XSDT".as_ptr(), xsdt, 4);
        w::<u32>(xsdt, 4, xsdt_len);
        *xsdt.add(8) = 1;
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), xsdt.add(10), 6);
        std::ptr::copy_nonoverlapping(b"CHXSDT  ".as_ptr(), xsdt.add(16), 8);
        w::<u32>(xsdt, 24, 1);
        std::ptr::copy_nonoverlapping(b"CLHV".as_ptr(), xsdt.add(28), 4);
        w::<u32>(xsdt, 32, 1);
        w::<u64>(xsdt, 36, madt_addr);
        let xsdt_slice = std::slice::from_raw_parts(xsdt, xsdt_len as usize);
        *xsdt.add(9) = cksum(xsdt_slice);

        // RSDP at ACPI_RSDP_ADDR
        let rsdp = host_mem.add(ACPI_RSDP_ADDR as usize);
        std::ptr::write_bytes(rsdp, 0, 36);
        std::ptr::copy_nonoverlapping(b"RSD PTR ".as_ptr(), rsdp, 8);
        std::ptr::copy_nonoverlapping(b"CLOUDH".as_ptr(), rsdp.add(9), 6);
        *rsdp.add(15) = 2; // Revision 2
        w::<u32>(rsdp, 16, ACPI_TABLES_ADDR as u32); // RSDT addr
        w::<u32>(rsdp, 20, 36); // Length
        w::<u64>(rsdp, 24, ACPI_TABLES_ADDR); // XSDT addr
        let rsdp20 = std::slice::from_raw_parts(rsdp, 20);
        *rsdp.add(8) = cksum(rsdp20);
        let rsdp36 = std::slice::from_raw_parts(rsdp, 36);
        *rsdp.add(32) = cksum(rsdp36);

        // Set acpi_rsdp_addr in boot_params (offset 0x070)
        let bp = host_mem.add(BOOT_PARAMS_ADDR as usize);
        w::<u64>(bp, 0x070, ACPI_RSDP_ADDR);

        // Also set in PVH hvm_start_info rsdp_paddr (offset 24)
        let pvh = host_mem.add(0x6000);
        w::<u64>(pvh, 24, ACPI_RSDP_ADDR);
    }

    fn cksum(data: &[u8]) -> u8 {
        let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        (!sum).wrapping_add(1)
    }

    println!("  ACPI tables: RSDP at {ACPI_RSDP_ADDR:#X}, MADT at {:#X}", ACPI_TABLES_ADDR + 64);
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

/// Simple interrupt source that injects a fixed vector via WHvRequestInterrupt.
/// Used for devices with known vector assignments (e.g., serial port on IRQ 4 → vector 0x34).
struct WhpFixedVectorInterrupt {
    vm: Arc<dyn hypervisor::Vm>,
    vector: u8,
}

impl InterruptSourceGroup for WhpFixedVectorInterrupt {
    fn trigger(&self, _index: InterruptIndex) -> std::io::Result<()> {
        use hypervisor::whp::WhpVm;
        let whp = self.vm.as_any().downcast_ref::<WhpVm>()
            .ok_or_else(|| std::io::Error::other("not WhpVm"))?;
        whp.request_interrupt(self.vector, 0)
            .map_err(|e| std::io::Error::other(format!("WHvRequestInterrupt: {e}")))?;
        Ok(())
    }

    fn notifier(&self, _index: InterruptIndex) -> Option<platform::EventFd> {
        None
    }

    fn update(
        &self,
        _index: InterruptIndex,
        _config: InterruptSourceConfig,
        _masked: bool,
        _set_gsi: bool,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn set_gsi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── PCI virtio-blk device ────────────────────────────────────────────────────

/// Static PCI config space for the host bridge (device 0).
fn host_bridge_config() -> [u8; 256] {
    let mut cfg = [0u8; 256];
    // Vendor ID: Intel (0x8086)
    cfg[0x00] = 0x86; cfg[0x01] = 0x80;
    // Device ID: i440FX (0x1237)
    cfg[0x02] = 0x37; cfg[0x03] = 0x12;
    // Command: 0
    // Status: 0
    // Class: Host bridge (0x060000)
    cfg[0x09] = 0x00; // prog_if
    cfg[0x0A] = 0x00; // subclass
    cfg[0x0B] = 0x06; // class
    // Header type: 0 (standard)
    cfg[0x0E] = 0x00;
    cfg
}

/// A self-contained legacy virtio-PCI block device.
struct PciBlkDevice {
    config: [u8; 256],

    // BAR tracking
    io_bar_base: u32,
    io_bar_sizing: bool,
    msix_bar_base: u32,
    msix_bar_sizing: bool,

    // Virtio state
    device_features: u32,
    guest_features: u32,
    device_status: u8,
    isr_status: u8,
    queue_select: u16,
    msix_config_vector: u16,

    // Queue 0 (requestq) state
    queue_pfn: u32,
    queue_msix_vector: u16,
    last_avail_idx: u16,

    // Block device
    capacity: u64, // in 512-byte sectors
    disk_file: block::raw_sync::RawFileDiskSync,

    // Guest memory for virtqueue access
    guest_mem: GuestMem,

    // MSI-X table host pointer (direct access to the mapped page)
    msix_table_host: *mut u8,

    // VM for interrupt injection
    vm: Arc<dyn hypervisor::Vm>,

    debug: bool,
}

// SAFETY: msix_table_host points to a page that lives for the VM lifetime.
unsafe impl Send for PciBlkDevice {}

impl PciBlkDevice {
    fn new(
        disk_path: &str,
        guest_mem: GuestMem,
        vm: Arc<dyn hypervisor::Vm>,
        msix_table_host: *mut u8,
    ) -> anyhow::Result<Self> {
        use block::disk_file::DiskSize;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .with_context(|| format!("Failed to open disk: {disk_path}"))?;
        let disk_file = block::raw_sync::RawFileDiskSync::new(file);
        let disk_size = disk_file.logical_size()
            .map_err(|e| anyhow!("Failed to query disk size: {e}"))?;
        let capacity = disk_size / 512;

        let mut config = [0u8; 256];

        // Vendor ID: Red Hat (0x1AF4)
        config[0x00] = 0xF4; config[0x01] = 0x1A;
        // Device ID: transitional virtio-blk (0x1001)
        config[0x02] = 0x01; config[0x03] = 0x10;
        // Command: I/O space enable (bit 0) + Memory space enable (bit 1) + Bus master (bit 2)
        config[0x04] = 0x07; config[0x05] = 0x00;
        // Status: Capabilities list (bit 4)
        config[0x06] = 0x10; config[0x07] = 0x00;
        // Revision ID
        config[0x08] = 0x00;
        // Class: Mass storage (0x01), subclass other (0x80), prog_if 0
        config[0x09] = 0x00; // prog_if
        config[0x0A] = 0x00; // subclass: SCSI (or 0x80 for other)
        config[0x0B] = 0x01; // class: mass storage
        // Header type: standard (0x00)
        config[0x0E] = 0x00;
        // Subsystem Vendor ID: Red Hat (0x1AF4)
        config[0x2C] = 0xF4; config[0x2D] = 0x1A;
        // Subsystem ID: block (0x0002)
        config[0x2E] = 0x02; config[0x2F] = 0x00;
        // Capabilities pointer → MSI-X cap at 0x40
        config[0x34] = 0x40;
        // Interrupt Pin: INTA# (1)
        config[0x3D] = 0x01;

        // BAR 0: I/O space at default address
        let bar0_val = VIRTIO_BLK_IO_BAR_DEFAULT | 0x01; // bit 0 = I/O indicator
        config[0x10..0x14].copy_from_slice(&bar0_val.to_le_bytes());

        // BAR 1: Memory space at MSIX_BAR_GPA
        let bar1_val = MSIX_BAR_GPA as u32; // bits 2:1 = 00 (32-bit), bit 0 = 0 (memory)
        config[0x14..0x18].copy_from_slice(&bar1_val.to_le_bytes());

        // MSI-X Capability at offset 0x40 (12 bytes)
        config[0x40] = 0x11; // Cap ID = MSI-X
        config[0x41] = 0x00; // Next cap = none
        // Message Control: table size = MSIX_TABLE_ENTRIES - 1
        let msg_ctrl = (MSIX_TABLE_ENTRIES - 1) as u16; // bit 15 (enable) = 0 initially
        config[0x42..0x44].copy_from_slice(&msg_ctrl.to_le_bytes());
        // Table Offset/BIR: offset=0, BIR=1 (BAR 1)
        let table_bir: u32 = 0x0000_0001; // offset 0, BIR 1
        config[0x44..0x48].copy_from_slice(&table_bir.to_le_bytes());
        // PBA Offset/BIR: offset=0x800, BIR=1
        let pba_bir: u32 = 0x0000_0801; // offset 0x800, BIR 1
        config[0x48..0x4C].copy_from_slice(&pba_bir.to_le_bytes());

        Ok(PciBlkDevice {
            config,
            io_bar_base: VIRTIO_BLK_IO_BAR_DEFAULT,
            io_bar_sizing: false,
            msix_bar_base: MSIX_BAR_GPA as u32,
            msix_bar_sizing: false,
            device_features: 0, // no special features for now; minimal legacy device
            guest_features: 0,
            device_status: 0,
            isr_status: 0,
            queue_select: 0,
            msix_config_vector: VIRTIO_MSI_NO_VECTOR,
            queue_pfn: 0,
            queue_msix_vector: VIRTIO_MSI_NO_VECTOR,
            last_avail_idx: 0,
            capacity,
            disk_file,
            guest_mem,
            msix_table_host,
            vm,
            debug: std::env::var("CH_DEBUG").is_ok(),
        })
    }

    /// Read a PCI config register (4-byte aligned).
    fn read_config(&self, reg: usize) -> u32 {
        if reg >= 64 { return 0; } // 256 bytes / 4
        // Handle BAR sizing
        match reg {
            4 => { // BAR 0
                if self.io_bar_sizing {
                    // Return size mask: ~(size-1) with I/O bit
                    return !(VIRTIO_BLK_IO_BAR_SIZE - 1) | 0x01;
                }
            }
            5 => { // BAR 1
                if self.msix_bar_sizing {
                    // Return size mask for MMIO BAR
                    return !(MSIX_BAR_SIZE - 1);
                }
            }
            _ => {}
        }
        u32::from_le_bytes(self.config[reg * 4..reg * 4 + 4].try_into().unwrap())
    }

    /// Write to PCI config space. `reg` is the 4-byte register index.
    /// `offset` is byte offset within the register (0-3), `data` is the bytes to write.
    fn write_config(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if reg >= 64 { return; }
        let byte_offset = reg * 4 + offset as usize;

        match reg {
            1 => {
                // Command register (0x04-0x05): allow writes to lower byte
                if offset == 0 && !data.is_empty() {
                    self.config[0x04] = data[0] & 0x07; // I/O, Mem, BusMaster
                }
            }
            4 => {
                // BAR 0 (I/O)
                let mut bar_val = u32::from_le_bytes(self.config[0x10..0x14].try_into().unwrap());
                // Apply write
                for (i, &b) in data.iter().enumerate() {
                    let pos = offset as usize + i;
                    if pos < 4 {
                        bar_val = (bar_val & !(0xFF << (pos * 8))) | ((b as u32) << (pos * 8));
                    }
                }
                if bar_val == 0xFFFFFFFF || (bar_val & !0x03) == 0xFFFFFFFC {
                    self.io_bar_sizing = true;
                    self.config[0x10..0x14].copy_from_slice(&bar_val.to_le_bytes());
                } else {
                    self.io_bar_sizing = false;
                    // Keep I/O indicator bit, align to BAR size
                    let addr = bar_val & !(VIRTIO_BLK_IO_BAR_SIZE - 1) & !0x03;
                    self.io_bar_base = addr;
                    let new_bar = addr | 0x01;
                    self.config[0x10..0x14].copy_from_slice(&new_bar.to_le_bytes());
                }
                return;
            }
            5 => {
                // BAR 1 (MMIO) — read-only; MSI-X backing page is fixed at MSIX_BAR_GPA
                // The kernel sizes the BAR by writing 0xFFFFFFFF. We return the size mask
                // but always restore the original address, preventing relocation.
                let mut bar_val = u32::from_le_bytes(self.config[0x14..0x18].try_into().unwrap());
                for (i, &b) in data.iter().enumerate() {
                    let pos = offset as usize + i;
                    if pos < 4 {
                        bar_val = (bar_val & !(0xFF << (pos * 8))) | ((b as u32) << (pos * 8));
                    }
                }
                if bar_val == 0xFFFFFFFF || (bar_val & !0x0F) == 0xFFFFFFF0 {
                    self.msix_bar_sizing = true;
                    self.config[0x14..0x18].copy_from_slice(&bar_val.to_le_bytes());
                } else {
                    self.msix_bar_sizing = false;
                    // Always restore fixed address — backing page cannot be relocated
                    let fixed_bar = MSIX_BAR_GPA as u32;
                    self.config[0x14..0x18].copy_from_slice(&fixed_bar.to_le_bytes());
                }
                return;
            }
            _ => {
                // MSI-X Message Control at config offset 0x42-0x43 (reg 16, offset 2-3)
                if byte_offset == 0x42 || byte_offset == 0x43 {
                    for (i, &b) in data.iter().enumerate() {
                        let pos = byte_offset + i;
                        if pos == 0x42 || pos == 0x43 {
                            self.config[pos] = b;
                        }
                    }
                    return;
                }
                // Other registers: allow writes to writable areas
                for (i, &b) in data.iter().enumerate() {
                    let pos = byte_offset + i;
                    if pos < 256 {
                        self.config[pos] = b;
                    }
                }
            }
        }
    }

    fn msix_enabled(&self) -> bool {
        let msg_ctrl = u16::from_le_bytes(self.config[0x42..0x44].try_into().unwrap());
        (msg_ctrl & 0x8000) != 0
    }

    fn msix_function_masked(&self) -> bool {
        let msg_ctrl = u16::from_le_bytes(self.config[0x42..0x44].try_into().unwrap());
        (msg_ctrl & 0x4000) != 0
    }

    /// Read from the virtio I/O BAR.
    fn io_read(&mut self, offset: u16, data: &mut [u8]) {
        match offset {
            VIRTIO_PCI_HOST_FEATURES => {
                if data.len() >= 4 {
                    data[..4].copy_from_slice(&self.device_features.to_le_bytes());
                }
            }
            VIRTIO_PCI_GUEST_FEATURES => {
                if data.len() >= 4 {
                    data[..4].copy_from_slice(&self.guest_features.to_le_bytes());
                }
            }
            VIRTIO_PCI_QUEUE_PFN => {
                if data.len() >= 4 {
                    data[..4].copy_from_slice(&self.queue_pfn.to_le_bytes());
                }
            }
            VIRTIO_PCI_QUEUE_NUM => {
                if data.len() >= 2 {
                    let size = if self.queue_select == 0 { BLK_QUEUE_SIZE } else { 0 };
                    data[..2].copy_from_slice(&size.to_le_bytes());
                }
            }
            VIRTIO_PCI_QUEUE_SEL => {
                if data.len() >= 2 {
                    data[..2].copy_from_slice(&self.queue_select.to_le_bytes());
                }
            }
            VIRTIO_PCI_STATUS => {
                data[0] = self.device_status;
            }
            VIRTIO_PCI_ISR => {
                data[0] = self.isr_status;
                self.isr_status = 0; // read-clear
            }
            VIRTIO_PCI_MSIX_CONFIG_VECTOR => {
                if data.len() >= 2 {
                    data[..2].copy_from_slice(&self.msix_config_vector.to_le_bytes());
                }
            }
            VIRTIO_PCI_MSIX_QUEUE_VECTOR => {
                if data.len() >= 2 {
                    let vec = if self.queue_select == 0 { self.queue_msix_vector } else { VIRTIO_MSI_NO_VECTOR };
                    data[..2].copy_from_slice(&vec.to_le_bytes());
                }
            }
            _ => {
                // Device-specific config at VIRTIO_PCI_CONFIG_OFF
                if offset >= VIRTIO_PCI_CONFIG_OFF {
                    let cfg_off = (offset - VIRTIO_PCI_CONFIG_OFF) as usize;
                    let cap_bytes = self.capacity.to_le_bytes();
                    for (i, d) in data.iter_mut().enumerate() {
                        let pos = cfg_off + i;
                        if pos < cap_bytes.len() {
                            *d = cap_bytes[pos];
                        } else {
                            *d = 0;
                        }
                    }
                } else {
                    data.fill(0);
                }
            }
        }
    }

    /// Write to the virtio I/O BAR.
    fn io_write(&mut self, offset: u16, data: &[u8]) {
        match offset {
            VIRTIO_PCI_GUEST_FEATURES => {
                if data.len() >= 4 {
                    self.guest_features = u32::from_le_bytes(data[..4].try_into().unwrap());
                }
            }
            VIRTIO_PCI_QUEUE_PFN => {
                if data.len() >= 4 {
                    let pfn = u32::from_le_bytes(data[..4].try_into().unwrap());
                    if self.queue_select == 0 {
                        if self.debug {
                            eprintln!("[virtio-blk] queue 0 PFN={pfn:#x} (addr={:#x})", (pfn as u64) * 4096);
                        }
                        self.queue_pfn = pfn;
                        if pfn == 0 {
                            self.last_avail_idx = 0;
                        }
                    }
                }
            }
            VIRTIO_PCI_QUEUE_SEL => {
                if data.len() >= 2 {
                    self.queue_select = u16::from_le_bytes(data[..2].try_into().unwrap());
                }
            }
            VIRTIO_PCI_QUEUE_NOTIFY => {
                if data.len() >= 2 {
                    let queue_idx = u16::from_le_bytes(data[..2].try_into().unwrap());
                    if queue_idx == 0 && self.device_status & VIRTIO_STATUS_DRIVER_OK != 0 && self.queue_pfn != 0 {
                        self.process_queue();
                    }
                }
            }
            VIRTIO_PCI_STATUS => {
                if data.is_empty() { return; }
                let new_status = data[0];
                if new_status == 0 {
                    // Device reset
                    if self.debug { eprintln!("[virtio-blk] device reset"); }
                    self.device_status = 0;
                    self.guest_features = 0;
                    self.queue_pfn = 0;
                    self.queue_select = 0;
                    self.last_avail_idx = 0;
                    self.isr_status = 0;
                    self.msix_config_vector = VIRTIO_MSI_NO_VECTOR;
                    self.queue_msix_vector = VIRTIO_MSI_NO_VECTOR;
                    return;
                }
                if self.debug && new_status != self.device_status {
                    eprintln!("[virtio-blk] status {:#04x} → {:#04x}", self.device_status, new_status);
                }
                self.device_status = new_status;
            }
            VIRTIO_PCI_MSIX_CONFIG_VECTOR => {
                if data.len() >= 2 {
                    self.msix_config_vector = u16::from_le_bytes(data[..2].try_into().unwrap());
                }
            }
            VIRTIO_PCI_MSIX_QUEUE_VECTOR => {
                if data.len() >= 2 && self.queue_select == 0 {
                    self.queue_msix_vector = u16::from_le_bytes(data[..2].try_into().unwrap());
                    if self.debug {
                        eprintln!("[virtio-blk] queue 0 MSI-X vector={}", self.queue_msix_vector);
                    }
                }
            }
            _ => {} // Ignore writes to read-only or unknown registers
        }
    }

    /// Process pending requests in the virtqueue.
    fn process_queue(&mut self) {
        if self.queue_pfn == 0 { return; }

        let queue_addr = (self.queue_pfn as u64) * 4096;
        let desc_table = queue_addr;
        let avail_ring = queue_addr + (BLK_QUEUE_SIZE as u64) * 16;
        let used_ring = align_up(avail_ring + 6 + 2 * BLK_QUEUE_SIZE as u64, 4096);

        // Read avail index
        let avail_idx: u16 = self.guest_mem
            .read_obj(GuestAddress(avail_ring + 2))
            .unwrap_or(0);

        let mut processed = 0u32;

        while self.last_avail_idx != avail_idx {
            let avail_slot = (self.last_avail_idx % BLK_QUEUE_SIZE) as u64;
            let desc_idx: u16 = self.guest_mem
                .read_obj(GuestAddress(avail_ring + 4 + avail_slot * 2))
                .unwrap_or(0);

            let bytes_written = self.process_descriptor_chain(desc_table, desc_idx);

            // Update used ring
            let used_idx: u16 = self.guest_mem
                .read_obj(GuestAddress(used_ring + 2))
                .unwrap_or(0);
            let used_slot = (used_idx % BLK_QUEUE_SIZE) as u64;
            let used_elem_addr = used_ring + 4 + used_slot * 8;
            // Write used element: {id: u32, len: u32}
            let _ = self.guest_mem.write_obj(desc_idx as u32, GuestAddress(used_elem_addr));
            let _ = self.guest_mem.write_obj(bytes_written, GuestAddress(used_elem_addr + 4));
            // Increment used index
            let _ = self.guest_mem.write_obj(used_idx.wrapping_add(1), GuestAddress(used_ring + 2));

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            processed += 1;
        }

        if processed > 0 {
            if self.debug {
                eprintln!("[virtio-blk] processed {processed} requests");
            }
            self.isr_status |= 0x01; // Used buffer notification
            self.inject_interrupt();
        }
    }

    /// Process a single descriptor chain. Returns total bytes written to device-writable descriptors.
    fn process_descriptor_chain(&mut self, desc_table: u64, first_idx: u16) -> u32 {
        // Walk the chain to collect header, data descriptors, and status descriptor
        let mut descs: Vec<(u64, u32, u16)> = Vec::new(); // (addr, len, flags)
        let mut idx = first_idx;
        let mut chain_len = 0u32;

        loop {
            if chain_len >= BLK_QUEUE_SIZE as u32 { break; } // prevent infinite loops
            let desc_addr = desc_table + (idx as u64) * 16;

            let addr: u64 = self.guest_mem.read_obj(GuestAddress(desc_addr)).unwrap_or(0);
            let len: u32 = self.guest_mem.read_obj(GuestAddress(desc_addr + 8)).unwrap_or(0);
            let flags: u16 = self.guest_mem.read_obj(GuestAddress(desc_addr + 12)).unwrap_or(0);
            let next: u16 = self.guest_mem.read_obj(GuestAddress(desc_addr + 14)).unwrap_or(0);

            descs.push((addr, len, flags));
            chain_len += 1;

            if flags & VRING_DESC_F_NEXT == 0 { break; }
            idx = next;
        }

        if descs.len() < 2 {
            // Need at least header + status
            return 0;
        }

        // First descriptor: virtio_blk_req header (16 bytes, read-only)
        let (hdr_addr, hdr_len, hdr_flags) = descs[0];
        if hdr_len < 16 || (hdr_flags & VRING_DESC_F_WRITE) != 0 {
            // Invalid header
            self.write_status_byte(&descs, VIRTIO_BLK_S_IOERR);
            return 1;
        }

        let req_type: u32 = self.guest_mem.read_obj(GuestAddress(hdr_addr)).unwrap_or(u32::MAX);
        let sector: u64 = self.guest_mem.read_obj(GuestAddress(hdr_addr + 8)).unwrap_or(0);

        // Last descriptor: status byte (1 byte, write-only)
        let (status_addr, status_len, status_flags) = descs[descs.len() - 1];
        if status_len < 1 || (status_flags & VRING_DESC_F_WRITE) == 0 {
            return 0; // Invalid status descriptor
        }

        // Middle descriptors: data buffers
        let data_descs = &descs[1..descs.len() - 1];
        let mut total_written = 0u32;

        let status = match req_type {
            VIRTIO_BLK_T_IN => {
                // Read from disk to guest
                let mut disk_offset = sector * 512;
                if sector >= self.capacity {
                    VIRTIO_BLK_S_IOERR
                } else {
                    let mut ok = true;
                    for &(addr, len, flags) in data_descs {
                        if flags & VRING_DESC_F_WRITE == 0 {
                            ok = false; break; // data must be writable for reads
                        }
                        if disk_offset + len as u64 > self.capacity * 512 {
                            ok = false; break;
                        }
                        let mut buf = vec![0u8; len as usize];
                        if self.disk_file.file_mut().seek(SeekFrom::Start(disk_offset)).is_err() {
                            ok = false; break;
                        }
                        if self.disk_file.file_mut().read_exact(&mut buf).is_err() {
                            ok = false; break;
                        }
                        if self.guest_mem.write(&buf, GuestAddress(addr)).is_err() {
                            ok = false; break;
                        }
                        disk_offset += len as u64;
                        total_written += len;
                    }
                    if ok { VIRTIO_BLK_S_OK } else { VIRTIO_BLK_S_IOERR }
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Write from guest to disk
                let mut disk_offset = sector * 512;
                if sector >= self.capacity {
                    VIRTIO_BLK_S_IOERR
                } else {
                    let mut ok = true;
                    for &(addr, len, _flags) in data_descs {
                        if disk_offset + len as u64 > self.capacity * 512 {
                            ok = false; break;
                        }
                        let mut buf = vec![0u8; len as usize];
                        if self.guest_mem.read(&mut buf, GuestAddress(addr)).is_err() {
                            ok = false; break;
                        }
                        if self.disk_file.file_mut().seek(SeekFrom::Start(disk_offset)).is_err() {
                            ok = false; break;
                        }
                        if self.disk_file.file_mut().write_all(&buf).is_err() {
                            ok = false; break;
                        }
                        disk_offset += len as u64;
                    }
                    if ok { VIRTIO_BLK_S_OK } else { VIRTIO_BLK_S_IOERR }
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                if self.disk_file.file_mut().sync_all().is_ok() {
                    VIRTIO_BLK_S_OK
                } else {
                    VIRTIO_BLK_S_IOERR
                }
            }
            VIRTIO_BLK_T_GET_ID => {
                // Write device ID string (up to 20 bytes) to data buffer
                let id = b"virtio-blk-whp\0\0\0\0\0\0";
                for &(addr, len, flags) in data_descs {
                    if flags & VRING_DESC_F_WRITE != 0 {
                        let write_len = (len as usize).min(id.len());
                        let _ = self.guest_mem.write(&id[..write_len], GuestAddress(addr));
                        total_written += write_len as u32;
                    }
                }
                VIRTIO_BLK_S_OK
            }
            _ => VIRTIO_BLK_S_UNSUPP,
        };

        // Write status byte
        let _ = self.guest_mem.write_obj(status, GuestAddress(status_addr));
        total_written += 1; // status byte

        total_written
    }

    fn write_status_byte(&self, descs: &[(u64, u32, u16)], status: u8) {
        if let Some(&(addr, _, _)) = descs.last() {
            let _ = self.guest_mem.write_obj(status, GuestAddress(addr));
        }
    }

    /// Inject an interrupt via MSI-X (if enabled) or set ISR for polling.
    fn inject_interrupt(&self) {
        if !self.msix_enabled() || self.msix_function_masked() {
            return;
        }

        let vector_idx = self.queue_msix_vector;
        if vector_idx == VIRTIO_MSI_NO_VECTOR || vector_idx >= MSIX_TABLE_ENTRIES {
            return;
        }

        // Read MSI-X table entry from guest memory (mapped as host page)
        // Entry format: addr_lo(4) + addr_hi(4) + data(4) + vector_ctrl(4)
        let entry_offset = (vector_idx as usize) * 16;
        let (addr_lo, data, vector_ctrl) = unsafe {
            let entry = self.msix_table_host.add(entry_offset);
            let a = std::ptr::read_unaligned(entry as *const u32);
            let d = std::ptr::read_unaligned(entry.add(8) as *const u32);
            let c = std::ptr::read_unaligned(entry.add(12) as *const u32);
            (a, d, c)
        };

        // Check if this entry is masked
        if vector_ctrl & 1 != 0 {
            return;
        }

        let vector = (data & 0xFF) as u8;
        let destination = (addr_lo >> 12) & 0xFF;

        if vector == 0 { return; }

        use hypervisor::whp::WhpVm;
        if let Some(whp) = self.vm.as_any().downcast_ref::<WhpVm>() {
            let _ = whp.request_interrupt(vector, destination);
        }
    }
}

fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

// ── VmOps: I/O handler ───────────────────────────────────────────────────────

struct SerialVmOps {
    seen_ports: std::sync::Mutex<std::collections::BTreeSet<u64>>,
    debug: bool,
    /// Tracks the start time for PIT counter decrement emulation.
    pit_start: std::time::Instant,
    /// PIT counter 2 reload value.
    pit_reload: AtomicU16,
    /// Port 0x61 (NMI Status and Control) register value.
    port61: AtomicU8,
    /// PIC master IMR (port 0x21) — track writes for probe detection.
    pic_master_imr: AtomicU8,
    /// PIC slave IMR (port 0xA1).
    pic_slave_imr: AtomicU8,
    /// Real IOAPIC device for interrupt routing.
    ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>>,
    /// VM reference for interrupt injection from PIO handler.
    vm: Option<Arc<dyn hypervisor::Vm>>,
    /// 16550 UART serial device (proper DLAB, FIFO, interrupt handling).
    serial: Arc<Mutex<devices::legacy::serial::Serial>>,
    /// PCI config address register (port 0xCF8).
    pci_config_address: AtomicU32,
    /// PCI virtio-blk device.
    pci_blk: Option<Arc<Mutex<PciBlkDevice>>>,
    /// Host bridge config space (static).
    host_bridge_config: [u8; 256],
}

impl SerialVmOps {
    fn new(
        ioapic: Option<Arc<Mutex<devices::ioapic::Ioapic>>>,
        vm: Option<Arc<dyn hypervisor::Vm>>,
        pci_blk: Option<Arc<Mutex<PciBlkDevice>>>,
        serial: Arc<Mutex<devices::legacy::serial::Serial>>,
    ) -> Self {
        SerialVmOps {
            seen_ports: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            debug: std::env::var("CH_DEBUG").is_ok(),
            pit_start: std::time::Instant::now(),
            pit_reload: AtomicU16::new(0xFFFF),
            port61: AtomicU8::new(0),
            pic_master_imr: AtomicU8::new(0xFF),
            pic_slave_imr: AtomicU8::new(0xFF),
            ioapic,
            vm,
            serial,
            pci_config_address: AtomicU32::new(0),
            pci_blk,
            host_bridge_config: host_bridge_config(),
        }
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
            // Serial UART registers (0x3F8-0x3FF) — delegate to 16550 device
            0x3F8..=0x3FF => {
                use vm_device::BusDevice;
                self.serial.lock().unwrap().read(0x3F8, port - 0x3F8, data);
            }
            // PIT counter 2 data port — return simulated decrementing counter
            0x42 => {
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
                data[0] = self.pic_master_imr.load(Ordering::Relaxed);
            }
            0xA0 => { data[0] = 0x00; } // Slave PIC CMD
            0xA1 => {
                data[0] = self.pic_slave_imr.load(Ordering::Relaxed);
            }
            // PCI Config Address port
            0xCF8..=0xCFB => {
                let addr = self.pci_config_address.load(Ordering::Relaxed);
                let off = (port - 0xCF8) as usize;
                for (i, d) in data.iter_mut().enumerate() {
                    let pos = off + i;
                    if pos < 4 {
                        *d = (addr >> (pos * 8)) as u8;
                    }
                }
            }
            // PCI Config Data port
            0xCFC..=0xCFF => {
                let config_addr = self.pci_config_address.load(Ordering::Relaxed);
                let enabled = (config_addr & 0x8000_0000) != 0;
                if !enabled {
                    data.fill(0xFF);
                } else {
                    let bus = (config_addr >> 16) & 0xFF;
                    let device = (config_addr >> 11) & 0x1F;
                    let function = (config_addr >> 8) & 0x07;
                    let reg = ((config_addr >> 2) & 0x3F) as usize;

                    if bus != 0 || function != 0 {
                        data.fill(0xFF);
                    } else {
                        let reg_val = match device {
                            PCI_HOST_BRIDGE_SLOT => {
                                if reg < 64 {
                                    u32::from_le_bytes(self.host_bridge_config[reg*4..reg*4+4].try_into().unwrap())
                                } else {
                                    0xFFFFFFFF
                                }
                            }
                            PCI_BLK_SLOT => {
                                if let Some(ref blk) = self.pci_blk {
                                    blk.lock().unwrap().read_config(reg)
                                } else {
                                    0xFFFFFFFF
                                }
                            }
                            _ => 0xFFFFFFFF,
                        };
                        let byte_off = (port - 0xCFC) as usize;
                        for (i, d) in data.iter_mut().enumerate() {
                            let pos = byte_off + i;
                            if pos < 4 {
                                *d = (reg_val >> (pos * 8)) as u8;
                            }
                        }
                    }
                }
            }
            _ => {
                // Check if port is in the virtio-blk I/O BAR range
                if let Some(ref blk) = self.pci_blk {
                    let mut blk = blk.lock().unwrap();
                    let bar_base = blk.io_bar_base as u64;
                    let bar_end = bar_base + VIRTIO_BLK_IO_BAR_SIZE as u64;
                    if port >= bar_base && port < bar_end {
                        blk.io_read((port - bar_base) as u16, data);
                        return Ok(());
                    }
                }
                data.fill(0xFF);
            }
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
        match port {
            // Serial UART registers (0x3F8-0x3FF) — delegate to 16550 device
            0x3F8..=0x3FF => {
                use vm_device::BusDevice;
                self.serial.lock().unwrap().write(0x3F8, port - 0x3F8, data);
            }
            p if p == DEBUG_EXIT_PORT => {
                // Signal shutdown via error; the run loop will break and
                // terminal state will be properly restored.
                return Err(HypervisorVmError::IoBusWrite(anyhow::anyhow!(
                    "Guest requested shutdown via debug exit port"
                )));
            }
            0x42 if !data.is_empty() => {
                self.pit_reload.store(data[0] as u16, Ordering::Relaxed);
            }
            0x61 if !data.is_empty() => {
                self.port61.store(data[0], Ordering::Relaxed);
            }
            0x21 if !data.is_empty() => {
                self.pic_master_imr.store(data[0], Ordering::Relaxed);
            }
            0xA1 if !data.is_empty() => {
                self.pic_slave_imr.store(data[0], Ordering::Relaxed);
            }
            // PCI Config Address port
            0xCF8..=0xCFB => {
                let mut addr = self.pci_config_address.load(Ordering::Relaxed);
                let off = (port - 0xCF8) as usize;
                for (i, &b) in data.iter().enumerate() {
                    let pos = off + i;
                    if pos < 4 {
                        addr = (addr & !(0xFF << (pos * 8))) | ((b as u32) << (pos * 8));
                    }
                }
                self.pci_config_address.store(addr, Ordering::Relaxed);
            }
            // PCI Config Data port
            0xCFC..=0xCFF => {
                let config_addr = self.pci_config_address.load(Ordering::Relaxed);
                let enabled = (config_addr & 0x8000_0000) != 0;
                if !enabled { return Ok(()); }

                let bus = (config_addr >> 16) & 0xFF;
                let device = (config_addr >> 11) & 0x1F;
                let function = (config_addr >> 8) & 0x07;
                let reg = ((config_addr >> 2) & 0x3F) as usize;
                let byte_off = port - 0xCFC;

                if bus == 0 && function == 0 && device == PCI_BLK_SLOT {
                    if let Some(ref blk) = self.pci_blk {
                        blk.lock().unwrap().write_config(reg, byte_off, data);
                    }
                }
                // Host bridge config writes are ignored (read-only)
            }
            _ => {
                // Check if port is in the virtio-blk I/O BAR range
                if let Some(ref blk) = self.pci_blk {
                    let mut blk = blk.lock().unwrap();
                    let bar_base = blk.io_bar_base as u64;
                    let bar_end = bar_base + VIRTIO_BLK_IO_BAR_SIZE as u64;
                    if port >= bar_base && port < bar_end {
                        blk.io_write((port - bar_base) as u16, data);
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}
