use core::arch::asm;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::serial_println!("\nKERNEL PANIC: {}", info);
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}