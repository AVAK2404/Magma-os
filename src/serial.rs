use core::arch::asm;
use core::fmt::{self, Write};

const COM1: u16 = 0x3F8;

unsafe fn outb(port: u16, val: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    val
}

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // Disable interrupts
        outb(COM1 + 3, 0x80); // Enable DLAB (set baud rate divisor)
        outb(COM1 + 0, 0x01); // Divisor low byte (115200 baud)
        outb(COM1 + 1, 0x00); // Divisor high byte
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit (8N1)
        outb(COM1 + 2, 0xC7); // Enable FIFO, clear them, 14-byte threshold
        outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR set
    }
}

fn is_transmit_empty() -> bool {
    unsafe { (inb(COM1 + 5) & 0x20) != 0 }
}

pub fn write_byte(byte: u8) {
    if byte == b'\n' {
        write_byte(b'\r');
    }
    while !is_transmit_empty() {
        core::hint::spin_loop();
    }
    unsafe {
        outb(COM1, byte);
    }
}

pub struct SerialPort;

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            write_byte(byte);
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let mut serial = SerialPort;
    let _ = serial.write_fmt(args);
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
