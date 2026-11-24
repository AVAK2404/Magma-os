//! Dynamic Kernel Heap Allocator
//! Coalescing linked-list free-block allocator using SpinLock
//!
//! Design:
//! - Uses a back-pointer slot (8 bytes) immediately before user payload
//! - This allows recovery of block header from any user pointer in dealloc
//! - Alignment header = 8 bytes (header) + 8 bytes (back-pointer) = 16 bytes base

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use crate::sync::SpinLock;

/// Size of the header stored in the free block (FreeBlock struct)
const HEADER_SIZE: usize = core::mem::size_of::<FreeBlock>();

/// Size of back-pointer slot (stores pointer to block header before payload)
const BACK_PTR_SIZE: usize = core::mem::size_of::<usize>();

/// Total header overhead per allocation (header + back-pointer)
const ALLOC_HEADER_SIZE: usize = HEADER_SIZE + BACK_PTR_SIZE;

pub const HEAP_START: u64 = 0xFFFF_C000_0000_0000;
pub const HEAP_SIZE: usize = 16 * 1024 * 1024; // 16 MiB

/// Align an address up to the given alignment.
const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/// Free block header in the linked list
#[repr(C)]
struct FreeBlock {
    size: usize,
    is_free: bool,
    next: *mut FreeBlock,
}

unsafe impl Send for FreeBlock {}

/// Free list allocator implementation
struct FreeListAllocator {
    head: *mut FreeBlock,
    heap_start: usize,
    heap_size: usize,
}

impl FreeListAllocator {
    const fn empty() -> Self {
        FreeListAllocator {
            head: null_mut(),
            heap_start: 0,
            heap_size: 0,
        }
    }

    unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        self.heap_start = heap_start;
        self.heap_size = heap_size;

        // SAFETY: heap_start is the HEAP_START virtual address, which is mapped and writable.
        let first_block = heap_start as *mut FreeBlock;
        unsafe {
            (*first_block).size = heap_size;
            (*first_block).is_free = true;
            (*first_block).next = null_mut();
        }
        self.head = first_block;
    }

    /// Check if a block pointer is within heap bounds
    fn is_valid_block(&self, block: *const FreeBlock) -> bool {
        let addr = block as usize;
        addr >= self.heap_start && addr < self.heap_start + self.heap_size
    }
}

unsafe impl Send for FreeListAllocator {}

/// Locked heap wrapper implementing GlobalAlloc
pub struct LockedHeap {
    inner: SpinLock<FreeListAllocator>,
}

impl LockedHeap {
    pub const fn empty() -> Self {
        LockedHeap {
            inner: SpinLock::new(FreeListAllocator::empty()),
        }
    }

    pub fn init(&self) -> Result<(), &'static str> {
        unsafe {
            self.inner.lock().init(HEAP_START as usize, HEAP_SIZE);
        }

        Ok(())
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();

        // SAFETY: The allocator is properly initialized before use, and we hold the lock.
        let allocator = self.inner.lock();
        if allocator.head.is_null() {
            return null_mut();
        }

        let block_align = align.max(8);

        let mut curr = allocator.head;
        while !curr.is_null() {
            // SAFETY: curr is a valid pointer from the free list, and we have the lock.
            let is_free = unsafe { (*curr).is_free };
            let block_size = unsafe { (*curr).size };

            if !is_free {
                curr = unsafe { (*curr).next };
                continue;
            }

            // curr is 8-byte aligned (guaranteed by our allocation strategy)
            let curr_addr = curr as usize;

            // Calculate user address with proper alignment
            // Layout: [FreeBlock header][back-pointer][user payload]
            // The user payload must be aligned to `align`
            let min_user_offset = ALLOC_HEADER_SIZE;
            let user_addr = align_up(curr_addr + min_user_offset, block_align);

            // Calculate total block size needed (rounded up for alignment)
            let aligned_size = align_up(size, block_align);
            let total_needed = user_addr - curr_addr + aligned_size;

            if block_size >= total_needed {
                // Calculate remaining space after allocation
                let remaining = block_size - total_needed;

                // SAFETY: We have the lock and curr is a valid block pointer.
                // The back-pointer slot is at user_addr - 8 bytes, which is within our block.
                unsafe {
                    // Write the FreeBlock header address at the back-pointer slot
                    let back_ptr = (user_addr - BACK_PTR_SIZE) as *mut usize;
                    core::ptr::write(back_ptr, curr_addr);

                    let allocated = &mut *curr;
                    allocated.is_free = false;
                    (*allocated).size = total_needed;

                    if remaining >= ALLOC_HEADER_SIZE + 16 {
                        // Split: create a new free block after our allocation
                        let next_block = (user_addr + aligned_size) as *mut FreeBlock;
                        (*next_block).size = remaining;
                        (*next_block).is_free = true;
                        (*next_block).next = (*curr).next;

                        (*curr).next = next_block;
                    } else {
                        // Merge remaining space into this block (no split possible)
                        // The remaining bytes are wasted, but this avoids creating
                        // tiny blocks that can't be used
                    }

                    // Lock is released when exiting the unsafe block via drop(allocator) at the end.
                    // No need for explicit drop(allocated) - it's just a mutable reference.
                }

                drop(allocator); // Release lock guard before return

                return user_addr as *mut u8;
            }

            curr = unsafe { (*curr).next };
        }

        null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }

        // SAFETY: We have the lock on the allocator.
        let allocator = self.inner.lock();

        // Bounds check first - ensure pointer has back-pointer space before it
        // and is within the heap range
        let ptr_addr = ptr as usize;
        if ptr_addr <= allocator.heap_start || ptr_addr >= allocator.heap_start + allocator.heap_size {
            return;
        }
        if ptr_addr < BACK_PTR_SIZE {
            return;
        }

        // Recover the FreeBlock header address from the back-pointer slot
        // The back-pointer is stored at (ptr - BACK_PTR_SIZE)
        let block_addr = unsafe { *((ptr as *const usize).sub(1)) };
        let block = block_addr as *mut FreeBlock;

        // Validate block header is within heap bounds
        // Block header must start at heap_start or later
        if block_addr < allocator.heap_start || block_addr >= allocator.heap_start + allocator.heap_size - HEADER_SIZE {
            return;
        }
        // Block's size field must not extend past heap boundary
        let block_end = unsafe { (*block).size };
        if block_addr + block_end > allocator.heap_start + allocator.heap_size {
            return;
        }

        // SAFETY: block is a valid pointer within the heap range.
        unsafe {
            let block_ref = &mut *block;
            block_ref.is_free = true;

            // Coalesce forward: merge with next block if free and adjacent
            let next = (*block).next;
            if !next.is_null() && (*next).is_free {
                let block_end = block_addr + (*block).size;
                if block_end == next as usize {
                    (*block).size += (*next).size;
                    (*block).next = (*next).next;
                }
            }

            // Coalesce backward: iterate from head and merge any free block
            // that ends at our (possibly merged) block's start
            let mut curr = allocator.head;
            while !curr.is_null() {
                let next_in_list = (*curr).next;

                // Check if we should merge curr into our block
                let curr_addr = curr as usize;
                let curr_end = curr_addr + (*curr).size;

                // Only try to merge if curr is free and ends where our block starts
                if (*curr).is_free && curr_end == block_addr {
                    // We found a free block that ends where our block starts - merge backward
                    // After this merge, the free block is now at 'curr', not 'block'
                    (*curr).size += (*block).size;
                    (*curr).next = (*block).next;

                    // Re-check forward coalescing for the merged block
                    let merged_next = (*curr).next;
                    if !merged_next.is_null() && (*merged_next).is_free {
                        let merged_addr = curr as usize;
                        let merged_size = (*curr).size;
                        let merged_end = merged_addr + merged_size;
                        if merged_end == merged_next as usize {
                            (*curr).size += (*merged_next).size;
                            (*curr).next = (*merged_next).next;
                        }
                    }
                    break;
                }

                // If next block is free and adjacent to curr, coalesce them
                if (*curr).is_free && !next_in_list.is_null() && (*next_in_list).is_free {
                    let next_addr = next_in_list as usize;
                    if curr_end == next_addr {
                        (*curr).size += (*next_in_list).size;
                        (*curr).next = (*next_in_list).next;
                    }
                }

                curr = next_in_list;
            }
        }
    }
}