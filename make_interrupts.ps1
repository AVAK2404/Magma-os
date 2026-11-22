$code = @"
//! x86-64 Interrupt Descriptor Table (IDT), Exception Handlers, PIC, and PIT
//! Phase 1C: IDT, exception handling, 8259 PIC remapping, PIT timer

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::serial_println;

// Global tick counter for timer interrupts
pub static TICKS: AtomicU64 = AtomicU64::new(0);

/// Interrupt Stack Frame (pushed by CPU on exception/interrupt)
#[repr(C)]
pub struct InterruptStackFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// IDT Entry (16 bytes for 64-bit)
#[derive(Copy, Clone)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    options: u16,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn new() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            options: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }
    
    fn set_handler(&mut self, handler: u64, selector: u16, options: u16) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = selector;
        self.options = options;
    }
}

/// IDT with 256 entries
#[repr(C, align(16))]
struct InterruptDescriptorTable {
    entries: [IdtEntry; 256],
}

impl InterruptDescriptorTable {
    const fn new() -> Self {
        InterruptDescriptorTable {
            entries: [IdtEntry::new(); 256],
        }
    }
}

/// IDT Descriptor for lidt instruction
#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

// Static IDT
static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

// Kernel code segment selector (from GDT)
const KERNEL_CODE_SELECTOR: u16 = 0x08;

// IDT entry options
const IDT_PRESENT: u16 = 1 << 15;
const IDT_DPL0: u16 = 0 << 13;
const IDT_DPL3: u16 = 3 << 13;
const IDT_INTERRUPT_GATE: u16 = 0xE << 8;
const IDT_TRAP_GATE: u16 = 0xF << 8;
const IDT_IST1: u16 = 1 << 0;

/// Exception handler for Breakpoint (#BP, vector 3)
extern "x86-interrupt" fn breakpoint_handler(frame: &mut InterruptStackFrame) {
    serial_println!("[EXCEPTION] Breakpoint (#BP) at RIP=0x{:016X}", frame.rip);
}

/// Exception handler for Double Fault (#DF, vector 8)
extern "x86-interrupt" fn double_fault_handler(frame: &mut InterruptStackFrame, error_code: u64) -> ! {
    serial_println!("[EXCEPTION] Double Fault (#DF) at RIP=0x{:016X}, error_code=0x{:X}", frame.rip, error_code);
    serial_println!("  RSP=0x{:016X}, RFLAGS=0x{:016X}", frame.rsp, frame.rflags);
    halt();
}

/// Exception handler for General Protection Fault (#GP, vector 13)
extern "x86-interrupt" fn general_protection_fault_handler(frame: &mut InterruptStackFrame, error_code: u64) -> ! {
    serial_println!("[EXCEPTION] General Protection Fault (#GP) at RIP=0x{:016X}, error_code=0x{:X}", frame.rip, error_code);
    serial_println!("  RSP=0x{:016X}, RFLAGS=0x{:016X}", frame.rsp, frame.rflags);
    halt();
}

/// Exception handler for Page Fault (#PF, vector 14)
extern "x86-interrupt" fn page_fault_handler(frame: &mut InterruptStackFrame, error_code: u64) -> ! {
    let cr2: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
    }
    serial_println!("[EXCEPTION] Page Fault (#PF) at RIP=0x{:016X}, CR2=0x{:016X}, error_code=0x{:X}", frame.rip, cr2, error_code);
    serial_println!("  RSP=0x{:016X}, RFLAGS=0x{:016X}", frame.rsp, frame.rflags);
    halt();
}

/// Timer interrupt handler (IRQ 0, vector 32)
extern "x86-interrupt" fn timer_handler(_frame: &mut InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::SeqCst);
    unsafe {
        asm!("out dx, al", in("dx") 0x20u16, in("al") 0x20u8, options(nomem, nostack, preserves_flags));
    }
}

/// Default handler for unhandled interrupts
extern "x86-interrupt" fn default_handler(_frame: &mut InterruptStackFrame) {
    serial_println!("[INTERRUPT] Unhandled interrupt");
}

#[inline(never)]
fn halt() -> ! {
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

/// 8259 PIC I/O ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// Initialize 8259 PIC
fn init_pic() {
    unsafe {
        let _mask1: u8;
        let _mask2: u8;
        asm!("in al, dx", out("al") _mask1, in("dx") PIC1_DATA, options(nomem, nostack, preserves_flags));
        asm!("in al, dx", out("al") _mask2, in("dx") PIC2_DATA, options(nomem, nostack, preserves_flags));
        
        asm!("out dx, al", in("dx") PIC1_CMD, in("al") 0x11u8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIC2_CMD, in("al") 0x11u8, options(nomem, nostack, preserves_flags));
        
        asm!("out dx, al", in("dx") PIC1_DATA, in("al") 0x20u8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIC2_DATA, in("al") 0x28u8, options(nomem, nostack, preserves_flags));
        
        asm!("out dx, al", in("dx") PIC1_DATA, in("al") 0x04u8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIC2_DATA, in("al") 0x02u8, options(nomem, nostack, preserves_flags));
        
        asm!("out dx, al", in("dx") PIC1_DATA, in("al") 0x01u8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIC2_DATA, in("al") 0x01u8, options(nomem, nostack, preserves_flags));
        
        asm!("out dx, al", in("dx") PIC1_DATA, in("al") 0xFEu8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIC2_DATA, in("al") 0xFFu8, options(nomem, nostack, preserves_flags));
    }
}

/// Initialize PIT (Programmable Interval Timer) channel 0
fn init_pit(hz: u32) {
    let divisor = 1193182 / hz;
    let low = (divisor & 0xFF) as u8;
    let high = ((divisor >> 8) & 0xFF) as u8;
    
    unsafe {
        asm!("out dx, al", in("dx") 0x43u16, in("al") 0x36u8, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") 0x40u16, in("al") low, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") 0x40u16, in("al") high, options(nomem, nostack, preserves_flags));
    }
}

/// Initialize IDT, PIC, and PIT
pub fn init() {
    unsafe {
        IDT.entries[3].set_handler(
            breakpoint_handler as u64,
            KERNEL_CODE_SELECTOR,
            IDT_PRESENT | IDT_DPL3 | IDT_TRAP_GATE,
        );
        
        IDT.entries[8].set_handler(
            double_fault_handler as u64,
            KERNEL_CODE_SELECTOR,
            IDT_PRESENT | IDT_DPL0 | IDT_INTERRUPT_GATE | IDT_IST1,
        );
        
        IDT.entries[13].set_handler(
            general_protection_fault_handler as u64,
            KERNEL_CODE_SELECTOR,
            IDT_PRESENT | IDT_DPL0 | IDT_INTERRUPT_GATE,
        );
        
        IDT.entries[14].set_handler(
            page_fault_handler as u64,
            KERNEL_CODE_SELECTOR,
            IDT_PRESENT | IDT_DPL0 | IDT_INTERRUPT_GATE,
        );
        
        IDT.entries[32].set_handler(
            timer_handler as u64,
            KERNEL_CODE_SELECTOR,
            IDT_PRESENT | IDT_DPL0 | IDT_INTERRUPT_GATE,
        );
        
        for i in 0..256 {
            if i != 3 && i != 8 && i != 13 && i != 14 && i != 32 {
                IDT.entries[i].set_handler(
                    default_handler as u64,
                    KERNEL_CODE_SELECTOR,
                    IDT_PRESENT | IDT_DPL0 | IDT_INTERRUPT_GATE,
                );
            }
        }
        
        let idt_desc = IdtDescriptor {
            limit: (core::mem::size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: &IDT as *const _ as u64,
        };
        
        asm!("lidt [{}]", in(reg) &idt_desc, options(nostack, preserves_flags));
        
        init_pic();
        init_pit(100);
        
        asm!("sti", options(nomem, nostack, preserves_flags));
    }
}

/// Get current tick count
pub fn ticks() -> u64 {
    TICKS.load(Ordering::SeqCst)
}
"@
Set-Content -Path "src\interrupts.rs" -Value $code -Encoding UTF8
Remove-Item "make_interrupts.ps1"
