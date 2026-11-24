//! PCI Bus Enumeration - Configuration Mechanism #1 (0xCF8/0xCFC)
//!
//! Implements PCI configuration space access via I/O ports 0xCF8 (address) and 0xCFC (data).
//! Supports scanning all buses (0..=255), devices (0..32), and functions (0..8).
//! Decodes BAR0..BAR5 to determine I/O, 32-bit MMIO, and 64-bit MMIO regions.
//!
//! BAR size probing: temporarily disables Memory/I/O decoding in the PCI Command register
//! to safely determine BAR sizes, then restores both BAR and Command values.

#![allow(dead_code)]

use crate::serial;
use core::sync::atomic::{AtomicBool, Ordering};

// PCI Configuration Mechanism #1 I/O ports
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

// PCI Command register offset and bits
const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_IO: u16 = 1 << 0;  // I/O space enable
const PCI_COMMAND_MEMORY: u16 = 1 << 1;  // Memory space enable

// PCI Header Type register offset
const PCI_HEADER_TYPE: u8 = 0x0E;

// Base Address Registers offsets (BAR0-BAR5)
const PCI_BARS: [u8; 6] = [0x10, 0x14, 0x18, 0x1C, 0x20, 0x24];

// Maximum number of PCI devices we'll track
const MAX_PCI_DEVICES: usize = 64;

/// Representation of a PCI BAR (Base Address Register)
#[derive(Clone, Copy, Debug)]
pub enum BarType {
    /// I/O space BAR (16-bit address)
    Io { base: u32, size: u32 },
    /// 32-bit MMIO BAR
    Mmio32 { base: u32, size: u32 },
    /// 64-bit MMIO BAR (spans two BAR registers)
    Mmio64 { base: u64, size: u64 },
    /// Unused/unimplemented BAR
    Unused,
}

/// PCI device information extracted from configuration space
#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    /// Bus number (0-255)
    pub bus: u8,
    /// Device number (0-31)
    pub device: u8,
    /// Function number (0-7)
    pub function: u8,
    /// Vendor ID (16-bit)
    pub vendor_id: u16,
    /// Device ID (16-bit)
    pub device_id: u16,
    /// Class code (24-bit: base class << 16 | subclass << 8 | prog_if)
    pub class_code: u32,
    /// Revision ID
    pub revision_id: u8,
    /// Header type (determines if multifunction)
    pub header_type: u8,
    /// Interrupt line (IRQ)
    pub interrupt_line: u8,
    /// Interrupt pin (1=INTA, 2=INTB, 3=INTC, 4=INTD, 0=reserved)
    pub interrupt_pin: u8,
    /// Decoded BAR information
    pub bars: [Option<BarType>; 6],
}

/// Static storage for discovered PCI devices
static mut PCI_DEVICES: [Option<PciDevice>; MAX_PCI_DEVICES] = [None; MAX_PCI_DEVICES];
static mut PCI_COUNT: usize = 0;

/// Lock for PCI device table access (using atomic for simplicity)
static PCI_LOCK: AtomicBool = AtomicBool::new(false);

/// Spin until we acquire the lock
fn acquire_lock() {
    while PCI_LOCK.compare_exchange(
        false,
        true,
        Ordering::Acquire,
        Ordering::Relaxed,
    ).is_err() {
        core::hint::spin_loop();
    }
}

/// Release the lock
fn release_lock() {
    PCI_LOCK.store(false, Ordering::Release);
}

/// Low-level PCI configuration read
/// Combines bus, device, function, and offset into the address
#[inline]
fn pci_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // Validate parameters
    if device > 31 || function > 7 || (offset as usize) > 255 {
        return 0;
    }

    // Build address:
    // Bit 31: Enable bit
    // Bits 30-24: Reserved (0)
    // Bits 23-16: Bus number
    // Bits 22-11: Device number
    // Bits 10-8: Function number
    // Bits 7-0: Register offset

    unsafe {
        let address: u32 = (1u32 << 31)  // Enable bit
            | ((bus as u32) << 16)
            | ((device as u32) << 11)
            | ((function as u32) << 8)
            | (offset as u32);

        // Write address to CF8
        core::arch::asm!(
            "out dx, eax",
            in("dx") PCI_CONFIG_ADDRESS,
            in("eax") address,
            options(nomem, nostack, preserves_flags),
        );

        // Read data from CFC
        let data: u32;
        core::arch::asm!(
            "in eax, dx",
            out("eax") data,
            in("dx") PCI_CONFIG_DATA,
            options(nomem, nostack, preserves_flags),
        );
        data
    }
}

/// Low-level PCI configuration write
#[inline]
fn pci_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    if device > 31 || function > 7 || (offset as usize) > 255 {
        return;
    }

    let address: u32 = (1u32 << 31)  // Enable bit
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | (offset as u32);

    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") PCI_CONFIG_ADDRESS,
            in("eax") address,
            options(nomem, nostack, preserves_flags),
        );

        core::arch::asm!(
            "out dx, eax",
            in("dx") PCI_CONFIG_DATA,
            in("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read 16-bit from PCI configuration space
/// Reads 32-bit aligned dword at (offset & !3), shifts right by ((offset & 3) * 8)
#[inline]
fn pci_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let dword_addr = offset & !3;
    let shift = (offset & 3) * 8;
    (pci_read32(bus, device, function, dword_addr) >> shift) as u16
}

/// Read 8-bit from PCI configuration space
/// Reads 32-bit aligned dword at (offset & !3), shifts right by ((offset & 3) * 8)
#[inline]
fn pci_read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let dword_addr = offset & !3;
    let shift = (offset & 3) * 8;
    (pci_read32(bus, device, function, dword_addr) >> shift) as u8
}

/// Write 16-bit to PCI configuration space
#[inline]
fn pci_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    // Read-modify-write for 16-bit: read 32-bit, mask, write back
    let dword_addr = offset & !2;  // Align to 32-bit boundary
    let shift = (offset & 2) * 8;  // Shift amount within dword

    let current = pci_read32(bus, device, function, dword_addr);
    let mask = !(0xFFFF << shift);
    let new_value = (current & mask) | ((value as u32) << shift);
    pci_write32(bus, device, function, dword_addr, new_value);
}

/// Determine BAR type and size by probing
/// Returns (base_address, size, is_64bit_bar) when successful
fn probe_bar(bus: u8, device: u8, function: u8, bar_index: usize) -> Option<BarType> {
    let bar_offset = PCI_BARS[bar_index];

    // Read current BAR value
    let bar_value = pci_read32(bus, device, function, bar_offset);

    // Check if BAR is unused (all 1s means no device implemented this BAR)
    if bar_value == 0xFFFFFFFF {
        return None;
    }

    // Check BAR type from bit 0
    if (bar_value & 0x01) != 0 {
        // I/O space BAR (bit 0 = 1)
        let io_base = bar_value & 0xFFFFFFFC;
        let io_size = pci_io_size(bus, device, function, bar_offset);
        if io_size > 0 {
            return Some(BarType::Io {
                base: io_base,
                size: io_size,
            });
        }
        return None;
    }

    // Memory BAR (bit 0 = 0)
    let mem_type = (bar_value >> 1) & 0x03;

    match mem_type {
        0 => {
            // 32-bit memory BAR
            let size = pci_mmio32_size(bus, device, function, bar_offset);
            Some(BarType::Mmio32 {
                base: bar_value & 0xFFFFFFF0,
                size,
            })
        }
        2 => {
            // 64-bit memory BAR (uses bar_index and bar_index+1)
            let base_low = bar_value & 0xFFFFFFF0;

            // Read high 32 bits from next BAR
            let bar_high = pci_read32(bus, device, function, PCI_BARS[bar_index + 1]);

            let size = pci_mmio64_size(bus, device, function, bar_offset);
            Some(BarType::Mmio64 {
                base: (bar_high as u64) << 32 | (base_low as u64),
                size,
            })
        }
        _ => None,  // Reserved type
    }
}

/// Get I/O BAR size by writing 0xFFFFFFFF and reading back
fn pci_io_size(bus: u8, device: u8, function: u8, bar_offset: u8) -> u32 {
    // Read current Command register
    let cmd_current = pci_read16(bus, device, function, PCI_COMMAND);

    // Save current BAR value
    let bar_current = pci_read32(bus, device, function, bar_offset);

    // Disable memory and I/O decoding in Command register
    let cmd_new = cmd_current & !PCI_COMMAND_MEMORY & !PCI_COMMAND_IO;
    pci_write16(bus, device, function, PCI_COMMAND, cmd_new);

    // Write 0xFFFFFFFF to BAR to probe size
    pci_write32(bus, device, function, bar_offset, 0xFFFFFFFF);

    // Read back - the size is the complement of the mask bits
    let size_mask = pci_read32(bus, device, function, bar_offset);

    // Write back original BAR value
    pci_write32(bus, device, function, bar_offset, bar_current);

    // Restore Command register
    pci_write16(bus, device, function, PCI_COMMAND, cmd_current);

    // Size is the bitwise complement of the mask plus 1
    let size = !size_mask | 0x3;
    if size == 0xFFFFFFFF {
        0
    } else {
        size + 1
    }
}

/// Get 32-bit MMIO BAR size by writing 0xFFFFFFFF and reading back
fn pci_mmio32_size(bus: u8, device: u8, function: u8, bar_offset: u8) -> u32 {
    // Read current Command register
    let cmd_current = pci_read16(bus, device, function, PCI_COMMAND);

    let bar_current = pci_read32(bus, device, function, bar_offset);

    // Disable memory and I/O decoding in Command register
    let cmd_new = cmd_current & !PCI_COMMAND_MEMORY & !PCI_COMMAND_IO;
    pci_write16(bus, device, function, PCI_COMMAND, cmd_new);

    // Write 0xFFFFFFFF to BAR to probe size
    pci_write32(bus, device, function, bar_offset, 0xFFFFFFFF);

    // Read back - the size is the complement of the mask bits
    let size_mask = pci_read32(bus, device, function, bar_offset);

    // Write back original BAR value
    pci_write32(bus, device, function, bar_offset, bar_current);

    // Restore Command register
    pci_write16(bus, device, function, PCI_COMMAND, cmd_current);

    // Size is the bitwise complement of the mask plus 1
    let size = !(size_mask & 0xFFFFFFF0) | 0xF;
    if size == 0xFFFFFFFF {
        0
    } else {
        size + 1
    }
}

/// Get 64-bit MMIO BAR size by writing 0xFFFFFFFFFFFFFFFF and reading back
fn pci_mmio64_size(bus: u8, device: u8, function: u8, bar_offset: u8) -> u64 {
    let bar_current_low = pci_read32(bus, device, function, bar_offset);

    // Get the index and calculate high BAR offset
    let bar_idx = bar_index_from_offset(bar_offset);
    let bar_high_offset = PCI_BARS[bar_idx + 1];

    // Read high part before probing
    let bar_high_current = pci_read32(bus, device, function, bar_high_offset);

    // Write 0xFFFFFFFF to both low and high parts
    pci_write32(bus, device, function, bar_offset, 0xFFFFFFFF);
    pci_write32(bus, device, function, bar_high_offset, 0xFFFFFFFF);

    // Read back
    let size_mask_low = pci_read32(bus, device, function, bar_offset);
    let size_mask_high = pci_read32(bus, device, function, bar_high_offset);

    // Write back original values
    pci_write32(bus, device, function, bar_offset, bar_current_low);
    pci_write32(bus, device, function, bar_high_offset, bar_high_current);

    let size_mask = (size_mask_high as u64) << 32 | (size_mask_low as u64);
    let size = !(size_mask & 0xFFFFFFF0);
    if size == 0xFFFFFFFFFFFF {
        0
    } else {
        size + 1
    }
}

/// Helper to get BAR index from offset
fn bar_index_from_offset(offset: u8) -> usize {
    ((offset - PCI_BARS[0]) as usize) / 4
}

/// Check if a device has valid vendor/device ID (non-zero, non-0xFFFF)
fn is_valid_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id != 0 && vendor_id != 0xFFFF && device_id != 0 && device_id != 0xFFFF
}

/// Scan one PCI device/function
/// header_type_base is the header type with multifunction bit masked off (0x7F)
fn scan_device(bus: u8, device: u8, function: u8, header_type_base: u8) -> Option<PciDevice> {
    // Read vendor and device ID (always present in config space 0)
    let vendor_id = pci_read16(bus, device, function, 0x00);
    let device_id = pci_read16(bus, device, function, 0x02);

    if !is_valid_device(vendor_id, device_id) {
        return None;
    }

    // Read other fields
    let class_code = pci_read32(bus, device, function, 0x08) >> 8;  // Class is at offset 0x08
    let revision_id = pci_read8(bus, device, function, 0x08);
    let header_type = pci_read8(bus, device, function, PCI_HEADER_TYPE);
    let interrupt_line = pci_read8(bus, device, function, 0x3C);
    let interrupt_pin = pci_read8(bus, device, function, 0x3D);

    // Probe BARs based on header type
    let mut bars: [Option<BarType>; 6] = [None, None, None, None, None, None];

    // Determine which BARs to probe based on header type
    // 0x00 = standard host bridge (BAR0-BAR5)
    // 0x01 = PCI-to-PCI bridge (BAR0-BAR1 only)
    let bar_range = if header_type_base == 0x01 {
        2  // Only BAR0 and BAR1 for PCI-to-PCI bridges
    } else {
        6  // BAR0-BAR5 for standard devices
    };

    // Probing BARs
    for i in 0..bar_range {
        // For 64-bit BARs, we skip the next index
        if i > 0 && bars[i - 1].is_some() {
            if let Some(BarType::Mmio64 { .. }) = bars[i - 1] {
                // This BAR index is used by the 64-bit BAR's upper 32 bits
                continue;
            }
        }

        let bar_type = probe_bar_with_header(bus, device, function, i, header_type_base);
        bars[i] = bar_type;
    }

    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        class_code,
        revision_id,
        header_type,
        interrupt_line,
        interrupt_pin,
        bars,
    })
}

/// Check if this is a multifunction device (header type bit 7)
fn is_multifunction(header_type: u8) -> bool {
    (header_type & 0x80) != 0
}

/// Parse class code into base class, subclass, and prog_if
fn decode_class_code(class_code: u32) -> (u8, u8, u8) {
    let base_class = (class_code >> 16) as u8;
    let subclass = ((class_code >> 8) & 0xFF) as u8;
    let prog_if = (class_code & 0xFF) as u8;
    (base_class, subclass, prog_if)
}

/// Initialize PCI enumeration
/// Scans all buses, devices, and functions, storing discovered devices in static table
pub fn init() {
    acquire_lock();

    let mut device_count = 0;

    // Scan all buses 0..=255
    for bus in 0..=255u8 {
        // Scan all devices 0..=31
        for device in 0..=31u8 {
            // First check function 0
            // Read header_type for function 0 to determine BAR range
            let header_type_fn0 = pci_read8(bus, device, 0, PCI_HEADER_TYPE);

            if let Some(pci_dev) = scan_device(bus, device, 0, header_type_fn0 & 0x7F) {
                // Log the device
                let (class, subclass, prog_if) = decode_class_code(pci_dev.class_code);

                serial::write_str_safe("[PCI] Device: bus=");
                write_u8_hex(bus);
                serial::write_str_safe(" dev=");
                write_u8_hex(device);
                serial::write_str_safe(".0 vid=");
                write_u16_hex(pci_dev.vendor_id);
                serial::write_str_safe(":");
                write_u16_hex(pci_dev.device_id);
                serial::write_str_safe(" class=");
                write_u8_hex(class);
                serial::write_str_safe("/");
                write_u8_hex(subclass);
                serial::write_str_safe("/");
                write_u8_hex(prog_if);

                // Log active BARs
                for (i, bar) in pci_dev.bars.iter().enumerate() {
                    match bar {
                        Some(BarType::Io { base, size }) => {
                            serial::write_str_safe(" BAR");
                            write_u8_hex(i as u8);
                            serial::write_str_safe("=I/O 0x");
                            write_u32_hex(*base);
                            serial::write_str_safe("-0x");
                            // End address is base + size - 1 (inclusive range)
                            // Use checked_sub to handle size == 0 case
                            let end = size.checked_sub(1).unwrap_or(0);
                            write_u32_hex(base.wrapping_add(end));
                        }
                        Some(BarType::Mmio32 { base, size }) => {
                            serial::write_str_safe(" BAR");
                            write_u8_hex(i as u8);
                            serial::write_str_safe("=MMIO32 0x");
                            write_u32_hex(*base);
                            serial::write_str_safe("-0x");
                            // End address is base + size - 1 (inclusive range)
                            let end = size.checked_sub(1).unwrap_or(0);
                            write_u32_hex(base.wrapping_add(end));
                        }
                        Some(BarType::Mmio64 { base, size }) => {
                            serial::write_str_safe(" BAR");
                            write_u8_hex(i as u8);
                            serial::write_str_safe("=MMIO64 0x");
                            write_u64_hex(*base);
                            serial::write_str_safe("-0x");
                            // End address is base + size - 1 (inclusive range)
                            let end = size.checked_sub(1).unwrap_or(0);
                            write_u64_hex(base.wrapping_add(end));
                        }
                        Some(BarType::Unused) => {}
                        None => {}
                    }
                }
                serial::write_str_safe("\r\n");

                // Store device if capacity allows
                if device_count < MAX_PCI_DEVICES {
                    unsafe {
                        PCI_DEVICES[device_count] = Some(pci_dev);
                    }
                    device_count += 1;
                }
            }

            // Check if device is multifunction
            if !is_multifunction(header_type_fn0) {
                continue;  // Skip functions 1-7 for single-function devices
            }

            // Determine BAR probing based on header type (mask off multifunction bit)
            let header_type_base = header_type_fn0 & 0x7F;

            // Scan functions 1-7
            for function in 1..8u8 {
                if let Some(pci_dev) = scan_device(bus, device, function, header_type_base) {
                    let (class, subclass, prog_if) = decode_class_code(pci_dev.class_code);

                    serial::write_str_safe("[PCI] Device: bus=");
                    write_u8_hex(bus);
                    serial::write_str_safe(" dev=");
                    write_u8_hex(device);
                    serial::write_str_safe(".");
                    write_u8_hex(function);
                    serial::write_str_safe(" vid=");
                    write_u16_hex(pci_dev.vendor_id);
                    serial::write_str_safe(":");
                    write_u16_hex(pci_dev.device_id);
                    serial::write_str_safe(" class=");
                    write_u8_hex(class);
                    serial::write_str_safe("/");
                    write_u8_hex(subclass);
                    serial::write_str_safe("/");
                    write_u8_hex(prog_if);

                    for (_i, bar) in pci_dev.bars.iter().enumerate() {
                        if let Some(_) = bar {
                            // Log that we found a BAR but skip detailed output for brevity
                        }
                    }
                    serial::write_str_safe("\r\n");

                    if device_count < MAX_PCI_DEVICES {
                        unsafe {
                            PCI_DEVICES[device_count] = Some(pci_dev);
                        }
                        device_count += 1;
                    }
                }
            }
        }
    }

    unsafe {
        PCI_COUNT = device_count;
    }

    release_lock();
}

/// Get the number of discovered PCI devices
pub fn device_count() -> usize {
    unsafe { PCI_COUNT }
}

/// Get an iterator over discovered devices (simplified version)
pub fn devices() -> PciDeviceIterator {
    PciDeviceIterator {
        count: 0,
        total: unsafe { PCI_COUNT },
    }
}

/// Iterator over discovered PCI devices
pub struct PciDeviceIterator {
    count: usize,
    total: usize,
}

impl PciDeviceIterator {
    pub fn next(&mut self) -> Option<PciDevice> {
        if self.count >= self.total {
            return None;
        }
        unsafe {
            let dev = PCI_DEVICES[self.count];
            self.count += 1;
            dev
        }
    }
}

/// Find a PCI device by class and subclass
/// Returns the first matching device, or None if not found
pub fn find_by_class(class_code: u8, subclass: u8) -> Option<PciDevice> {
    acquire_lock();
    let result = find_by_class_inner(class_code, subclass);
    release_lock();
    result
}

/// Internal implementation of find_by_class (assumes lock held)
fn find_by_class_inner(class_code: u8, subclass: u8) -> Option<PciDevice> {
    unsafe {
        for i in 0..PCI_COUNT {
            let dev = PCI_DEVICES[i];
            if let Some(pci_dev) = dev {
                let (c, s, _) = decode_class_code(pci_dev.class_code);
                if c == class_code && s == subclass {
                    return Some(pci_dev);
                }
            }
        }
    }
    None
}

/// Probe a BAR with header-type-based protection
/// Determines which BARs to probe based on header_type_base:
/// - 0x00: Probe all 6 BARs (BAR0-BAR5) for standard devices
/// - 0x01: Probe only BAR0-BAR1 for PCI-to-PCI bridges
fn probe_bar_with_header(bus: u8, device: u8, function: u8, bar_index: usize, header_type_base: u8) -> Option<BarType> {
    // Protection: Only probe BARs based on header type
    // 0x00 = standard host bridge (BAR0-BAR5)
    // 0x01 = PCI-to-PCI bridge (BAR0-BAR1 only)
    let bar_range = if header_type_base == 0x01 {
        2  // Only BAR0 and BAR1 for PCI-to-PCI bridges
    } else {
        6  // BAR0-BAR5 for standard devices
    };

    if bar_index >= bar_range {
        return None;
    }

    probe_bar(bus, device, function, bar_index)
}

/// Helper to write u32 as hex
pub fn write_u32_hex(mut val: u32) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 8];
    let mut idx = 8;
    while val > 0 {
        idx -= 1;
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &buf[idx..] {
        serial::write_byte(b);
    }
}

/// Helper to write u64 as hex
pub fn write_u64_hex(mut val: u64) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 16];
    let mut idx = 16;
    while val > 0 {
        idx -= 1;
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &buf[idx..] {
        serial::write_byte(b);
    }
}

/// Helper to write u8 as 2-digit hex (always 2 digits, including leading zero for val < 16)
pub fn write_u8_hex(val: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    serial::write_byte(HEX[(val >> 4) as usize]);
    serial::write_byte(HEX[(val & 0xF) as usize]);
}

/// Helper to write u16 as hex
pub fn write_u16_hex(val: u16) {
    write_u8_hex((val >> 8) as u8);
    write_u8_hex(val as u8);
}