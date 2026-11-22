#![no_std]
#![no_main]

extern crate alloc;

mod framebuffer;
mod panic;
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

/// Global allocator for no_std environment using atomic bump pointer
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
    
    fn init(&self, start: usize, size: usize) {
        self.heap_start.store(start, Ordering::SeqCst);
        // Note: heap_end is immutable after init in this simple design
        // We use a workaround with static mut for heap_end
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
        
        // heap_end is stored as const; we need another way
        // For simplicity, just check a reasonable limit
        if end > start + 1024 * 1024 {  // 1MB limit
            return null_mut();
        }
        
        if self.heap_start.compare_exchange(start, end, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            aligned_start as *mut u8
        } else {
            null_mut()
        }
    }
    
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't support deallocation
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

// Mutable heap end for allocator initialization
static mut HEAP_END: usize = 0;

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

    // Memory map
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

    // Initialize allocator (use first usable memory region for heap)
    unsafe {
        ALLOCATOR.init(0xFFFFFFFF81000000, 1024 * 1024);
        HEAP_END = 0xFFFFFFFF81000000 + 1024 * 1024;
    }

    // Initialize framebuffer
    let fb_entry = FRAMEBUFFER_REQUEST.response().and_then(|r| r.framebuffers().first());
    if let Some(entry) = fb_entry {
        framebuffer::init(entry);
        let (width, _height) = framebuffer::FRAMEBUFFER.get().unwrap().dimensions();
        
        // Clear framebuffer to dark blue-ish color
        let clear_color: u32 = 0x0B0F17;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            fb.clear(clear_color);
        }
        
        // Draw header bar
        let header_color: u32 = 0xE85D04;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            fb.fill_rect(0, 0, width, 32, header_color);
        }
        
        // Draw boot screen text
        let text_color: u32 = 0xFFFFFF;
        if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
            // Render "MAGMA OS"
            fb.draw_str(10, 64, "MAGMA OS", text_color);
            // Render "Kernel Initialised | x86_64 | Limine"
            fb.draw_str(10, 96, "Kernel Initialised | x86_64 | Limine", text_color);
            // Render "Usable RAM: <X> MiB"
            let ram_str = format!("Usable RAM: {} MiB", total_usable / (1024 * 1024));
            fb.draw_str(10, 128, &ram_str, text_color);
            // Render "Status: RUNNING"
            fb.draw_str(10, 160, "Status: RUNNING", text_color);
        }
    }

    halt();
}

fn halt() -> ! {
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}
