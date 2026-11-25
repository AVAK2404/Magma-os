//! Read-Only FAT32 Filesystem Driver
//!
//! Implements a read-only FAT32 filesystem driver for Magma OS.
//! Supports multi-volume detection and basic file operations.

use crate::ata;
use crate::gpt::{PartitionInfo, PartitionTableType, get_partition, get_partition_table_type, partition_count};
use crate::serial;
use alloc::vec::Vec;
use alloc::string::String;

/// FAT32 BIOS Parameter Block (BPB) structure
/// Located at the beginning of the FAT32 volume
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32Bpb {
    /// Jump instruction (3 bytes)
    pub jmp_boot: [u8; 3],
    /// OEM name (8 bytes)
    pub oem_name: [u8; 8],
    /// Bytes per sector (usually 512)
    pub bytes_per_sector: u16,
    /// Sectors per cluster (1, 2, 4, 8, 16, 32, 64, 128)
    pub sectors_per_cluster: u8,
    /// Reserved sectors (usually 32 for FAT32)
    pub reserved_sectors: u16,
    /// Number of FATs (usually 2)
    pub num_fats: u8,
    /// Root directory entries (0 for FAT32)
    pub root_dir_entries: u16,
    /// Total sectors (16-bit, 0 if using 32-bit)
    pub total_sectors_16: u16,
    /// Media descriptor byte
    pub media_descriptor: u8,
    /// Sectors per FAT (16-bit, 0 if using 32-bit)
    pub sectors_per_fat_16: u16,
    /// Sectors per track
    pub sectors_per_track: u16,
    /// Number of heads
    pub num_heads: u16,
    /// Hidden sectors
    pub hidden_sectors: u32,
    /// Total sectors (32-bit)
    pub total_sectors_32: u32,
    /// Sectors per FAT (32-bit)
    pub sectors_per_fat_32: u32,
    /// Flags
    pub flags: u16,
    /// FAT version (0x00 for FAT32)
    pub fat_version: u16,
    /// Root directory cluster
    pub root_cluster: u32,
    /// FSInfo sector
    pub fsinfo_sector: u16,
    /// Backup boot sector
    pub backup_boot_sector: u16,
    /// Reserved (12 bytes)
    pub reserved: [u8; 12],
    /// Drive number
    pub drive_number: u8,
    /// Reserved (1 byte)
    pub reserved_nt: u8,
    /// Extended boot signature (0x29)
    pub extended_boot_sig: u8,
    /// Volume serial number
    pub volume_serial: u32,
    /// Volume label (11 bytes)
    pub volume_label: [u8; 11],
    /// Filesystem type (8 bytes, "FAT32   ")
    pub fs_type: [u8; 8],
}

/// FAT32 directory entry structure (32 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32DirEntry {
    /// Short filename (8 bytes)
    pub name: [u8; 8],
    /// Extension (3 bytes)
    pub ext: [u8; 3],
    /// Attributes
    pub attributes: u8,
    /// Reserved (NT)
    pub reserved_nt: u8,
    /// Creation time (tenths of second)
    pub creation_time_tenths: u8,
    /// Creation time
    pub creation_time: u16,
    /// Creation date
    pub creation_date: u16,
    /// Last access date
    pub last_access_date: u16,
    /// High 16 bits of first cluster
    pub first_cluster_hi: u16,
    /// Last modification time
    pub last_mod_time: u16,
    /// Last modification date
    pub last_mod_date: u16,
    /// Low 16 bits of first cluster
    pub first_cluster_lo: u16,
    /// File size in bytes
    pub file_size: u32,
}

/// File attributes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FatAttribute {
    ReadOnly = 0x01,
    Hidden = 0x02,
    System = 0x04,
    VolumeId = 0x08,
    Directory = 0x10,
    Archive = 0x20,
    LongFileName = 0x0F,
}

/// Long filename directory entry
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
struct LongFileNameEntry {
    /// Sequence number (high bit = last entry, low 6 bits = sequence)
    pub sequence: u8,
    /// First 5 characters of name (UTF-16 LE)
    pub name1: [u8; 10],
    /// Attributes (always 0x0F for LFN)
    pub attributes: u8,
    /// Reserved
    pub reserved: u8,
    /// Checksum of short name
    pub checksum: u8,
    /// Next 6 characters of name (UTF-16 LE)
    pub name2: [u8; 12],
    /// Reserved
    pub reserved2: u16,
    /// Last 2 characters of name (UTF-16 LE)
    pub name3: [u8; 4],
}

/// FAT32 Volume structure
#[derive(Debug, Clone)]
pub struct Fat32Volume {
    /// Starting LBA of this volume
    pub start_lba: u64,
    /// BIOS Parameter Block
    pub bpb: Fat32Bpb,
    /// FAT cache (will be populated on demand)
    fat_cache: Vec<u32>,
    /// Cluster size in bytes
    cluster_size: u32,
    /// First data sector
    first_data_sector: u64,
    /// First FAT sector
    first_fat_sector: u64,
}

impl Fat32Volume {
    /// Create a new FAT32 volume from a partition
    pub fn from_partition(info: &PartitionInfo) -> Option<Self> {
        // Read the boot sector (first sector of the partition)
        let boot_sector = match ata::read_sectors(info.start_lba as u32, 1) {
            Some(data) => data,
            None => {
                serial::write_str_safe("[FAT32] Failed to read boot sector\r\n");
                return None;
            }
        };

        // Parse BPB from boot sector
        let bpb: &Fat32Bpb = unsafe { &*(boot_sector.as_ptr() as *const Fat32Bpb) };

        // Validate FAT32 signature
        if bpb.fs_type != *b"FAT32   " {
            serial::write_str_safe("[FAT32] Invalid FS type: ");
            for b in &bpb.fs_type {
                serial::write_byte(*b);
            }
            serial::write_str_safe("\r\n");
            return None;
        }

        // Validate extended boot signature
        if bpb.extended_boot_sig != 0x29 {
            serial::write_str_safe("[FAT32] Invalid extended boot signature\r\n");
            return None;
        }

        let cluster_size = bpb.sectors_per_cluster as u32 * bpb.bytes_per_sector as u32;
        let reserved_sectors = bpb.reserved_sectors as u64;
        let num_fats = bpb.num_fats as u64;
        let sectors_per_fat = bpb.sectors_per_fat_32 as u64;

        let first_fat_sector = info.start_lba + reserved_sectors;
        let first_data_sector = first_fat_sector + (num_fats * sectors_per_fat);

        serial::write_str_safe("[FAT32] Volume found at LBA ");
        print_u64(info.start_lba);
        serial::write_str_safe("\r\n");
        serial::write_str_safe("  Bytes per sector: ");
        print_u64(bpb.bytes_per_sector as u64);
        serial::write_str_safe("\r\n");
        serial::write_str_safe("  Sectors per cluster: ");
        print_u64(bpb.sectors_per_cluster as u64);
        serial::write_str_safe("\r\n");
        serial::write_str_safe("  Reserved sectors: ");
        print_u64(bpb.reserved_sectors as u64);
        serial::write_str_safe("\r\n");
        serial::write_str_safe("  Root cluster: ");
        print_u64(bpb.root_cluster as u64);
        serial::write_str_safe("\r\n");

        Some(Fat32Volume {
            start_lba: info.start_lba,
            bpb: *bpb,
            fat_cache: Vec::new(),
            cluster_size,
            first_data_sector,
            first_fat_sector,
        })
    }

    /// Read a cluster from the volume
    pub fn read_cluster(&self, cluster: u32) -> Option<Vec<u8>> {
        if cluster < 2 {
            return None; // Cluster 0 and 1 are reserved
        }

        // Calculate sector offset from first data sector
        let cluster_offset = (cluster - 2) as u64 * self.bpb.sectors_per_cluster as u64;
        let sector = self.first_data_sector + cluster_offset;

        // Read all sectors in the cluster
        let sectors_per_cluster = self.bpb.sectors_per_cluster as u32;
        let mut data = Vec::with_capacity(self.cluster_size as usize);

        for i in 0..sectors_per_cluster {
            let sector_data = match ata::read_sectors((sector + i as u64) as u32, 1) {
                Some(d) => d,
                None => return None,
            };
            data.extend_from_slice(&sector_data);
        }

        Some(data)
    }

    /// Read the next cluster in the FAT chain
    pub fn read_fat_entry(&self, cluster: u32) -> Option<u32> {
        // Calculate FAT sector and offset
        let fat_offset = cluster as u64 * 4; // Each FAT entry is 4 bytes
        let sector_offset = fat_offset / 512; // 512 bytes per sector
        let byte_offset = (fat_offset % 512) as usize;

        let fat_sector = self.first_fat_sector + sector_offset;

        // Read the FAT sector
        let sector_data = match ata::read_sectors(fat_sector as u32, 1) {
            Some(d) => d,
            None => return None,
        };

        // Read the 4-byte entry
        if byte_offset + 4 > sector_data.len() {
            return None;
        }

        let entry_bytes = &sector_data[byte_offset..byte_offset + 4];
        let entry = u32::from_le_bytes(entry_bytes.try_into().unwrap());

        // Mask out the high 4 bits (FAT32 uses 28 bits)
        let entry = entry & 0x0FFFFFFF;

        // Check for special values
        match entry {
            0x0FFFFFF8..=0x0FFFFFFF => None, // End of file
            0x0FFFFFF7 => None, // Bad cluster
            _ => Some(entry),
        }
    }

    /// Read a file by following the FAT chain
    pub fn read_file(&self, start_cluster: u32, size: u32) -> Option<Vec<u8>> {
        let mut data = Vec::with_capacity(size as usize);
        let mut current_cluster = start_cluster;
        let mut remaining = size;

        loop {
            if remaining == 0 {
                break;
            }

            // Read current cluster
            let cluster_data = match self.read_cluster(current_cluster) {
                Some(d) => d,
                None => break,
            };

            // Determine how much to copy from this cluster
            let to_copy = core::cmp::min(remaining, cluster_data.len() as u32);
            data.extend_from_slice(&cluster_data[..to_copy as usize]);
            remaining -= to_copy;

            // Get next cluster
            current_cluster = match self.read_fat_entry(current_cluster) {
                Some(c) => c,
                None => break,
            };
        }

        Some(data)
    }

    /// List directory contents
    pub fn list_dir(&self, cluster: u32) -> Option<Vec<Fat32DirEntry>> {
        let mut entries = Vec::new();
        let mut current_cluster = cluster;

        loop {
            // Read current cluster
            let cluster_data = match self.read_cluster(current_cluster) {
                Some(d) => d,
                None => break,
            };

            // Process directory entries (32 bytes each)
            for offset in (0..cluster_data.len()).step_by(32) {
                if offset + 32 > cluster_data.len() {
                    break;
                }

                let entry_bytes = &cluster_data[offset..offset + 32];
                let entry: &Fat32DirEntry = unsafe { &*(entry_bytes.as_ptr() as *const Fat32DirEntry) };

                // Check for end of directory
                if entry.name[0] == 0x00 {
                    return Some(entries);
                }

                // Skip deleted entries
                if entry.name[0] == 0xE5 {
                    continue;
                }

                // Skip long filename entries
                if entry.attributes == FatAttribute::LongFileName as u8 {
                    continue;
                }

                // Skip volume ID
                if entry.attributes & FatAttribute::VolumeId as u8 != 0 {
                    continue;
                }

                entries.push(*entry);
            }

            // Get next cluster
            current_cluster = match self.read_fat_entry(current_cluster) {
                Some(c) => c,
                None => break,
            };
        }

        Some(entries)
    }

    /// Find a file by name in a directory
    pub fn find_file(&self, dir_cluster: u32, filename: &str) -> Option<Fat32DirEntry> {
        let entries = match self.list_dir(dir_cluster) {
            Some(e) => e,
            None => return None,
        };

        // Convert filename to uppercase 8.3 format
        let (name, ext) = split_filename(filename);

        for entry in entries {
            if entry.attributes & FatAttribute::Directory as u8 != 0 {
                continue; // Skip directories
            }

            // Compare short name
            let entry_name = parse_short_name(&entry.name);
            let entry_ext = parse_short_name(&entry.ext);

            if entry_name == name && entry_ext == ext {
                return Some(entry);
            }
        }

        None
    }

    /// Find a directory by name
    pub fn find_dir(&self, parent_cluster: u32, dirname: &str) -> Option<Fat32DirEntry> {
        let entries = match self.list_dir(parent_cluster) {
            Some(e) => e,
            None => return None,
        };

        let name = dirname.to_uppercase();

        for entry in entries {
            if entry.attributes & FatAttribute::Directory as u8 == 0 {
                continue; // Skip files
            }

            // Compare short name
            let entry_name = parse_short_name(&entry.name);

            if entry_name == name {
                return Some(entry);
            }
        }

        None
    }

    /// Get the first cluster of a directory entry
    pub fn get_first_cluster(&self, entry: &Fat32DirEntry) -> u32 {
        ((entry.first_cluster_hi as u32) << 16) | (entry.first_cluster_lo as u32)
    }
}

/// Split a filename into name and extension (8.3 format)
fn split_filename(filename: &str) -> (String, String) {
    let parts: Vec<&str> = filename.split('.').collect();

    if parts.len() > 1 {
        let name = parts[..parts.len() - 1].join(".").to_uppercase();
        let ext = parts[parts.len() - 1].to_uppercase();
        (name, ext)
    } else {
        (filename.to_uppercase(), String::new())
    }
}

/// Parse 8-byte short filename to string
fn parse_short_name(bytes: &[u8]) -> String {
    let mut result = String::new();
    for &b in bytes {
        if b == b' ' {
            break;
        }
        if b >= 32 && b < 127 {
            result.push(b as char);
        }
    }
    result
}

/// Global volume storage
static mut VOLUMES: [Option<Fat32Volume>; 16] = [const { None }; 16];
static mut VOLUME_ROLES: [crate::gpt::PartitionRole; 16] = [const { crate::gpt::PartitionRole::Unknown }; 16];
static mut VOLUME_COUNT: usize = 0;

/// Initialize FAT32 volumes from detected partitions
pub fn init() -> Result<(), &'static str> {
    let table_type = get_partition_table_type();

    match table_type {
        PartitionTableType::Superfloppy => {
            // Entire disk is one FAT32 volume
            serial::write_str_safe("[FAT32] Superfloppy format - single volume\r\n");

            let info = PartitionInfo {
                start_lba: 0,
                sector_count: 0, // Will use entire disk
                role: crate::gpt::PartitionRole::Data,
                is_gpt: false,
            };

            if let Some(volume) = Fat32Volume::from_partition(&info) {
                unsafe {
                    VOLUMES[0] = Some(volume);
                    VOLUME_ROLES[0] = info.role;
                    VOLUME_COUNT = 1;
                }
                Ok(())
            } else {
                Err("Failed to initialize Superfloppy volume")
            }
        }
        PartitionTableType::Mbr | PartitionTableType::Gpt => {
            // Scan all partitions for FAT32 volumes
            let mut count = 0;

            for i in 0..partition_count() {
                if let Some(info) = get_partition(i) {
                    if let Some(volume) = Fat32Volume::from_partition(&info) {
                        unsafe {
                            VOLUMES[count] = Some(volume);
                            VOLUME_ROLES[count] = info.role;
                            count += 1;
                            if count >= 16 {
                                break;
                            }
                        }
                    }
                }
            }

            unsafe {
                VOLUME_COUNT = count;
            }

            serial::write_str_safe("[FAT32] Found ");
            print_u64(count as u64);
            serial::write_str_safe(" FAT32 volume(s)\r\n");

            Ok(())
        }
        PartitionTableType::Invalid => {
            Err("No partition table detected")
        }
    }
}

/// Get volume count
pub fn volume_count() -> usize {
    unsafe { VOLUME_COUNT }
}

/// Get volume by index
pub fn get_volume(index: usize) -> Option<&'static Fat32Volume> {
    unsafe {
        VOLUMES[index].as_ref()
    }
}

/// Get partition role for a volume by index
pub fn get_volume_role(index: usize) -> crate::gpt::PartitionRole {
    unsafe {
        if index < VOLUME_COUNT {
            VOLUME_ROLES[index]
        } else {
            crate::gpt::PartitionRole::Unknown
        }
    }
}

/// List files in a directory on a specific volume
pub fn list_dir(volume_idx: usize, path: &str) -> Option<Vec<Fat32DirEntry>> {
    let volume = get_volume(volume_idx)?;

    // Parse path to get directory cluster
    let cluster = match resolve_path(volume, path) {
        Some(c) => c,
        None => return None,
    };

    volume.list_dir(cluster)
}

/// Read a file from a specific volume
pub fn read_file(volume_idx: usize, path: &str) -> Option<Vec<u8>> {
    let volume = get_volume(volume_idx)?;

    // Resolve path to parent directory cluster and filename
    let (parent_cluster, filename) = resolve_file_path(volume, path)?;

    // Find the file
    let entry = match volume.find_file(parent_cluster, &filename) {
        Some(e) => e,
        None => return None,
    };

    let start_cluster = volume.get_first_cluster(&entry);
    let size = entry.file_size;

    volume.read_file(start_cluster, size)
}

/// Resolve a path to a cluster number (for directory listing)
fn resolve_path(volume: &Fat32Volume, path: &str) -> Option<u32> {
    let parts = path.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>();

    let mut current_cluster = volume.bpb.root_cluster;

    for part in parts {
        let dirname = part.to_uppercase();

        // Try to find directory
        let entry = match volume.find_dir(current_cluster, &dirname) {
            Some(e) => e,
            None => return None,
        };

        current_cluster = volume.get_first_cluster(&entry);
    }

    Some(current_cluster)
}

/// Resolve a file path to its parent directory cluster and filename
fn resolve_file_path(volume: &Fat32Volume, path: &str) -> Option<(u32, String)> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return None;
    }

    let filename = parts[parts.len() - 1].to_uppercase();

    // If there's only one part, the file is in the root directory
    if parts.len() == 1 {
        return Some((volume.bpb.root_cluster, filename));
    }

    // For paths with directories, resolve the parent directory
    let mut current_cluster = volume.bpb.root_cluster;
    for part in &parts[..parts.len() - 1] {
        let dirname = part.to_uppercase();

        // Try to find directory
        let entry = match volume.find_dir(current_cluster, &dirname) {
            Some(e) => e,
            None => return None,
        };

        current_cluster = volume.get_first_cluster(&entry);
    }

    Some((current_cluster, filename))
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

/// File information structure for directory listing
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub ext: String,
    pub size: u32,
    pub is_directory: bool,
    pub attributes: u8,
}

impl FileInfo {
    pub fn from_dir_entry(entry: &Fat32DirEntry) -> Self {
        FileInfo {
            name: parse_short_name(&entry.name),
            ext: parse_short_name(&entry.ext),
            size: entry.file_size,
            is_directory: entry.attributes & FatAttribute::Directory as u8 != 0,
            attributes: entry.attributes,
        }
    }
}

/// Get file info for all files in a directory
pub fn get_file_info(volume_idx: usize, path: &str) -> Option<Vec<FileInfo>> {
    let entries = list_dir(volume_idx, path)?;

    Some(entries.iter().map(|e| FileInfo::from_dir_entry(e)).collect())
}
