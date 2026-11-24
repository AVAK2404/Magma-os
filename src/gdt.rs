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

/// IST index for double fault handler (vector 8)
pub const DOUBLE_FAULT_IST_INDEX: u8 = 1;

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
/// Must be exactly 104 bytes per Intel SDM Volume 3 Figure 5-5
/// Valid region: 104 bytes (0x00-0x67)
/// Note: This struct is packed (no padding), size is strictly 104 bytes
#[repr(C, packed)]
#[derive(Copy, Clone)]
struct Tss {
    // 64-bit TSS layout (offsets per Intel SDM Volume 3 Figure 5-5):
    // 0x00-0x07: RSP0 (for privilege level 0)
    // 0x08-0x0F: RSP1 (for privilege level 1)
    // 0x10-0x17: RSP2 (for privilege level 2)
    // 0x18-0x57: IST[0-7] (8 entries of 8 bytes each = 64 bytes total)
    // 0x58-0x5F: IO map base address (iomap_base at 0x58, rest of u16 unused)
    // 0x60-0x67: Reserved (part of valid region)
    // Total: 104 bytes (0x68)
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    ist: [u64; 8],      // Interrupt Stack Table - IST[0] through IST[7]
    iomap_base: u16,   // Offset 0x58 - I/O permission map base address (0x68 = fully permissive)
    _reserved: [u8; 6], // Offset 0x5A-0x5F: reserved for IOPB
    _reserved2: u64,    // Offset 0x60-0x67: 8 bytes reserved
}

// Compile-time assertions for Tss layout
const _: () = assert!(core::mem::size_of::<Tss>() == 104, "Tss must be exactly 104 bytes");
// Note: align_of::<Tss>() is 1 due to packed, but we wrap it in AlignedTss for proper alignment

/// Wrapper to ensure TSS is 16-byte aligned in memory
/// The inner Tss remains packed with size 104 bytes
#[repr(C, align(16))]
struct AlignedTss(Tss);

/// TSS instance - wrapped for 16-byte alignment, but inner Tss is packed
static mut TSS_DATA: AlignedTss = AlignedTss(Tss {
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    ist: [0; 8],
    iomap_base: 0x68, // IOPB offset = 0x68 (past end of valid region = fully permissive)
    _reserved: [0; 6],
    _reserved2: 0,
});

/// GDT pointer structure for lgdt instruction
#[repr(C,packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// IST stack for double fault handler (16 KiB)
static mut IST_STACK: [u8; IST_STACK_SIZE] = [0u8; IST_STACK_SIZE];

/// Initialize the GDT with all required segments and TSS
#[allow(static_mut_refs)]
pub fn init() {
    unsafe {
        // Set up IST stack pointer for double fault handler
        // Stack grows downward, so we point to the top of the IST stack
        let ist_stack_top = (&raw const IST_STACK as *const u8).add(IST_STACK_SIZE) as u64;

        // Set up TSS with IST stack pointer for double fault
        // Access inner Tss via &raw mut TSS_DATA.0 for safe writes to packed fields
        let tss_ref = &raw mut TSS_DATA.0 as *mut Tss;
        (*tss_ref).ist[DOUBLE_FAULT_IST_INDEX as usize] = ist_stack_top;
        (*tss_ref).rsp0 = ist_stack_top;

        // Trace: About to initialize GDT entries
        crate::serial::write_str_safe("[GDT] Initializing GDT entries...\r\n");

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
        // Access: Present (1), DPL=3 (11), Data segment (1), Read/Write (1) = 0xF3
        GDT.user_data = GdtEntry::data_segment(
            0,
            0xFFFFF,
            0xF3,        // access: present, DPL=3, data segment, read/write
            0xCF,        // flags: 4KB granularity
        );

        // User code segment: 0x20
        // Access: Present (1), DPL=3 (11), Code segment (1), Exec/Read (1) = 0xFB
        GDT.user_code = GdtEntry::code_segment(
            0,
            0xFFFFF,
            0xFB,        // access: present, DPL=3, code segment, exec/read
            0xAF,        // flags: 64-bit, 4KB granularity
        );

        // TSS descriptor: 0x28
        // Type = 0x9 (TSS 64-bit, available), Present = 1
        // Access byte = 0x89: P=1, DPL=0, S=1, Type=1001 (64-bit TSS available)
        // Limit for 64-bit TSS: valid region is bytes 0x00-0x67 per Intel SDM Figure 5-5
        // Tss struct is 104 bytes (0x68), so limit should be 0x67 (last valid byte)
        // Using limit 0x67 ensures the IOPL field at offset 0x67 is accessible
        const TSS_LIMIT: u16 = 0x67;
        let tss_ptr = &raw const TSS_DATA.0 as u64;
        GDT.tss = GdtEntry {
            limit_low: TSS_LIMIT,
            base_low: (tss_ptr & 0xFFFF) as u16,
            base_mid: ((tss_ptr >> 16) & 0xFF) as u8,
            access: 0x89,    // Present, DPL=0, 64-bit TSS available
            flags: 0,
            base_high: ((tss_ptr >> 24) & 0xFF) as u8,
        };
        // Second half of 64-bit TSS descriptor base
        GDT.tss_high = GdtEntry {
            limit_low: 0,
            base_low: ((tss_ptr >> 32) & 0xFFFF) as u16,
            base_mid: ((tss_ptr >> 48) & 0xFF) as u8,
            access: 0,
            flags: 0,
            base_high: ((tss_ptr >> 56) & 0xFF) as u8,
        };

        // Padding entries
        for i in 0..8 {
            GDT._padding[i] = GdtEntry::null();
        }

        // Load GDT
        // GDT limit is size - 1 (standard practice for segment limits)
        let gdt_size = core::mem::size_of::<Gdt>() as u16 - 1;
        let gdt_ptr = GdtPointer {
            limit: gdt_size,
            base: &raw const GDT as u64,
        };

        // Trace: About to load GDT
        crate::serial::write_str_safe("[GDT] Before lgdt...\r\n");

        // Reload data segments first (we're still in the current code segment)
        asm!(
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            options(nomem, nostack),
        );

        // Load GDT now with data segments already set up
        asm!(
            "lgdt [{}]",
            in(reg) &gdt_ptr,
            options(nomem, nostack, preserves_flags),
        );

        // Trace: GDT loaded successfully
        crate::serial::write_str_safe("[GDT] After lgdt...\r\n");

        // Reload CS via far return (retfq) to reload with kernel code segment (0x08)
        // This assembly code does a far return, setting CS to 0x08
        // After retfq, execution continues at label 2: (the return address target)
        // Using Intel syntax as specified by the user
        asm!(
            "push 0x08",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            tmp = lateout(reg) _,
            options(nomem, nostack),
        );

        // Trace: Segment reload complete (after far return in new CS)
        crate::serial::write_str_safe("[GDT] After CS reload...\r\n");

        // Reset data segments again since we're now in new CS
        asm!(
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            options(nomem, nostack),
        );

        // Load TSS - ltr takes a 16-bit selector
        // Use "ltr {sel:x}" inline asm for clean load
        let sel: u16 = 0x28;
        core::arch::asm!(
            "ltr {sel:x}",
            sel = in(reg) sel,
            options(nomem, nostack, preserves_flags),
        );

        // Trace: TSS loaded
        crate::serial::write_str_safe("[GDT] After ltr (TSS loaded)...\r\n");
    }
}