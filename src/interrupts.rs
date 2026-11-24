//! Interrupt Descriptor Table (IDT), PIC remapping, PIT timer setup
//!
//! This module sets up:
//! - 256-entry 64-bit IDT
//! - Exception handlers for #BP, #DF, #GP, #PF
//! - 8259 PIC remapping to vectors 32-47
//! - 8259 PIT channel 0 at 100 Hz
//! - Local APIC initialization with ExtINT pass-through for PIC IRQ0

use core::sync::atomic::AtomicU64;

// ============================================================================
// HHDM Offset Support for MMIO Access
// ============================================================================

/// HHDM offset for converting physical addresses to virtual addresses
static mut HHDM_OFFSET: u64 = 0;

/// Set the HHDM offset (called once during initialization)
pub fn set_hhdm_offset(offset: u64) {
    unsafe { HHDM_OFFSET = offset; }
}

// ============================================================================
// Local APIC Constants and Map
// ============================================================================

/// Local APIC base physical address (standard on x86-64)
const LOCAL_APIC_BASE_PHYS: u64 = 0xFEE00000;

/// Local APIC register offsets (from base address, byte offsets)
const APIC_LVR: u64 = 0x30;           // Local Version Register (offset 0x30)
const APIC_SVR: u64 = 0xF0;            // Spurious Interrupt Vector Register
const APIC_LVT_LINT0: u64 = 0x350;     // LVT LINT0 Register (offset 0x350)
const APIC_LVT_LINT1: u64 = 0x310;    // LVT LINT1 Register


/// Local APIC Spurious Interrupt Vector bits:
/// - Bits 0-7: Vector number
/// - Bit 8: APIC Software Enable (AEE) / Spurious Interrupt Enable (SVE)
const APIC_SVR_ENABLE: u32 = 0x00000100;  // Bit 8: Enable APIC
const APIC_SVR_VECTOR: u32 = 0x60;         // Vector 96 for spurious interrupts


/// LVT LINT0 for PIC IRQ0 passthrough (ExtINT mode)
/// LVT LINT0 register layout at offset 0x350:
/// - Bits 0-7: Vector (32 for timer/IRQ0)
/// - Bit 8: Mask (0 = unmasked)
/// - Bits 9-11: Delivery mode (111 = ExtINT)
/// - Value 0x00000700: Vector 0 unmasked, ExtINT delivery mode
const APIC_LINT0_EXTINT: u32 = 0x0000_0700;  // ExtINT delivery mode for PIC IRQ0 passthrough

/// Hex digit lookup table for debug output
const HEX: &[u8; 16] = b"0123456789ABCDEF";

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

/// IDT entry structure - strictly 16 bytes (128 bits) per x86-64 specification
#[repr(C,packed)]
#[derive(Copy, Clone)]
struct IdtEntry {
    // First 32 bits
    addr_low: u16,
    gdt_selector: u16,
    // Next 8 bits
    ist_index: u8,
    access_attr: u8,
    // Next 32 bits
    addr_mid: u16,
    addr_high: u32,
    // Next 32 bits
    reserved: u32,
}

// Verify IDT entry is exactly 16 bytes
const _: () = assert!(core::mem::size_of::<IdtEntry>() == 16);

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
        }
    }

    /// Create an interrupt gate entry (64-bit)
    /// ist_index: 0-7 selects IST entry
    ///
    /// 64-bit Interrupt Gate layout (little-endian):
    /// - Byte 4: IST[0-2] in bits 0-2 (type is in byte 5)
    /// - Byte 5: P|0xDPL|S|Type = 0x80|0x00|0x10|0x0E = 0x9E (for DPL=0)
    const fn interrupt_gate(handler_addr: u64, selector: u16, ist_index: u8, dpl: u8) -> Self {
        // Byte 4: IST in bits 0-2, zero elsewhere
        let byte4 = ist_index & 0x07;
        // Byte 5: P=1, DPL, S=0, Type=0xE (1110 = 64-bit interrupt gate)
        // 0x80 (Present) | 0x00 (S=0 for system descriptor) | 0x0E (Type) | (dpl << 5)
        let byte5 = 0x80 | 0x00 | 0x0E | (dpl << 5);

        IdtEntry {
            addr_low: (handler_addr & 0xFFFF) as u16,
            gdt_selector: selector,
            ist_index: byte4,
            access_attr: byte5,
            addr_mid: ((handler_addr >> 16) & 0xFFFF) as u16,
            addr_high: ((handler_addr >> 32) & 0xFFFFFFFF) as u32,
            reserved: 0,
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
// Local APIC Access Functions (MMIO)
// ============================================================================

/// Access Local APIC registers via MMIO using HHDM offset
/// Local APIC is at physical address 0xFEE00000
/// Uses byte offsets for correct MMIO access
#[inline]
fn apic_read(offset: u64) -> u32 {
    unsafe {
        let hhdm = HHDM_OFFSET;
        let lapic_virt = ((LOCAL_APIC_BASE_PHYS + hhdm) as *const u8).add(offset as usize);
        core::ptr::read_volatile(lapic_virt as *const u32)
    }
}

/// Write Local APIC register via MMIO using HHDM offset
/// Uses byte offsets for correct MMIO access
#[inline]
fn apic_write(offset: u64, value: u32) {
    unsafe {
        let hhdm = HHDM_OFFSET;
        let lapic_virt = ((LOCAL_APIC_BASE_PHYS + hhdm) as *mut u8).add(offset as usize);
        core::ptr::write_volatile(lapic_virt as *mut u32, value);
    }
}

// ============================================================================
// Interrupt Stack Frame
// ============================================================================

/// Structure to hold interrupt frame on the stack for x86-interrupt ABI
#[repr(C)]
pub struct InterruptStackFrame {
    pub instruction_pointer: u64,
    pub code_segment: u64,
    pub cpu_flags: u64,
    pub stack_pointer: u64,
    pub stack_segment: u64,
}

// ============================================================================
// Exception Handlers
// ============================================================================

/// Breakpoint exception handler (#BP, vector 3)
/// No IST - uses current stack
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial::write_str_safe("[INT] #BP handler called\r\n");
    // Print RIP for debugging
    let mut rip = stack_frame.instruction_pointer;
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let nibble = (rip & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        rip >>= 4;
    }
    crate::serial::write_str_safe("RIP=0x");
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe("\r\n");
}

/// IST index for double fault handler (vector 8)
/// Uses IST[1] per user specification
const DOUBLE_FAULT_IST_INDEX: u8 = 1;

/// Double Fault handler (#DF, vector 8)
/// Uses IST[1] stack - must set IST index in IDT entry
/// x86-64 #DF pushes error code (value 0xE) onto IST stack
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn double_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) -> ! {
    // Print error code for debugging
    let mut ec = error_code;
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let nibble = (ec & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        ec >>= 4;
    }
    crate::serial::write_str_safe("#DF error_code=0x");
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe("\r\n");

    // Print RIP for debugging
    let mut rip = stack_frame.instruction_pointer;
    for i in (0..16).rev() {
        let nibble = (rip & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        rip >>= 4;
    }
    crate::serial::write_str_safe("RIP=0x");
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe("\r\n");
    halt_system();
}

/// General Protection Fault handler (#GP, vector 13)
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn gp_handler(stack_frame: InterruptStackFrame, _error_code: u64) {
    crate::serial::write_str_safe("#GP: RIP=0x\r\n");
    let mut rip = stack_frame.instruction_pointer;
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let nibble = (rip & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        rip >>= 4;
    }
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe("\r\n");
}

/// Page Fault handler (#PF, vector 14)
/// Prints CR2 (faulting address) and error code
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn page_fault_handler(stack_frame: InterruptStackFrame, _error_code: u64) {
    let cr2: u64;
    unsafe {
        core::arch::asm!(
            "mov {0}, cr2",
            out(reg) cr2,
            options(nomem, nostack, preserves_flags),
        );
    }

    crate::serial::write_str_safe("#PF: CR2=0x\r\n");
    let mut cr2_val = cr2;
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let nibble = (cr2_val & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        cr2_val >>= 4;
    }
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe(" RIP=0x\r\n");

    let mut rip = stack_frame.instruction_pointer;
    for i in (0..16).rev() {
        let nibble = (rip & 0xF) as u8;
        buf[i] = if nibble < 10 { b'0' + nibble } else { b'A' + (nibble - 10) };
        rip >>= 4;
    }
    crate::serial::write_str_safe(core::str::from_utf8(&buf).unwrap());
    crate::serial::write_str_safe("\r\n");

    halt_system();
}

/// Timer interrupt handler (IRQ 0, vector 32)
/// Increments tick counter and sends EOI - NO allocations
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    increment_ticks();
    send_eoi(0); // Send EOI to master PIC (0x20 port)
}

/// Local APIC Timer interrupt handler (vector 240 / 0xF0)
/// Called by LAPIC timer configured via init_lapic_timer()
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn lapic_timer_handler(_stack_frame: InterruptStackFrame) {
    // Write EOVI (End of Interrupt to Local Vector Table)
    // No EOI needed for timer - interrupt is level-triggered automatically
    increment_ticks();
}

/// Default handler for unexpected interrupts
/// Catches all unused IDT entries to prevent triple faults from spurious interrupts
#[unsafe(no_mangle)]
pub extern "x86-interrupt" fn default_handler(_stack_frame: InterruptStackFrame) {
    // Intentionally empty - just returns (does nothing)
}

// ============================================================================
// Halt Functions
// ============================================================================

/// Halting loop for unrecoverable errors
pub fn halt() -> ! {
    crate::serial::write_str_safe("System halted.\r\n");
    unsafe {
        loop {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Halt from assembly - infinite halt loop
pub fn halt_system() -> ! {
    loop {
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// ============================================================================
// PIC Setup
// ============================================================================

/// Remap the PIC to vectors 32-47
fn remap_pic() {
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

    // Mask all IRQs EXCEPT IRQ0 (timer)
    // Master PIC mask: bit 0 = IRQ0 (unmasked: 0), bits 1-7 = IRQ1-7 (masked: 1)
    // 0xFE = 11111110b - IRQ0 unmasked (0), IRQ1-7 masked (1)
    outb(PIC_DATA, 0xFE);
    // Slave PIC mask: all IRQs masked (IRQ8-15)
    outb(PIC2_DATA, 0xFF);

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

    // Channel 0, LSB and MSB, mode 3 (square wave), rate generator
    // 0x36 = Bits 6-7:01 (Channel 0), Bits 4-5:10 (LSB MSB), Bits 0-3:0110 (Mode 3)
    outb(PIT_COMMAND, 0x36);
    outb(PIT_CHANNEL0, (divisor & 0xFF) as u8);  // LSB
    outb(PIT_CHANNEL0, (divisor >> 8) as u8);    // MSB

    crate::serial::write_str_safe("[PIT] Initialized for 100Hz (divisor 11932, mode 3)\r\n");
}

// ============================================================================
// IDT Setup
// ============================================================================

/// Initialize the IDT with all required handlers
pub fn init() {
    unsafe {
        let idt_base = &raw const IDT as u64;

        crate::serial::write_str_safe("[IDT] Initializing IDT...\r\n");

        // Clear all entries first
        for i in 0..IDT_ENTRIES {
            IDT[i] = IdtEntry::null();
        }

        // Set up all 256 IDT entries with default handler first
        // This prevents triple faults from spurious interrupts
        // IMPORTANT: ist_index must be 0 for normal stack use (0xFF would mask to 0x07 = IST[7]!)
        for i in 0..IDT_ENTRIES {
            IDT[i] = IdtEntry::interrupt_gate(default_handler as *const () as u64, 0x08, 0x00, 0);
        }

        // Set up exception handlers
        // #BP (vector 3) - no IST (ist=0), DPL=0
        IDT[3] = IdtEntry::interrupt_gate(breakpoint_handler as *const () as u64, 0x08, 0x00, 0);

        // #DF (vector 8) - IST[1], DPL=0
        // IST index 1 selects TSS.ist[1]
        IDT[8] = IdtEntry::interrupt_gate(double_fault_handler as *const () as u64, 0x08, DOUBLE_FAULT_IST_INDEX, 0);

        // #GP (vector 13) - no IST (ist=0), DPL=0
        IDT[13] = IdtEntry::interrupt_gate(gp_handler as *const () as u64, 0x08, 0x00, 0);

        // #PF (vector 14) - no IST (ist=0), DPL=0
        IDT[14] = IdtEntry::interrupt_gate(page_fault_handler as *const () as u64, 0x08, 0x00, 0);

        // IRQ0 (timer) - no IST (ist=0), DPL=0
        IDT[32] = IdtEntry::interrupt_gate(timer_handler as *const () as u64, 0x08, 0x00, 0);

        // LAPIC Timer (vector 240 / 0xF0) - no IST, DPL=0
        IDT[240] = IdtEntry::interrupt_gate(lapic_timer_handler as *const () as u64, 0x08, 0x00, 0);

        // Load IDT
        let idt_size = core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() as u16;
        let idt_ptr = IdtPointer {
            limit: idt_size - 1,
            base: idt_base,
        };

        crate::serial::write_str_safe("[IDT] Loading IDT with lidt...\r\n");
        core::arch::asm!(
            "lidt [{}]",
            in(reg) &idt_ptr,
            options(nomem, nostack, preserves_flags),
        );
        crate::serial::write_str_safe("[IDT] IDT loaded successfully\r\n");

        // Remap PIC and initialize PIT FIRST (before enabling interrupts)
        // This unmasks IRQ0 and sets up the timer
        remap_pic();
        init_pit();

        // Enable interrupts AFTER PIC is set up
        // STI enables interrupts - timer interrupts will now fire via IRQ0
        // Local APIC init will be called from main.rs after PMM init
        crate::serial::write_str_safe("[IDT] Enabling interrupts (STI)...\r\n");
        core::arch::asm!(
            "sti",
            options(nomem, nostack),
        );
        crate::serial::write_str_safe("[IDT] Interrupts enabled\r\n");
    }
}

/// Initialize Local APIC with HHDM offset already set
/// Call this from main.rs after PMM is initialized
/// - Enable the APIC via SVR register
/// - Configure LVT LINT0 for PIC IRQ0 passthrough (ExtINT mode)
/// - Mask LINT1 (we don't use it)
pub fn init_local_apic_with_hhdm() {
    // Verify HHDM offset is set before proceeding
    unsafe {
        if HHDM_OFFSET == 0 {
            crate::serial::write_str_safe("[APIC] ERROR: HHDM offset not set - cannot access MMIO\r\n");
            return;
        }
    }

    // Map Local APIC MMIO region (0xFEE00000, 4KB) before any register access
    // This ensures the virtual address 0xFFFF8000FEE00000 is mapped with proper flags
    let lapic_phys = LOCAL_APIC_BASE_PHYS;
    if crate::paging::map_mmio_region(lapic_phys, 0x1000).is_none() {
        crate::serial::write_str_safe("[APIC] ERROR: Failed to map Local APIC MMIO region\r\n");
        return;
    }
    crate::serial::write_str_safe("[APIC] Local APIC MMIO mapped\r\n");

    // Map I/O APIC MMIO region (0xFEC00000, 4KB) for I/O APIC routing
    let ioapic_phys = 0xFEC00000;
    if crate::paging::map_mmio_region(ioapic_phys, 0x1000).is_none() {
        crate::serial::write_str_safe("[APIC] ERROR: Failed to map I/O APIC MMIO region\r\n");
        return;
    }
    crate::serial::write_str_safe("[APIC] I/O APIC MMIO mapped\r\n");

    // First, verify APIC is present by reading version register at byte offset 0x30
    let apic_version = apic_read(APIC_LVR);
    let version = (apic_version & 0xFF) as u8;

    if version == 0 {
        crate::serial::write_str_safe("[APIC] No Local APIC detected (version=0)\r\n");
        return;
    }

    // Log version in format [APIC] Local APIC version=0x...
    crate::serial::write_str_safe("[APIC] Local APIC version=0x");
    crate::serial::write_byte(HEX[(version >> 4) as usize]);
    crate::serial::write_byte(HEX[(version & 0xF) as usize]);
    crate::serial::write_str_safe("\r\n");

    // Mask LINT1 (not used)
    // Delivery mode: Fixed, Vector: 0xFF (masked)
    apic_write(APIC_LVT_LINT1, (0xFF) | (1 << 7));  // Masked, vector 0xFF

    // Configure LINT0 for PIC IRQ0 passthrough (ExtINT mode)
    // This allows the PIC to forward IRQ0 to the Local APIC
    // Vector 32 is used for timer interrupts (same as PIC IRQ0)
    apic_write(APIC_LVT_LINT0, APIC_LINT0_EXTINT);

    // Enable the Local APIC by setting the SVR register
    // Vector bits 0-7 select which vector to use for spurious interrupts
    // Bit 8 (APIC_SVR_ENABLE) enables the APIC
    apic_write(APIC_SVR, APIC_SVR_ENABLE | APIC_SVR_VECTOR);

    crate::serial::write_str_safe("[APIC] Initialized with ExtINT pass-through for IRQ0\r\n");
}

// ============================================================================
// Local APIC Timer
// ============================================================================

/// Local APIC Timer Register offsets (byte offsets from LAPIC base)
/// LVTT = Local Vector Table Timer at offset 0x320
/// TMICT = Timer Initial Count register at offset 0x380
/// DCR = Division Configuration Register at offset 0x3E0
const APIC_LVTT: u64 = 0x320;     // Local Vector Table Timer
const APIC_TMICT: u64 = 0x380;    // Timer Initial Count (write-only)
const APIC_DCR: u64 = 0x3E0;      // Division Configuration Register

/// Local APIC Timer flags (LVT Timer register)
/// Bits 17-16: Timer Mode (00=Fixed, 01=Periodic, 10=One-shot, 11=Express)
/// Bit 16: Interrupt Mask (0=unmasked, 1=mocked)
/// Bits 7-0: Vector number
const APIC_TMR_PERIODIC: u32 = 0 << 17;  // Periodic mode
const APIC_LVTT_MASKED: u32 = 1 << 16;   // Mask bit

/// Local APIC Timer divisors (DCR register)
/// Bits 3-0: Divide-by value
const APIC_TCR_DIV_1: u32 = 0;
const APIC_TCR_DIV_2: u32 = 1;
const APIC_TCR_DIV_4: u32 = 2;
const APIC_TCR_DIV_8: u32 = 3;
const APIC_TCR_DIV_16: u32 = 0x8;
const APIC_TCR_DIV_32: u32 = 0x9;
const APIC_TCR_DIV_64: u32 = 0xA;
const APIC_TCR_DIV_128: u32 = 0xB;

/// Initialize the Local APIC timer for periodic interrupts
///
/// # Arguments
/// * `hz` - Desired interrupt frequency in Hz (e.g., 100 for 100 Hz)
///
/// The LVT Timer is programmed for periodic mode at the specified frequency.
/// The Local APIC timer runs at the processor's crystal clock (typically 1-3 GHz).
pub fn init_lapic_timer(hz: u32) {
    unsafe {
        if HHDM_OFFSET == 0 {
            crate::serial::write_str_safe("[APIC-TIMER] ERROR: HHDM offset not set\r\n");
            return;
        }
    }

    // Use a default LAPIC frequency (typically CPU crystal clock)
    // For calibration, we'd compare against PIT ticks
    // Here we use a simple estimate: 100 MHz as a reasonable default
    let lapic_freq_mhz: u64 = 100;  // 100 MHz APIC timer clock

    // Calculate timer initial count for periodic mode
    // Timer counts down from initial_value to 0, then generates interrupt
    // Formula: initial_value = freq / hz
    let timer_freq = lapic_freq_mhz * 1_000_000;  // Convert to Hz
    let initial_count: u32 = if hz > 0 {
        (timer_freq / hz as u64) as u32
    } else {
        1_000_000  // Default to 100 Hz if invalid input
    };

    // Program LVT Timer for periodic mode, unmasked, vector 0xF0 (240)
    // Vector 240 (0xF0) is reserved for our timer ISR
    // LVT Timer: bits 7-0 = vector, bit 17 = periodic mode (0), bit 16 = masked (0)
    let lvt_timer_value = APIC_TMR_PERIODIC | (240 << 0);

    // Set division ratio (use divide-by-1 for simplicity)
    apic_write(APIC_DCR, APIC_TCR_DIV_1);

    // Write initial count to TMICT (write-only, starts the timer)
    apic_write(APIC_TMICT, initial_count);

    // Unmask the timer in LVT register
    apic_write(APIC_LVTT, lvt_timer_value & !APIC_LVTT_MASKED);

    // Log initialization details
    crate::serial::write_str_safe("[APIC-TIMER] Initialized at ");
    if hz >= 1000 {
        crate::serial::write_byte(b'0' + ((hz / 1000) % 10) as u8);
        crate::serial::write_byte(b'k');
    } else {
        // Print 3-digit Hz value (or less with leading zeros)
        let hundreds = (hz / 100) as u8;
        let tens = ((hz / 10) % 10) as u8;
        let ones = (hz % 10) as u8;
        crate::serial::write_byte(b'0' + (hundreds >> 2)); // First digit
        crate::serial::write_byte(b'0' + tens);            // Second digit
        crate::serial::write_byte(b'0' + ones);            // Third digit
    }
    crate::serial::write_str_safe(" Hz, initial_count=");
    write_u32_hex(initial_count);
    crate::serial::write_str_safe("\r\n");
}

/// Helper to write u32 as hex
fn write_u32_hex(mut val: u32) {
    if val == 0 {
        crate::serial::write_byte(b'0');
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 8];
    let mut idx = 0;
    while val > 0 {
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
        idx += 1;
    }
    for i in (0..idx).rev() {
        crate::serial::write_byte(buf[i]);
    }
}