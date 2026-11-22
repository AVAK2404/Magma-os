$code = @"
#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod framebuffer;
mod gdt;
mod interrupts;
#[macro_use]
mod panic;
#[macro_use]
mod serial;

use alloc::format;
use core::arch::asm;
use core::alloc::Layout;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};
use limine::request::FramebufferRequest;
use limine::request::MemmapRequest;

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

struct BumpAllocator {
    heap_start: AtomicUsize,
    heap_end: usize,
}

impl BumpAllocator {
    const fn new() -> Self {
        BumpAllocator {
            heap_start: AtomicUsize::new(0),
            heap_end: 0,
        }
    }
    
    fn init(&self, start: usize, _size: usize) {
        self.heap_start.store(start, Ordering::SeqCst);
    }
}

unsafe impl alloc::alloc::GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let start = self.heap_start.load(Ordering::SeqCst);
        if start == 0 {
            return null_mut();
        }
        
        let align = layout.align();
        let size = layout.size();
        let aligned_start = (start + align - 1) & !(align - 1);
        let end = aligned_start + size;
        
        if end > start + 1024 * 1024 {
            return null_mut();
        }
        
        if self.heap_start.compare_exchange(start, end, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            aligned_start as *mut u8
        } else {
            null_mut()
        }
    }
    
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    serial::init();

    if !BASE_REVISION.is_supported() {
        serial_println!("ERROR: Limine base revision not supported.");
        halt();
    }

    serial_println!("WELCOME TO MAGMA OS");
    serial_println!("Magma Kernel Initialised.");
    serial_println!("Architecture: x86_64");
    serial_println!("Bootloader: Limine");

    let memmap_resp = MEMMAP_REQUEST.response();
    let mut total_usable: u64 = 0;
    
    if let Some(resp) = memmap_resp {
        let entries: &[&limine::memmap::Entry] = resp.entries();
        for entry in entries {
            let base = *entry;
            serial_println!("MEMMAP: base=0x{:016x} len=0x{:016x} type={}", 
                base.base, base.length, base.type_);
            if base.type_ == limine::memmap::MEMMAP_USABLE {
                total_usable += base.length as u64;
            }
        }
    }
    serial_println!("MEMMAP: Total usable RAM = {} MiB", total_usable / (1024 * 1024));

    unsafe {
        ALLOCATOR.init(0xFFFFFFFF81000000, 1024 * 1024);
    }

    let fb_entry = FRAMEBUFFER_REQUEST.response().and_then(|r| r.framebuffers().first());
    if let Some(entry) = fb_entry {
        framebuffer::init(entry);
        let (width, _height) = framebuffer::FRAMEBUFFER.get().unwrap().dimensions();
        
        let clear_color: u32 = 0x0B0F17;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            fb.clear(clear_color);
        }
        
        let header_color: u32 = 0xE85D04;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            fb.fill_rect(0, 0, width, 32, header_color);
        }
        
        let text_color: u32 = 0xFFFFFF;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            fb.draw_str(10, 64, "MAGMA OS", text_color);
            fb.draw_str(10, 96, "Kernel Initialised | x86_64 | Limine", text_color);
            let ram_str = format!("Usable RAM: {} MiB", total_usable / (1024 * 1024));
            fb.draw_str(10, 128, &ram_str, text_color);
            fb.draw_str(10, 160, "Status: RUNNING", text_color);
        }
    }

    gdt::init();
    serial_println!("[OK] GDT & TSS loaded");

    interrupts::init();
    serial_println!("[OK] IDT, PIC (32..47), and 100Hz PIT enabled");

    unsafe {
        core::arch::asm!("int3", options(nomem, nostack));
    }
    serial_println!("[OK] Breakpoint (#BP) caught and recovered");

    while interrupts::ticks() < 25 {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
    
    let tick_count = interrupts::ticks();
    serial_println!("[OK] Hardware timer verified: {} ticks", tick_count);
    
    let fb_entry = FRAMEBUFFER_REQUEST.response().and_then(|r| r.framebuffers().first());
    if let Some(_entry) = fb_entry {
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            let text_color: u32 = 0xFFFFFF;
            fb.draw_str(10, 192, "Interrupts: IDT + 100Hz PIT ACTIVE", text_color);
        }
    }

    halt();
}

fn halt() -> ! {
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}
"@
Set-Content -Path "src\main.rs" -Value $code -Encoding UTF8
Remove-Item "make_main.ps1"
