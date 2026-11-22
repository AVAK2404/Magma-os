//! Interrupt Descriptor Table (IDT), PIC remapping, and PIT timer setup
//!
//! This module sets up:
//! - 256-entry 64-bit IDT
//! - Exception handlers for #BP, #DF, #GP, #PF
//! - 8259 PIC remapping to vectors 32-47
//! - 8259 PIT channel 0 at 100 Hz

use core::sync::atomic::AtomicU64;

/// Number of IDT entries
const IDT_ENTRIES: usize = 256;

/// PIC commands
const PIC_COMMAND: u16 = 0x20;
const PIC_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// PIC initialization sequence
const ICW1_INIT: u8 = 0x11;
const ICW1_EDGE_TRIGGERED: u8 = 0x01;  // Edge triggered
const ICW4_8086: u8 = 0x01;            // 8086/88 mode

/// IRQ0 vector offset (first timer interrupt)
const TIMER_IRQ_VECTOR: u8 = 32;

/// Ticks counter
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Get the current tick count
pub fn ticks() -> u64 {
    TICKS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Increment the tick count (called from timer interrupt handler)
fn increment_ticks() {
    TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// IDT entry structure (256 bits = 16 bytes each)
#[repr(C,packed)]
#[derive(Copy, Clone)]
struct IdtEntry {
    // First 64-bit word
    addr_low: u16,
    gdt_selector: u16,
    ist_index: u8,
    access_attr: u8,
    // Second 64-bit word
    addr_mid: u16,
    addr_high: u32,
    // Third 64-bit word (reserved in 64-bit mode)
    reserved: u32,
    // Fourth 64-bit word
    zero: u64,
}

impl IdtEntry {
    /// Create a null IDT entry
    const fn null() -> Self {
        IdtEntry {
            addr_low: 0,
            gdt_selector: 0,
            ist_index: 0,
            access_attr: 0,
            addr_mid: 0,
            addr_high: 0,
            reserved: 0,
            zero: 0,
        }
    }

    /// Create an interrupt gate entry
    /// ist_index: 0-7 selects IST entry, use 0xFF for no IST
    const fn interrupt_gate(handler_addr: u64, selector: u16, ist_index: u8, dpl: u8) -> Self {
        IdtEntry {
            addr_low: (handler_addr & 0xFFFF) as u16,
            gdt_selector: selector,
            ist_index,
            access_attr: (0x8E | (dpl & 0x60)), // Present, DPL, 64-bit interrupt gate
            addr_mid: ((handler_addr >> 16) & 0xFFFF) as u16,
            addr_high: ((handler_addr >> 32) & 0xFFFFFFFF) as u32,
            reserved: 0,
            zero: 0,
        }
    }
}

/// The IDT - declared mutable for initialization
static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::null(); IDT_ENTRIES];

/// IDT pointer structure
#[repr(C,packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

// ============================================================================
// I/O Functions
// ============================================================================

/// I/O port read
#[inline]
fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    val
}

/// I/O port write
#[inline]
fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Send End of Interrupt (EOI) to PIC
fn send_eoi(interrupt_id: u8) {
    outb(PIC_COMMAND, 0x20);
    if interrupt_id >= 8 {
        outb(PIC2_COMMAND, 0x20);
    }
}

// ============================================================================
// Interrupt Frame and Handler Macros
// ============================================================================

/// Structure to hold interrupt frame on the stack for x86-interrupt ABI
#[repr(C)]
pub struct InterruptFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
}

// ============================================================================
// Exception Handlers
// ============================================================================

/// Breakpoint exception handler (#BP, vector 3)
/// Logs RIP and returns to continue execution
#[unsafe(naked)]
pub extern "x86-interrupt" fn breakpoint_handler(frame: InterruptFrame) {
    core::arch::naked_asm!(
        r#"
            push rbp
            mov rbp, rsp
            call breakpoint_handler_fn
            pop rbp
            iretq
        "#
    );
}

#[unsafe(no_mangle)]
extern "C" fn breakpoint_handler_fn(_frame: InterruptFrame) {
    crate::serial_println!("#BP handler called");
}

/// Double Fault handler (#DF, vector 8)
/// Uses IST[0] stack - must set IST index in IDT entry
#[unsafe(naked)]
pub extern "x86-interrupt" fn double_fault_handler(frame: InterruptFrame) {
    core::arch::naked_asm!(
        r#"
            push rbp
            mov rbp, rsp
            call double_fault_handler_fn
            pop rbp
            iretq
        "#
    );
}

#[unsafe(no_mangle)]
extern "C" fn double_fault_handler_fn(_frame: InterruptFrame) {
    crate::serial_println!("#DF: RIP=0x{:016x}", _frame.rip);
    halt_system();
}

/// General Protection Fault handler (#GP, vector 13)
#[unsafe(naked)]
pub extern "x86-interrupt" fn gp_handler(frame: InterruptFrame) {
    core::arch::naked_asm!(
        r#"
            push rbp
            mov rbp, rsp
            call gp_handler_fn
            pop rbp
            iretq
        "#
    );
}

#[unsafe(no_mangle)]
extern "C" fn gp_handler_fn(_frame: InterruptFrame) {
    crate::serial_println!("#GP: RIP=0x{:016x}", _frame.rip);
    halt_system();
}

/// Page Fault handler (#PF, vector 14)
#[unsafe(naked)]
pub extern "x86-interrupt" fn page_fault_handler(frame: InterruptFrame) {
    core::arch::naked_asm!(
        r#"
            push rbp
            mov rbp, rsp
            push rbx
            push r12
            push r13
            push r14
            push r15
            call page_fault_handler_fn
            pop r15
            pop r14
            pop r13
            pop r12
            pop rbx
            pop rbp
            iretq
        "#
    );
}

#[unsafe(no_mangle)]
extern "C" fn page_fault_handler_fn(frame: InterruptFrame) {
    let cr2: u64;
    unsafe {
        core::arch::asm!("mov {0}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
    }
    crate::serial_println!("#PF: CR2=0x{:016x} RIP=0x{:016x}", cr2, frame.rip);
    halt_system();
}

/// Timer interrupt handler (IRQ 0, vector 32)
/// Increments tick counter and sends EOI
#[unsafe(naked)]
pub extern "x86-interrupt" fn timer_handler(_frame: InterruptFrame) {
    core::arch::naked_asm!(
        r#"
            push rbp
            mov rbp, rsp
            call timer_handler_fn
            pop rbp
            mov al, 0x20
            out 0x20, al
            iretq
        "#
    );
}

#[unsafe(no_mangle)]
extern "C" fn timer_handler_fn() {
    increment_ticks();
}

/// Halting loop for unrecoverable errors
pub fn halt() -> ! {
    crate::serial_println!("System halted.");
    unsafe {
        loop {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Halt from assembly - infinite halt loop
/// Uses naked function to create minimal halt loop
#[unsafe(naked)]
fn halt_system() -> ! {
    core::arch::naked_asm!(
        r#"
            hlt
            jmp halt_system
        "#
    );
}

// ============================================================================
// PIC Setup
// ============================================================================

/// Remap the PIC to vectors 32-47
fn remap_pic() {
    // Save current masks
    let mask1 = inb(PIC_DATA);
    let mask2 = inb(PIC2_DATA);

    // Start initialization
    outb(PIC_COMMAND, ICW1_INIT | ICW1_EDGE_TRIGGERED);
    outb(PIC2_COMMAND, ICW1_INIT | ICW1_EDGE_TRIGGERED);

    // ICW2: Vector offset (32 for master, 40 for slave)
    outb(PIC_DATA, TIMER_IRQ_VECTOR);        // Master: start at 32
    outb(PIC2_DATA, TIMER_IRQ_VECTOR + 8);   // Slave: start at 40

    // ICW3: Tell master PIC about slave at IRQ2 (0x04), slave at IRQ2 (0x02)
    outb(PIC_DATA, 0x04);
    outb(PIC2_DATA, 0x02);

    // ICW4: 8086 mode
    outb(PIC_DATA, ICW4_8086);
    outb(PIC2_DATA, ICW4_8086);

    // Restore masks (unmask IRQ0, mask others)
    // mask1 & 0xFE = unmask IRQ0
    outb(PIC_DATA, mask1 & 0xFE);
    outb(PIC2_DATA, mask2);

    // Send clear commands
    outb(PIC_COMMAND, 0x20);
    outb(PIC2_COMMAND, 0x20);
}

// ============================================================================
// PIT Setup
// ============================================================================

/// PIT ports
const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL0: u16 = 0x40;

/// Initialize PIT channel 0 for ~100 Hz
fn init_pit() {
    // 1193180 Hz / 100 = 11931.875
    // Using 11932 gives ~99.99 Hz, close enough
    let divisor: u16 = 11932;

    // Channel 0, LSB and MSB, mode 3 (square wave)
    outb(PIT_COMMAND, 0x30);
    outb(PIT_CHANNEL0, (divisor & 0xFF) as u8);  // LSB
    outb(PIT_CHANNEL0, (divisor >> 8) as u8);    // MSB
}

// ============================================================================
// IDT Setup
// ============================================================================

/// Initialize the IDT with all required handlers
pub fn init() {
    unsafe {
        let idt_base = &raw const IDT as u64;

        // Clear all entries
        for i in 0..IDT_ENTRIES {
            IDT[i] = IdtEntry::null();
        }

        // Set up exception handlers
        // #BP (vector 3) - no IST (ist=0xFF means "no IST"), DPL=0
        IDT[3] = IdtEntry::interrupt_gate(breakpoint_handler as *const () as u64, 0x08, 0xFF, 0);

        // #DF (vector 8) - IST[0], DPL=0 (ist_index=0 uses IST[0] on TSS)
        IDT[8] = IdtEntry::interrupt_gate(double_fault_handler as *const () as u64, 0x08, 0, 0);

        // #GP (vector 13) - no IST (ist=0xFF means "no IST"), DPL=0
        IDT[13] = IdtEntry::interrupt_gate(gp_handler as *const () as u64, 0x08, 0xFF, 0);

        // #PF (vector 14) - no IST (ist=0xFF means "no IST"), DPL=0
        IDT[14] = IdtEntry::interrupt_gate(page_fault_handler as *const () as u64, 0x08, 0xFF, 0);

        // IRQ0 (timer) - vector 32, no IST (ist=0xFF means "no IST"), DPL=0
        IDT[32] = IdtEntry::interrupt_gate(timer_handler as *const () as u64, 0x08, 0xFF, 0);

        // Load IDT
        let idt_size = core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() as u16;
        let idt_ptr = IdtPointer {
            limit: idt_size - 1,
            base: idt_base,
        };

        core::arch::asm!(
            "lidt [{}]",
            in(reg) &idt_ptr,
            options(nomem, nostack, preserves_flags),
        );

        core::arch::asm!(
            "sti",
            options(nomem, nostack),
        );
    }

    // Remap PIC and initialize PIT
    remap_pic();
    init_pit();
}