//! PS/2 Keyboard Driver
//!
//! Implements PS/2 keyboard input handling with IRQ 1 routed through I/O APIC.
//! Decodes Scancode Set 1 make/break codes and provides a ring buffer for key events.

use crate::serial;

/// PS/2 keyboard I/O ports
const KB_DATA_PORT: u16 = 0x60;
const KB_STATUS_PORT: u16 = 0x64;

/// Key modifiers flags
pub const MOD_NONE: u32 = 0;
pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_CAPSLOCK: u32 = 1 << 3;

/// Key event structure
#[derive(Copy, Clone)]
pub struct KeyEvent {
    pub ch: u8,            // ASCII character (0 if non-printable)
    pub scancode: u8,      // Raw scancode
    pub modifiers: u32,    // Modifier flags
    pub press: bool,       // true = press, false = release
}

/// Ring buffer for key events
const KEY_EVENT_BUFFER_SIZE: usize = 128;

#[allow(static_mut_refs)]
static mut KEY_EVENT_BUFFER: [KeyEvent; KEY_EVENT_BUFFER_SIZE] = [KeyEvent {
    ch: 0,
    scancode: 0,
    modifiers: 0,
    press: false,
}; KEY_EVENT_BUFFER_SIZE];

#[allow(static_mut_refs)]
static mut BUFFER_HEAD: usize = 0;
#[allow(static_mut_refs)]
static mut BUFFER_TAIL: usize = 0;

/// Keyboard state
#[allow(static_mut_refs)]
static mut KBD_STATE: KbdState = KbdState {
    left_shift: false,
    right_shift: false,
    left_ctrl: false,
    right_ctrl: false,
    left_alt: false,
    right_alt: false,
    capslock: false,
};

struct KbdState {
    left_shift: bool,
    right_shift: bool,
    left_ctrl: bool,
    right_ctrl: bool,
    left_alt: bool,
    right_alt: bool,
    capslock: bool,
}

/// Write u8 as 2-digit hex
fn write_u8_hex(val: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    serial::write_byte(HEX[(val >> 4) as usize]);
    serial::write_byte(HEX[(val & 0xF) as usize]);
}

/// Write a byte to an I/O port
#[inline]
fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Flush the PS/2 output buffer
fn flush_output_buffer() {
    // Read and discard any pending bytes in the output buffer
    while inb(KB_STATUS_PORT) & 0x01 != 0 {
        let _ = inb(KB_DATA_PORT);
    }
}

/// Read a byte from I/O port
#[inline]
fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    val
}

/// Get current modifier state
#[allow(static_mut_refs)]
fn get_modifiers() -> u32 {
    let state = unsafe { &*core::ptr::addr_of!(KBD_STATE) };
    let mut mods = MOD_NONE;
    if state.left_shift || state.right_shift {
        mods |= MOD_SHIFT;
    }
    if state.left_ctrl || state.right_ctrl {
        mods |= MOD_CTRL;
    }
    if state.left_alt || state.right_alt {
        mods |= MOD_ALT;
    }
    if state.capslock {
        mods |= MOD_CAPSLOCK;
    }
    mods
}

/// Translate scancode to ASCII character using Scancode Set 1
/// Standard US QWERTY keyboard scancodes
fn scancode_to_ascii(scancode: u8, mods: u32) -> u8 {
    let shift = mods & MOD_SHIFT != 0;
    let capslock = mods & MOD_CAPSLOCK != 0;

    // Base ASCII mapping for Scancode Set 1 (make codes)
    // Standard US QWERTY keyboard scancodes - all unique values
    let ch = match scancode {
        // Control characters and punctuation
        0x01 => b'\x1B',   // ESC
        0x0E => b'\x08',   // Backspace
        0x0F => b'\x09',   // Tab
        0x1C => b'\x0A',  // Enter (LF, not CR)
        0x39 => b' ',    // Space

        // Numbers row (top row) - Scancode Set 1
        0x0B => b'0',
        0x02 => b'1',
        0x03 => b'2',
        0x04 => b'3',
        0x05 => b'4',
        0x06 => b'5',
        0x07 => b'6',
        0x08 => b'7',
        0x09 => b'8',
        0x0A => b'9',
        0x0C => b'=',

        // Letters (QWERTY layout) - Scancode Set 1
        0x1E => b'A',     // A (lowercase, shift transforms)
        0x30 => b'B',
        0x2E => b'C',
        0x20 => b'D',
        0x12 => b'E',
        0x21 => b'F',
        0x22 => b'G',
        0x23 => b'H',
        0x17 => b'I',
        0x24 => b'J',
        0x25 => b'K',
        0x26 => b';',    // ;
        0x32 => b'M',
        0x31 => b'N',
        0x18 => b'O',
        0x19 => b'P',
        0x10 => b'Q',
        0x13 => b'R',
        0x1F => b'S',
        0x14 => b'T',
        0x16 => b'U',
        0x2F => b'V',
        0x11 => b'W',
        0x2D => b'X',
        0x15 => b'Y',
        0x2C => b'Z',
        0x2A => b'-',    // -
        0x27 => b'[',   // [
        0x28 => b']',   // ]
        0x2B => b'\\',  // \
        0x29 => b'\'', // ' (apostrophe)
        0x33 => b'`',   // ` (grave accent) - moved from position
        0x34 => b',',   // ,
        0x35 => b'.',  // .
        0x36 => b'/',  // /

        _ => b'\0',  // Unknown scancode
    };

    if ch != b'\0' {
        // Apply shift/capslock transformations
        let result = if shift {
            // Shift modifies many characters
            match ch {
                b'0' => b')',
                b'1' => b'!',
                b'2' => b'@',
                b'3' => b'#',
                b'4' => b'$',
                b'5' => b'%',
                b'6' => b'^',
                b'7' => b'&',
                b'8' => b'*',
                b'9' => b'(',
                b'-' => b'_',
                b'=' => b'+',
                b'[' => b'{',
                b']' => b'}',
                b'\\' => b'|',
                b';' => b':',
                b'\'' => b'"',
                b'`' => b'~',
                b',' => b'<',
                b'.' => b'>',
                b'/' => b'?',
                b' ' => b' ',
                _ if ch.is_ascii_lowercase() => ch.to_ascii_uppercase(),
                _ => ch,
            }
        } else if capslock {
            // CapsLock inverts letter case
            if ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase()
            } else if ch.is_ascii_uppercase() {
                ch.to_ascii_lowercase()
            } else {
                ch
            }
        } else {
            ch
        };

        return result;
    }

    0
}

/// Pop a key event from the buffer
#[allow(static_mut_refs)]
pub fn pop_event() -> Option<KeyEvent> {
    unsafe {
        if BUFFER_HEAD == BUFFER_TAIL {
            return None;
        }
        let event = KEY_EVENT_BUFFER[BUFFER_TAIL];
        BUFFER_TAIL = (BUFFER_TAIL + 1) % KEY_EVENT_BUFFER_SIZE;
        Some(event)
    }
}

/// Push a key event to the buffer
#[allow(static_mut_refs)]
fn push_event(event: KeyEvent) {
    unsafe {
        let next_head = (BUFFER_HEAD + 1) % KEY_EVENT_BUFFER_SIZE;
        if next_head == BUFFER_TAIL {
            // Buffer full - drop oldest event
        }
        KEY_EVENT_BUFFER[BUFFER_HEAD] = event;
        BUFFER_HEAD = next_head;
    }
}

/// Get current modifier state (for external access)
pub fn get_modifier_state() -> u32 {
    get_modifiers()
}

/// PS/2 i8042 Controller Configuration Byte bits
const KB_CCCR_DISABLE_PORT1: u8 = 0x01;  // Bit 0: Port 1 disabled
const KB_CCCR_IRQ1_ENABLE: u8 = 0x02;     // Bit 1: IRQ1 disabled (active low)
const KB_CCCR_SCANCODE_SET: u8 = 0x40;    // Bit 6: Scancode Set 1 (if 0) or Set 2/3

/// I/O ports for i8042 controller commands
const KB_COMMAND_PORT: u16 = 0x64;

/// Read the Controller Command/Status byte from port 0x60 (after reading status 0x64)
fn read_ccb() -> Option<u8> {
    // Command: 0x20 = Read CCB
    outb(KB_COMMAND_PORT, 0x20);
    // Wait for output buffer to have data (up to 100 microseconds)
    for _ in 0..100 {
        if inb(KB_STATUS_PORT) & 0x01 != 0 {
            return Some(inb(KB_DATA_PORT));
        }
        core::hint::spin_loop();
    }
    None
}

/// Write the Controller Command/Status byte to port 0x60 (after reading status 0x64)
fn write_ccb(value: u8) -> bool {
    // Wait for input buffer to be empty (up to 100 microseconds)
    for _ in 0..100 {
        if inb(KB_STATUS_PORT) & 0x02 == 0 {
            // Command: 0x60 = Write CCB
            outb(KB_COMMAND_PORT, 0x60);
            // Wait for input buffer to be empty again before writing data
            for _ in 0..100 {
                if inb(KB_STATUS_PORT) & 0x02 == 0 {
                    outb(KB_DATA_PORT, value);
                    return true;
                }
                core::hint::spin_loop();
            }
        }
        core::hint::spin_loop();
    }
    false
}

/// Initialize the PS/2 keyboard controller with proper i8042 protocol
pub fn init() {
    // Step 1: Disable port 1 (keyboard)
    if let Some(mut ccb) = read_ccb() {
        ccb |= KB_CCCR_DISABLE_PORT1;  // Set bit 0 to disable port 1
        if !write_ccb(ccb) {
            serial::write_str_safe("[KBD] Failed to disable port 1\r\n");
            return;
        }
    }

    // Step 2: Set Scancode Set 1 (8048 mode) - clear bit 6
    if let Some(mut ccb) = read_ccb() {
        ccb &= !KB_CCCR_SCANCODE_SET;  // Clear bit 6 for Scancode Set 1
        if !write_ccb(ccb) {
            serial::write_str_safe("[KBD] Failed to set scancode mode\r\n");
            return;
        }
    }

    // Step 3: Enable IRQ1 - clear bit 1 (IRQ1 disabled is 1, enabled is 0)
    if let Some(mut ccb) = read_ccb() {
        ccb &= !KB_CCCR_IRQ1_ENABLE;  // Clear bit 1 to enable IRQ1
        if !write_ccb(ccb) {
            serial::write_str_safe("[KBD] Failed to enable IRQ1\r\n");
            return;
        }
    }

    // Step 4: Re-enable port 1
    if let Some(mut ccb) = read_ccb() {
        ccb &= !KB_CCCR_DISABLE_PORT1;  // Clear bit 0 to enable port 1
        if !write_ccb(ccb) {
            serial::write_str_safe("[KBD] Failed to enable port 1\r\n");
            return;
        }
    }

    // Step 5: Flush any pending scan codes
    flush_output_buffer();

    serial::write_str_safe("[KBD] PS/2 Keyboard controller initialized\r\n");
}

/// Handle keyboard IRQ - called from the IRQ handler
pub fn handle_irq() {
    // Check status port before reading data port
    // Bit 0 = Output Buffer Full (1 = data available at 0x60)
    if inb(KB_STATUS_PORT) & 0x01 == 0 {
        // No data available - this shouldn't happen in IRQ context
        return;
    }

    // Read scancode from data port
    let scancode = inb(KB_DATA_PORT);

    // Check for break code prefix (0xF0) - we track this in a static
    // A break code has the 0x80 bit set, or comes as 0xF0 followed by the code
    static mut BREAK_CODE_PENDING: bool = false;

    if scancode == 0xF0 {
        // Next byte is the actual break code scancode
        unsafe {
            BREAK_CODE_PENDING = true;
        }
        return;
    }

    // Check if this is a break code
    let is_break = unsafe { BREAK_CODE_PENDING };
    unsafe {
        BREAK_CODE_PENDING = false;
    }

    // Ignore break codes - we only generate events on key press
    if is_break {
        // Still need to update modifier state on release
        match scancode {
            0xAA => {
                // Left Shift release
                unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_shift = false; }
            }
            0xB6 => {
                // Right Shift release
                unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_shift = false; }
            }
            0x9D => {
                // Left Ctrl release
                unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_ctrl = false; }
            }
            0x38 => {
                // Left Alt release
                unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_alt = false; }
            }
            0xBD => {
                // Right Alt release
                unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_alt = false; }
            }
            _ => {}
        }
        return;
    }

    // Handle special keys for modifier tracking
    match scancode {
        // Make codes (key press)
        0x2A => {
            // Left Shift press
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_shift = true; }
            return;
        }
        0xAA => {
            // Left Shift release
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_shift = false; }
            return;
        }
        0x36 => {
            // Right Shift press
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_shift = true; }
            return;
        }
        0xB6 => {
            // Right Shift release
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_shift = false; }
            return;
        }
        0x1D => {
            // Left Ctrl press
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_ctrl = true; }
            return;
        }
        0x9D => {
            // Left Ctrl release
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_ctrl = false; }
            return;
        }
        0xE0 => {
            // Extended key prefix - skip for now, handled in next byte
            return;
        }
        0x38 => {
            // Left Alt press
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_alt = true; }
            return;
        }
        0xB8 => {
            // Left Alt release
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).left_alt = false; }
            return;
        }
        0x5C => {
            // Right Alt press (Option/AltGr)
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_alt = true; }
            return;
        }
        0xBD => {
            // Right Alt release
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).right_alt = false; }
            return;
        }
        0x45 => {
            // CapsLock toggle (make code only)
            unsafe { (*core::ptr::addr_of_mut!(KBD_STATE)).capslock = !(*core::ptr::addr_of!(KBD_STATE)).capslock; }
            return;
        }

        // Break codes (0xF0 prefix + scancode) - handled by skipping 0xF0
        _ => {
            // Check if this could be a break code
            if scancode == 0xF0 {
                return; // Next byte will be the actual break code scancode
            }

            // Regular make code - process it
            let mods = get_modifiers();
            let ch = scancode_to_ascii(scancode, mods);

            let event = KeyEvent {
                ch,
                scancode,
                modifiers: mods,
                press: true,
            };

            push_event(event);

            // Log printable keys
            if ch >= 32 && ch < 127 {
                serial::write_str_safe("[KBD] key='");
                serial::write_byte(ch);
                serial::write_str_safe("' (scancode=0x");
                write_u8_hex(scancode);
                serial::write_str_safe(")\r\n");
            }
        }
    }
}

/// Check if keyboard buffer has events
#[allow(static_mut_refs)]
pub fn has_events() -> bool {
    unsafe { BUFFER_HEAD != BUFFER_TAIL }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scancode_to_ascii() {
        // Test letter A (scancode 0x1E)
        assert_eq!(scancode_to_ascii(0x1E, MOD_NONE), b'A');

        // Test letter 1 (scancode 0x02)
        assert_eq!(scancode_to_ascii(0x02, MOD_NONE), b'1');
    }
}