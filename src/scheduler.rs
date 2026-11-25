//! Phase 1E: Kernel Thread Scheduler
//!
//! Implements round-robin thread scheduling with:
//! - ThreadState enum: Ready, Running, Blocked, Terminated
//! - Thread struct with TID, state, stack pointer, RSP
//! - Context switch using saved RSP
//! - LAPIC timer integration at 100 Hz (vector 240)
//!
//! Each thread has a 16 KiB kernel stack allocated from the physical memory manager.

use crate::sync::SpinLock;

/// Maximum number of threads supported
pub const MAX_THREADS: usize = 64;

/// Thread states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadState {
    Ready = 0,
    Running = 1,
    Blocked = 2,
    Terminated = 3,
}

impl Default for ThreadState {
    fn default() -> Self {
        ThreadState::Ready
    }
}

/// Thread control block
#[derive(Debug, Clone, Copy, Default)]
pub struct Thread {
    /// Unique thread identifier
    pub tid: usize,
    /// Current thread state
    pub state: ThreadState,
    /// Saved stack pointer (where to restore RSP on context switch)
    pub rsp_stored: u64,
    /// Physical address of the kernel stack (16 KiB)
    pub stack_phys: u64,
}

/// Thread registry - maps TID to Thread
#[repr(C)]
struct ThreadTable {
    threads: [Option<Thread>; MAX_THREADS],
    next_tid: usize,
    running: Option<usize>, // TID of currently running thread
}

impl ThreadTable {
    /// Const constructor for static initialization
    const fn new() -> Self {
        const NONE_ENTRY: Option<Thread> = None;
        ThreadTable {
            threads: [NONE_ENTRY; MAX_THREADS],
            next_tid: 0,
            running: None,
        }
    }

    /// Allocate a new thread with a 16 KiB kernel stack
    fn create_thread(&mut self, entry_point: fn() -> !) -> Option<usize> {
        let tid = self.next_tid;
        if tid >= MAX_THREADS {
            return None;
        }

        // Allocate 16 KiB contiguous stack (4 pages)
        let stack_size: usize = 16 * 1024; // 16 KiB
        let frame_count = stack_size.div_ceil(crate::pmm::PAGE_SIZE as usize);

        let dma_region = crate::pmm::alloc_contiguous_frames(frame_count, 512)?;

        // The stack grows down, so we set RSP to the top of the stack
        // stack_virt is the base address, stack_top is the virtual address of the stack
        let stack_virt = dma_region.virt_addr;
        let stack_top = (stack_virt as u64).wrapping_add(stack_size as u64);

        // Stack alignment for x86-64 System V ABI:
        // - On function entry, RSP % 16 == 8
        // - switch_context pops 6 registers (48 bytes), then ret (8 bytes)
        // - After ret, RSP = stack_top (0 mod 16) ✓
        // - thread_trampoline: sti; call r12 (entry_point)
        // - After 'call entry_point': RSP = stack_top - 8 (8 mod 16) ✓ ABI alignment
        //
        // Stack layout for unified switch_context (with callee-saved register save/restore):
        // switch_context pops in order: r15, r14, r13, r12, rbx, rbp
        // rsp_stored = stack_top - 56, so after 6 pops:
        // - Pop r15: reads stack_top - 56
        // - Pop r14: reads stack_top - 48
        // - Pop r13: reads stack_top - 40
        // - Pop r12: reads stack_top - 32
        // - Pop rbx: reads stack_top - 24
        // - Pop rbp: reads stack_top - 16
        // - Ret: reads stack_top - 8 (thread_trampoline), RSP = stack_top
        // After ret, thread_trampoline: sti; call r12 (entry_point)
        // After 'call entry_point': RSP = stack_top - 8 (8 mod 16) ✓ ABI alignment
        unsafe {
            (stack_top.wrapping_sub(56) as *mut u64).write_unaligned(0);  // r15
            (stack_top.wrapping_sub(48) as *mut u64).write_unaligned(0);  // r14
            (stack_top.wrapping_sub(40) as *mut u64).write_unaligned(0);  // r13
            (stack_top.wrapping_sub(32) as *mut u64).write_unaligned(entry_point as u64);  // r12 = entry_point
            (stack_top.wrapping_sub(24) as *mut u64).write_unaligned(0);  // rbx
            (stack_top.wrapping_sub(16) as *mut u64).write_unaligned(0);  // rbp
            (stack_top.wrapping_sub(8) as *mut u64).write_unaligned(thread_trampoline as *const () as u64); // return address
        }

        // Store the physical address of the stack
        let stack_phys = dma_region.phys_addr;

        // rsp_stored = stack_top - 56 (points to r15 in frame)
        // After switch_context loads this RSP and pops 6 regs (48 bytes):
        // - Pop r15: RSP = stack_top - 56, read from stack_top - 56
        // - Pop r14: RSP = stack_top - 48, read from stack_top - 48
        // - Pop r13: RSP = stack_top - 40, read from stack_top - 40
        // - Pop r12: RSP = stack_top - 32, read from stack_top - 32
        // - Pop rbx: RSP = stack_top - 24, read from stack_top - 24
        // - Pop rbp: RSP = stack_top - 16, read from stack_top - 16
        // - Ret: pops from stack_top - 8 (thread_trampoline), RSP = stack_top
        // RSP = stack_top after ret, which is 0 mod 16 ✓
        // thread_trampoline: sti; call r12 (entry_point)
        // After 'call entry_point': RSP = stack_top - 8 (8 mod 16) ✓ ABI alignment
        let rsp_stored = stack_top.wrapping_sub(56);

        let thread = Thread {
            tid,
            state: ThreadState::Ready,
            rsp_stored,
            stack_phys,
        };

        self.threads[tid] = Some(thread);
        self.next_tid = self.next_tid.wrapping_add(1);

        Some(tid)
    }

    /// Add a new ready thread
    fn add_ready_thread(&mut self, entry_point: fn() -> !) -> Option<usize> {
        self.create_thread(entry_point)
    }

    /// Find the next thread to run (round-robin)
    fn schedule_next(&mut self) -> Option<usize> {
        let current = self.running;
        let next = if let Some(cur_tid) = current {
            // Round-robin: find next ready thread
            let mut searched = 0usize;
            let mut candidate = (cur_tid + 1) % MAX_THREADS;

            // Look for a Ready thread
            loop {
                if let Some(ref t) = self.threads[candidate] {
                    if t.state == ThreadState::Ready {
                        break Some(candidate);
                    }
                }
                candidate = (candidate + 1) % MAX_THREADS;
                searched += 1;
                if searched >= MAX_THREADS {
                    break None;
                }
            }
        } else {
            // No current thread - find first Ready thread
            let mut result = None;
            for i in 0..MAX_THREADS {
                if let Some(ref t) = self.threads[i] {
                    if t.state == ThreadState::Ready {
                        result = Some(i);
                        break;
                    }
                }
            }
            result
        };

        next
    }
}

static THREAD_TABLE: SpinLock<ThreadTable> = SpinLock::new(ThreadTable::new());

/// Create and spawn a new kernel thread
pub fn spawn_thread(entry: fn() -> !) -> Option<usize> {
    let mut table = THREAD_TABLE.lock();
    table.add_ready_thread(entry)
}

/// Thread exit - called when a thread function returns
fn thread_exit() -> ! {
    crate::sync::without_interrupts(|| {
        let mut table = THREAD_TABLE.lock();
        if let Some(tid) = table.running {
            if let Some(ref mut t) = table.threads[tid] {
                t.state = ThreadState::Terminated;
            }
        }
    });
    loop {
        yield_cpu();
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Idle thread function - runs forever in an infinite loop
/// This is the entry point for TID 0 (idle thread)
pub fn idle_thread() -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Thread trampoline - sets up the stack for thread entry
/// This is the entry point written on the stack
#[unsafe(naked)]
unsafe extern "C" fn thread_trampoline() {
    core::arch::naked_asm!(
        "sti",
        "call r12",
        "call {exit}",
        exit = sym thread_exit,
    );
}

/// Initialize the scheduler and create the idle thread (TID 0)
pub fn init() {
    let mut table = THREAD_TABLE.lock();

    if table.running.is_none() {
        // Allocate a 16 KiB stack for the idle thread (4 frames = 16 KiB)
        let stack_size: usize = 16 * 1024; // 16 KiB
        let frame_count = stack_size.div_ceil(crate::pmm::PAGE_SIZE as usize); // 4 frames

        let dma_region = match crate::pmm::alloc_contiguous_frames(frame_count, 512) {
            Some(region) => region,
            None => return, // Allocation failed
        };

        let stack_virt = dma_region.virt_addr;
        let stack_top = (stack_virt as u64).wrapping_add(stack_size as u64);
        // Align stack_top down to 16 bytes inside the region
        let stack_top = (stack_top as u64) & !0xF;

        // Stack layout for unified switch_context (with callee-saved register save/restore):
        //   stack_top - 8:  return address (thread_trampoline)
        //   stack_top - 16: rbp = 0
        //   stack_top - 24: rbx = 0
        //   stack_top - 32: r12 = idle_thread (passed to thread_trampoline)
        //   stack_top - 40: r13 = 0
        //   stack_top - 48: r14 = 0
        //   stack_top - 56: r15 = 0
        //
        // When switch_context is called:
        // - It pushes rbp, rbx, r12, r13, r14, r15 on current stack
        // - Stores current RSP to *old_rsp_ptr
        // - Sets RSP = new_rsp (which is rsp_stored = stack_top - 56)
        // - Pops r15, r14, r13, r12, rbx, rbp from new thread's stack
        // - 'ret' pops return address (thread_trampoline) and jumps to it
        // - thread_trampoline: sti; call r12 (which is idle_thread)
        // - After 'call r12': RSP = stack_top - 8 (8 mod 16) ✓ ABI alignment
        unsafe {
            // Initialize the unified stack frame for switch_context
            (stack_top.wrapping_sub(8) as *mut u64).write_unaligned(thread_trampoline as *const () as u64);  // return address
            (stack_top.wrapping_sub(16) as *mut u64).write_unaligned(0);  // rbp
            (stack_top.wrapping_sub(24) as *mut u64).write_unaligned(0);  // rbx
            (stack_top.wrapping_sub(32) as *mut u64).write_unaligned(idle_thread as *const () as u64);  // r12 = idle_thread
            (stack_top.wrapping_sub(40) as *mut u64).write_unaligned(0);  // r13
            (stack_top.wrapping_sub(48) as *mut u64).write_unaligned(0);  // r14
            (stack_top.wrapping_sub(56) as *mut u64).write_unaligned(0);  // r15
        }

        // rsp_stored = stack_top - 56 (points to r15 in frame)
        // After switch_context pops 6 registers (48 bytes): RSP = stack_top - 48
        // After 'ret' pops return address: RSP = stack_top - 56, jumps to thread_trampoline
        let rsp_stored = stack_top.wrapping_sub(56);

        table.threads[0] = Some(Thread {
            tid: 0,
            state: ThreadState::Running,
            rsp_stored,
            stack_phys: dma_region.phys_addr,
        });
        table.running = Some(0);
        table.next_tid = 1;
    } else {
        drop(table);
    }
}

/// Called by the timer interrupt handler each tick (LAPIC timer at 100 Hz)
pub fn on_timer_tick(current_rsp: u64) {
    crate::sync::without_interrupts(|| {
        let mut table = THREAD_TABLE.lock();

        // Check if we have a running thread
        let current_tid_opt = table.running;

        match current_tid_opt {
            Some(cur_tid) => {
                // Save current thread's RSP to its TCB and mark as Ready
                let old_rsp_ptr = {
                    let thread = &mut table.threads[cur_tid];
                    match thread {
                        Some(t) if t.state == ThreadState::Running => {
                            t.rsp_stored = current_rsp;
                            t.state = ThreadState::Ready;
                            &mut t.rsp_stored as *mut u64
                        }
                        _ => {
                            drop(table);
                            return;
                        }
                    }
                };

                // Find next thread to run
                if let Some(next_tid) = table.schedule_next() {
                    let new_rsp = {
                        let next_thread = &mut table.threads[next_tid];
                        match next_thread {
                            Some(t) => {
                                t.state = ThreadState::Running;
                                t.rsp_stored
                            }
                            None => {
                                drop(table);
                                return;
                            }
                        }
                    };

                    table.running = Some(next_tid);
                    drop(table); // Release lock before switch

                    unsafe {
                        switch_context(old_rsp_ptr, new_rsp);
                    }
                    return;
                }

                // No next thread available - restore current thread to Running
                // This handles the case where only the idle thread is ready
                if let Some(thread) = &mut table.threads[cur_tid] {
                    thread.state = ThreadState::Running;
                }
            }
            None => {}
        }
    });
}

/// Yield the CPU voluntarily
/// This function is called from a running thread's context
pub fn yield_cpu() {
    crate::sync::without_interrupts(|| {
        // Get pointer to current thread's rsp_stored field for context switch
        let (old_rsp_ptr, new_rsp) = {
            let mut table = THREAD_TABLE.lock();
            let cur_tid = table.running;

            // Get pointer to current thread's rsp_stored and mark it Ready
            let old_rsp_ptr = if let Some(tid) = cur_tid {
                if let Some(thread) = &mut table.threads[tid] {
                    if thread.state == ThreadState::Running {
                        thread.state = ThreadState::Ready;
                        &mut thread.rsp_stored as *mut u64
                    } else {
                        drop(table);
                        return;
                    }
                } else {
                    drop(table);
                    return;
                }
            } else {
                drop(table);
                return;
            };

            // Find next thread to run
            match table.schedule_next() {
                Some(next_tid) => {
                    let next_thread = &mut table.threads[next_tid];
                    if let Some(t) = next_thread {
                        t.state = ThreadState::Running;
                        let saved_rsp = t.rsp_stored;
                        table.running = Some(next_tid);
                        (old_rsp_ptr, saved_rsp)
                    } else {
                        drop(table);
                        return;
                    }
                }
                None => {
                    // No ready threads available - restore current thread to Running
                    if let Some(tid) = cur_tid {
                        if let Some(thread) = &mut table.threads[tid] {
                            thread.state = ThreadState::Running;
                        }
                    }
                    return;
                }
            }
        };

        // Perform the context switch
        // old_rsp_ptr: where to store current RSP after saving callee-saved regs
        // new_rsp: RSP of the incoming thread to switch to
        unsafe {
            switch_context(old_rsp_ptr, new_rsp);
        }
    });
}

/// Yield the CPU (public wrapper for kernel code that wants to yield)
pub fn yield_now() {
    yield_cpu();
}

/// Context switch implementation - unified callee-saved approach
/// Saves callee-saved registers, stores old RSP, swaps to new RSP, restores registers
///
/// # Arguments
/// * `old_rsp_ptr` - Pointer to where to store the current RSP
/// * `new_rsp` - RSP of the incoming thread (where to restore)
///
/// This is called from interrupt context (timer handler) or from yield_cpu().
/// Note: This is a noreturn function - it never returns to the caller.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(old_rsp_ptr: *mut u64, new_rsp: u64) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}

/// Get thread count
pub fn thread_count() -> usize {
    let table = THREAD_TABLE.lock();
    table.threads.iter().filter(|t| t.is_some()).count()
}

/// Get the number of ready threads
pub fn ready_count() -> usize {
    let table = THREAD_TABLE.lock();
    table.threads.iter().filter(|t| match t {
        Some(thread) => thread.state == ThreadState::Ready,
        None => false,
    }).count()
}

/// Get thread state by TID
pub fn get_thread_state(tid: usize) -> ThreadState {
    let table = THREAD_TABLE.lock();
    if let Some(thread) = table.threads[tid] {
        thread.state
    } else {
        ThreadState::Terminated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thread_state_values() {
        assert_eq!(ThreadState::Ready as u8, 0);
        assert_eq!(ThreadState::Running as u8, 1);
        assert_eq!(ThreadState::Blocked as u8, 2);
        assert_eq!(ThreadState::Terminated as u8, 3);
    }
}