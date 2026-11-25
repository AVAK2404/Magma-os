//! SpinLock synchronization primitive for no_std kernel
//! Uses AtomicBool for lock state with Acquire/Release ordering
//! Provides interrupt-safe locking via without_interrupts()

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use core::ops::{Deref, DerefMut};
use core::hint::spin_loop;
use core::arch::asm;

/// A simple spinlock that protects mutable data
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

impl<T> SpinLock<T> {
    /// Create a new SpinLock with the given data
    pub const fn new(data: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Lock the SpinLock and return a guard
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            // Try to acquire the lock
            if self.locked.compare_exchange(
                false,
                true,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_ok() {
                break;
            }
            // Spin until we can acquire
            spin_loop();
        }
        SpinLockGuard { lock: self }
    }
}

impl<T> Default for SpinLock<T>
where
    T: Default,
{
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Execute a closure while interrupts are disabled.
/// Saves RFLAGS, disables interrupts (cli), executes f, then restores RFLAGS.
/// This is used for interrupt-safe locking - the SpinLock's lock() method
/// can deadlock if a timer interrupt fires while we hold the lock.
pub fn without_interrupts<R, F: FnOnce() -> R>(f: F) -> R {
    // Save RFLAGS and disable interrupts
    let flags: u64;
    unsafe {
        asm!("pushf; pop {0}", out(reg) flags, options(nomem, nostack));
        asm!("cli", options(nomem, nostack));
    }

    let result = f();

    // Restore RFLAGS (which may re-enable interrupts if they were enabled)
    unsafe {
        asm!("push {0}; popfq", in(reg) flags, options(nomem, nostack));
    }

    result
}

/// Guard returned by SpinLock::lock()
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: We hold the lock, so mutable access is safe
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: We hold the lock, so exclusive mutable access is safe
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock
        self.lock.locked.store(false, Ordering::Release);
    }
}

// Safety: SpinLock can be sent between threads
unsafe impl<T: Send> Send for SpinLock<T> {}

// Safety: SpinLock can be shared between threads
unsafe impl<T: Send> Sync for SpinLock<T> {}