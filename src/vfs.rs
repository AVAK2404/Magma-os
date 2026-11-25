//! Virtual Filesystem (VFS) Layer
//!
//! Provides a unified interface for filesystem operations across multiple
//! filesystem types (FAT32, etc.) with mount point management.

use crate::ata;
use crate::fat32::{self, FileInfo};
use crate::gpt::{PartitionInfo, get_partition, partition_count};
use crate::serial;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Maximum number of mount points
const MAX_MOUNTS: usize = 16;

/// Mount point structure
#[derive(Debug, Clone)]
pub struct MountPoint {
    /// Mount path (e.g., "/", "/boot", "/data")
    pub path: String,
    /// Volume index in the underlying filesystem
    pub volume_idx: usize,
    /// Filesystem type
    pub fs_type: FilesystemType,
}

/// Filesystem type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemType {
    Fat32,
    Unknown,
}

/// Global mount table
static mut MOUNTS: [Option<MountPoint>; MAX_MOUNTS] = [const { None }; MAX_MOUNTS];
static mut MOUNT_COUNT: usize = 0;

/// Check if a path has an explicit mount point
fn has_explicit_mount(path: &str) -> bool {
    unsafe {
        for i in 0..MOUNT_COUNT {
            if let Some(mount) = &MOUNTS[i] {
                if mount.path == path {
                    return true;
                }
            }
        }
    }
    false
}

/// Initialize VFS
/// Auto-detects and mounts all available FAT32 volumes
pub fn init() -> Result<(), &'static str> {
    // Initialize FAT32 first
    match fat32::init() {
        Ok(_) => {}
        Err(e) => {
            serial::write_str_safe("[VFS] FAT32 init failed: ");
            serial::write_str_safe(e);
            serial::write_str_safe("\r\n");
        }
    }

    // Auto-mount all FAT32 volumes with role-based paths
    // Volume 0 (BootEsp) -> /efi
    // Volume 1 (RecoveryVault) -> /recovery
    // Volume N (Data or others) -> /volN
    let vol_count = fat32::volume_count();

    for i in 0..vol_count {
        let role = fat32::get_volume_role(i);

        // Determine mount path based on partition index
        // Volume 0 always mounts at /efi, Volume 1 always at /recovery
        let mount_path = match i {
            0 => "/efi".to_string(),
            1 => "/recovery".to_string(),
            _ => format!("/vol{}", i),
        };

        let mount = MountPoint {
            path: mount_path.clone(),
            volume_idx: i,
            fs_type: FilesystemType::Fat32,
        };

        unsafe {
            if MOUNT_COUNT < MAX_MOUNTS {
                MOUNTS[MOUNT_COUNT] = Some(mount);
                MOUNT_COUNT += 1;

                serial::write_str_safe("[VFS] Mounted FAT32 volume ");
                print_u64(i as u64);
                serial::write_str_safe(" at ");
                serial::write_str_safe(&mount_path);
                match role {
                    crate::gpt::PartitionRole::BootEsp => serial::write_str_safe(" (ESP)\r\n"),
                    crate::gpt::PartitionRole::RecoveryVault => serial::write_str_safe(" (Recovery)\r\n"),
                    _ => serial::write_str_safe("\r\n"),
                }
            }
        }
    }

    let mount_count = unsafe { MOUNT_COUNT };
    if mount_count == 0 {
        serial::write_str_safe("[VFS] No filesystems mounted\r\n");
    }

    Ok(())
}

/// Find a mount point by path with proper boundary matching
fn find_mount(path: &str) -> Option<(usize, &str)> {
    unsafe {
        for i in 0..MOUNT_COUNT {
            if let Some(mount) = &MOUNTS[i] {
                // Check for exact match first
                if path == mount.path {
                    return Some((i, &path[mount.path.len()..])); // Empty relative path
                }
                // Check if path starts with mount path followed by '/' for proper boundary matching
                if path.starts_with(&mount.path) && (path.len() == mount.path.len() || path.as_bytes().get(mount.path.len()) == Some(&b'/')) {
                    let relative_path = &path[mount.path.len()..];
                    return Some((i, relative_path));
                }
            }
        }
    }
    None
}

/// List directory contents
/// Returns auto-generated entries for "/" when no explicit root mount exists
pub fn list_dir(path: &str) -> Option<Vec<FileInfo>> {
    // Special handling for root path when no explicit "/" mount exists
    if path == "/" {
        let has_root = has_explicit_mount("/");
        if !has_root {
            // Return auto-generated directory entries for mounted volumes
            let mut entries = Vec::new();

            unsafe {
                for i in 0..MOUNT_COUNT {
                    if let Some(mount) = &MOUNTS[i] {
                        let (name, is_dir) = if mount.path == "/efi" {
                            ("EFI", true)
                        } else if mount.path == "/recovery" {
                            ("RECOVERY", true)
                        } else {
                            // Parse volume name from path like "/vol0" -> "vol0"
                            let vol_name = &mount.path[1..]; // Skip leading '/'
                            (vol_name, true)
                        };

                        entries.push(FileInfo {
                            name: name.to_string(),
                            ext: String::new(),
                            size: 0,
                            is_directory: is_dir,
                            attributes: if is_dir { 0x10 } else { 0x20 }, // Directory or Archive
                        });
                    }
                }
            }

            if entries.is_empty() {
                return None;
            }

            // Sort entries by name for consistency
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            return Some(entries);
        }
    }

    let (mount_idx, relative_path) = find_mount(path)?;

    let mount = unsafe { MOUNTS[mount_idx].as_ref()? };

    match mount.fs_type {
        FilesystemType::Fat32 => {
            fat32::get_file_info(mount.volume_idx, relative_path)
        }
        FilesystemType::Unknown => None,
    }
}

/// Read a file
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    let (mount_idx, relative_path) = find_mount(path)?;

    let mount = unsafe { MOUNTS[mount_idx].as_ref()? };

    match mount.fs_type {
        FilesystemType::Fat32 => {
            fat32::read_file(mount.volume_idx, relative_path)
        }
        FilesystemType::Unknown => None,
    }
}

/// Read file with fallback to other volumes
pub fn read_with_fallback(path: &str) -> Option<Vec<u8>> {
    // Try exact path first
    if let Some(data) = read_file(path) {
        return Some(data);
    }

    // Try all mounted volumes
    unsafe {
        for i in 0..MOUNT_COUNT {
            if let Some(mount) = &MOUNTS[i] {
                if let Some(data) = fat32::read_file(mount.volume_idx, path) {
                    return Some(data);
                }
            }
        }
    }

    None
}

/// Get mount point count
pub fn mount_count() -> usize {
    unsafe { MOUNT_COUNT }
}

/// Get mount point by index
pub fn get_mount(index: usize) -> Option<MountPoint> {
    unsafe { MOUNTS[index].clone() }
}

/// Mount a filesystem at a path
pub fn mount(path: &str, volume_idx: usize, fs_type: FilesystemType) -> Result<(), &'static str> {
    unsafe {
        if MOUNT_COUNT >= MAX_MOUNTS {
            return Err("Maximum mount points reached");
        }

        let mount = MountPoint {
            path: path.to_string(),
            volume_idx,
            fs_type,
        };

        MOUNTS[MOUNT_COUNT] = Some(mount);
        MOUNT_COUNT += 1;

        Ok(())
    }
}

/// Unmount a filesystem
pub fn unmount(path: &str) -> Result<(), &'static str> {
    unsafe {
        for i in 0..MOUNT_COUNT {
            if let Some(mount) = &MOUNTS[i] {
                if mount.path == path {
                    // Remove by shifting
                    for j in i..MOUNT_COUNT - 1 {
                        MOUNTS[j] = MOUNTS[j + 1].take();
                    }
                    MOUNT_COUNT -= 1;
                    return Ok(());
                }
            }
        }
        Err("Mount point not found")
    }
}

/// CRC32 checksum calculation (IEEE standard)
pub fn crc32(data: &[u8]) -> u32 {
    const CRC32_TABLE: [u32; 256] = generate_crc32_table();

    let mut crc = 0xFFFFFFFF_u32;

    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }

    !crc
}

/// Generate CRC32 lookup table
const fn generate_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;

    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;

        while j < 8 {
            if crc & 1 != 0 {
                crc = 0xEDB88320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }

        table[i] = crc;
        i += 1;
    }

    table
}

/// Verify ATA read functionality
pub fn verify_ata_read() -> bool {
    // Try to read sector 0
    match ata::read_sectors(0, 1) {
        Some(data) => {
            // Check for valid MBR signature
            let sig = u16::from_le_bytes([data[510], data[511]]);
            if sig == 0xAA55 {
                serial::write_str_safe("[VFS] ATA read verified: MBR signature found\r\n");
                true
            } else {
                serial::write_str_safe("[VFS] ATA read verified: Data read but no MBR signature\r\n");
                true
            }
        }
        None => {
            serial::write_str_safe("[VFS] ATA read failed\r\n");
            false
        }
    }
}

/// Get partition information for all detected partitions
pub fn get_all_partitions() -> Vec<PartitionInfo> {
    let mut partitions = Vec::new();

    for i in 0..partition_count() {
        if let Some(info) = get_partition(i) {
            partitions.push(info);
        }
    }

    partitions
}

/// Get filesystem type for a path
pub fn get_filesystem_type(path: &str) -> Option<FilesystemType> {
    let (mount_idx, _) = find_mount(path)?;
    let mount = unsafe { MOUNTS[mount_idx].as_ref()? };
    Some(mount.fs_type)
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

/// Check if a path exists
pub fn exists(path: &str) -> bool {
    read_file(path).is_some()
}

/// Check if a directory exists
pub fn directory_exists(path: &str) -> bool {
    // For now, we'll use the same logic as exists since FAT32 doesn't distinguish
    // between files and directories in the same way. We'll check if the path
    // can be listed as a directory.
    // Try to list directory contents - if it succeeds, it's a directory
    list_dir(path).is_some()
}

/// Get file size
pub fn file_size(path: &str) -> Option<u32> {
    let (mount_idx, relative_path) = find_mount(path)?;

    let mount = unsafe { MOUNTS[mount_idx].as_ref()? };

    match mount.fs_type {
        FilesystemType::Fat32 => {
            if let Some(entries) = fat32::get_file_info(mount.volume_idx, relative_path) {
                for entry in entries {
                    if entry.name == path || format!("{}.{}", entry.name, entry.ext) == path {
                        return Some(entry.size);
                    }
                }
            }
            None
        }
        FilesystemType::Unknown => None,
    }
}

/// Get all mounted paths
pub fn get_mount_paths() -> Vec<String> {
    let mut paths = Vec::new();

    unsafe {
        for i in 0..MOUNT_COUNT {
            if let Some(mount) = &MOUNTS[i] {
                paths.push(mount.path.clone());
            }
        }
    }

    paths
}
