//! Global Descriptor Table (GDT) and Task State Segment (TSS) for x86-64
//!
//! This module sets up the GDT with:
//! - Null descriptor (0x00)
//! - Kernel code segment (0x08)
//! - Kernel data segment (0x10)
//! - User data segment (0x18)
//! - User code segment (0x20)
//! - 16-byte TSS descriptor (0x28)
//!
use core::arch::asm;

/// IST index for double fault handler
pub const DOUBLE_FAULT_IST_INDEX: u8 = 0;

/// Stack size for IST stack (16 KiB)
const IST_STACK_SIZE: usize = 16 * 1024;

/// GDT entries - each is 8 bytes
#[repr(C,packed)]
#[derive(Copy, Clone)]
struct GdtEntry {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    flags: u8,
    base_high: u8,
}

impl GdtEntry {
    /// Create a null descriptor (required, must be first)
    const fn null() -> Self {
        Self {
            limit_low: 0,
            base_low: 0,
            base_mid: 0,
            access: 0,
            flags: 0,
            base_high: 0,
        }
    }

    /// Create a 64-bit code segment descriptor
    const fn code_segment(base: u64, limit: u32, access: u8, flags: u8) -> Self {
        GdtEntry {
            limit_low: (limit & 0xFFFF) as u16,
            base_low: (base & 0xFFFF) as u16,
            base_mid: ((base >> 16) & 0xFF) as u8,
            access,
            flags,
            base_high: ((base >> 24) & 0xFF) as u8,
        }
    }

    /// Create a 64-bit data segment descriptor
    const fn data_segment(base: u64, limit: u32, access: u8, flags: u8) -> Self {
        GdtEntry {
            limit_low: (limit & 0xFFFF) as u16,
            base_low: (base & 0xFFFF) as u16,
            base_mid: ((base >> 16) & 0xFF) as u8,
            access,
            flags,
            base_high: ((base >> 24) & 0xFF) as u8,
        }
    }
}

/// The GDT itself with proper padding for TSS
#[repr(C,packed)]
struct Gdt {
    null: GdtEntry,           // 0x00 - Null descriptor
    kernel_code: GdtEntry,    // 0x08 - Kernel code segment
    kernel_data: GdtEntry,    // 0x10 - Kernel data segment
    user_data: GdtEntry,      // 0x18 - User data segment
    user_code: GdtEntry,      // 0x20 - User code segment
    tss: GdtEntry,            // 0x28 - TSS descriptor low part
    tss_high: GdtEntry,       // 0x30 - TSS descriptor high part (64-bit base)
    // Additional padding entries (not used but ensure proper GDT size)
    _padding: [GdtEntry; 8],
}

/// Global GDT - declared mutable for initialization
static mut GDT: Gdt = Gdt {
    null: GdtEntry::null(),
    kernel_code: GdtEntry::null(),
    kernel_data: GdtEntry::null(),
    user_data: GdtEntry::null(),
    user_code: GdtEntry::null(),
    tss: GdtEntry::null(),
    tss_high: GdtEntry::null(),
    _padding: [GdtEntry::null(); 8],
};

/// Task State Segment structure (64-bit)
/// Must be 16-byte aligned for x86-64 TSS
#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct Tss {
    reserved0: u16,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: [u16; 12],
    ist: [u64; 8],    // Interrupt Stack Table - IST[0] through IST[7]
    reserved2: [u16; 128],
    tss_limit: u16,
    reserved3: u64,
}

/// TSS instance - aligned to 16 bytes
static mut TSS_DATA: Tss = Tss {
    reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved1: [0; 12],
    ist: [0; 8],
    reserved2: [0; 128],
    tss_limit: 0,
    reserved3: 0,
};

/// GDT pointer structure for lgdt instruction
#[repr(C,packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// IST stack for double fault handler (16 KiB)
static mut IST_STACK: [u8; IST_STACK_SIZE] = [0u8; IST_STACK_SIZE];

/// Initialize the GDT with all required segments and TSS
pub fn init() {
    unsafe {
        // Set up IST stack pointer for double fault handler
        // Stack grows downward, so we point to the top of the IST stack
        let ist_stack_top = (&raw const IST_STACK as *const u8).add(IST_STACK_SIZE) as u64;

        // Set up TSS with IST stack pointer
        let tss_ptr = &raw const TSS_DATA as u64;
        TSS_DATA.ist[DOUBLE_FAULT_IST_INDEX as usize] = ist_stack_top;
        TSS_DATA.rsp0 = ist_stack_top;

        // Initialize GDT entries
        GDT.null = GdtEntry::null();

        // Kernel code segment: 0x08
        // Access: Present (1), DPL=0 (0), Code segment (1), Read (1) = 0x9B
        // Flags: 64-bit (1), 4KB granularity (1) = 0xAF
        GDT.kernel_code = GdtEntry::code_segment(
            0,           // base
            0xFFFFF,     // limit (4GB with 4KB granularity)
            0x9B,        // access: present, DPL=0, code segment, exec/read
            0xAF,        // flags: 64-bit, 4KB granularity
        );

        // Kernel data segment: 0x10
        GDT.kernel_data = GdtEntry::data_segment(
            0,
            0xFFFFF,
            0x93,        // access: present, DPL=0, data segment, read/write
            0xCF,        // flags: 4KB granularity
        );

        // User data segment: 0x18
        GDT.user_data = GdtEntry::data_segment(
            0,
            0xFFFFF,
            0x9B,        // access: present, DPL=3, data segment, read/write
            0xCF,
        );

        // User code segment: 0x20
        GDT.user_code = GdtEntry::code_segment(
            0,
            0xFFFFF,
            0x9B,        // access: present, DPL=3, code segment, exec/read
            0xAF,
        );

        // TSS descriptor: 0x28
        // Type = 0x9 (TSS 64-bit, available), Present = 1
        GDT.tss = GdtEntry {
            limit_low: 0,
            base_low: (tss_ptr & 0xFFFF) as u16,
            base_mid: ((tss_ptr >> 16) & 0xFF) as u8,
            access: 0x83,    // Present, DPL=0, TSS available
            flags: 0,
            base_high: ((tss_ptr >> 24) & 0xFF) as u8,
        };
        GDT.tss_high = GdtEntry {
            limit_low: 0,
            base_low: ((tss_ptr >> 32) & 0xFFFF) as u16,
            base_mid: ((tss_ptr >> 48) & 0xFF) as u8,
            access: 0,
            flags: 0,
            base_high: 0,
        };

        // Padding entries
        for i in 0..8 {
            GDT._padding[i] = GdtEntry::null();
        }

        // Load GDT
        let gdt_size = core::mem::size_of::<Gdt>() as u16;
        let gdt_ptr = GdtPointer {
            limit: gdt_size,
            base: &raw const GDT as u64,
        };

        // Load GDT
        asm!(
            "lgdt [{}]",
            in(reg) &gdt_ptr,
            options(nomem, nostack, preserves_flags),
        );

        // Reload CS via far jump using local labels
        let _tmp: u64;
        asm!(
            "push 0x08",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            tmp = lateout(reg) _,
            options(nomem, nostack),
        );

        // Reload data segments
        asm!(
            "mov ax, 0x10\n   mov ds, ax\n   mov es, ax\n   mov fs, ax\n   mov gs, ax\n   mov ss, ax",
            options(nomem, nostack),
        );

        // Load TSS - ltr takes a selector
        // We embed the selector value directly in the instruction
        asm!(
            "push {sel}",
            "pop ax",
            "ltr ax",
            sel = in(reg) 0x28u16,
            options(nomem, nostack),
        );
    }
}