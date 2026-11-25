//! GPT/MBR Partition Table Detection
//!
//! Implements detection and parsing of partition tables for:
//! - GPT (GUID Partition Table)
//! - MBR (Master Boot Record)
//! - Superfloppy (no partition table, entire disk is one filesystem)

use crate::ata;
use crate::serial;

/// Partition role classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PartitionRole {
    /// Boot/EFI System Partition (ESP)
    BootEsp,
    /// Recovery partition
    RecoveryVault,
    /// Standard data partition
    Data,
    /// Unknown or other partition type
    Unknown,
}

/// GUID structure for partition type identification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    /// Create a GUID from bytes (little-endian for data1, data2, data3)
    fn from_bytes(bytes: &[u8; 16]) -> Self {
        Guid {
            data1: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_le_bytes([bytes[4], bytes[5]]),
            data3: u16::from_le_bytes([bytes[6], bytes[7]]),
            data4: [bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]],
        }
    }

    /// Check if this GUID matches the EFI System Partition type
    fn is_esp(&self) -> bool {
        // EFI System Partition GUID: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
        self.data1 == 0xC12A7328
            && self.data2 == 0xF81F
            && self.data3 == 0x11D2
            && self.data4 == [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]
    }

    /// Check if this GUID matches a known recovery partition type
    fn is_recovery(&self) -> bool {
        // Recovery partition GUID: DE94BBA4-06D1-4D40-A16A-BFD50179D6AC
        self.data1 == 0xDE94BBA4
            && self.data2 == 0x06D1
            && self.data3 == 0x4D40
            && self.data4 == [0xA1, 0x6A, 0xBF, 0xD5, 0x01, 0x79, 0xD6, 0xAC]
    }
}

/// MBR partition entry (16 bytes)
#[derive(Debug, Clone, Copy)]
struct MbrPartition {
    status: u8,
    chs_start: [u8; 3],
    partition_type: u8,
    chs_end: [u8; 3],
    lba_start: u32,
    sector_count: u32,
}

impl MbrPartition {
    fn from_bytes(bytes: &[u8; 16]) -> Self {
        MbrPartition {
            status: bytes[0],
            chs_start: [bytes[1], bytes[2], bytes[3]],
            partition_type: bytes[4],
            chs_end: [bytes[5], bytes[6], bytes[7]],
            lba_start: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            sector_count: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }
    }

    fn is_valid(&self) -> bool {
        self.status == 0x80 || self.status == 0x00
    }

    fn get_role(&self) -> PartitionRole {
        match self.partition_type {
            0x0B | 0x0C => PartitionRole::BootEsp,  // FAT32 LBA (0x0B) or FAT32 LBA (0x0C)
            _ => PartitionRole::Data,
        }
    }
}

/// GPT partition entry (128 bytes)
#[derive(Debug, Clone, Copy)]
struct GptPartition {
    partition_type_guid: Guid,
    unique_guid: Guid,
    starting_lba: u64,
    ending_lba: u64,
    attributes: u64,
    partition_name: [u8; 72],
}

impl GptPartition {
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 128 {
            return None;
        }

        let mut partition_name = [0u8; 72];
        partition_name.copy_from_slice(&bytes[56..128]);

        Some(GptPartition {
            partition_type_guid: Guid::from_bytes(&bytes[0..16].try_into().unwrap()),
            unique_guid: Guid::from_bytes(&bytes[16..32].try_into().unwrap()),
            starting_lba: u64::from_le_bytes([
                bytes[32], bytes[33], bytes[34], bytes[35],
                bytes[36], bytes[37], bytes[38], bytes[39],
            ]),
            ending_lba: u64::from_le_bytes([
                bytes[40], bytes[41], bytes[42], bytes[43],
                bytes[44], bytes[45], bytes[46], bytes[47],
            ]),
            attributes: u64::from_le_bytes([
                bytes[48], bytes[49], bytes[50], bytes[51],
                bytes[52], bytes[53], bytes[54], bytes[55],
            ]),
            partition_name,
        })
    }

    fn get_role(&self) -> PartitionRole {
        if self.partition_type_guid.is_esp() {
            PartitionRole::BootEsp
        } else if self.partition_type_guid.is_recovery() {
            PartitionRole::RecoveryVault
        } else {
            PartitionRole::Data
        }
    }
}

/// GPT header structure
#[derive(Debug, Clone, Copy)]
struct GptHeader {
    signature: [u8; 8],
    revision: u32,
    header_size: u32,
    header_crc32: u32,
    reserved: u32,
    my_lba: u64,
    alternate_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    disk_guid: Guid,
    partition_entry_lba: u64,
    partition_entry_count: u32,
    partition_entry_size: u32,
    partition_array_crc32: u32,
}

impl GptHeader {
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 92 {
            return None;
        }

        let signature: [u8; 8] = bytes[0..8].try_into().unwrap();

        // Check GPT signature: "EFI PART"
        if &signature != b"EFI PART" {
            return None;
        }

        Some(GptHeader {
            signature,
            revision: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            header_size: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            header_crc32: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            reserved: u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            my_lba: u64::from_le_bytes([
                bytes[24], bytes[25], bytes[26], bytes[27],
                bytes[28], bytes[29], bytes[30], bytes[31],
            ]),
            alternate_lba: u64::from_le_bytes([
                bytes[32], bytes[33], bytes[34], bytes[35],
                bytes[36], bytes[37], bytes[38], bytes[39],
            ]),
            first_usable_lba: u64::from_le_bytes([
                bytes[40], bytes[41], bytes[42], bytes[43],
                bytes[44], bytes[45], bytes[46], bytes[47],
            ]),
            last_usable_lba: u64::from_le_bytes([
                bytes[48], bytes[49], bytes[50], bytes[51],
                bytes[52], bytes[53], bytes[54], bytes[55],
            ]),
            disk_guid: Guid::from_bytes(&bytes[56..72].try_into().unwrap()),
            partition_entry_lba: u64::from_le_bytes([
                bytes[72], bytes[73], bytes[74], bytes[75],
                bytes[76], bytes[77], bytes[78], bytes[79],
            ]),
            partition_entry_count: u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]),
            partition_entry_size: u32::from_le_bytes([bytes[84], bytes[85], bytes[86], bytes[87]]),
            partition_array_crc32: u32::from_le_bytes([bytes[88], bytes[89], bytes[90], bytes[91]]),
        })
    }
}

/// Partition information structure
#[derive(Debug, Clone, Copy)]
pub struct PartitionInfo {
    pub start_lba: u64,
    pub sector_count: u64,
    pub role: PartitionRole,
    pub is_gpt: bool,
}

/// Detected partition table type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionTableType {
    /// No partition table (Superfloppy)
    Superfloppy,
    /// MBR partition table
    Mbr,
    /// GPT partition table
    Gpt,
    /// Invalid or unrecognized
    Invalid,
}

/// Global storage for partition information
#[allow(static_mut_refs)]
static mut PARTITIONS: [PartitionInfo; 128] = [
    PartitionInfo {
        start_lba: 0,
        sector_count: 0,
        role: PartitionRole::Unknown,
        is_gpt: false,
    };
    128
];

#[allow(static_mut_refs)]
static mut PARTITION_COUNT: usize = 0;

#[allow(static_mut_refs)]
static mut PARTITION_TABLE_TYPE: PartitionTableType = PartitionTableType::Invalid;

/// Initialize partition detection
/// Scans the disk for GPT, MBR, or Superfloppy layout
pub fn init() -> Result<(), &'static str> {
    if !ata::is_present() {
        serial::write_str_safe("[GPT] No ATA controller present\r\n");
        return Err("No ATA controller");
    }

    // Read LBA 0 (MBR or boot sector)
    let mbr_data = match ata::read_sectors(0, 1) {
        Some(data) => data,
        None => {
            serial::write_str_safe("[GPT] Failed to read LBA 0\r\n");
            return Err("Failed to read LBA 0");
        }
    };

    // Check for MBR signature at offset 510
    let mbr_sig = u16::from_le_bytes([mbr_data[510], mbr_data[511]]);

    // Check for GPT signature at LBA 1
    let gpt_header_data = match ata::read_sectors(1, 1) {
        Some(data) => data,
        None => {
            serial::write_str_safe("[GPT] Failed to read LBA 1\r\n");
            return Err("Failed to read LBA 1");
        }
    };

    let gpt_sig: [u8; 8] = gpt_header_data[0..8].try_into().unwrap();

    if &gpt_sig == b"EFI PART" {
        // GPT detected
        unsafe {
            PARTITION_TABLE_TYPE = PartitionTableType::Gpt;
        }
        serial::write_str_safe("[GPT] GPT partition table detected\r\n");
        return parse_gpt();
    } else if mbr_sig == 0xAA55 {
        // Valid MBR signature
        // Check if this is a protective MBR for GPT
        // Protective MBR has partition type 0xEE at offset 4
        let partition_type = mbr_data[450]; // First partition type in MBR

        if partition_type == 0xEE {
            // Protective MBR - try GPT
            unsafe {
                PARTITION_TABLE_TYPE = PartitionTableType::Gpt;
            }
            serial::write_str_safe("[GPT] Protective MBR detected, trying GPT\r\n");
            return parse_gpt();
        } else {
            // Regular MBR
            unsafe {
                PARTITION_TABLE_TYPE = PartitionTableType::Mbr;
            }
            serial::write_str_safe("[GPT] MBR partition table detected\r\n");
            return parse_mbr(&mbr_data);
        }
    } else {
        // No valid partition table - Superfloppy
        unsafe {
            PARTITION_TABLE_TYPE = PartitionTableType::Superfloppy;
            PARTITION_COUNT = 1;
            PARTITIONS[0] = PartitionInfo {
                start_lba: 0,
                sector_count: 0, // Will be determined from drive info
                role: PartitionRole::Data,
                is_gpt: false,
            };
        }
        serial::write_str_safe("[GPT] No partition table - Superfloppy format\r\n");
        return Ok(());
    }
}

/// Parse MBR partition table
fn parse_mbr(mbr_data: &[u8]) -> Result<(), &'static str> {
    unsafe {
        PARTITION_COUNT = 0;
    }

    // MBR partitions start at offset 446 (0x1BE)
    for i in 0..4 {
        let offset = 446 + (i * 16);
        let partition_bytes: [u8; 16] = mbr_data[offset..offset + 16].try_into().unwrap();
        let partition = MbrPartition::from_bytes(&partition_bytes);

        if partition.is_valid() && partition.partition_type != 0x00 {
            let info = PartitionInfo {
                start_lba: partition.lba_start as u64,
                sector_count: partition.sector_count as u64,
                role: partition.get_role(),
                is_gpt: false,
            };

            unsafe {
                if PARTITION_COUNT < 128 {
                    PARTITIONS[PARTITION_COUNT] = info;
                    PARTITION_COUNT += 1;
                }
            }

            serial::write_str_safe("[GPT] MBR Partition ");
            serial::write_byte(b'0' + (i as u8));
            serial::write_str_safe(": LBA=");
            print_u64(partition.lba_start as u64);
            serial::write_str_safe(" count=");
            print_u64(partition.sector_count as u64);
            serial::write_str_safe(" type=0x");
            write_u8_hex(partition.partition_type);
            serial::write_str_safe("\r\n");
        }
    }

    Ok(())
}

/// Parse GPT partition table
fn parse_gpt() -> Result<(), &'static str> {
    // Read GPT header from LBA 1
    let header_data = match ata::read_sectors(1, 1) {
        Some(data) => data,
        None => {
            serial::write_str_safe("[GPT] Failed to read GPT header\r\n");
            return Err("Failed to read GPT header");
        }
    };

    let header = match GptHeader::from_bytes(&header_data) {
        Some(h) => h,
        None => {
            serial::write_str_safe("[GPT] Invalid GPT header\r\n");
            return Err("Invalid GPT header");
        }
    };

    serial::write_str_safe("[GPT] GPT header: revision=");
    print_u64(header.revision as u64);
    serial::write_str_safe(" entries=");
    print_u64(header.partition_entry_count as u64);
    serial::write_str_safe("\r\n");

    // Read GPT header from LBA 0 (MBR) - first 34 sectors for protective MBR
    // Sectors 0-33 (34 sectors total = 17.4 KiB) contain:
    // - Sector 0: Protective MBR (512 bytes)
    // - Sector 1: GPT header
    // - Sectors 2+: Partition entries (at least 128 entries * 128 bytes = 16 KiB)
    let _mbr_data = match ata::read_sectors(0, 34) {
        Some(data) => data,
        None => {
            serial::write_str_safe("[GPT] Failed to read MBR area\r\n");
            return Err("Failed to read MBR area");
        }
    };

    // Read backup GPT structures from last LBA
    // Backup header is at alternate_lba, entries at alternate_lba - 1 (for 32 sectors of entries)
    let backup_header_lba = header.alternate_lba;
    let _backup_entries_lba = backup_header_lba - 1; // 32 sectors for 128 entries

    // Read backup GPT header (1 sector)
    let backup_header_data = match ata::read_sectors(backup_header_lba as u32, 1) {
        Some(data) => data,
        None => {
            serial::write_str_safe("[GPT] Failed to read backup GPT header\r\n");
            return Err("Failed to read backup GPT header");
        }
    };

    let _backup_header = match GptHeader::from_bytes(&backup_header_data) {
        Some(h) => h,
        None => {
            serial::write_str_safe("[GPT] Invalid backup GPT header\r\n");
            return Err("Invalid backup GPT header");
        }
    };

    // Add persistent read-only fences for MBR area (34 sectors at LBA 0)
    // This protects the partition table from accidental modification
    crate::ata::add_readonly_fence(0, 34);

    // Add persistent read-only fence for backup GPT header area (33 sectors)
    // This protects: backup header (1 sector) + backup entries (32 sectors)
    crate::ata::add_readonly_fence(backup_header_lba as u32, 33);

    // Read partition entries using 512-byte sector math
    let entry_size = header.partition_entry_size;
    let entries_per_sector = 512 / entry_size;

    unsafe {
        PARTITION_COUNT = 0;
    }

    let mut entry_sector = header.partition_entry_lba;
    let mut entries_read: u32 = 0;

    while entries_read < header.partition_entry_count {
        let remaining = header.partition_entry_count - entries_read;
        let sectors_to_read = if remaining >= entries_per_sector {
            entries_per_sector
        } else {
            remaining
        };

        let data = match ata::read_sectors(entry_sector as u32, sectors_to_read as u32) {
            Some(d) => d,
            None => {
                serial::write_str_safe("[GPT] Failed to read partition entries\r\n");
                return Err("Failed to read partition entries");
            }
        };

        // Calculate number of entries in this sector (may be partial for last sector)
        let entries_this_sector = if entries_read + entries_per_sector > header.partition_entry_count {
            header.partition_entry_count - entries_read
        } else {
            entries_per_sector
        } as usize;

        for i in 0..entries_this_sector {
            if entries_read as usize >= header.partition_entry_count as usize {
                break;
            }

            // Read 128-byte partition entry using 512-byte sector math
            let offset = (i as u64) * (entry_size as u64);
            let entry_data = &data[offset as usize..(offset + entry_size as u64) as usize];

            if let Some(entry) = GptPartition::from_bytes(entry_data) {
                // Check if partition type is empty (all zeros)
                let is_empty = entry.partition_type_guid.data1 == 0
                    && entry.partition_type_guid.data2 == 0
                    && entry.partition_type_guid.data3 == 0
                    && entry.partition_type_guid.data4 == [0; 8];

                if !is_empty {
                    let sector_count = entry.ending_lba - entry.starting_lba + 1;
                    let info = PartitionInfo {
                        start_lba: entry.starting_lba,
                        sector_count,
                        role: entry.get_role(),
                        is_gpt: true,
                    };

                    let partition_count;
                    unsafe {
                        if PARTITION_COUNT < 128 {
                            PARTITIONS[PARTITION_COUNT] = info;
                            PARTITION_COUNT += 1;
                            partition_count = PARTITION_COUNT;
                        } else {
                            partition_count = 0;
                        }
                    }

                    // Register persistent read-only fence ONLY for BootEsp and RecoveryVault partitions
                    // Data partitions are not fenced (allows for potential future modifications)
                    match info.role {
                        PartitionRole::BootEsp | PartitionRole::RecoveryVault => {
                            crate::ata::add_readonly_fence(entry.starting_lba as u32, sector_count as u32);
                        }
                        _ => {}
                    }

                    serial::write_str_safe("[GPT] GPT Partition ");
                    print_u64(partition_count as u64);
                    serial::write_str_safe(": LBA=");
                    print_u64(entry.starting_lba);
                    serial::write_str_safe(" count=");
                    print_u64(sector_count);
                    serial::write_str_safe(" role=");
                    match info.role {
                        PartitionRole::BootEsp => serial::write_str_safe("ESP\r\n"),
                        PartitionRole::RecoveryVault => serial::write_str_safe("Recovery\r\n"),
                        PartitionRole::Data => serial::write_str_safe("Data\r\n"),
                        PartitionRole::Unknown => serial::write_str_safe("Unknown\r\n"),
                    }
                }
            }

            entries_read += 1;
        }

        entry_sector += sectors_to_read as u64;
    }

    Ok(())
}

/// Get the partition table type
pub fn get_partition_table_type() -> PartitionTableType {
    unsafe { PARTITION_TABLE_TYPE }
}

/// Get partition count
pub fn partition_count() -> usize {
    unsafe { PARTITION_COUNT }
}

/// Get partition info by index
pub fn get_partition(index: usize) -> Option<PartitionInfo> {
    unsafe {
        if index < PARTITION_COUNT {
            Some(PARTITIONS[index])
        } else {
            None
        }
    }
}

/// Helper to print u64 as decimal
fn print_u64(mut val: u64) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut idx = 19;
    while val > 0 {
        buf[idx] = b'0' + ((val % 10) as u8);
        val /= 10;
        idx -= 1;
    }
    for &b in &buf[idx + 1..=19] {
        serial::write_byte(b);
    }
}

/// Helper to write u8 as 2-digit hex
fn write_u8_hex(val: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    serial::write_byte(HEX[(val >> 4) as usize]);
    serial::write_byte(HEX[(val & 0xF) as usize]);
}

/// Find a partition by role
pub fn find_partition_by_role(role: PartitionRole) -> Option<PartitionInfo> {
    for i in 0..partition_count() {
        if let Some(info) = get_partition(i) {
            if info.role == role {
                return Some(info);
            }
        }
    }
    None
}

/// Get all partitions with a specific role
pub fn get_partitions_by_role(role: PartitionRole) -> alloc::vec::Vec<PartitionInfo> {
    let mut result = alloc::vec::Vec::new();
    for i in 0..partition_count() {
        if let Some(info) = get_partition(i) {
            if info.role == role {
                result.push(info);
            }
        }
    }
    result
}
