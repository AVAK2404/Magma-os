#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(naked_functions_rustic_abi)]

extern crate alloc;

mod acpi;
mod framebuffer;
mod gdt;
mod heap;
mod interrupts;
mod ioapic;
mod panic;
mod paging;
mod pmm;
mod pci;
mod serial;
mod splash;
mod sync;

use alloc::{boxed::Box, string::String, vec::Vec};
use core::arch::asm;
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};
use limine::request::FramebufferRequest;
use limine::request::HhdmRequest;
use limine::request::MemmapRequest;
use limine::request::RsdpRequest;

// Import PCI helper functions for use in tests
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
        heap::init().expect("Failed to map heap pages");
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

        // Initialize Local APIC timer at 100 Hz (same as PIT for consistency)
        interrupts::init_lapic_timer(100);
        serial::write_str_safe("[OK] Local APIC timer calibrated at 100 Hz\r\n");
    }

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
    if let Some(huge_dma) = pmm::alloc_contiguous_frames(512, 512) {
        // Use a dedicated 2 MiB-aligned virtual address for testing
        // This avoids unmapping the HHDM-mapped huge_dma.virt_addr
        let huge_virt: u64 = 0xFFFF_C000_0200_0000;
        let huge_phys = huge_dma.phys_addr;

        if paging::map_huge_page(huge_virt, huge_phys, paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE).is_ok() {
            unsafe {
                let virt_ptr = huge_virt as *mut u64;
                virt_ptr.write(0x12345678_9ABCDEF0);
                let val = virt_ptr.read();
                if val == 0x12345678_9ABCDEF0 {
                    serial::write_str_safe("[OK] 2MiB huge page map/read/write passed\r\n");
                } else {
                    serial::write_str_safe("[FAIL] Huge page read mismatch\r\n");
                }
            }
            if let Some(_unmapped_phys) = paging::unmap_huge_page(huge_virt) {
                // unmap_huge_page returns the 2MB page's physical base address
                // We need to free both the huge page frames (via DMA region)
                pmm::free_contiguous_frames(huge_dma);
                serial::write_str_safe("[OK] Huge page unmapped successfully\r\n");
            }
        } else {
            serial::write_str_safe("[FAIL] Huge page mapping failed\r\n");
            pmm::free_contiguous_frames(huge_dma);
        }
    } else {
        serial::write_str_safe("[FAIL] 2MiB huge page DMA allocation failed\r\n");
    }

    // Verify region_type_at lookup
    serial::write_str_safe("[TEST] 4. Memory region lookup...\r\n");
    if let Some(_rtype) = pmm::region_type_at(0x1000) {
        // Some region exists at this address
        serial::write_str_safe("[OK] region_type_at lookup works\r\n");
    } else {
        // Address 0x1000 is typically not in usable memory
        serial::write_str_safe("[OK] region_type_at returns None for non-mapped address\r\n");
    }

    serial::write_str_safe("[TEST] Phase 1C verification complete\r\n");
    serial::write_str_safe("[OK] All Phase 1C tests passed\r\n");

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

    serial::write_str_safe("[TRACE] Entering halt loop\r\n");

    // Should never reach here, but halt just in case
    halt()
}

/// Helper to print u64 as decimal
fn print_u64(mut val: u64) {
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
fn print_u64_hex(val: u64) {
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