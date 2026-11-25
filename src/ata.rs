//! Primary IDE Controller Driver (PIO Mode, LBA28)
//!
//! Implements a block driver for Primary IDE channel using PIO mode with LBA28 addressing.
//! Supports reading sectors from storage devices connected to the primary IDE channel.
//!
//! Uses standard x86 ATA Primary Channel I/O ports (base 0x1F0, control 0x3F6).

use crate::pci::write_u16_hex;
use crate::print_u64;
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
const IDE_CMD_WRITE_SECTORS: u8 = 0x30;   // WRITE SECTORS (PIO)
const IDE_CMD_FLUSH_CACHE: u8 = 0xE7;     // FLUSH CACHE
const IDE_CMD_IDENTIFY: u8 = 0xEC;        // IDENTIFY DEVICE

// Status bits
const IDE_STATUS_BSY: u8 = 0x80;          // Busy
const IDE_STATUS_DRDY: u8 = 0x40;         // Drive ready
const IDE_STATUS_DRQ: u8 = 0x08;          // Data request
const IDE_STATUS_ERR: u8 = 0x01;          // Error
const IDE_STATUS_DF: u8 = 0x20;           // Drive fault

// Alternate Status port (control register, doesn't clear interrupts)
const IDE_ALT_STATUS_PORT: u16 = 0x3F6;

/// Drive information storage
#[derive(Clone)]
pub struct DriveInfo {
    pub model: [u8; 40],     // ATA model string (40 bytes, space-padded)
    pub serial: [u8; 20],    // Serial number (20 bytes)
    pub max_lba: u32,        // Maximum LBA for LBA28 (28-bit)
    pub lba0_signature: u16, // Boot signature at LBA 0 (0xAA55 if bootable)
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
    const TIMEOUT_MAX: u32 = 2_000_000;

    loop {
        let status = inb(base + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) == 0 && (status & IDE_STATUS_DRDY) != 0 {
            return true;
        }
        timeout += 1;
        if timeout >= TIMEOUT_MAX {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Wait for DRQ (Data Request) - drive has data to send
/// Returns true if DRQ set, false on timeout or error
fn wait_drq(base: u16) -> bool {
    let mut timeout: u32 = 0;
    const TIMEOUT_MAX: u32 = 2_000_000;

    // 400ns settling delay after command
    io_wait_400ns();

    loop {
        let status = inb(base + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) != 0 {
            // Device is still busy, keep waiting
        } else {
            // BSY is clear, check for errors
            if (status & (IDE_STATUS_ERR | IDE_STATUS_DF)) != 0 {
                return false;
            }
            if (status & IDE_STATUS_DRQ) != 0 {
                return true;
            }
        }
        timeout += 1;
        if timeout >= TIMEOUT_MAX {
            return false;
        }
        core::hint::spin_loop();
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

/// 400ns settling delay via Alternate Status port reads.
/// Reads the Alternate Status register (0x3F6) 4 times to satisfy
/// the ATA 400ns settling delay without clearing pending interrupts.
#[inline]
fn io_wait_400ns() {
    let _ = inb(IDE_ALT_STATUS_PORT);
    let _ = inb(IDE_ALT_STATUS_PORT);
    let _ = inb(IDE_ALT_STATUS_PORT);
    let _ = inb(IDE_ALT_STATUS_PORT);
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

/// Extract model string from IDENTIFY data (words 10-19 for model)
/// Per ATA spec: Each 16-bit word stores bytes with HIGH byte first
fn extract_model(id_data: &[u16; 256]) -> [u8; 40] {
    let mut model = [0u8; 40];
    for i in 0..20 {
        let word = id_data[10 + i];
        // ATA spec: bytes are stored with high byte first
        model[i * 2] = ((word >> 8) & 0xFF) as u8;
        model[i * 2 + 1] = (word & 0xFF) as u8;
    }
    model
}

/// Extract serial number from IDENTIFY data
/// Per ATA spec: Serial number is in words 20-29 (20 bytes)
/// Per ATA spec: Each 16-bit word stores bytes with HIGH byte first
fn extract_serial(id_data: &[u16; 256]) -> [u8; 20] {
    let mut serial = [0u8; 20];
    for i in 0..10 {
        let word = id_data[20 + i];  // Serial number is at words 20-29 (20 bytes)
        // ATA spec: bytes are stored with high byte first
        serial[i * 2] = ((word >> 8) & 0xFF) as u8;
        serial[i * 2 + 1] = (word & 0xFF) as u8;
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

    // 400ns delay after drive select
    io_wait_400ns();

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

/// Read boot signature (0xAA55) from LBA 0
fn read_boot_signature(base: u16) -> u16 {
    // Issue READ SECTORS for LBA 0
    if !wait_ready(base) {
        return 0;
    }

    // Select master drive and LBA (bits 5-4 = LBA bits 28-29 = 0)
    outb(base + IDE_DRIVE_PORT, 0xE0);

    // Set nIEN to disable interrupts during read operation
    outb(IDE_ALT_STATUS_PORT, 0x02);

    // 400ns delay after drive select
    io_wait_400ns();

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
        core::hint::spin_loop();
    }

    // Check for errors
    let status = inb(base + IDE_STATUS_PORT);
    if (status & IDE_STATUS_ERR) != 0 {
        return 0;
    }

    // Boot signature is at bytes 510-511 (0x55 at 510, 0xAA at 511)
    // Read as little-endian: [510, 511] gives 0xAA55
    u16::from_le_bytes([buffer[510], buffer[511]])
}

/// Initialize the ATA driver by detecting the IDE controller and identifying the drive
pub fn init() -> Result<(), &'static str> {
    // Set nIEN bit (bit 1) at control port 0x3F6 to disable interrupts
    // This is required for polling-based PIO operations
    // nIEN = 1 disables IRQ7 interrupts during ATA operations
    outb(0x3F6, 0x02);

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
                    if info.lba0_signature == 0xAA55 {
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
/// Supports arbitrary count > 255 by chunking into <= 255-sector LBA28 commands
#[allow(dead_code)]
pub fn read_sectors(lba: u32, count: u32) -> Option<alloc::vec::Vec<u8>> {
    // Use alloc crate since we're in no_std with extern alloc
    // Note: This requires heap to be initialized

    // Validate LBA28 limits (28-bit addressing: max 2^28 - 1 sectors = ~137 GB)
    if lba > 0x0FFFFFFF {
        serial::write_str_safe("[ATA] LBA out of range for LBA28\r\n");
        return None;
    }

    // Validate count - LBA28 sector count register is 8-bit, max 255 per command
    // But we support chunking, so limit total to prevent overflow
    if count == 0 || count > (0x0FFFFFFF + 1) {
        serial::write_str_safe("[ATA] Invalid sector count for LBA28\r\n");
        return None;
    }

    // Total bytes to read - calculate as u64 to prevent overflow for large reads
    let total_bytes = ((count as u64) * 512) as usize;

    // Allocate buffer on heap
    let mut buffer: alloc::vec::Vec<u8> = alloc::vec![0u8; total_bytes];

    let mut sector_offset: u32 = 0;
    let mut buf_offset: usize = 0;

    while sector_offset < count {
        let remaining = count - sector_offset;
        // LBA28 can read up to 255 sectors per command (sector count is 8-bit)
        let sectors_to_read = if remaining > 255 { 255 } else { remaining };
        let current_lba = lba + sector_offset;

        // Wait for controller to be ready (bounded timeout for BSY clear)
        let mut bsy_timeout: u32 = 0;
        const BSY_TIMEOUT_MAX: u32 = 2_000_000;
        loop {
            let status = inb(IO_BASE + IDE_STATUS_PORT);
            if (status & IDE_STATUS_BSY) == 0 {
                break;
            }
            bsy_timeout += 1;
            if bsy_timeout >= BSY_TIMEOUT_MAX {
                serial::write_str_safe("[ATA] Timeout waiting for BSY clear\r\n");
                return None;
            }
            core::hint::spin_loop();
        }

        // Drive-select settling: wait for DRDY after setting drive select
        // Select drive 0 (master) and LBA
        // bits 6-5: LBA bits 28-29 (0 for LBA28)
        // bit 6: drive select (0 = master)
        // Keep nIEN set (bit 1 = 0) to disable interrupts during reads
        outb(IO_BASE + IDE_DRIVE_PORT, 0xE0 | ((current_lba >> 24) as u8 & 0x0F));

        // Set nIEN to disable interrupts during read operation
        outb(IDE_ALT_STATUS_PORT, 0x02);

        // 400ns delay after drive select
        io_wait_400ns();

        // Wait for DRDY (drive ready) after drive-select
        let mut drdy_timeout: u32 = 0;
        loop {
            let status = inb(IO_BASE + IDE_STATUS_PORT);
            if (status & IDE_STATUS_DRDY) != 0 {
                break;
            }
            drdy_timeout += 1;
            if drdy_timeout >= BSY_TIMEOUT_MAX {
                serial::write_str_safe("[ATA] Timeout waiting for DRDY after drive-select\r\n");
                return None;
            }
            core::hint::spin_loop();
        }

        // Set sector count
        outb(IO_BASE + IDE_SECTOR_COUNT_PORT, sectors_to_read as u8);

        // Set LBA low/mid/high bytes
        outb(IO_BASE + IDE_LBA_LOW_PORT, (current_lba & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_MID_PORT, ((current_lba >> 8) & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_HIGH_PORT, ((current_lba >> 16) & 0xFF) as u8);

        // Issue READ SECTORS command
        outb(IO_BASE + IDE_STATUS_PORT, IDE_CMD_READ_SECTORS);

        // Read all sectors in this chunk
        for sector in 0..sectors_to_read {
            // Wait for DRQ
            if !wait_drq(IO_BASE) {
                serial::write_str_safe("[ATA] Timeout waiting for data request\r\n");
                return None;
            }

            // Read 256 words (512 bytes) from data port
            let offset = buf_offset + (sector as usize * 512);
            for i in 0..256 {
                let word = inw(IO_BASE + IDE_DATA_PORT);
                buffer[offset + (i * 2)] = (word & 0xFF) as u8;
                buffer[offset + (i * 2 + 1)] = ((word >> 8) & 0xFF) as u8;
            }
        }

        buf_offset += sectors_to_read as usize * 512;
        sector_offset += sectors_to_read;
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
            core::hint::spin_loop();
            serial::write_str_safe("[ATA] Timeout waiting for command completion\r\n");
            return None;
        }
        core::hint::spin_loop();
    }

    // Check for errors
    let status = inb(IO_BASE + IDE_STATUS_PORT);
    if (status & IDE_STATUS_ERR) != 0 {
        // Error bit set
        let error = inb(IO_BASE + IDE_FEATURE_PORT);
        serial::write_str_safe("[ATA] Error during read, error code: 0x");
        write_u16_hex(error as u16);
        serial::write_str_safe("\r\n");
        return None;
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

/// Read boot sector from LBA 0 and verify 0xAA55 signature
pub fn verify_boot_signature() -> bool {
    match identify_drive(IO_BASE) {
        Some(info) => info.lba0_signature == 0xAA55,
        None => false,
    }
}

/// Maximum number of write-fenced LBAs to track
const MAX_WRITE_FENCES: usize = 256;

/// Write fence entry - tracks pending writes that need flushing
#[derive(Clone, Copy, Default)]
struct WriteFence {
    lba: u32,
    sector_count: u32,
    flushed: bool,
    persistent: bool,
}

/// Static storage for write fence registry
#[allow(static_mut_refs)]
static mut WRITE_FENCE_REGISTRY: [WriteFence; MAX_WRITE_FENCES] = [const { WriteFence { lba: 0, sector_count: 0, flushed: true, persistent: false } }; MAX_WRITE_FENCES];

#[allow(static_mut_refs)]
static mut WRITE_FENCE_COUNT: usize = 0;

/// Add a write fence entry for tracking pending writes
/// This registers that sectors starting at `lba` for `sector_count` sectors
/// have been written and need to be flushed to disk
pub fn add_write_fence(lba: u32, sector_count: u32) {
    unsafe {
        if WRITE_FENCE_COUNT < MAX_WRITE_FENCES {
            WRITE_FENCE_REGISTRY[WRITE_FENCE_COUNT] = WriteFence {
                lba,
                sector_count,
                flushed: false,
                persistent: false,
            };
            WRITE_FENCE_COUNT += 1;
        }
    }
}

/// Add a read-only fence for persistent partition boundaries.
/// Unlike regular write fences, these survive cache flushes.
pub fn add_readonly_fence(lba: u32, sector_count: u32) {
    unsafe {
        if WRITE_FENCE_COUNT < MAX_WRITE_FENCES {
            WRITE_FENCE_REGISTRY[WRITE_FENCE_COUNT] = WriteFence {
                lba,
                sector_count,
                flushed: true, // Already "flushed" in the sense of not needing write flush
                persistent: true,
            };
            WRITE_FENCE_COUNT += 1;
        }
    }
}

/// Flush the drive's write cache using FLUSH CACHE command
/// This ensures all pending writes are committed to the storage medium
/// Persistent fences (read-only partition boundaries) are preserved across flushes
pub fn flush_cache() -> bool {
    // Wait for controller to be ready
    if !wait_ready(IO_BASE) {
        serial::write_str_safe("[ATA] Flush cache: timeout waiting for ready\r\n");
        return false;
    }

    // Select master drive
    outb(IO_BASE + IDE_DRIVE_PORT, 0xE0);

    // Issue FLUSH CACHE command
    outb(IO_BASE + IDE_STATUS_PORT, IDE_CMD_FLUSH_CACHE);

    // Wait for completion
    let mut timeout: u32 = 0;
    loop {
        let status = inb(IO_BASE + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) == 0 {
            break;
        }
        timeout += 1;
        if timeout >= 1_000_000 {
            serial::write_str_safe("[ATA] Flush cache: timeout\r\n");
            return false;
        }
    }

    // Check for errors
    let status = inb(IO_BASE + IDE_STATUS_PORT);
    if (status & 0x01) != 0 {
        // Error bit set
        let error = inb(IO_BASE + IDE_FEATURE_PORT);
        serial::write_str_safe("[ATA] Flush cache error: 0x");
        write_u16_hex(error as u16);
        serial::write_str_safe("\r\n");
        return false;
    }

    // Mark all non-persistent pending write fences as flushed
    // Persistent fences are preserved (moved to front of registry)
    unsafe {
        let mut persistent_index = 0;

        // First pass: mark non-persistent fences as flushed
        for i in 0..WRITE_FENCE_COUNT {
            let mut fence = WRITE_FENCE_REGISTRY[i];
            if !fence.persistent {
                fence.flushed = true;
            }
            WRITE_FENCE_REGISTRY[i] = fence;
        }

        // Second pass: compact - move persistent fences to front
        for i in 0..WRITE_FENCE_COUNT {
            let fence = WRITE_FENCE_REGISTRY[i];
            if fence.persistent {
                WRITE_FENCE_REGISTRY[persistent_index] = fence;
                persistent_index += 1;
            }
        }

        // Clear the rest with default (flushed = true, persistent = false)
        for i in persistent_index..WRITE_FENCE_COUNT {
            WRITE_FENCE_REGISTRY[i] = WriteFence {
                lba: 0,
                sector_count: 0,
                flushed: true,
                persistent: false,
            };
        }

        WRITE_FENCE_COUNT = persistent_index;
    }

    true
}

/// Check if a write at the given LBA range overlaps with any pending write fences
/// Returns true if the write should be rejected (overlap found), false if safe to proceed
fn check_write_fence_overlap(lba: u32, sector_count: u32) -> bool {
    let write_end = lba.saturating_add(sector_count);

    unsafe {
        for i in 0..WRITE_FENCE_COUNT {
            let fence = WRITE_FENCE_REGISTRY[i];
            if fence.flushed {
                continue;
            }

            // Calculate fence range (exclusive end)
            let fence_end = fence.lba.saturating_add(fence.sector_count);

            // Check for overlap: [lba, write_end) overlaps with [fence.lba, fence_end)
            if lba < fence_end && write_end > fence.lba {
                return true; // Overlap detected - reject the write
            }
        }
    }
    false
}

/// Clear all write fences (reset the fence registry)
pub fn clear_write_fences() {
    unsafe {
        for i in 0..WRITE_FENCE_COUNT {
            WRITE_FENCE_REGISTRY[i] = WriteFence {
                lba: 0,
                sector_count: 0,
                flushed: true,
                persistent: false,
            };
        }
        WRITE_FENCE_COUNT = 0;
    }
}

/// Check if a write range overlaps with pending write fences
pub fn has_write_fence_overlap(lba: u32, sector_count: u32) -> bool {
    check_write_fence_overlap(lba, sector_count)
}

/// Write sectors using LBA28 PIO mode
/// Returns true if successful, false on error
/// Supports arbitrary count > 255 by chunking into <= 255-sector LBA28 commands
pub fn write_sectors(lba: u32, count: u32, data: &[u8]) -> bool {
    // Validate LBA28 limits
    if lba > 0x0FFFFFFF {
        serial::write_str_safe("[ATA] LBA out of range for LBA28\r\n");
        return false;
    }

    // Validate data length
    let expected_bytes = (count * 512) as usize;
    if data.len() < expected_bytes {
        serial::write_str_safe("[ATA] Insufficient data for write\r\n");
        return false;
    }

    // Check for write-fence overlap before touching hardware
    if check_write_fence_overlap(lba, count) {
        serial::write_str_safe("[ATA] Write rejected: overlaps with pending write fence\r\n");
        return false;
    }

    let mut sector_offset: u32 = 0;

    while sector_offset < count {
        let remaining = count - sector_offset;
        // LBA28 can write up to 255 sectors per command (sector count is 8-bit)
        let sectors_to_write = if remaining > 255 { 255 } else { remaining };
        let current_lba = lba + sector_offset;

        // Wait for controller to be ready
        if !wait_ready(IO_BASE) {
            serial::write_str_safe("[ATA] Write: timeout waiting for ready\r\n");
            return false;
        }

        // Select drive 0 (master) and LBA
        outb(IO_BASE + IDE_DRIVE_PORT, 0xE0 | ((current_lba >> 24) as u8 & 0x0F));

        // Set nIEN to disable interrupts during write operation
        // Must remain set throughout the write to prevent spurious interrupts
        outb(IDE_ALT_STATUS_PORT, 0x02);

        // 400ns delay after drive select
        io_wait_400ns();

        // Set sector count
        outb(IO_BASE + IDE_SECTOR_COUNT_PORT, sectors_to_write as u8);

        // Set LBA low/mid/high bytes
        outb(IO_BASE + IDE_LBA_LOW_PORT, (current_lba & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_MID_PORT, ((current_lba >> 8) & 0xFF) as u8);
        outb(IO_BASE + IDE_LBA_HIGH_PORT, ((current_lba >> 16) & 0xFF) as u8);

        // Issue WRITE SECTORS command
        outb(IO_BASE + IDE_STATUS_PORT, IDE_CMD_WRITE_SECTORS);

        // Wait for DRQ (ready to receive data)
        if !wait_drq(IO_BASE) {
            serial::write_str_safe("[ATA] Write: timeout waiting for DRQ\r\n");
            return false;
        }

        // Write sectors
        for sector in 0..sectors_to_write {
            let data_offset = ((sector_offset + sector) * 512) as usize;

            // Write 256 words (512 bytes) to data port
            for i in 0..256 {
                let word_offset = data_offset + (i * 2);
                let word = u16::from_le_bytes([
                    data[word_offset],
                    data[word_offset + 1],
                ]);
                outw(IO_BASE + IDE_DATA_PORT, word);
            }
        }

        // Wait for command completion
        let mut timeout: u32 = 0;
        loop {
            let status = inb(IO_BASE + IDE_STATUS_PORT);
            if (status & IDE_STATUS_BSY) == 0 {
                break;
            }
            timeout += 1;
            if timeout >= 1_000_000 {
                core::hint::spin_loop();
                serial::write_str_safe("[ATA] Write: timeout waiting for completion\r\n");
                return false;
            }
            core::hint::spin_loop();
        }

        // Check for errors
        let status = inb(IO_BASE + IDE_STATUS_PORT);
        if (status & IDE_STATUS_ERR) != 0 {
            let error = inb(IO_BASE + IDE_FEATURE_PORT);
            serial::write_str_safe("[ATA] Write error at LBA ");
            print_u64(current_lba as u64);
            serial::write_str_safe(": 0x");
            write_u16_hex(error as u16);
            serial::write_str_safe("\r\n");
            return false;
        }

        sector_offset += sectors_to_write;
    }

    // Add write fence entry
    add_write_fence(lba, count);

    true
}

/// Write u16 to I/O port
#[inline]
fn outw(port: u16, val: u16) {
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") val,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read a single sector using LBA28 PIO mode
/// This is a convenience function for reading one sector (512 bytes)
pub fn read_sector(lba: u32) -> Option<alloc::vec::Vec<u8>> {
    // Validate LBA28 limits
    if lba > 0x0FFFFFFF {
        serial::write_str_safe("[ATA] LBA out of range for LBA28\r\n");
        return None;
    }

    // Wait for controller to be ready
    if !wait_ready(IO_BASE) {
        serial::write_str_safe("[ATA] read_sector: timeout waiting for ready\r\n");
        return None;
    }

    // Select drive (0xE0 = master, bit 6=1 for master, bits 5-4 = LBA bits 28-29)
    // Bit 1 of 0xE0 (nIEN = 0) - IRQ enabled during operation
    // We want nIEN = 1 (disable IRQ) by setting bit 1 = 0xE2
    // But typical usage is 0xE0 for normal operation
    outb(IO_BASE + IDE_DRIVE_PORT, 0xE0 | ((lba >> 24) as u8 & 0x0F));

    // 400ns delay after drive select (per ATA spec)
    // On modern hardware, a few iterations of a spin loop is sufficient
    // This ensures proper drive/head settling time
    let mut delay = 0u32;
    while delay < 4 {
        delay += 1;
    }

    // Set sector count = 1
    outb(IO_BASE + IDE_SECTOR_COUNT_PORT, 1);

    // Set LBA low/mid/high bytes
    outb(IO_BASE + IDE_LBA_LOW_PORT, (lba & 0xFF) as u8);
    outb(IO_BASE + IDE_LBA_MID_PORT, ((lba >> 8) & 0xFF) as u8);
    outb(IO_BASE + IDE_LBA_HIGH_PORT, ((lba >> 16) & 0xFF) as u8);

    // Set nIEN bit (bit 1) at control port 0x3F6 to disable interrupts
    // This is required for polling-based PIO operations as per LBA28 timing spec
    outb(0x3F6, 0x02);

    // Issue READ SECTORS command
    outb(IO_BASE + IDE_STATUS_PORT, IDE_CMD_READ_SECTORS);

    // Wait for DRQ (Data Request) - drive has data to send
    if !wait_drq(IO_BASE) {
        serial::write_str_safe("[ATA] read_sector: timeout waiting for data request\r\n");
        return None;
    }

    // Read 256 words (512 bytes) from data port
    let mut buffer: [u8; 512] = [0; 512];
    for i in 0..256 {
        let word = inw(IO_BASE + IDE_DATA_PORT);
        buffer[i * 2] = (word & 0xFF) as u8;
        buffer[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
    }

    // Wait for command completion
    let mut timeout: u32 = 0;
    loop {
        let status = inb(IO_BASE + IDE_STATUS_PORT);
        if (status & IDE_STATUS_BSY) == 0 {
            break;
        }
        timeout += 1;
        if timeout >= 1_000_000 {
            serial::write_str_safe("[ATA] read_sector: timeout waiting for completion\r\n");
            return None;
        }
    }

    // Check for errors
    let status = inb(IO_BASE + IDE_STATUS_PORT);
    if (status & 0x01) != 0 {
        let error = inb(IO_BASE + IDE_FEATURE_PORT);
        serial::write_str_safe("[ATA] read_sector error: 0x");
        write_u16_hex(error as u16);
        serial::write_str_safe("\r\n");
        return None;
    }

    Some(buffer.to_vec())
}