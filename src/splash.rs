//! Phase 1E: Modular Console Boot Splash
//! Displays a 64x64 Magma Core emblem with alpha-blending and a progress bar

use crate::framebuffer;

/// Progress phase constants
pub const PHASE_RESET: u8 = 0;
pub const PHASE_GDT: u8 = 1;
pub const PHASE_IDT: u8 = 2;
pub const PHASE_HEAP: u8 = 3;
pub const PHASE_COMPLETE: u8 = 4;

/// Current splash phase
static mut PROGRESS: u8 = 0;

/// Splash theme colors
pub struct SplashTheme {
    pub background: u32,
    pub header: u32,
    pub text: u32,
    pub progress_bg: u32,
    pub progress_fg: u32,
}

impl Default for SplashTheme {
    fn default() -> Self {
        SplashTheme {
            background: 0x0B0F17,      // Dark blue-ish
            header: 0xE85D04,          // Orange header
            text: 0xFFFFFF,            // White text
            progress_bg: 0x333333,     // Dark gray progress bar background
            progress_fg: 0x00FFFF,     // Cyan progress bar fill
        }
    }
}

/// Draw a static string at position (x, y) without heap allocation
fn draw_static_str(fb: &framebuffer::Framebuffer, x: usize, y: usize, s: &str, fg: u32, bg: u32, scale: usize) {
    let mut current_x = x;
    for c in s.chars() {
        if scale == 0 { return; }
        let char_w = if c == ' ' { 8 } else if (' '..='~').contains(&c) { 8 } else { 8 };
        for row in 0..8 {
            for bit in 0..8 {
                let pixel_color = if (get_char_bitmap(c, row) >> bit) & 1 != 0 { fg } else { bg };
                let px = current_x + bit * scale;
                let py = y + row * scale;
                fb.fill_rect(px, py, scale, scale, pixel_color);
            }
        }
        current_x += char_w * scale;
    }
}

/// Get 8x8 bitmap for ASCII characters
fn get_char_bitmap(c: char, _row: usize) -> u8 {
    // Simple 8x8 pattern for common chars
    match c {
        ' ' => 0x00,
        '!' => 0x18,
        '?' => 0x3C,
        ':' => 0x00,
        _ => 0x00,
    }
}

/// Generate 64x64 Magma Core emblem sprite
fn generate_emblem() -> [[u32; 64]; 64] {
    let mut sprite: [[u32; 64]; 64] = [[0u32; 64]; 64];

    // Draw a simple "M" shape with anti-aliased edges
    for y in 0..64 {
        for x in 0..64 {
            // Create the "M" shape
            let is_in_m = if y < 32 {
                // Top half of M
                let threshold = 12.0 + 8.0 * ((x as f32 / 32.0).min(1.0 - (x as f32 / 32.0).min(1.0)));
                (y as f32).abs() < threshold
            } else {
                // Bottom half of M (inverted V)
                let threshold = 12.0 + 8.0 * ((1.0 - ((y - 32) as f32) / 32.0).max(0.0));
                (y as f32 - 32.0).abs() < threshold
            };

            if !is_in_m {
                sprite[y][x] = 0x00000000; // Transparent
            } else {
                // Gradient with molten orange/gold
                let glow = ((64.0 - (x as f32 - 32.0).abs()).max(0.0) * (64.0 - (y as f32 - 32.0).abs()).max(0.0) / 100.0).min(1.0);

                let r = if glow > 0.5 { 255u32 } else { 222 };   // 0xFF or 0xDE
                let g = if glow > 0.5 { 215 } else { 139 };       // 0xD7 or 0x8B
                let b = if glow > 0.5 { 0 } else { 69 };          // 0x00 or 0x45

                // Alpha based on glow (more transparent at edges)
                let a = (glow * 200.0 + 55.0) as u8;

                sprite[y][x] = ((a as u32) << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    sprite
}

/// Blend two colors using alpha
fn blend_alpha(dst: u32, src: u32, alpha: u8) -> u32 {
    if alpha == 255 {
        return src;
    }
    if alpha == 0 {
        return dst;
    }

    let src_rgb = src;
    let dst_rgb = dst;

    let src_r = (src_rgb >> 16) & 0xFF;
    let src_g = (src_rgb >> 8) & 0xFF;
    let src_b = src_rgb & 0xFF;

    let dst_r = (dst_rgb >> 16) & 0xFF;
    let dst_g = (dst_rgb >> 8) & 0xFF;
    let dst_b = dst_rgb & 0xFF;

    let inv_alpha = 255 - alpha as u16;
    let alpha = alpha as u16;

    let r = ((src_r as u16 * alpha + dst_r as u16 * inv_alpha) / 255) as u32;
    let g = ((src_g as u16 * alpha + dst_g as u16 * inv_alpha) / 255) as u32;
    let b = ((src_b as u16 * alpha + dst_b as u16 * inv_alpha) / 255) as u32;

    (r << 16) | (g << 8) | b
}

/// Initialize the splash screen
pub fn init() {
    let theme = SplashTheme::default();
    if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
        // Clear with background color
        let (width, _height) = fb.dimensions();
        fb.clear(theme.background);

        // Draw header bar
        fb.fill_rect(0, 0, width, 64, theme.header);

        // Draw "MAGMA OS" text in header
        draw_static_str(fb, 16, 16, "MAGMA OS", theme.text, theme.background, 3);

        // Draw Magma Core emblem centered
        let emblem = generate_emblem();
        let emblem_x = (width - 64) / 2;
        let emblem_y = 120;

        // Blit with alpha blending
        for row in 0..64 {
            for col in 0..64 {
                let pixel = fb.pixel_addr(emblem_x + col, emblem_y + row);
                if !pixel.is_null() {
                    let sprite_color = emblem[row][col];
                    let a = ((sprite_color >> 24) & 0xFF) as u8;
                    let rgb = sprite_color & 0x00FFFFFF;
                    let current = unsafe { core::ptr::read(pixel) };
                    let blended = blend_alpha(current, rgb, a);
                    unsafe { core::ptr::write(pixel, blended); }
                }
            }
        }

        // Draw progress bar
        let bar_x = (width - 300) / 2;
        let bar_y = 250;
        let bar_w = 300;
        let bar_h = 20;

        fb.fill_rect(bar_x, bar_y, bar_w, bar_h, theme.progress_bg);

        // Progress text (static)
        draw_static_str(fb, bar_x + 50, bar_y + 2, "Initializing...", theme.text, theme.background, 2);
    }
}

/// Set the current progress value (0 to 100)
pub fn set_progress(percent: u32) {
    if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
        let theme = SplashTheme::default();
        let (width, _) = fb.dimensions();
        let bar_x = (width - 300) / 2;
        let bar_y = 250;
        let bar_w = 300u32;
        let bar_h = 20u32;

        // Draw filled progress bar using integer arithmetic
        let filled_w = (bar_w * percent) / 100;
        if filled_w > 0 {
            fb.fill_rect(bar_x, bar_y, filled_w as usize, bar_h as usize, theme.progress_fg);
        }
    }

    // Serial trace for debugging
    crate::serial::write_str_safe("[SPLASH] Progress set to ");
    if percent >= 100 {
        crate::serial::write_str_safe("100%\r\n");
    } else if percent >= 10 {
        let tens = ((percent / 10) % 10) as u8 + b'0';
        let ones = (percent % 10) as u8 + b'0';
        let buf = [tens, ones, b'%', b'\r', b'\n'];
        crate::serial::write_str_safe(unsafe { core::str::from_utf8_unchecked(&buf) });
    } else {
        let ones = (percent % 10) as u8 + b'0';
        let buf = [ones, b'%', b'\r', b'\n'];
        crate::serial::write_str_safe(unsafe { core::str::from_utf8_unchecked(&buf) });
    }
}

/// Update progress based on phase
pub fn set_phase(phase: u8, _label: &[u8]) {
    unsafe {
        PROGRESS = phase;
    }

    // Calculate progress percentage as integer (to avoid floating-point)
    // Monotonic order: GDT (25%) -> IDT (50%) -> Heap (75%) -> Complete (100%)
    let pct = match phase {
        PHASE_GDT => 25,
        PHASE_IDT => 50,
        PHASE_HEAP => 75,
        PHASE_COMPLETE => 100,
        _ => 5,
    };

    set_progress(pct);

    if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
        let theme = SplashTheme::default();
        let (width, _) = fb.dimensions();

        // Clear and redraw status area
        fb.fill_rect(0, 300, width, 100, theme.background);

        // Draw phase status
        let phase_str = match phase {
            PHASE_GDT => "GDT Loading",
            PHASE_IDT => "IDT Init",
            PHASE_HEAP => "Heap Setting",
            PHASE_COMPLETE => "Boot Complete",
            _ => "Initializing",
        };
        draw_static_str(fb, (width - 100) / 2, 320, phase_str, theme.text, theme.background, 2);

        // Draw percentage manually using integer arithmetic
        if pct >= 100 {
            draw_static_str(fb, (width - 24) / 2, 380, "100%", theme.progress_fg, theme.background, 3);
        } else if pct >= 10 {
            let tens = ((pct / 10) % 10) as u8 + b'0';
            let ones = (pct % 10) as u8 + b'0';
            // Draw tens and ones digit - bind to local variable to avoid temp drop issue
            let buf = [tens, ones, b'%'];
            let pct_str = unsafe { core::str::from_utf8_unchecked(&buf) };
            draw_static_str(fb, (width - 16) / 2, 380, pct_str, theme.progress_fg, theme.background, 3);
        } else {
            let ones = pct as u8 + b'0';
            // Bind to local variable to avoid temp drop issue
            let buf = [ones, b'%'];
            let pct_str = unsafe { core::str::from_utf8_unchecked(&buf) };
            draw_static_str(fb, (width - 8) / 2, 380, pct_str, theme.progress_fg, theme.background, 3);
        }
    }
}

/// Finalize splash screen
pub fn complete() {
    set_progress(100);

    if let Some(fb) = framebuffer::FRAMEBUFFER.get() {
        let theme = SplashTheme::default();
        let (width, _height) = fb.dimensions();

        // Clear status area
        fb.fill_rect(0, 300, width, 100, theme.background);

        // Draw "Boot Complete"
        draw_static_str(fb, 10, 380, "Boot Complete", theme.progress_fg, theme.background, 3);

        // Draw the phases
        draw_static_str(fb, 10, 420, "GDT IDT Heap OK", theme.text, theme.background, 1);
    }
}