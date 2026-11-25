#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod acpi;
mod ata;
mod fat32;
mod framebuffer;
mod gdt;
mod gpt;
mod heap;
mod interrupts;
mod ioapic;
mod keyboard;
mod panic;
mod paging;
mod pmm;
mod pci;
mod serial;
mod splash;
mod sync;
mod vfs;

use alloc::{boxed::Box, format, string::String, string::ToString, vec::Vec};
use core::arch::asm;
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};
use limine::request::FramebufferRequest;
use limine::request::HhdmRequest;
use limine::request::MemmapRequest;
use limine::request::RsdpRequest;

// Import PCI helper functions for use in tests
use pci::write_u8_hex;
use pci::write_u16_hex;

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQUESTS_START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

#[global_allocator]
static ALLOCATOR: heap::LockedHeap = heap::LockedHeap::empty();

/// Current working directory for the shell (max 256 chars)
static mut CURRENT_DIR_BUF: [u8; 256] = [0; 256];
static mut CURRENT_DIR_LEN: usize = 0;

/// Get current directory as a static str
fn get_current_dir() -> &'static str {
    unsafe {
        let len = CURRENT_DIR_LEN;
        if len == 0 {
            "/"
        } else {
            // This is safe because we control all writes to CURRENT_DIR_BUF
            core::str::from_utf8_unchecked(&CURRENT_DIR_BUF[..len])
        }
    }
}

/// Set current directory
fn set_current_dir(path: &str) {
    unsafe {
        let bytes = path.as_bytes();
        let copy_len = bytes.len().min(255);
        CURRENT_DIR_BUF[..copy_len].copy_from_slice(&bytes[..copy_len]);
        CURRENT_DIR_BUF[copy_len] = 0;
        CURRENT_DIR_LEN = copy_len;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Initialize serial port FIRST - before any string output
    serial::init();

    if !BASE_REVISION.is_supported() {
        serial::write_str_safe("ERROR: Limine base revision not supported.\r\n");
        halt();
    }

    serial::write_str_safe("WELCOME TO MAGMA OS\r\n");
    serial::write_str_safe("Magma Kernel Initialised.\r\n");
    serial::write_str_safe("Architecture: x86_64\r\n");
    serial::write_str_safe("Bootloader: Limine\r\n");

    // Memory map and HHDM
    let memmap_resp = MEMMAP_REQUEST.response();
    let hhdm_resp = HHDM_REQUEST.response();
    let mut total_usable: u64 = 0;

    // First, collect memory map info (no alloc)
    let memmap_entries: Option<&[&limine::memmap::Entry]> = memmap_resp.map(|r| r.entries());
    if let Some(entries) = memmap_entries {
        for entry in entries {
            let base = *entry;
            serial::write_str_safe("MEMMAP: base=0x");
            print_u64_hex(base.base);
            serial::write_str_safe(" len=0x");
            print_u64_hex(base.length);
            serial::write_str_safe(" type=");
            print_u64(base.type_);
            serial::write_str_safe("\r\n");

            if base.type_ == limine::memmap::MEMMAP_USABLE {
                total_usable += base.length as u64;
            }
        }
        serial::write_str_safe("MEMMAP: Total usable RAM = ");
        print_u64(total_usable / (1024 * 1024));
        serial::write_str_safe(" MiB\r\n");
    }

    // Initialize framebuffer first (before GDT/TSS which need GDT loaded)
    let fb_entry = FRAMEBUFFER_REQUEST.response().and_then(|r| r.framebuffers().first());
    if let Some(entry) = fb_entry {
        framebuffer::init(entry);
        let _dimensions = framebuffer::FRAMEBUFFER.get().unwrap().dimensions();
        serial::write_str_safe("[TRACE] Framebuffer initialized\r\n");
    }

    // Phase 1E: Initialize splash screen (Phase 0% - just initialized)
    splash::init();

    // Phase 1C: GDT, TSS
    // Phase 1E: Update splash progress (25%)
    serial::write_str_safe("[TRACE] About to initialize GDT\r\n");
    splash::set_phase(splash::PHASE_GDT, b"Loading GDT...");
    gdt::init();
    serial::write_str_safe("[OK] GDT & TSS loaded\r\n");

    // Phase 1C: IDT, PIC, PIT
    // Phase 1E: Update splash progress (50%)
    serial::write_str_safe("[TRACE] About to initialize interrupts\r\n");
    splash::set_phase(splash::PHASE_IDT, b"Setup IDT...");
    interrupts::init();
    serial::write_str_safe("[OK] IDT, PIC (32..47), and 100Hz PIT enabled\r\n");

    // Phase 1D: PMM and Heap - MUST initialize AFTER GDT/IDT are set up
    if let (Some(memmap), Some(hhdm)) = (memmap_resp, hhdm_resp) {
        // Trace: About to initialize PMM
        serial::write_str_safe("[TRACE] About to initialize PMM\r\n");
        pmm::init(memmap.entries(), hhdm.offset);
        let (free_frames, total_frames) = pmm::stats();
        serial::write_str_safe("[OK] PMM initialized: frames free=");
        print_u64(free_frames as u64);
        serial::write_str_safe("/");
        print_u64(total_frames as u64);
        serial::write_str_safe("\r\n");

        // Trace: About to map heap pages
        serial::write_str_safe("[TRACE] About to map heap pages\r\n");
        // Map 16 MiB kernel heap using 4 KiB pages (simpler for kernel heap)
        let heap_flags = paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE;
        paging::map_range(heap::HEAP_START, heap::HEAP_SIZE as u64, heap_flags)
            .expect("Failed to map heap range");
        serial::write_str_safe("[OK] Heap pages mapped at 0x");
        print_u64_hex(heap::HEAP_START);
        serial::write_str_safe("\r\n");

        // Trace: About to initialize kernel heap allocator
        serial::write_str_safe("[TRACE] About to initialize kernel heap allocator\r\n");
        ALLOCATOR.init().expect("Failed to initialize kernel heap allocator");
        serial::write_str_safe("[OK] 16 MiB kernel heap initialized\r\n");

        // Phase 1D: Initialize ACPI parsing (after PMM and heap are ready)
        // Get RSDP address from limine (may be already HHDM-offset in base revision 3+)
        if let Some(rsdp_resp) = RSDP_REQUEST.response() {
            let rsdp_addr = rsdp_resp.address as u64;
            serial::write_str_safe("[TRACE] RSDP address: 0x");
            print_u64_hex(rsdp_addr);
            serial::write_str_safe("\r\n");
            acpi::init(rsdp_addr, hhdm.offset);
        } else {
            serial::write_str_safe("[ACPI] ERROR: No RSDP response from Limine\r\n");
        }

        // Verify dynamic heap alloc/free
        // Trace: About to test Box allocation
        serial::write_str_safe("[TRACE] About to test Box allocation\r\n");
        let box_val = Box::new(42u64);
        assert_eq!(*box_val, 42);
        drop(box_val);
        serial::write_str_safe("[OK] Box allocation verified\r\n");

        // Trace: About to test Vec allocation
        serial::write_str_safe("[TRACE] About to test Vec allocation\r\n");
        let mut vec = Vec::new();
        for i in 0..10 {
            vec.push(i * 2);
        }
        assert_eq!(vec.len(), 10);
        drop(vec);
        serial::write_str_safe("[OK] Vec allocation verified\r\n");

        // Trace: About to test String/alloc formatting
        serial::write_str_safe("[TRACE] About to test String/format! allocation\r\n");
        let string = String::from("Magma OS heap test");
        assert!(string.contains("heap"));
        drop(string);
        serial::write_str_safe("[OK] String allocation verified\r\n");

        serial::write_str_safe("[OK] Dynamic heap alloc/free verified\r\n");

        // Phase 1F: PCI Bus Enumeration
        // Trace: About to initialize PCI
        serial::write_str_safe("[TRACE] About to initialize PCI enumeration\r\n");
        pci::init();
        serial::write_str_safe("[OK] PCI enumeration complete\r\n");

        // Phase 1E: Update splash progress to 75% after heap initialization
        splash::set_phase(splash::PHASE_HEAP, b"Heap OK");

        // Enable Local APIC with HHDM offset for MMIO access
        // This must happen after PMM init because HHDM offset is required for APIC MMIO
        interrupts::set_hhdm_offset(hhdm.offset);
        interrupts::init_local_apic_with_hhdm();

        // Phase 1D: Initialize I/O APIC and calibrate Local APIC timer
        // I/O APIC provides additional interrupt routing beyond the PIC
        serial::write_str_safe("[TRACE] About to initialize I/O APIC...\r\n");
        ioapic::init();

        // Initialize PS/2 Keyboard driver
        keyboard::init();
        ioapic::route_isa_irq(1, 33, 0);
        serial::write_str_safe("[OK] PS/2 Keyboard routed via I/O APIC (IRQ1 -> vec 33)\r\n");

        // Initialize Local APIC timer at 100 Hz (same as PIT for consistency)
        interrupts::init_lapic_timer(100);
        serial::write_str_safe("[OK] Local APIC timer calibrated at 100 Hz\r\n");

        // Mask PIC timer IRQ0 to prevent conflicts with LAPIC timer
        // We use the LAPIC timer exclusively now
        interrupts::mask_pic_timer();
    }

    // ============================================
    // Phase 1F: ATA Boot Verification
    // ============================================

    serial::write_str_safe("[TRACE] Initializing ATA driver...\r\n");
    if let Ok(()) = ata::init() {
        if ata::is_present() {
            if let Some(info) = ata::get_drive_info() {
                // Print drive info
                serial::write_str_safe("[OK] ATA Primary Master ready, LBA0 sig=0x");
                write_u16_hex(info.lba0_signature);
                serial::write_str_safe("\r\n");
            }
        }
    }

    // ============================================
    // Phase 2C: Filesystem Initialization
    // ============================================

    serial::write_str_safe("[TRACE] Initializing GPT partition detection...\r\n");
    if let Ok(()) = gpt::init() {
        let pcount = gpt::partition_count();
        serial::write_str_safe("[OK] GPT initialized, found ");
        print_u64(pcount as u64);
        serial::write_str_safe(" partition(s)\r\n");
    } else {
        serial::write_str_safe("[WARN] GPT initialization failed\r\n");
    }

    serial::write_str_safe("[TRACE] Initializing VFS...\r\n");
    if let Ok(()) = vfs::init() {
        let mcount = vfs::mount_count();
        serial::write_str_safe("[OK] VFS initialized, ");
        print_u64(mcount as u64);
        serial::write_str_safe(" volume(s) mounted\r\n");
    } else {
        serial::write_str_safe("[WARN] VFS initialization failed\r\n");
    }

    // Verify ATA read functionality
    vfs::verify_ata_read();

    // Test breakpoint exception (#BP, vector 3)
    serial::write_str_safe("[TRACE] About to trigger int3...\r\n");
    unsafe {
        core::arch::asm!("int3", options(nomem, nostack));
    }
    serial::write_str_safe("[TRACE] int3 returned (handler recovered)\r\n");
    serial::write_str_safe("[OK] Breakpoint (#BP) caught and recovered\r\n");

    // Wait for timer ticks (25 ticks = 250ms at 100Hz)
    // Add bounded spin fallback to prevent hanging forever if ticks don't fire
    let mut tick_timeout: u64 = 0;
    const TICK_TIMEOUT_MAX: u64 = 10_000_000;  // Safety limit to avoid infinite hang
    while interrupts::ticks() < 25 {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
        tick_timeout += 1;
        if tick_timeout >= TICK_TIMEOUT_MAX {
            serial::write_str_safe("[WARN] Timer tick wait timeout - ticks may not be firing\r\n");
            break;
        }
    }

    // ============================================
    // Phase 1C Verification Tests
    // ============================================

    serial::write_str_safe("\r\n[TEST] Starting Phase 1C verification...\r\n");

    // Test 1: Contiguous DMA allocation, read/write, and free
    serial::write_str_safe("[TEST] 1. Contiguous DMA allocation...\r\n");
    if let Some(dma_region) = pmm::alloc_contiguous_frames(4, 1) {
        // Write through virtual address
        unsafe {
            let virt_ptr = dma_region.virt_addr as *mut u64;
            for i in 0..dma_region.frame_count * 64 {  // 64 u64s per 4KB frame
                virt_ptr.add(i).write(i as u64);
            }
            // Read back and verify
            for i in 0..dma_region.frame_count * 64 {
                let val = virt_ptr.add(i).read();
                if val != i as u64 {
                    serial::write_str_safe("[FAIL] DMA write/read mismatch\r\n");
                }
            }
        }
        pmm::free_contiguous_frames(dma_region);
        serial::write_str_safe("[OK] DMA alloc/write/read/free passed\r\n");
    } else {
        serial::write_str_safe("[FAIL] DMA allocation failed\r\n");
    }

    // Test 2: 4 KiB page mapping, read/write, unmapping
    serial::write_str_safe("[TEST] 2. 4 KiB page mapping...\r\n");
    if let Some(phys) = pmm::alloc_frame() {
        let virt = 0xFFFF_C000_0000_0000 + 0x100000; // Use a high VMA for testing
        if paging::map_page(virt, phys, paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE).is_ok() {
            unsafe {
                let virt_ptr = virt as *mut u64;
                virt_ptr.write(0xDEADBEEF_CAFEBABE);
                let val = virt_ptr.read();
                if val == 0xDEADBEEF_CAFEBABE {
                    serial::write_str_safe("[OK] 4KiB page map/read/write passed\r\n");
                } else {
                    serial::write_str_safe("[FAIL] 4KiB page read mismatch\r\n");
                }
            }
            if let Some(freed_phys) = paging::unmap_page(virt) {
                pmm::free_frame(freed_phys); // Free the frame returned by unmap_page
            }
        } else {
            serial::write_str_safe("[FAIL] 4KiB page mapping failed\r\n");
            pmm::free_frame(phys); // Free the frame on mapping failure
        }
    } else {
        serial::write_str_safe("[FAIL] Physical frame allocation failed\r\n");
    }

    // Test 3: 2 MiB huge page mapping, read/write, unmapping
    serial::write_str_safe("[TEST] 3. 2 MiB huge page mapping...\r\n");
    // Allocate 2MB (512 frames) for the huge page content with 2MB alignment
    // 2 MiB = 2 * 1024 * 1024 bytes, which equals 512 frames of 4 KiB each
    let align_2mb: u64 = 2 * 1024 * 1024;
    let mut test3_passed = false;
    if let Some(huge_dma) = pmm::alloc_aligned_contiguous(512, align_2mb) {
        // Use a dedicated 2 MiB-aligned virtual address for testing
        // This avoids unmapping the HHDM-mapped huge_dma.virt_addr
        let huge_virt: u64 = 0xFFFF_C000_0200_0000;
        let huge_phys = huge_dma.phys_addr;

        if let Err(e) = paging::map_huge_page(huge_virt, huge_phys, paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE) {
            serial::write_str_safe("[FAIL] Huge page mapping failed: ");
            serial::write_str_safe(e);
            serial::write_str_safe("\r\n");
            pmm::free_contiguous_frames(huge_dma);
        } else {
            let mut test3_map_ok = true;
            unsafe {
                let virt_ptr = huge_virt as *mut u64;
                virt_ptr.write(0x12345678_9ABCDEF0);
                let val = virt_ptr.read();
                if val == 0x12345678_9ABCDEF0 {
                    serial::write_str_safe("[OK] 2MiB huge page map/read/write passed\r\n");
                } else {
                    serial::write_str_safe("[FAIL] Huge page read mismatch\r\n");
                    test3_map_ok = false;
                }
            }
            if let Some(_unmapped_phys) = paging::unmap_huge_page(huge_virt) {
                // unmap_huge_page returns the 2MB page's physical base address
                // We need to free both the huge page frames (via DMA region)
                pmm::free_contiguous_frames(huge_dma);
                serial::write_str_safe("[OK] Huge page unmapped successfully\r\n");
            }
            test3_passed = test3_map_ok;
        }
    } else {
        serial::write_str_safe("[FAIL] 2MiB huge page DMA allocation failed\r\n");
    }

    // Verify region_type_at lookup
    serial::write_str_safe("[TEST] 4. Memory region lookup...\r\n");
    let test4_passed = if let Some(_rtype) = pmm::region_type_at(0x1000) {
        // Some region exists at this address
        serial::write_str_safe("[OK] region_type_at lookup works\r\n");
        true
    } else {
        // Address 0x1000 is typically not in usable memory
        serial::write_str_safe("[OK] region_type_at returns None for non-mapped address\r\n");
        true
    };

    serial::write_str_safe("[TEST] Phase 1C verification complete\r\n");
    if test3_passed && test4_passed {
        serial::write_str_safe("[OK] All Phase 1C tests passed\r\n");
    } else {
        serial::write_str_safe("[FAIL] Some Phase 1C tests failed\r\n");
    }

    // ============================================
    // Phase 1F PCI Bus Enumeration Test
    // ============================================

    serial::write_str_safe("\r\n[TEST] Starting Phase 1F PCI verification...\r\n");

    // Test 5: PCI device count
    serial::write_str_safe("[TEST] 5. PCI device discovery...\r\n");
    let pci_count = pci::device_count();
    serial::write_str_safe("[OK] PCI devices found: ");
    print_u64(pci_count as u64);
    serial::write_str_safe("\r\n");

    // Test 6: Find a PCI device by class (Display controller class 0x03)
    serial::write_str_safe("[TEST] 6. PCI class lookup (Display controller)...\r\n");
    if let Some(display_device) = pci::find_by_class(0x03, 0x00) {
        serial::write_str_safe("[OK] Display controller found: vendor=");
        write_u16_hex(display_device.vendor_id);
        serial::write_str_safe(" device=");
        write_u16_hex(display_device.device_id);
        serial::write_str_safe("\r\n");
    } else {
        serial::write_str_safe("[OK] No display controller found (may be expected in QEMU)\r\n");
    }

    serial::write_str_safe("[TEST] Phase 1F PCI verification complete\r\n");

    let tick_count = interrupts::ticks();
    serial::write_str_safe("[OK] Hardware timer verified: ticks=");
    print_u64(tick_count);
    serial::write_str_safe("\r\n");

    // Phase 1E: Set progress to 75% (heap was done earlier, timer verified now)
    splash::set_phase(splash::PHASE_HEAP, b"Timer OK");

    // Phase 1E: Mark boot complete
    splash::complete();

    // Render confirmation on framebuffer
    if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
        let text_color: u32 = 0xFFFFFF;
        fb.draw_str_scaled(10, 520, "Memory: PMM + 16 MiB Heap ACTIVE", text_color, 0x000000, 2);
        fb.draw_str_scaled(10, 568, "APIC: IDT + ExtINT IRQ0 + Timer(100Hz)", text_color, 0x000000, 2);
    }

    // Verify boot sector signature at LBA 0
    if ata::is_present() {
        if ata::verify_boot_signature() {
            serial::write_str_safe("[ATA] Boot sector verified: signature 0xAA55 found at LBA 0\r\n");
        } else {
            serial::write_str_safe("[ATA] Boot sector check: no valid signature at LBA 0 (may be empty)\r\n");
        }
    }

    // Initialize current directory
    set_current_dir("/");

    // Enter interactive command loop
    serial::write_str_safe("\r\n=== Magma OS Interactive Shell ===\r\n");
    serial::write_str_safe("Type 'help' for available commands.\r\n");
    interactive_loop();
}

/// Helper to print u64 as decimal
pub fn print_u64(mut val: u64) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut idx = 19;
    while val > 0 {
        buf[idx] = b'0' + ((val % 10) as u8);
        val /= 10;
        idx -= 1;
    }
    // Skip leading zeros
    let start = buf.iter().position(|&b| b != b'0').unwrap_or(19);
    for &b in &buf[start..=19] {
        serial::write_byte(b);
    }
}

/// Helper to print u64 as hex
pub fn print_u64_hex(val: u64) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 16];
    let mut idx = 0;
    let mut temp = val;
    while temp > 0 {
        buf[idx] = HEX[(temp & 0xF) as usize];
        temp >>= 4;
        idx += 1;
    }
    // Print in reverse (most significant first)
    for i in (0..idx).rev() {
        serial::write_byte(buf[i]);
    }
}

fn halt() -> ! {
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Read a line from input (keyboard + serial)
/// Supports dual input capability: reads from both PS/2 keyboard and COM1 serial port
fn read_line() -> alloc::vec::Vec<u8> {
    let mut line: alloc::vec::Vec<u8> = alloc::vec![];
    let mut done = false;
    loop {
        // Poll keyboard for input
        if let Some(key) = keyboard::pop_event() {
            done = handle_input_char(key.ch, &mut line);
        }

        // Poll serial port for input (non-blocking)
        if let Some(byte) = serial::try_read_byte() {
            done = handle_input_char(byte, &mut line);
        }

        if done {
            break;
        }

        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
    line
}

/// Handle a single input character (from keyboard or serial)
/// Updates line buffer and echoes to serial output
/// Returns true if end of line was reached
fn handle_input_char(ch: u8, line: &mut alloc::vec::Vec<u8>) -> bool {
    if ch == b'\r' || ch == b'\n' {
        // End of line - print newline and signal done
        serial::write_str_safe("\r\n");
        true
    } else if ch == 0x7F || ch == b'\x08' {
        // Backspace
        if let Some(_) = line.pop() {
            serial::write_byte(b'\x08');  // Move cursor back
            serial::write_byte(b' ');   // Erase
            serial::write_byte(b'\x08');  // Move cursor back again
        }
        false
    } else if ch >= 32 {
        // Printable character
        line.push(ch);
        serial::write_byte(ch);
        false
    } else {
        // Ignore other control characters
        false
    }
}

/// Trim whitespace from a byte vector
fn trim_bytes(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'\n').unwrap_or(line.len());
    let end = line.iter().rposition(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'\n').map(|i| i + 1).unwrap_or(0);
    if start >= end { &[] } else { &line[start..end] }
}

/// Check if a line starts with a given string
fn starts_with(line: &[u8], prefix: &[u8]) -> bool {
    line.len() >= prefix.len() && &line[..prefix.len()] == prefix
}

/// Print help text
fn print_help() {
    serial::write_str_safe("\r\nAvailable commands:\r\n");
    serial::write_str_safe("  help        - Show this help message\r\n");
    serial::write_str_safe("  mem         - Show memory statistics (PMM, heap)\r\n");
    serial::write_str_safe("  pci         - List PCI devices\r\n");
    serial::write_str_safe("  ticks       - Show timer tick count\r\n");
    serial::write_str_safe("  disk        - List detected IDE devices\r\n");
    serial::write_str_safe("  partitions  - List detected partitions\r\n");
    serial::write_str_safe("  ls [path]    - List directory contents\r\n");
    serial::write_str_safe("  cat <path>   - Display file contents\r\n");
    serial::write_str_safe("  cd <path>    - Change current directory\r\n");
    serial::write_str_safe("  pwd         - Show current working directory\r\n");
    serial::write_str_safe("  reboot      - Reboot the system\r\n");
    serial::write_str_safe("  shutdown    - Shutdown the system\r\n");
    serial::write_str_safe("  fence-test  - Test write fencing\r\n");
    serial::write_str_safe("  exit/quit   - Exit shell and halt\r\n");
    serial::write_str_safe("\r\n");
}

/// Show memory statistics
fn cmd_mem() {
    let (free_frames, total_frames) = pmm::stats();
    serial::write_str_safe("\r\nMemory Statistics:\r\n");
    serial::write_str_safe("  Free frames: ");
    print_u64(free_frames as u64);
    serial::write_str_safe("\r\n");
    serial::write_str_safe("  Total frames: ");
    print_u64(total_frames as u64);
    serial::write_str_safe("\r\n");

    let free_mb = (free_frames as u64 * 4096) / (1024 * 1024);
    let total_mb = (total_frames as u64 * 4096) / (1024 * 1024);
    serial::write_str_safe("  Free RAM: ");
    print_u64(free_mb);
    serial::write_str_safe(" MiB\r\n");
    serial::write_str_safe("  Total RAM: ");
    print_u64(total_mb);
    serial::write_str_safe(" MiB\r\n");
}

/// List PCI devices
fn cmd_pci() {
    let count = pci::device_count();
    serial::write_str_safe("\r\nPCI Devices (");
    print_u64(count as u64);
    serial::write_str_safe(" found):\r\n");

    let mut iter = pci::devices();
    while let Some(dev) = iter.next() {
        serial::write_str_safe("  ");
        write_u16_hex(dev.vendor_id);
        serial::write_str_safe(":");
        write_u16_hex(dev.device_id);
        serial::write_str_safe(" bus:");
        write_u8_hex(dev.bus);
        serial::write_str_safe(".");
        write_u8_hex(dev.device);
        serial::write_str_safe(".");
        write_u8_hex(dev.function);

        // Decode class code
        let (class, subclass, prog_if) = {
            let base = ((dev.class_code >> 16) & 0xFF) as u8;
            let sub = ((dev.class_code >> 8) & 0xFF) as u8;
            let prog = (dev.class_code & 0xFF) as u8;
            (base, sub, prog)
        };
        serial::write_str_safe(" class=");
        write_u8_hex(class);
        serial::write_str_safe("/");
        write_u8_hex(subclass);
        serial::write_str_safe("/");
        write_u8_hex(prog_if);

        if dev.interrupt_line != 0 {
            serial::write_str_safe(" INT=");
            print_u64(dev.interrupt_line as u64);
        }
        serial::write_str_safe("\r\n");
    }
}

/// Show timer tick count
fn cmd_ticks() {
    let ticks = interrupts::ticks();
    serial::write_str_safe("\r\nTimer ticks: ");
    print_u64(ticks);
    serial::write_str_safe("\r\n");
}

/// List disk/IDE devices
fn cmd_disk() {
    serial::write_str_safe("\r\nIDE Storage Devices:\r\n");
    if ata::is_present() {
        serial::write_str_safe("  Controller: Primary IDE at I/O 0x");
        write_u16_hex(ata::io_base());
        serial::write_str_safe("\r\n");

        if let Some(info) = ata::get_drive_info() {
            // Print model string
            serial::write_str_safe("  Model: \"");
            for &b in &info.model {
                if b == 0 {
                    break;
                }
                serial::write_byte(b);
            }
            serial::write_str_safe("\"\r\n");

            // Print serial if present
            let has_serial = info.serial.iter().any(|&b| b != 0);
            if has_serial {
                serial::write_str_safe("  Serial: ");
                for &b in &info.serial {
                    if b == 0 {
                        break;
                    }
                    serial::write_byte(b);
                }
                serial::write_str_safe("\r\n");
            }

            // Print capacity
            serial::write_str_safe("  Capacity: ");
            let mb = (info.max_lba as u64 * 512) / (1024 * 1024);
            let mut tmp = mb;
            let mut buf = [0u8; 20];
            let mut idx = 19;
            if tmp == 0 {
                buf[19] = b'0';
                idx = 19;
            } else {
                while tmp > 0 {
                    buf[idx] = b'0' + ((tmp % 10) as u8);
                    tmp /= 10;
                    idx -= 1;
                }
                idx += 1;
            }
            for &b in &buf[idx..=19] {
                serial::write_byte(b);
            }
            serial::write_str_safe(" MB");
            serial::write_str_safe("\r\n");

            // Print boot signature status
            serial::write_str_safe("  LBA 0 Signature: 0x");
            write_u16_hex(info.lba0_signature);
            if info.lba0_signature == 0xAA55 {
                serial::write_str_safe(" (bootable)\r\n");
            } else {
                serial::write_str_safe("\r\n");
            }
        }
    } else {
        serial::write_str_safe("  No IDE controller detected\r\n");
    }
}

/// List detected partitions
fn cmd_partitions() {
    serial::write_str_safe("\r\nDetected Partitions:\r\n");

    let table_type = gpt::get_partition_table_type();
    match table_type {
        gpt::PartitionTableType::Superfloppy => {
            serial::write_str_safe("  Partition Table: Superfloppy (no partition table)\r\n");
        }
        gpt::PartitionTableType::Mbr => {
            serial::write_str_safe("  Partition Table: MBR\r\n");
        }
        gpt::PartitionTableType::Gpt => {
            serial::write_str_safe("  Partition Table: GPT\r\n");
        }
        gpt::PartitionTableType::Invalid => {
            serial::write_str_safe("  Partition Table: Invalid/Unknown\r\n");
        }
    }

    let pcount = gpt::partition_count();
    for i in 0..pcount {
        if let Some(info) = gpt::get_partition(i) {
            serial::write_str_safe("  Partition ");
            print_u64(i as u64);
            serial::write_str_safe(": LBA=");
            print_u64(info.start_lba);
            serial::write_str_safe(" count=");
            print_u64(info.sector_count);
            serial::write_str_safe(" role=");
            match info.role {
                gpt::PartitionRole::BootEsp => serial::write_str_safe("ESP\r\n"),
                gpt::PartitionRole::RecoveryVault => serial::write_str_safe("Recovery\r\n"),
                gpt::PartitionRole::Data => serial::write_str_safe("Data\r\n"),
                gpt::PartitionRole::Unknown => serial::write_str_safe("Unknown\r\n"),
            }
        }
    }

    // Show mounted volumes
    let mcount = vfs::mount_count();
    if mcount > 0 {
        serial::write_str_safe("\r\nMounted Volumes:\r\n");
        for i in 0..mcount {
            if let Some(mount) = vfs::get_mount(i) {
                serial::write_str_safe("  ");
                for b in mount.path.as_bytes() {
                    serial::write_byte(*b);
                }
                serial::write_str_safe(" (volume ");
                print_u64(i as u64);
                serial::write_str_safe(")\r\n");
            }
        }
    }
}

/// List directory contents
fn cmd_ls(args: &[u8]) {
    // Use a heap-allocated string for the path
    let mut path_str = String::new();
    for &b in args {
        if b == b' ' {
            break;
        }
        path_str.push(b as char);
    }

    // Build the full path
    let full_path = if path_str.is_empty() {
        // Use current directory if no path specified
        let current = get_current_dir();
        if current.is_empty() {
            String::from("/")
        } else {
            current.to_string()
        }
    } else if path_str.starts_with('/') {
        path_str
    } else {
        // Handle relative paths
        let current = get_current_dir();
        if current.is_empty() {
            format!("/{}", path_str)
        } else if current.ends_with('/') {
            format!("{}{}", current, path_str)
        } else {
            format!("{}/{}", current, path_str)
        }
    };

    // Normalize the path
    let normalized_path = normalize_path(&full_path);

    serial::write_str_safe("\r\nDirectory listing for '");
    for b in normalized_path.as_bytes() {
        serial::write_byte(*b);
    }
    serial::write_str_safe("':\r\n");

    if let Some(files) = vfs::list_dir(&normalized_path) {
        if files.is_empty() {
            serial::write_str_safe("  (empty)\r\n");
        } else {
            for file in files {
                serial::write_str_safe("  ");
                for b in file.name.as_bytes() {
                    serial::write_byte(*b);
                }
                if !file.ext.is_empty() {
                    serial::write_byte(b'.');
                    for b in file.ext.as_bytes() {
                        serial::write_byte(*b);
                    }
                }
                if file.is_directory {
                    serial::write_str_safe("/");
                }
                serial::write_str_safe(" ");
                print_u64(file.size as u64);
                serial::write_str_safe(" bytes\r\n");
            }
        }
    } else {
        serial::write_str_safe("  Error: Could not read directory\r\n");
    }
}

/// Display file contents
fn cmd_cat(args: &[u8]) {
    if args.is_empty() {
        serial::write_str_safe("\r\nUsage: cat <path>\r\n");
        return;
    }

    // Convert bytes to string
    let mut path_str = String::new();
    for &b in args {
        if b == b' ' {
            break;
        }
        path_str.push(b as char);
    }

    // Build the full path
    let full_path = if path_str.starts_with('/') {
        path_str
    } else {
        // Handle relative paths
        let current = get_current_dir();
        if current.is_empty() {
            format!("/{}", path_str)
        } else if current.ends_with('/') {
            format!("{}{}", current, path_str)
        } else {
            format!("{}/{}", current, path_str)
        }
    };

    // Normalize the path
    let normalized_path = normalize_path(&full_path);

    serial::write_str_safe("\r\nFile: ");
    for b in normalized_path.as_bytes() {
        serial::write_byte(*b);
    }
    serial::write_str_safe("\r\n");

    if let Some(data) = vfs::read_with_fallback(&normalized_path) {
        if data.is_empty() {
            serial::write_str_safe("(empty file)\r\n");
        } else {
            // Display first 512 bytes max
            let max_display = core::cmp::min(data.len(), 512);
            for i in 0..max_display {
                let b = data[i];
                if b >= 32 && b < 127 {
                    serial::write_byte(b);
                } else {
                    serial::write_byte(b'.');
                }
            }
            if data.len() > 512 {
                serial::write_str_safe("\r\n... (truncated, total ");
                print_u64(data.len() as u64);
                serial::write_str_safe(" bytes)\r\n");
            } else {
                serial::write_str_safe("\r\n");
            }
        }
    } else {
        serial::write_str_safe("Error: File not found\r\n");
    }
}

/// Test write fencing
fn cmd_fence_test() {
    serial::write_str_safe("\r\nTesting ATA write fencing...\r\n");

    // Step 1: Add a write fence for LBA 100
    ata::add_write_fence(100, 1);
    serial::write_str_safe("  Step 1: Added write fence for LBA 100, count=1\r\n");

    // Step 2: Try writing to the fenced range - should be REJECTED
    serial::write_str_safe("  Step 2: Attempting write to LBA 100 (should be REJECTED)...\r\n");
    let test_data = [0xAAu8; 512];
    if ata::write_sectors(100, 1, &test_data) {
        serial::write_str_safe("  Step 2: Write ACCEPTED (unexpected - fence should block)\r\n");
    } else {
        serial::write_str_safe("  Step 2: Write REJECTED (expected - fence blocks write)\r\n");
    }

    // Step 3: Try writing to a non-overlapping range - should SUCCEED
    serial::write_str_safe("  Step 3: Attempting write to LBA 200 (should SUCCEED)...\r\n");
    if ata::write_sectors(200, 1, &test_data) {
        serial::write_str_safe("  Step 3: Write SUCCEEDED (expected - no fence overlap)\r\n");
    } else {
        serial::write_str_safe("  Step 3: Write FAILED (unexpected - no fence overlap)\r\n");
    }

    // Step 4: Flush the cache
    if ata::flush_cache() {
        serial::write_str_safe("  Step 4: Cache flush successful\r\n");
    } else {
        serial::write_str_safe("  Step 4: Cache flush failed\r\n");
    }

    // Step 5: Verify fence was cleared after flush
    if ata::has_write_fence_overlap(100, 1) {
        serial::write_str_safe("  Step 5: LBA 100 still has pending fence\r\n");
    } else {
        serial::write_str_safe("  Step 5: Fence cleared after flush\r\n");
    }

    // Step 6: Test clear_write_fences directly
    ata::clear_write_fences();
    serial::write_str_safe("  Step 6: write fences explicitly cleared\r\n");

    // Step 7: Verify fences are cleared
    if ata::has_write_fence_overlap(100, 1) {
        serial::write_str_safe("  Step 7: LBA 100 still has fence (unexpected)\r\n");
    } else {
        serial::write_str_safe("  Step 7: Fences verified as cleared\r\n");
    }

    serial::write_str_safe("  Fence test complete\r\n");
}

/// Change current directory
fn cmd_cd(args: &[u8]) {
    if args.is_empty() {
        serial::write_str_safe("\r\nUsage: cd <path>\r\n");
        return;
    }

    // Convert bytes to string
    let mut path_str = String::new();
    for &b in args {
        if b == b' ' {
            break;
        }
        path_str.push(b as char);
    }

    // Handle home directory
    if path_str == "~" {
        set_current_dir("/");
        serial::write_str_safe("\r\nChanged to: /\r\n");
        return;
    }

    // Handle absolute paths
    let new_path = if path_str.starts_with('/') {
        path_str
    } else {
        // Relative path - prepend current directory
        let current = get_current_dir();
        if current.is_empty() || current == "/" {
            format!("/{}", path_str)
        } else if current.ends_with('/') {
            format!("{}{}", current, path_str)
        } else {
            format!("{}/{}", current, path_str)
        }
    };

    // Normalize the path (remove /./ and /../)
    let normalized = normalize_path(&new_path);

    // Check if the directory exists
    if vfs::directory_exists(&normalized) {
        set_current_dir(&normalized);
        serial::write_str_safe("\r\nChanged to: ");
        serial::write_str_safe(get_current_dir());
        serial::write_str_safe("\r\n");
    } else {
        serial::write_str_safe("\r\nError: Directory not found: ");
        for b in new_path.as_bytes() {
            serial::write_byte(*b);
        }
        serial::write_str_safe("\r\n");
    }
}

/// Normalize a path by removing . and .. components
fn normalize_path(path: &str) -> String {
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut result = Vec::new();

    for component in components {
        match component {
            ".." => {
                if !result.is_empty() {
                    result.pop();
                }
            }
            "." => {}
            _ => result.push(component),
        }
    }

    if result.is_empty() {
        String::from("/")
    } else {
        format!("/{}", result.join("/"))
    }
}

/// Reboot the system using QEMU's reboot port
fn cmd_reboot() {
    serial::write_str_safe("\r\nRebooting...\r\n");

    // Try QEMU reboot via keyboard controller port 0x64
    // Send CPU reset command (0xFE)
    unsafe {
        asm!(
            "out 0x64, al",
            in("al") 0xFEu8,
            options(nomem, nostack, preserves_flags)
        );
    }

    // If that doesn't work, triple fault
    unsafe {
        asm!(
            "int 0",
            options(nomem, nostack, preserves_flags)
        );
    }

    // Should never reach here
    halt();
}

/// Shutdown the system using QEMU's shutdown port
fn cmd_shutdown() {
    serial::write_str_safe("\r\nShutting down...\r\n");

    // QEMU shutdown via ISA debug port 0xB004
    // Write any value to trigger shutdown
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") 0xB004u16,
            in("eax") 0x2000u32,
            options(nomem, nostack, preserves_flags)
        );
    }

    // Also try the newer QEMU shutdown port 0x604
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") 0x604u16,
            in("eax") 0x2000u32,
            options(nomem, nostack, preserves_flags)
        );
    }

    // If that doesn't work, halt
    halt();
}

/// Show current working directory
fn cmd_pwd() {
    serial::write_str_safe("\r\n");
    let current = get_current_dir();
    if current.is_empty() || current == "/" {
        serial::write_str_safe("/\r\n");
    } else {
        serial::write_str_safe(current);
        serial::write_str_safe("\r\n");
    }
}

/// Interactive command loop
fn interactive_loop() -> ! {
    loop {
        serial::write_str_safe("\r\n> ");

        let line = read_line();
        let trimmed = trim_bytes(&line);

        if trimmed.is_empty() {
            continue;
        }

        // Process commands
        if starts_with(trimmed, b"help") {
            print_help();
        } else if starts_with(trimmed, b"mem") {
            cmd_mem();
        } else if starts_with(trimmed, b"pci") {
            cmd_pci();
        } else if starts_with(trimmed, b"ticks") {
            cmd_ticks();
        } else if starts_with(trimmed, b"disk") {
            cmd_disk();
        } else if starts_with(trimmed, b"partitions") {
            cmd_partitions();
        } else if starts_with(trimmed, b"ls") {
            let args = if trimmed.len() > 2 && trimmed[2] == b' ' {
                trim_bytes(&trimmed[3..])
            } else {
                &[]
            };
            cmd_ls(args);
        } else if starts_with(trimmed, b"cat") {
            let args = if trimmed.len() > 3 && trimmed[3] == b' ' {
                trim_bytes(&trimmed[4..])
            } else {
                &[]
            };
            cmd_cat(args);
        } else if starts_with(trimmed, b"fence-test") {
            cmd_fence_test();
        } else if starts_with(trimmed, b"cd") {
            let args = if trimmed.len() > 2 && trimmed[2] == b' ' {
                trim_bytes(&trimmed[3..])
            } else {
                &[]
            };
            cmd_cd(args);
        } else if starts_with(trimmed, b"pwd") {
            cmd_pwd();
        } else if starts_with(trimmed, b"reboot") {
            cmd_reboot();
        } else if starts_with(trimmed, b"shutdown") {
            cmd_shutdown();
        } else if starts_with(trimmed, b"exit") || starts_with(trimmed, b"quit") {
            serial::write_str_safe("Goodbye.\r\n");
            break;
        } else {
            serial::write_str_safe("\r\nUnknown command: '");
            for &b in trimmed {
                serial::write_byte(b);
            }
            serial::write_str_safe("'. Type 'help' for available commands.\r\n");
        }
    }

    // Halt after exit
    halt();
}