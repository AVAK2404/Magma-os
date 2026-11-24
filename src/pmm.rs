//! Physical Memory Manager - Bitmap allocator for 4KB frames
//!
//! Uses a bitmap where each bit represents one 4KB physical frame.
//! 0 = free, 1 = allocated/reserved.
//!
//! Features:
//! - Memory-map ownership table with region classification
//! - DMA-capable contiguous frame allocation
//! - Generic range reservation for MMIO regions

use crate::sync::SpinLock;
use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

pub const PAGE_SIZE: u64 = 4096;

/// Memory region type classification from Limine memory map
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
#[allow(dead_code)]
pub enum RegionType {
    Usable = 0,
    Reserved = 1,
    ACPIReclaimable = 2,
    ACPINVS = 3,
    BadMemory = 4,
    BootloaderReclaimable = 5,
    ExecutableAndModules = 6,
    Framebuffer = 7,
    MappedReserved = 8,
    Unknown(u64),
}

impl RegionType {
    #[allow(dead_code)]
    fn from_limine(type_: u64) -> Self {
        match type_ {
            limine::memmap::MEMMAP_USABLE => RegionType::Usable,
            limine::memmap::MEMMAP_RESERVED => RegionType::Reserved,
            limine::memmap::MEMMAP_ACPI_RECLAIMABLE => RegionType::ACPIReclaimable,
            limine::memmap::MEMMAP_ACPI_NVS => RegionType::ACPINVS,
            limine::memmap::MEMMAP_BAD_MEMORY => RegionType::BadMemory,
            limine::memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => RegionType::BootloaderReclaimable,
            limine::memmap::MEMMAP_EXECUTABLE_AND_MODULES => RegionType::ExecutableAndModules,
            limine::memmap::MEMMAP_FRAMEBUFFER => RegionType::Framebuffer,
            limine::memmap::MEMMAP_MAPPED_RESERVED => RegionType::MappedReserved,
            _ => RegionType::Unknown(type_),
        }
    }

    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            RegionType::Usable => "Usable",
            RegionType::Reserved => "Reserved",
            RegionType::ACPIReclaimable => "ACPIReclaimable",
            RegionType::ACPINVS => "ACPINVS",
            RegionType::BadMemory => "BadMemory",
            RegionType::BootloaderReclaimable => "BootloaderReclaimable",
            RegionType::ExecutableAndModules => "ExecutableAndModules",
            RegionType::Framebuffer => "Framebuffer",
            RegionType::MappedReserved => "MappedReserved",
            RegionType::Unknown(_) => "Unknown",
        }
    }
}

/// Memory region ownership entry
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub region_type: RegionType,
}

/// Maximum number of memory regions tracked
const MAX_REGIONS: usize = 64;

/// Memory region entry for ownership table
#[derive(Copy, Clone)]
struct RegionEntry {
    base: u64,
    length: u64,
    region_type: RegionType,
}

impl Default for RegionEntry {
    fn default() -> Self {
        RegionEntry {
            base: 0,
            length: 0,
            region_type: RegionType::Unknown(0),
        }
    }
}

/// DMA-capable memory region info
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DmaRegion {
    pub phys_addr: u64,
    pub virt_addr: u64,
    pub frame_count: usize,
}

impl DmaRegion {
    pub fn byte_len(&self) -> u64 {
        (self.frame_count as u64) * PAGE_SIZE
    }
}

/// Bitmap state structure protected by SpinLock
struct BitmapState {
    data: *mut u8,
    size: usize,
    total_frames: usize,
    free_frames: usize,
    hhdm_offset: u64,
    region_count: AtomicUsize,
    regions: [RegionEntry; MAX_REGIONS],
}

unsafe impl Send for BitmapState {}

impl Default for BitmapState {
    fn default() -> Self {
        Self::new()
    }
}

impl BitmapState {
    pub const fn new() -> Self {
        // Use a const-compatible initialization for the array
        const DEFAULT_ENTRY: RegionEntry = RegionEntry {
            base: 0,
            length: 0,
            region_type: RegionType::Unknown(0),
        };
        const EMPTY_REGIONS: [RegionEntry; MAX_REGIONS] = [DEFAULT_ENTRY; MAX_REGIONS];

        BitmapState {
            data: core::ptr::null_mut(),
            size: 0,
            total_frames: 0,
            free_frames: 0,
            hhdm_offset: 0,
            region_count: AtomicUsize::new(0),
            regions: EMPTY_REGIONS,
        }
    }

    /// Add a region to the ownership table
    fn add_region(&mut self, entry: MemoryRegion) {
        let count = self.region_count.load(AtomicOrdering::Relaxed);
        if count < MAX_REGIONS {
            self.regions[count] = RegionEntry {
                base: entry.base,
                length: entry.length,
                region_type: entry.region_type,
            };
            self.region_count.store(count + 1, AtomicOrdering::Relaxed);
        }
    }
}

static BITMAP_STATE: SpinLock<BitmapState> = SpinLock::new(BitmapState::new());

/// Get the HHDM offset from initialized state
pub fn hhdm_offset() -> u64 {
    BITMAP_STATE.lock().hhdm_offset
}

/// Initialize the PMM using Limine memory map
pub fn init(entries: &[&limine::memmap::Entry], hhdm_offset: u64) {
    let entries = entries;

    // Find highest physical address among usable regions (type=0 = MEMMAP_USABLE)
    let mut highest_usable_addr: u64 = 0;

    for entry in entries {
        if entry.type_ == limine::memmap::MEMMAP_USABLE {
            let end = entry.base + entry.length;
            if end > highest_usable_addr {
                highest_usable_addr = end;
            }
        }
    }

    // Compute total frames and bitmap size
    let total_frames = (highest_usable_addr / PAGE_SIZE) as usize;
    let bitmap_bytes = total_frames.div_ceil(8);

    // Find first usable region at or above 1 MiB with enough space for bitmap
    let mut bitmap_phys: u64 = 0;
    for entry in entries {
        if entry.type_ != limine::memmap::MEMMAP_USABLE {
            continue;
        }

        if entry.base < 0x10_0000 {
            continue;
        }

        if entry.length >= bitmap_bytes as u64 {
            bitmap_phys = entry.base;
            break;
        }
    }

    if bitmap_phys == 0 {
        return;
    }

    // Virtual address of bitmap via HHDM
    let bitmap_virt = (bitmap_phys + hhdm_offset) as *mut u8;

    unsafe {
        let bitmap_slice = core::slice::from_raw_parts_mut(bitmap_virt, bitmap_bytes);

        // Fill all bits to 1 (all reserved)
        bitmap_slice.fill(0xFF);

        // Clear bits to 0 for every 4 KiB frame inside usable regions
        for entry in entries {
            if entry.type_ != limine::memmap::MEMMAP_USABLE {
                continue;
            }

            let mut frame_start = entry.base & !(PAGE_SIZE - 1);
            let frame_end = entry.base + entry.length;

            while frame_start < frame_end {
                let frame_idx = (frame_start / PAGE_SIZE) as usize;
                let byte_idx = frame_idx / 8;
                let bit_idx = frame_idx % 8;
                if byte_idx < bitmap_bytes {
                    bitmap_slice[byte_idx] &= !(1 << bit_idx);
                }
                frame_start += PAGE_SIZE;
            }
        }

        // Mark frame 0 as used
        set_frame_used(0, bitmap_slice, total_frames);

        // Mark all frames covering [bitmap_phys, bitmap_phys + bitmap_bytes) as used
        let bitmap_frame_count = bitmap_bytes.div_ceil(PAGE_SIZE as usize);
        for i in 0..bitmap_frame_count {
            let frame = (bitmap_phys / PAGE_SIZE + i as u64) as usize;
            set_frame_used(frame, bitmap_slice, total_frames);
        }

        // Count free frames: count 0 bits (free) in bitmap
        let mut free_count = 0usize;
        for &byte in bitmap_slice.iter() {
            free_count += (8 - byte.count_ones()) as usize;
        }

        // Store state
        let mut state = BITMAP_STATE.lock();
        state.data = bitmap_virt;
        state.size = bitmap_bytes;
        state.total_frames = total_frames;
        state.free_frames = free_count;
        state.hhdm_offset = hhdm_offset;

        // Populate memory-map ownership table
        for entry in entries {
            let mem_entry = MemoryRegion {
                base: entry.base,
                length: entry.length,
                region_type: RegionType::from_limine(entry.type_),
            };
            state.add_region(mem_entry);
        }
    }
}

/// Helper to set a frame as used
unsafe fn set_frame_used(frame_idx: usize, bitmap: &mut [u8], total_frames: usize) {
    if frame_idx >= total_frames {
        return;
    }
    let byte_idx = frame_idx / 8;
    let bit_idx = frame_idx % 8;
    if byte_idx < bitmap.len() {
        bitmap[byte_idx] |= 1 << bit_idx;
    }
}

/// Allocate a single 4KB frame
pub fn alloc_frame() -> Option<u64> {
    unsafe {
        let mut state = BITMAP_STATE.lock();

        if state.free_frames == 0 || state.data.is_null() {
            return None;
        }

        for byte_idx in 0..state.size {
            let byte = *state.data.add(byte_idx);
            if byte != 0xFF {
                for bit_idx in 0..8 {
                    if (byte & (1 << bit_idx)) == 0 {
                        let frame_idx = byte_idx * 8 + bit_idx;
                        if frame_idx >= state.total_frames {
                            break;
                        }

                        state.data.add(byte_idx).write(byte | (1 << bit_idx));
                        state.free_frames -= 1;

                        let phys_addr = (frame_idx as u64) * PAGE_SIZE;
                        let virt_addr = phys_addr + state.hhdm_offset;

                        let frame_slice = core::slice::from_raw_parts_mut(virt_addr as *mut u8, PAGE_SIZE as usize);
                        frame_slice.fill(0);

                        return Some(phys_addr);
                    }
                }
            }
        }

        None
    }
}

/// Free a single 4KB frame
pub fn free_frame(phys_addr: u64) {
    unsafe {
        let mut state = BITMAP_STATE.lock();

        let frame_idx = (phys_addr / PAGE_SIZE) as usize;
        if frame_idx >= state.total_frames || state.data.is_null() {
            return;
        }

        let byte_idx = frame_idx / 8;
        let bit_idx = frame_idx % 8;

        if byte_idx < state.size {
            let byte = *state.data.add(byte_idx);
            if (byte & (1 << bit_idx)) != 0 {
                state.data.add(byte_idx).write(byte & !(1 << bit_idx));
                state.free_frames += 1;
            }
        }
    }
}

/// Get free and total frame counts
pub fn stats() -> (usize, usize) {
    let state = BITMAP_STATE.lock();
    (state.free_frames, state.total_frames)
}

/// Reserve a physical memory range as non-allocatable
/// Used for MMIO regions, reserved areas, etc.
/// Checks if frames are already allocated before marking them.
#[allow(dead_code)]
pub fn reserve_range(phys_addr: u64, len: u64) {
    unsafe {
        let mut state = BITMAP_STATE.lock();
        if state.data.is_null() {
            return;
        }

        let frame_start = phys_addr / PAGE_SIZE;
        let frame_end = (phys_addr + len + PAGE_SIZE - 1) / PAGE_SIZE; // Round up

        for frame_idx in frame_start..frame_end {
            if frame_idx >= state.total_frames as u64 {
                break;
            }
            let frame_idx = frame_idx as usize;
            let byte_idx = frame_idx / 8;
            let bit_idx = frame_idx % 8;

            if byte_idx < state.size {
                // Check if already allocated (bit set)
                let byte = *state.data.add(byte_idx);
                if (byte & (1 << bit_idx)) == 0 {
                    // Set the bit to mark frame as allocated (reserved)
                    state.data.add(byte_idx).write(byte | (1 << bit_idx));
                    state.free_frames = state.free_frames.saturating_sub(1);
                }
            }
        }
    }
}

/// Lookup the region type for an address
/// Returns None if the address is not covered by any tracked region
#[allow(dead_code)]
pub fn region_type_at(phys_addr: u64) -> Option<RegionType> {
    let state = BITMAP_STATE.lock();
    let count = state.region_count.load(AtomicOrdering::Relaxed);
    for i in 0..count {
        let entry = state.regions[i];
        if phys_addr >= entry.base && phys_addr < entry.base + entry.length {
            return Some(entry.region_type);
        }
    }
    None
}

/// Allocate contiguous frames with optional alignment
/// Returns a DmaRegion with phys_addr, virt_addr, and frame_count
/// `align_frames` must be a power of 2, defaults to 1 (no alignment beyond frame boundary)
#[allow(dead_code)]
pub fn alloc_contiguous_frames(count: usize, align_frames: usize) -> Option<DmaRegion> {
    unsafe {
        let mut state = BITMAP_STATE.lock();

        if state.free_frames < count || state.data.is_null() {
            return None;
        }

        // Ensure alignment is power of 2 and at least 1
        let align = if align_frames > 0 {
            let aligned = align_frames.next_power_of_two();
            if aligned > count { aligned } else { align_frames.next_power_of_two().max(1) }
        } else {
            1
        };

        // Search for contiguous free frames
        let mut consecutive_free = 0usize;
        let mut candidate_start: Option<usize> = None;

        for byte_idx in 0..state.size {
            let byte = *state.data.add(byte_idx);

            // Check each bit in the byte
            for bit_idx in 0..8 {
                let frame_idx = byte_idx * 8 + bit_idx;
                if frame_idx >= state.total_frames {
                    break;
                }

                let is_allocated = (byte & (1 << bit_idx)) != 0;

                if !is_allocated {
                    if consecutive_free == 0 {
                        candidate_start = Some(frame_idx);
                    }
                    consecutive_free += 1;

                    if consecutive_free >= count {
                        // Found enough contiguous frames
                        let start_frame = candidate_start.unwrap();

                        // Align start to align_frames if needed
                        if align > 1 && start_frame % align != 0 {
                            // Try to find aligned region within the contiguous block
                            // Calculate how many frames we need after alignment
                            let misaligned_start = start_frame % align;
                            let aligned_start = start_frame + (align - misaligned_start);

                            // Check if we have enough frames after alignment
                            // We need count frames starting at aligned_start
                            if aligned_start + count <= start_frame + consecutive_free {
                                // Use aligned version - mark exactly 'count' frames
                                if mark_frames_allocated(&mut state, aligned_start, aligned_start + count) {
                                    let phys_addr = (aligned_start as u64) * PAGE_SIZE;
                                    let virt_addr = phys_addr + state.hhdm_offset;
                                    return Some(DmaRegion {
                                        phys_addr,
                                        virt_addr,
                                        frame_count: count,
                                    });
                                }
                            }
                        }

                        // No alignment needed or alignment failed within this region
                        // Mark exactly 'count' frames starting at start_frame
                        if mark_frames_allocated(&mut state, start_frame, start_frame + count) {
                            let phys_addr = (start_frame as u64) * PAGE_SIZE;
                            let virt_addr = phys_addr + state.hhdm_offset;
                            return Some(DmaRegion {
                                phys_addr,
                                virt_addr,
                                frame_count: count,
                            });
                        }
                    }
                } else {
                    // Reset on allocated frame
                    consecutive_free = 0;
                    candidate_start = None;
                }
            }
        }

        None
    }
}

/// Mark frames as allocated (helper function)
unsafe fn mark_frames_allocated(state: &mut BitmapState, start_frame: usize, end_frame: usize) -> bool {
    if start_frame >= state.total_frames {
        return false;
    }

    for frame_idx in start_frame..end_frame {
        let byte_idx = frame_idx / 8;
        let bit_idx = frame_idx % 8;

        if byte_idx >= state.size {
            return false;
        }

        // Check if already allocated - need unsafe for raw pointer access
        if unsafe { (*state.data.add(byte_idx) & (1 << bit_idx)) != 0 } {
            return false;
        }

        unsafe {
            state.data.add(byte_idx).write(*state.data.add(byte_idx) | (1 << bit_idx));
        }
    }

    state.free_frames = state.free_frames.saturating_sub(end_frame.saturating_sub(start_frame));
    true
}

/// Free contiguous frames from a DMA region
#[allow(dead_code)]
pub fn free_contiguous_frames(region: DmaRegion) {
    unsafe {
        let mut state = BITMAP_STATE.lock();

        if state.data.is_null() {
            return;
        }

        let frame_start = region.phys_addr / PAGE_SIZE;
        let frame_end = frame_start + region.frame_count as u64;

        for frame_idx in frame_start..frame_end {
            if frame_idx as usize >= state.total_frames {
                break;
            }
            let frame_idx = frame_idx as usize;
            let byte_idx = frame_idx / 8;
            let bit_idx = frame_idx % 8;

            if byte_idx < state.size {
                // Clear the bit to mark frame as free
                state.data.add(byte_idx).write(*state.data.add(byte_idx) & !(1 << bit_idx));
                state.free_frames += 1;
            }
        }
    }
}