//! I/O APIC Driver
//!
//! Implements MMIO access to I/O APIC registers and redirection table.
//! Uses IOREGSEL (offset 0x00) and IOWIN (offset 0x10) for register access.
//! Provides IRQ routing via the MADT's Interrupt Source Overrides.

use crate::serial;
use crate::pmm;

/// I/O APIC MMIO register offsets (byte offsets)
const IOREGSEL: u64 = 0x00;  // Register Select
const IOWIN: u64 = 0x10;     // Register Window (32-bit read/write)

/// I/O APIC register selections
const IOAPIC_ID: u32 = 0x00;
const IOAPIC_VERSION: u32 = 0x01;
const IOAPIC_ARB: u32 = 0x02;
const IOAPIC_RTE_BASE: u32 = 0x10;  // Redirection Table starts at 0x10

/// Read a 32-bit value from I/O APIC MMIO register
#[inline]
fn ioapic_read(lapic_base: u64, reg_sel: u32) -> u32 {
    unsafe {
        let hhdm = pmm::hhdm_offset();
        let ioapic_base = (lapic_base + hhdm) as *mut u8;
        // Write register select (32-bit value to byte offset 0x00)
        let regsel_ptr = ioapic_base.add(IOREGSEL as usize) as *mut u32;
        core::ptr::write_volatile(regsel_ptr, reg_sel);
        // Read register window (32-bit value at byte offset 0x10)
        let iowin_ptr = ioapic_base.add(IOWIN as usize) as *const u32;
        core::ptr::read_volatile(iowin_ptr)
    }
}

/// Write a 32-bit value to I/O APIC MMIO register
#[inline]
fn ioapic_write(lapic_base: u64, reg_sel: u32, value: u32) {
    unsafe {
        let hhdm = pmm::hhdm_offset();
        let ioapic_base = (lapic_base + hhdm) as *mut u8;
        // Write register select (32-bit value to byte offset 0x00)
        let regsel_ptr = ioapic_base.add(IOREGSEL as usize) as *mut u32;
        core::ptr::write_volatile(regsel_ptr, reg_sel);
        // Write register window (32-bit value at byte offset 0x10)
        let iowin_ptr = ioapic_base.add(IOWIN as usize) as *mut u32;
        core::ptr::write_volatile(iowin_ptr, value);
    }
}

/// Get I/O APIC entries from MADT
pub fn get_ioapic_entries() -> &'static [u32] {
    crate::acpi::ioapiics()
}

use crate::acpi::IsoEntry;

/// Get Interrupt Source Override entries from MADT
pub fn get_interrupt_overrides() -> &'static [IsoEntry] {
    crate::acpi::interrupt_overrides()
}

/// Initialize I/O APIC and log basic info
pub fn init() {
    let ioapic_entries = get_ioapic_entries();
    let ioapic_base = match ioapic_entries.first().copied() {
        Some(base) => base as u64,
        None => {
            serial::write_str_safe("[IOAPIC] No I/O APIC found in MADT\r\n");
            return;
        }
    };

    // Read ID register (not volatile - just for identification)
    let id_reg = ioapic_read(ioapic_base, IOAPIC_ID);
    let ioapic_id = ((id_reg >> 24) & 0xFF) as u8;

    // Read version register
    let version_reg = ioapic_read(ioapic_base, IOAPIC_VERSION);
    let version = (version_reg & 0xFF) as u8;
    let max_redir_entries = ((version_reg >> 16) & 0xFF) as u8;

    serial::write_str_safe("[IOAPIC] Initialized ID=");
    write_u8_hex(ioapic_id);
    serial::write_str_safe(" ver=0x");
    write_u8_hex(version);
    serial::write_str_safe(" max_redir=");
    write_u8_hex(max_redir_entries);
    serial::write_str_safe("\r\n");

    // Mask all redirection entries during initialization
    for i in 0..max_redir_entries as u32 {
        // Each entry is 2 32-bit registers: 0x10 + i*2 and 0x10 + i*2 + 1
        let vector = 0x10 + i * 2;
        // Set bit 16 (mask bit) to mask the interrupt
        // Delivery mode: Fixed, Vector: 0xFF (masked)
        ioapic_write(ioapic_base, vector, 0x0000_00FF);
        ioapic_write(ioapic_base, vector + 1, 0x0000_0000 | (1 << 16)); // Masked
    }
}

/// Route an ISA IRQ to a specific vector through the I/O APIC
///
/// # Arguments
/// * `isa_irq` - The ISA IRQ number (0-15)
/// * `vector` - The interrupt vector to deliver (typically 32+)
/// * `dest_apic_id` - The target Local APIC ID (0 for Bootstrap Processor)
///
/// This function looks up the GSI (Global System Interrupt) from the MADT
/// Interrupt Source Overrides and programs the corresponding Redirection Table Entry.
pub fn route_isa_irq(isa_irq: u8, vector: u8, dest_apic_id: u8) {
    let ioapic_entries = get_ioapic_entries();
    let ioapic_base = match ioapic_entries.first().copied() {
        Some(base) => base as u64,
        None => {
            serial::write_str_safe("[IOAPIC] route_isa_irq: No I/O APIC base\r\n");
            return;
        }
    };

    // Look up GSI from Interrupt Source Overrides
    let gsi = lookup_gsi_from_iso(isa_irq);

    // I/O APIC redirection table maps GSI 0-23 to IRQ lines 0-23
    // Each entry is 2 registers at 0x10 + gsi*2 and 0x11 + gsi*2
    let entry_low = 0x10 + (gsi as u32) * 2;
    let entry_high = entry_low + 1;

    // Construct Redirection Table Entry:
    // Low bits (vector, delivery mode, mask)
    // - Bits 0-7: Vector number
    // - Bit 16: Mask (0 = unmasked)
    // - Bits 10-11: Delivery mode (00 = Fixed)
    // - Bits 13-15: Reserved/Level/Polarity
    let low = (vector as u32) | (1 << 10);  // Vector + Fixed delivery mode

    // High bits (destination APIC ID, delivery mode, trigger, polarity)
    // - Bits 0-7: Destination APIC ID
    // - Bits 8-15: Reserved
    // - Bits 13-14: Trigger mode (00 = Edge)
    // - Bit 15: Polarity (0 = Active high)
    let high = (dest_apic_id as u32) << 24;

    ioapic_write(ioapic_base, entry_low, low);
    ioapic_write(ioapic_base, entry_high, high);
}

/// Look up the GSI (Global System Interrupt) for an ISA IRQ
/// using the MADT Interrupt Source Override table
fn lookup_gsi_from_iso(isa_irq: u8) -> u32 {
    let isos = get_interrupt_overrides();

    for iso in isos {
        if iso.bus == 0 && iso.source_irq == isa_irq {
            return iso.gsi;
        }
    }

    // Default: ISA IRQ maps to same-numbered GSI (IRQ0 = GSI0, etc.)
    isa_irq as u32
}

/// Write u8 as 2-digit hex
fn write_u8_hex(val: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    serial::write_byte(HEX[(val >> 4) as usize]);
    serial::write_byte(HEX[(val & 0xF) as usize]);
}

/// Get I/O APIC count from MADT
pub fn ioapic_count() -> u8 {
    crate::acpi::ioapic_count()
}