$code = @"
//! x86-64 Global Descriptor Table (GDT) and Task State Segment (TSS)
//! Phase 1C: GDT, TSS, and segment loading

use core::arch::asm;

// 16 KiB emergency stack for Double Fault IST
static mut DOUBLE_FAULT_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

/// TSS (Task State Segment) for 64-bit mode
#[repr(C, packed)]
pub struct TaskStateSegment {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

impl TaskStateSegment {
    pub const fn new() -> Self {
        TaskStateSegment {
            reserved0: 0,
            rsp: [0, 0, 0],
            reserved1: 0,
            ist: [0, 0, 0, 0, 0, 0, 0],
            reserved2: 0,
            reserved3: 0,
            iomap_base: core::mem::size_of::<TaskStateSegment>() as u16,
        }
    }
}

/// System segment descriptor (for TSS and LDT)
#[repr(C, packed)]
struct SystemDescriptor {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    limit_high_flags: u8,
    base_high: u32,
    reserved: u32,
}

impl SystemDescriptor {
    pub const fn new(base: u64, limit: u32, access: u8) -> Self {
        SystemDescriptor {
            limit_low: limit as u16,
            base_low: base as u16,
            base_mid: (base >> 16) as u8,
            access,
            limit_high_flags: ((limit >> 16) & 0xF) as u8,
            base_high: (base >> 32) as u32,
            reserved: 0,
        }
    }
}

/// GDT entry (8 bytes for code/data)
#[repr(C, align(8))]
pub struct GlobalDescriptorTable {
    null: u64,
    kernel_code: u64,
    kernel_data: u64,
    user_data: u64,
    user_code: u64,
    tss: SystemDescriptor,
}

impl GlobalDescriptorTable {
    pub const fn new() -> Self {
        let kernel_code: u64 = 0x00AF9A000000FFFF;
        let kernel_data: u64 = 0x00CF92000000FFFF;
        let user_data: u64 = 0x00AFF2000000FFFF;
        let user_code: u64 = 0x00AFFA000000FFFF;
        
        GlobalDescriptorTable {
            null: 0,
            kernel_code,
            kernel_data,
            user_data,
            user_code,
            tss: SystemDescriptor::new(0, 0, 0),
        }
    }
}

static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();
static mut TSS: TaskStateSegment = TaskStateSegment::new();

#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u64,
}

const KERNEL_CODE_SELECTOR: u16 = 0x08;
const KERNEL_DATA_SELECTOR: u16 = 0x10;
const USER_DATA_SELECTOR: u16 = 0x18;
const USER_CODE_SELECTOR: u16 = 0x20;
const TSS_SELECTOR: u16 = 0x28;

const DOUBLE_FAULT_IST_INDEX: usize = 0;

pub fn init() {
    unsafe {
        let stack_top = DOUBLE_FAULT_STACK.as_ptr() as u64 + core::mem::size_of::<[u8; 16 * 1024]>() as u64;
        TSS.ist[DOUBLE_FAULT_IST_INDEX] = stack_top;
        TSS.iomap_base = core::mem::size_of::<TaskStateSegment>() as u16;
        
        let tss_base = &TSS as *const _ as u64;
        let tss_limit = core::mem::size_of::<TaskStateSegment>() as u32 - 1;
        GDT.tss = SystemDescriptor::new(tss_base, tss_limit, 0x89);
        
        let gdt_desc = GdtDescriptor {
            limit: (core::mem::size_of::<GlobalDescriptorTable>() - 1) as u16,
            base: &GDT as *const _ as u64,
        };
        
        asm!("lgdt [{}]", in(reg) &gdt_desc, options(nostack, preserves_flags));
        
        asm!(
            "push {selector}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            selector = in(reg) KERNEL_CODE_SELECTOR,
            options(nostack)
        );
        
        asm!(
            "mov ds, {sel}",
            "mov es, {sel}",
            "mov fs, {sel}",
            "mov gs, {sel}",
            "mov ss, {sel}",
            sel = in(reg) KERNEL_DATA_SELECTOR,
            options(nostack, preserves_flags)
        );
        
        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
    }
}
"@
Set-Content -Path "src\gdt.rs" -Value $code -Encoding UTF8
Remove-Item "make_gdt.ps1"
