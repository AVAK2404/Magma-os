use core::arch::asm;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::serial::write_str_safe("\r\nKERNEL PANIC: ");

    // Get the panic message
    let msg = info.message();
    write_str_to_serial(msg);
    crate::serial::write_str_safe("\r\n");

    // Get the location (file:line:column)
    if let Some(loc) = info.location() {
        crate::serial::write_str_safe("Location: ");
        crate::serial::write_str_safe(loc.file());
        crate::serial::write_str_safe(":");
        write_u32_serial(loc.line());
        if loc.column() > 0 {
            crate::serial::write_str_safe(":");
            write_u32_serial(loc.column());
        }
        crate::serial::write_str_safe("\r\n");
    }

    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

/// Helper to write a `PanicMessage` to serial using the Write trait
fn write_str_to_serial(msg: core::panic::PanicMessage<'_>) {
    use core::fmt::Write;
    let mut serial = crate::serial::SerialPort;
    let _ = write!(&mut serial, "{}", msg);
}

/// Helper to write u32 as decimal to serial
fn write_u32_serial(mut val: u32) {
    if val == 0 {
        crate::serial::write_str_safe("0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut idx = 9;
    while val > 0 {
        buf[idx] = b'0' + ((val % 10) as u8);
        val /= 10;
        idx -= 1;
    }
    for &b in &buf[(idx + 1)..=9] {
        crate::serial::write_byte(b);
    }
}