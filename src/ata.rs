//! Primary IDE Controller Driver (PIO Mode, LBA28)
//!
//! Implements a block driver for Primary IDE channel using PIO mode with LBA28 addressing.
//! Supports reading sectors from storage devices connected to the primary IDE channel.
//!
//! Uses standard x86 ATA Primary Channel I/O ports (base 0x1F0, control 0x3F6).

use crate::serial;

// Standard ATA Primary Channel I/O ports
const IDE_BASE: u16 = 0x1F0;

// I/O port offsets for primary IDE channel (from base 0x1F0)
const IDE_DATA_PORT: u16 = 0x00;          // Data register
const IDE_FEATURE_PORT: u16 = 0x01;       // Feature/Error register
const IDE_SECTOR_COUNT_PORT: u16 = 0x02;  // Sector count register
const IDE_LBA_LOW_PORT: u16 = 0x03;       // LBA low byte register
const IDE_LBA_MID_PORT: u16 = 0x04;       // LBA middle byte register
const IDE_LBA_HIGH_PORT: u16 = 0x05;      // LBA high byte register
const IDE_DRIVE_PORT: u16 = 0x06;         // Drive/HEAD register
const IDE_STATUS_PORT: u16 = 0x07;        // Status/Command register

// Command opcodes
const IDE_CMD_READ_SECTORS: u8 = 0x20;    // READ SECTORS (PIO)
const IDE_CMD_IDENTIFY: u8 = 0xEC;        // IDENTIFY DEVICE

// Status bits
const IDE_STATUS_BSY: u8 = 0x80;          // Busy
const IDE_STATUS_DRDY: u8 = 0x40;         // Drive ready
const IDE_STATUS_DRQ: u8 = 0x08;          // Data request

/// Drive information storage
#[derive(Clone)]
pub struct DriveInfo {
    pub model: [u8; 40],     // ATA model string (40 bytes, space-padded)
    pub serial: [u8; 20],    // Serial number (20 bytes)
    pub max_lba: u32,        // Maximum LBA for LBA28 (28-bit)
    pub lba0_signature: u16, // Boot signature at LBA 0 (0x55AA if bootable)
}

impl DriveInfo {
    const SIZE: usize = 40 + 20 + 4 + 2; // model + serial + max_lba + lba0_signature
}

/// Global storage for detected drive info
/// Using static mut with allow attribute for Rust 2024 - this is acceptable for kernel development
/// where we have full control over concurrency and no runtime memory safety guarantees.
#[allow(static_mut_refs)]
static mut DRIVE_INFO_DATA: DriveInfo = DriveInfo {
    model: [0; 40],
    serial: [0; 20],
    max_lba: 0,
    lba0_signature: 0,
};

#[allow(static_mut_refs)]
static mut DRIVE_INFO_PRESENT: bool = false;

/// Base I/O address of the primary IDE channel (always standard 0x1F0)
const IO_BASE: u16 = IDE_BASE;

/// Detects the primary IDE controller (standard ports at 0x1F0).
/// Returns Some(()) if the controller responds, None otherwise.
pub fn detect_ide_controller() -> Option<()> {
    // Standard IDE ports are always at 0x1F0 for primary channel
    let status = inb(IO_BASE + IDE_STATUS_PORT);
    if status != 0xFF && status != 0x00 {
        serial::write_str_safe("[ATA] Primary IDE controller found at I/O 0x1F0\r\n");
        Some(())
    } else {
        serial::write_str_safe("[ATA] No IDE controller found\r\n");
        None
    }
}

/// Wait for the IDE controller to become ready (not busy, drive ready)
/// Returns true if ready, false on timeout
fn wait_ready(base: u16) -> bool {
    let mut timeout: u32 = 0;
    const TIMEOUT_MAX: u32 = 1_000_000;

    loop {
        let status = inb(base + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) == 0 {
            if (status & IDE_STATUS_DRDY) != 0 {
                return true;
            }
        }
        timeout += 1;
        if timeout >= TIMEOUT_MAX {
            return false;
        }
        // Small delay - busy loop
    }
}

/// Wait for DRQ (Data Request) - drive has data to send
/// Returns true if DRQ set, false on timeout
fn wait_drq(base: u16) -> bool {
    let mut timeout: u32 = 0;
    const TIMEOUT_MAX: u32 = 1_000_000;

    loop {
        let status = inb(base + IDE_STATUS_PORT);
        if status & IDE_STATUS_DRQ != 0 {
            return true;
        }
        if status & IDE_STATUS_BSY != 0 {
            return false;  // Device became busy - error
        }
        timeout += 1;
        if timeout >= TIMEOUT_MAX {
            return false;
        }
    }
}

/// Read a single byte from an I/O port
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

/// Write a single byte to an I/O port
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

/// Read 16-bit value from I/O port
#[inline]
fn inw(port: u16) -> u16 {
    let val: u16;
    unsafe {
        core::arch::asm!(
            "in ax, dx",
            out("ax") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    val
}

/// Write u16 as 2-digit hex
fn write_u16_hex(mut val: u16) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 4];
    let mut idx = 3;
    while val > 0 {
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
        idx -= 1;
    }
    for &b in &buf[idx..=3] {
        serial::write_byte(b);
    }
}

/// Extract model string from IDENTIFY data (words 10-19 for model)
fn extract_model(id_data: &[u16; 256]) -> [u8; 40] {
    let mut model = [0u8; 40];
    for i in 0..20 {
        let word = id_data[10 + i];
        model[i * 2] = (word & 0xFF) as u8;
        model[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
    }
    model
}

/// Extract serial number from IDENTIFY data
/// Per ATA spec: Serial number is in words 20-29 (20 bytes)
fn extract_serial(id_data: &[u16; 256]) -> [u8; 20] {
    let mut serial = [0u8; 20];
    for i in 0..10 {
        let word = id_data[20 + i];  // Serial number is at words 20-29 (20 bytes)
        serial[i * 2] = (word & 0xFF) as u8;
        serial[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
    }
    serial
}

/// Get maximum LBA from IDENTIFY data
fn extract_max_lba(id_data: &[u16; 256]) -> u32 {
    // LBA48 max sectors are in words 100-103 (64-bit)
    let lba48_lo = id_data[100] as u32;
    let lba48_hi = id_data[101] as u32;
    let max_lba_48 = (lba48_hi << 16) | lba48_lo;

    // Return LBA48 limit if supported, otherwise LBA28
    if max_lba_48 > 0 && max_lba_48 != 0xFFFFFFFF {
        max_lba_48
    } else {
        // Fall back to LBA28 max
        let lba28_lo = id_data[60] as u32;
        let lba28_hi = (id_data[61] as u32) & 0xF;  // Only low 4 bits for LBA28
        lba28_hi << 16 | lba28_lo
    }
}

/// Identify the drive attached to the IDE channel
/// Returns DriveInfo if successful
#[allow(static_mut_refs)]
fn identify_drive(base: u16) -> Option<DriveInfo> {
    let mut id_data: [u16; 256] = [0; 256];

    // Wait for ready
    if !wait_ready(base) {
        return None;
    }

    // Select master drive (bit 6 = 0 for master, bit 7 = 1)
    outb(base + IDE_DRIVE_PORT, 0xA0);

    // Wait for ready again
    if !wait_ready(base) {
        return None;
    }

    // Issue IDENTIFY command
    outb(base + IDE_STATUS_PORT, IDE_CMD_IDENTIFY);

    // Wait for DRQ
    if !wait_drq(base) {
        return None;
    }

    // Read 256 words (512 bytes) of IDENTIFY data
    for i in 0..256 {
        id_data[i] = inw(base + IDE_DATA_PORT);
    }

    // Check if valid device (word 0 should not be 0 or 0xFFFF)
    if id_data[0] == 0 || id_data[0] == 0xFFFF {
        return None;
    }

    // Extract drive information
    let model = extract_model(&id_data);
    let serial = extract_serial(&id_data);
    let max_lba = extract_max_lba(&id_data);

    // Read LBA 0 boot signature
    let lba0_signature = read_boot_signature(base);

    // Create result
    let info = DriveInfo {
        model,
        serial,
        max_lba,
        lba0_signature,
    };

    // Store in global - using unsafe to access mutable static
    unsafe {
        DRIVE_INFO_DATA = info.clone();
        DRIVE_INFO_PRESENT = true;
    }

    Some(info)
}

/// Read boot signature (0x55AA) from LBA 0
fn read_boot_signature(base: u16) -> u16 {
    // Issue READ SECTORS for LBA 0
    if !wait_ready(base) {
        return 0;
    }

    // Select master drive
    outb(base + IDE_DRIVE_PORT, 0xA0);

    // Wait for ready
    if !wait_ready(base) {
        return 0;
    }

    // Set sector count = 1
    outb(base + IDE_SECTOR_COUNT_PORT, 1);

    // Set LBA = 0
    outb(base + IDE_LBA_LOW_PORT, 0);
    outb(base + IDE_LBA_MID_PORT, 0);
    outb(base + IDE_LBA_HIGH_PORT, 0);

    // Select drive (LBA28: bit 6 = master, bits 5-4 = LBA bits 28-29 = 0)
    outb(base + IDE_DRIVE_PORT, 0xA0);

    // Issue READ SECTORS command
    outb(base + IDE_STATUS_PORT, IDE_CMD_READ_SECTORS);

    // Wait for DRQ
    if !wait_drq(base) {
        return 0;
    }

    // Read sector data
    let mut buffer: [u8; 512] = [0; 512];
    for i in 0..256 {
        let word = inw(base + IDE_DATA_PORT);
        buffer[i * 2] = (word & 0xFF) as u8;
        buffer[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
    }

    // Check for command completion
    let mut timeout: u32 = 0;
    loop {
        let status = inb(base + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) == 0 {
            break;
        }
        timeout += 1;
        if timeout >= 1_000_000 {
            return 0;
        }
    }

    // Boot signature is at bytes 510-511 (little-endian: 0x55AA at 510)
    u16::from_le_bytes([buffer[511], buffer[510]])
}

/// Initialize the ATA driver by detecting the IDE controller and identifying the drive
pub fn init() -> Result<(), &'static str> {
    match detect_ide_controller() {
        Some(()) => {
            // Try to identify the drive
            match identify_drive(IO_BASE) {
                Some(info) => {
                    // Print drive model
                    serial::write_str_safe("[ATA] Drive: \"");
                    // Find first non-null character and print until null or end
                    for &b in &info.model {
                        if b == 0 {
                            break;
                        }
                        serial::write_byte(b);
                    }
                    serial::write_str_safe("\" (");

                    // Print serial if present
                    let has_serial = info.serial.iter().any(|&b| b != 0);
                    if has_serial {
                        serial::write_str_safe("Serial: ");
                        for &b in &info.serial {
                            if b == 0 {
                                break;
                            }
                            serial::write_byte(b);
                        }
                    } else {
                        serial::write_str_safe("No serial");
                    }

                    // Print size info
                    serial::write_str_safe(", Max LBA: ");
                    let max_lba = info.max_lba;
                    if max_lba > 0 {
                        // Calculate size in MB
                        let mb = (max_lba as u64 * 512) / (1024 * 1024);
                        let mut tmp = mb;
                        let mut buf = [0u8; 20];
                        let mut idx = 19;
                        if tmp == 0 {
                            buf[19] = b'0';
                            idx = 19;
                        } else {
                            while tmp > 0 {
                                buf[idx] = b'0' + ((tmp % 10) as u8);
                                tmp /= 10;
                                idx -= 1;
                            }
                            idx += 1;
                        }
                        for &b in &buf[idx..=19] {
                            serial::write_byte(b);
                        }
                        serial::write_str_safe(" MB");
                    } else {
                        serial::write_str_safe("Unknown");
                    }

                    // Print boot signature status
                    serial::write_str_safe(", LBA0 Signature: 0x");
                    write_u16_hex(info.lba0_signature);
                    if info.lba0_signature == 0x55AA {
                        serial::write_str_safe(" (bootable)\r\n");
                    } else {
                        serial::write_str_safe("\r\n");
                    }

                    serial::write_str_safe("[ATA] Drive identified successfully\r\n");
                    Ok(())
                }
                None => {
                    serial::write_str_safe("[ATA] Failed to identify drive (no device present?)\r\n");
                    // Still return Ok - controller is present even if no drive
                    Ok(())
                }
            }
        }
        None => Err("No IDE controller found"),
    }
}

/// Read sectors using LBA28 PIO mode
/// Returns the data as a Vec<u8> containing the sector data
#[allow(dead_code)]
pub fn read_sectors(lba: u32, count: u32) -> Option<alloc::vec::Vec<u8>> {
    // Use alloc crate since we're in no_std with extern alloc
    // Note: This requires heap to be initialized

    // Validate LBA28 limits (28-bit addressing: max 2^28 - 1 sectors = ~137 GB)
    if lba > 0x0FFFFFFF {
        serial::write_str_safe("[ATA] LBA out of range for LBA28\r\n");
        return None;
    }

    // Total bytes to read
    let total_bytes = (count * 512) as usize;

    // Allocate buffer on heap
    let mut buffer: alloc::vec::Vec<u8> = alloc::vec![0u8; total_bytes];

    let mut sector_count: u32 = 0;

    while sector_count < count {
        let current_lba = lba + sector_count;

        // Wait for controller to be ready
        if !wait_ready(IO_BASE) {
            serial::write_str_safe("[ATA] Timeout waiting for ready\r\n");
            return None;
        }

        // Select drive 0 (master) and LBA
        // bits 6-5: LBA bits 28-29 (0 for LBA28)
        // bit 6: drive select (0 = master)
        outb(IO_BASE + IDE_DRIVE_PORT, 0xE0 | ((current_lba >> 24) as u8 & 0x0F));

        // Set sector count
        outb(IO_BASE + IDE_SECTOR_COUNT_PORT, count as u8);

        // Set LBA low/mid/high bytes
        outb(IO_BASE + IDE_LBA_LOW_PORT, (current_lba & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_MID_PORT, ((current_lba >> 8) & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_HIGH_PORT, ((current_lba >> 16) & 0xFF) as u8);

        // Issue READ SECTORS command
        outb(IO_BASE + IDE_STATUS_PORT, IDE_CMD_READ_SECTORS);

        // Read the sector(s)
        for _ in 0..count {
            // Wait for DRQ
            if !wait_drq(IO_BASE) {
                serial::write_str_safe("[ATA] Timeout waiting for data request\r\n");
                return None;
            }

            // Read 256 words (512 bytes) from data port
            let offset = (sector_count * 512) as usize;
            for i in 0..256 {
                let word = inw(IO_BASE + IDE_DATA_PORT);
                buffer[offset + (i * 2)] = (word & 0xFF) as u8;
                buffer[offset + (i * 2 + 1)] = ((word >> 8) & 0xFF) as u8;
            }

            sector_count += 1;

            // Break if we only wanted 1 sector
            if count == 1 {
                break;
            }
        }

        // Wait for command completion
        let mut done_timeout: u32 = 0;
        loop {
            let status = inb(IO_BASE + IDE_STATUS_PORT);
            if (status & IDE_STATUS_BSY) == 0 {
                break;
            }
            done_timeout += 1;
            if done_timeout >= 1_000_000 {
                serial::write_str_safe("[ATA] Timeout waiting for command completion\r\n");
                return None;
            }
        }

        // Check for errors
        let status = inb(IO_BASE + IDE_STATUS_PORT);
        if (status & 0x50) == 0x50 {
            // DRDY and DRQ both set = error occurred
            // Read error register (same port as feature on error)
            let error = inb(IO_BASE + IDE_FEATURE_PORT);
            serial::write_str_safe("[ATA] Error during read, error code: 0x");
            write_u16_hex(error as u16);
            serial::write_str_safe("\r\n");
            return None;
        }
    }

    Some(buffer)
}

/// Get the I/O base port of the detected IDE controller
pub fn io_base() -> u16 {
    IO_BASE
}

/// Check if an IDE controller is present
pub fn is_present() -> bool {
    let status = inb(IO_BASE + IDE_STATUS_PORT);
    status != 0xFF && status != 0x00
}

/// Get the detected drive info (returns a copy)
/// Returns None if drive has not been identified yet
#[allow(static_mut_refs)]
pub fn get_drive_info() -> Option<DriveInfo> {
    unsafe {
        if DRIVE_INFO_PRESENT {
            // Read via pointer to avoid creating a reference to mutable static
            // which triggers static_mut_refs lint
            let ptr = core::ptr::addr_of!(DRIVE_INFO_DATA);
            Some((*ptr).clone())
        } else {
            None
        }
    }
}

/// Read boot sector from LBA 0 and verify 0x55AA signature
pub fn verify_boot_signature() -> bool {
    match identify_drive(IO_BASE) {
        Some(info) => info.lba0_signature == 0x55AA,
        None => false,
    }
}