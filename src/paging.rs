//! 4-Level x86-64 Paging Mapper
//! Manages page tables, maps virtual to physical addresses, and invalidates TLB.

use core::arch::asm;
use crate::pmm;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const HUGE_PAGE: u64 = 1 << 7;
pub const NO_EXECUTE: u64 = 1 << 63;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;

/// Page table entry flags mask for physical address extraction
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Get current PML4 physical address from CR3
fn get_pml4_phys() -> u64 {
    let cr3: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    cr3 & ADDR_MASK
}

/// Invalidate TLB entry for a virtual address
fn invlpg(virt: u64) {
    unsafe {
        asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
}

/// Map a virtual address to a physical address with given flags
pub fn map_page(virt: u64, phys: u64, flags: u64) -> Result<(), &'static str> {
    let hhdm = pmm::hhdm_offset();
    if hhdm == 0 {
        return Err("HHDM offset not initialized");
    }

    let pml4_phys = get_pml4_phys();
    let pml4_virt = hhdm + pml4_phys;

    let pml4_index = ((virt >> 39) & 0x1FF) as usize;
    let pdpt_index = ((virt >> 30) & 0x1FF) as usize;
    let pd_index = ((virt >> 21) & 0x1FF) as usize;
    let pt_index = ((virt >> 12) & 0x1FF) as usize;

    unsafe {
        let pml4 = core::slice::from_raw_parts_mut(pml4_virt as *mut u64, 512);

        // 1. Get or create PDPT
        let pdpt_phys = if (pml4[pml4_index] & PRESENT) != 0 {
            pml4[pml4_index] & ADDR_MASK
        } else {
            let new_frame = pmm::alloc_frame().ok_or("Failed to allocate PDPT frame")?;
            pml4[pml4_index] = new_frame | PRESENT | WRITABLE | USER;
            new_frame
        };
        let pdpt_virt = hhdm + pdpt_phys;
        let pdpt = core::slice::from_raw_parts_mut(pdpt_virt as *mut u64, 512);

        // 2. Get or create PD
        let pd_phys = if (pdpt[pdpt_index] & PRESENT) != 0 {
            pdpt[pdpt_index] & ADDR_MASK
        } else {
            let new_frame = pmm::alloc_frame().ok_or("Failed to allocate PD frame")?;
            pdpt[pdpt_index] = new_frame | PRESENT | WRITABLE | USER;
            new_frame
        };
        let pd_virt = hhdm + pd_phys;
        let pd = core::slice::from_raw_parts_mut(pd_virt as *mut u64, 512);

        // 3. Get or create PT
        let pt_phys = if (pd[pd_index] & PRESENT) != 0 {
            pd[pd_index] & ADDR_MASK
        } else {
            let new_frame = pmm::alloc_frame().ok_or("Failed to allocate PT frame")?;
            pd[pd_index] = new_frame | PRESENT | WRITABLE | USER;
            new_frame
        };
        let pt_virt = hhdm + pt_phys;
        let pt = core::slice::from_raw_parts_mut(pt_virt as *mut u64, 512);

        // 4. Set PTE leaf
        pt[pt_index] = (phys & ADDR_MASK) | flags | PRESENT;
    }

    // Invalidate TLB
    invlpg(virt);

    Ok(())
}

/// Map a range of virtual memory of given size
pub fn map_range(virt_start: u64, size_bytes: u64, flags: u64) -> Result<(), &'static str> {
    let aligned_start = virt_start & !(pmm::PAGE_SIZE - 1);
    let aligned_end = (virt_start + size_bytes + pmm::PAGE_SIZE - 1) & !(pmm::PAGE_SIZE - 1);

    let mut current_virt = aligned_start;
    while current_virt < aligned_end {
        let phys = pmm::alloc_frame().ok_or("Failed to allocate frame for map_range")?;
        map_page(current_virt, phys, flags)?;
        current_virt += pmm::PAGE_SIZE;
    }

    Ok(())
}

/// Unmap a 4 KiB page and invalidate TLB
/// Returns the physical address if a page was mapped, None otherwise
/// Does NOT free the physical frame - caller is responsible for calling pmm::free_frame
pub fn unmap_page(virt_addr: u64) -> Option<u64> {
    let hhdm = pmm::hhdm_offset();
    if hhdm == 0 {
        return None;
    }

    let pml4_phys = get_pml4_phys();
    let pml4_virt = hhdm + pml4_phys;

    let pml4_index = ((virt_addr >> 39) & 0x1FF) as usize;
    let pdpt_index = ((virt_addr >> 30) & 0x1FF) as usize;
    let pd_index = ((virt_addr >> 21) & 0x1FF) as usize;
    let pt_index = ((virt_addr >> 12) & 0x1FF) as usize;

    unsafe {
        let pml4 = core::slice::from_raw_parts_mut(pml4_virt as *mut u64, 512);

        // Step through the hierarchy, checking each level exists
        if (pml4[pml4_index] & PRESENT) == 0 {
            return None;
        }
        let pdpt_phys = pml4[pml4_index] & ADDR_MASK;
        let pdpt_virt = hhdm + pdpt_phys;
        let pdpt = core::slice::from_raw_parts_mut(pdpt_virt as *mut u64, 512);

        if (pdpt[pdpt_index] & PRESENT) == 0 {
            return None;
        }
        let pd_phys = pdpt[pdpt_index] & ADDR_MASK;
        let pd_virt = hhdm + pd_phys;
        let pd = core::slice::from_raw_parts_mut(pd_virt as *mut u64, 512);

        if (pd[pd_index] & PRESENT) == 0 {
            return None;
        }
        let pt_phys = pd[pd_index] & ADDR_MASK;
        let pt_virt = hhdm + pt_phys;
        let pt = core::slice::from_raw_parts_mut(pt_virt as *mut u64, 512);

        // Extract physical address before clearing
        let phys_addr = (pt[pt_index] & ADDR_MASK) as u64;

        // Clear the PTE
        pt[pt_index] = 0;

        // Invalidate TLB
        invlpg(virt_addr);

        Some(phys_addr)
    }
}

/// Map a 2 MiB huge page
/// Uses the Page Directory (PD) level instead of PT
pub fn map_huge_page(virt_addr: u64, phys_addr: u64, flags: u64) -> Result<(), &'static str> {
    let hhdm = pmm::hhdm_offset();
    if hhdm == 0 {
        return Err("HHDM offset not initialized");
    }

    // Huge pages require 2 MiB alignment
    if (virt_addr & 0x1FFFFF) != 0 || (phys_addr & 0x1FFFFF) != 0 {
        return Err("Addresses must be 2 MiB aligned for huge pages");
    }

    let pml4_phys = get_pml4_phys();
    let pml4_virt = hhdm + pml4_phys;

    let pml4_index = ((virt_addr >> 39) & 0x1FF) as usize;
    let pdpt_index = ((virt_addr >> 30) & 0x1FF) as usize;
    let pd_index = ((virt_addr >> 21) & 0x1FF) as usize;

    unsafe {
        let pml4 = core::slice::from_raw_parts_mut(pml4_virt as *mut u64, 512);

        // 1. Get or create PDPT
        let pdpt_phys = if (pml4[pml4_index] & PRESENT) != 0 {
            pml4[pml4_index] & ADDR_MASK
        } else {
            let new_frame = pmm::alloc_frame().ok_or("Failed to allocate PDPT frame")?;
            pml4[pml4_index] = new_frame | PRESENT | WRITABLE | NO_EXECUTE;
            new_frame
        };
        let pdpt_virt = hhdm + pdpt_phys;
        let pdpt = core::slice::from_raw_parts_mut(pdpt_virt as *mut u64, 512);

        // 2. Check if PDPT entry is already mapped (might be a PD table or 1 GiB huge page)
        if (pdpt[pdpt_index] & PRESENT) != 0 {
            // If it's a 1 GiB huge page, we can't map a 2 MiB page at this level
            if (pdpt[pdpt_index] & HUGE_PAGE) != 0 {
                return Err("PDPT entry is already a 1 GiB huge page");
            }
            // PDPT entry points to a PD table - use the existing one
        } else {
            // Allocate new PD table
            let new_frame = pmm::alloc_frame().ok_or("Failed to allocate PD frame")?;
            pdpt[pdpt_index] = new_frame | PRESENT | WRITABLE | NO_EXECUTE;
        }

        // 3. Get PD physical address (extract from PDPT entry)
        let pd_phys = pdpt[pdpt_index] & ADDR_MASK;
        let pd_virt = hhdm + pd_phys;
        let pd = core::slice::from_raw_parts_mut(pd_virt as *mut u64, 512);

        // 4. Set 2 MiB huge page entry in PD
        pd[pd_index] = (phys_addr & ADDR_MASK) | flags | PRESENT | HUGE_PAGE;

        // 5. Invalidate TLB
        invlpg(virt_addr);

        Ok(())
    }
}

/// Unmap a 2 MiB huge page
/// Returns Some(phys_addr) if a huge page was unmapped, None otherwise
/// Does NOT free frames - caller is responsible for calling pmm::free_frame / pmm::free_contiguous_frames
pub fn unmap_huge_page(virt_addr: u64) -> Option<u64> {
    let hhdm = pmm::hhdm_offset();
    if hhdm == 0 {
        return None;
    }

    // Huge pages require 2 MiB alignment
    if (virt_addr & 0x1FFFFF) != 0 {
        return None;
    }

    let pml4_phys = get_pml4_phys();
    let pml4_virt = hhdm + pml4_phys;

    let pml4_index = ((virt_addr >> 39) & 0x1FF) as usize;
    let pdpt_index = ((virt_addr >> 30) & 0x1FF) as usize;
    let pd_index = ((virt_addr >> 21) & 0x1FF) as usize;

    unsafe {
        let pml4 = core::slice::from_raw_parts_mut(pml4_virt as *mut u64, 512);

        if (pml4[pml4_index] & PRESENT) == 0 {
            return None;
        }
        let pdpt_phys = pml4[pml4_index] & ADDR_MASK;
        let pdpt_virt = hhdm + pdpt_phys;
        let pdpt = core::slice::from_raw_parts_mut(pdpt_virt as *mut u64, 512);

        if (pdpt[pdpt_index] & PRESENT) == 0 {
            return None;
        }

        let pd_phys = pdpt[pdpt_index] & ADDR_MASK;
        let pd_virt = hhdm + pd_phys;
        let pd = core::slice::from_raw_parts_mut(pd_virt as *mut u64, 512);

        if (pd[pd_index] & PRESENT) == 0 {
            return None;
        }

        // Extract physical address before clearing
        let phys_addr = (pd[pd_index] & ADDR_MASK) as u64;

        // Clear the huge page entry
        pd[pd_index] = 0;

        // Invalidate TLB
        invlpg(virt_addr);

        Some(phys_addr)
    }
}

/// Map MMIO region as non-cacheable, write-through
/// Calls pmm::reserve_range to mark frames as allocated
/// Returns base virtual address if successful
pub fn map_mmio_region(phys_addr: u64, len: u64) -> Option<u64> {
    // Reserve the physical range first
    pmm::reserve_range(phys_addr, len);

    let hhdm = pmm::hhdm_offset();
    if hhdm == 0 {
        return None;
    }

    // Map the region using the reserved physical addresses
    // For MMIO, we use HHDM mapping with cache-disable flags
    let aligned_start = phys_addr & !(pmm::PAGE_SIZE - 1);
    let aligned_end = (phys_addr + len + pmm::PAGE_SIZE - 1) & !(pmm::PAGE_SIZE - 1);

    // MMIO page flags: PRESENT | WRITABLE | WRITE_THROUGH | NO_CACHE | NO_EXECUTE
    let mmio_flags = PRESENT | WRITABLE | WRITE_THROUGH | NO_CACHE | NO_EXECUTE;

    let mut current_phys = aligned_start;
    while current_phys < aligned_end {
        let virt = current_phys + hhdm;

        unsafe {
            let pml4_phys = get_pml4_phys();
            let pml4_virt = hhdm + pml4_phys;

            let pml4_index = ((virt >> 39) & 0x1FF) as usize;
            let pdpt_index = ((virt >> 30) & 0x1FF) as usize;
            let pd_index = ((virt >> 21) & 0x1FF) as usize;
            let pt_index = ((virt >> 12) & 0x1FF) as usize;

            let pml4 = core::slice::from_raw_parts_mut(pml4_virt as *mut u64, 512);

            // Get or create PDPT
            if (pml4[pml4_index] & PRESENT) == 0 {
                let new_frame = pmm::alloc_frame()?;
                pml4[pml4_index] = new_frame | PRESENT | WRITABLE | USER;
            }
            let pdpt_phys = pml4[pml4_index] & ADDR_MASK;
            let pdpt_virt = hhdm + pdpt_phys;
            let pdpt = core::slice::from_raw_parts_mut(pdpt_virt as *mut u64, 512);

            // Get or create PD
            if (pdpt[pdpt_index] & PRESENT) == 0 {
                let new_frame = pmm::alloc_frame()?;
                pdpt[pdpt_index] = new_frame | PRESENT | WRITABLE | USER;
            }
            let pd_phys = pdpt[pdpt_index] & ADDR_MASK;
            let pd_virt = hhdm + pd_phys;
            let pd = core::slice::from_raw_parts_mut(pd_virt as *mut u64, 512);

            // Get or create PT
            if (pd[pd_index] & PRESENT) == 0 {
                let new_frame = pmm::alloc_frame()?;
                pd[pd_index] = new_frame | PRESENT | WRITABLE | USER;
            }
            let pt_phys = pd[pd_index] & ADDR_MASK;
            let pt_virt = hhdm + pt_phys;
            let pt = core::slice::from_raw_parts_mut(pt_virt as *mut u64, 512);

            // Set PTE with MMIO flags
            pt[pt_index] = (current_phys & ADDR_MASK) | mmio_flags;

            // Invalidate TLB for this page
            invlpg(current_phys + hhdm);
        }

        current_phys += pmm::PAGE_SIZE;
    }

    Some(phys_addr + hhdm)
}