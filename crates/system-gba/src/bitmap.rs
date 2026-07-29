//! Bitmap video modes 3, 4, and 5.
//!
//! # Why these are not in `ppu-tile2d`
//!
//! Prompt 08 draws the line at what is genuinely shared, and these are not. There is no tile,
//! no tilemap, and in mode 3 no palette either: VRAM *is* the picture. Running that through a
//! tile pipeline would mean inventing a one-pixel tile and a fake map, and the shared crate
//! would grow a branch that no other system ever takes.
//!
//! # The three modes trade resolution against buffering
//!
//! | Mode | Size | Colour | Frames |
//! |---|---|---|---|
//! | 3 | 240×160 | direct 15-bit | one |
//! | 4 | 240×160 | 8-bit paletted | two |
//! | 5 | 160×128 | direct 15-bit | two |
//!
//! Mode 3 gets full resolution and full colour but has no room left for a second frame, so a
//! game drawing an animation in it tears. Mode 4 halves the colour depth to buy double
//! buffering; mode 5 shrinks the picture to buy it instead. Which compromise a game picks is
//! visible on screen, so all three have to be right.

use core_common::{Framebuffer, Rgba8};

use crate::video::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// Mode 5's smaller picture, which is centred in the display area.
pub const MODE5_WIDTH: u32 = 160;
pub const MODE5_HEIGHT: u32 = 128;

/// Expand a 15-bit BGR colour to RGBA8.
///
/// The same expansion the Game Boy Color uses, and for the same reason: `c << 3 | c >> 2` is
/// exact at both ends, where `c << 3` alone leaves white at 248 and casts a grey tint over
/// every bright colour on screen.
#[inline]
pub fn bgr555_to_rgba8(value: u16) -> Rgba8 {
    let expand = |c: u16| {
        let c = (c & 0x1F) as u8;
        (c << 3) | (c >> 2)
    };
    Rgba8 {
        r: expand(value),
        g: expand(value >> 5),
        b: expand(value >> 10),
        a: 0xFF,
    }
}

/// Render one scanline of a bitmap mode into `row`, which is RGBA8.
///
/// `frame_offset` selects the displayed buffer in modes 4 and 5 and is ignored in mode 3, which
/// has only one.
///
/// Pixels outside mode 5's smaller picture are left as the backdrop rather than being drawn
/// black: the area around it shows palette entry zero, the same as anywhere else nothing was
/// drawn.
pub fn render_scanline(
    mode: u16,
    line: u32,
    vram: &[u8],
    palette: &[u8],
    frame_offset: usize,
    row: &mut [u8],
) {
    let backdrop = backdrop_colour(palette);
    for pixel in row.chunks_exact_mut(4) {
        write_pixel(pixel, backdrop);
    }
    if line >= SCREEN_HEIGHT {
        return;
    }

    match mode {
        3 => {
            let base = (line * SCREEN_WIDTH * 2) as usize;
            for x in 0..SCREEN_WIDTH as usize {
                let offset = base + x * 2;
                let Some(colour) = halfword(vram, offset) else {
                    break;
                };
                write_pixel(&mut row[x * 4..x * 4 + 4], bgr555_to_rgba8(colour));
            }
        }
        4 => {
            let base = frame_offset + (line * SCREEN_WIDTH) as usize;
            for x in 0..SCREEN_WIDTH as usize {
                let Some(&index) = vram.get(base + x) else {
                    break;
                };
                // Index zero is transparent here exactly as it is in a tile mode, so it shows
                // the backdrop rather than palette entry zero drawn over itself.
                if index == 0 {
                    continue;
                }
                let Some(colour) = halfword(palette, index as usize * 2) else {
                    continue;
                };
                write_pixel(&mut row[x * 4..x * 4 + 4], bgr555_to_rgba8(colour));
            }
        }
        5 => {
            if line >= MODE5_HEIGHT {
                return;
            }
            let base = frame_offset + (line * MODE5_WIDTH * 2) as usize;
            for x in 0..MODE5_WIDTH as usize {
                let offset = base + x * 2;
                let Some(colour) = halfword(vram, offset) else {
                    break;
                };
                write_pixel(&mut row[x * 4..x * 4 + 4], bgr555_to_rgba8(colour));
            }
        }
        _ => {}
    }
}

/// Fill the whole framebuffer white, as forced blank does.
///
/// Forced blank is not "draw nothing": the screen goes white and video memory is left entirely
/// alone, which is what makes it usable as a way to hide a mid-frame rewrite of VRAM.
pub fn render_forced_blank(framebuffer: &mut Framebuffer) {
    framebuffer.fill(Rgba8::WHITE);
}

/// Palette entry zero, which shows wherever nothing was drawn.
fn backdrop_colour(palette: &[u8]) -> Rgba8 {
    match halfword(palette, 0) {
        Some(colour) => bgr555_to_rgba8(colour),
        None => Rgba8::BLACK,
    }
}

#[inline]
fn halfword(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

#[inline]
fn write_pixel(out: &mut [u8], colour: Rgba8) {
    out[0] = colour.r;
    out[1] = colour.g;
    out[2] = colour.b;
    out[3] = colour.a;
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: u16 = 0x001F;
    const GREEN: u16 = 0x03E0;
    const BLUE: u16 = 0x7C00;

    fn row() -> Vec<u8> {
        vec![0; SCREEN_WIDTH as usize * 4]
    }

    fn pixel(row: &[u8], x: usize) -> Rgba8 {
        Rgba8 {
            r: row[x * 4],
            g: row[x * 4 + 1],
            b: row[x * 4 + 2],
            a: row[x * 4 + 3],
        }
    }

    fn put(vram: &mut [u8], offset: usize, value: u16) {
        vram[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn five_bit_channels_expand_to_the_full_range() {
        // `c << 3` alone leaves white at 248 and casts a grey tint over every bright colour.
        assert_eq!(bgr555_to_rgba8(0x0000), Rgba8::BLACK);
        assert_eq!(bgr555_to_rgba8(0x7FFF), Rgba8::WHITE);
        assert_eq!(bgr555_to_rgba8(RED).r, 0xFF);
        assert_eq!(bgr555_to_rgba8(GREEN).g, 0xFF);
        assert_eq!(bgr555_to_rgba8(BLUE).b, 0xFF);
    }

    #[test]
    fn mode_three_reads_the_picture_straight_out_of_vram() {
        // No tile, no map, no palette: VRAM *is* the picture.
        let mut vram = vec![0u8; 0x1_8000];
        put(&mut vram, 0, RED);
        put(&mut vram, 2, GREEN);
        // Second line, first pixel.
        put(&mut vram, (SCREEN_WIDTH * 2) as usize, BLUE);

        let mut row = row();
        render_scanline(3, 0, &vram, &[0; 0x400], 0, &mut row);
        assert_eq!(pixel(&row, 0).r, 0xFF);
        assert_eq!(pixel(&row, 1).g, 0xFF);

        render_scanline(3, 1, &vram, &[0; 0x400], 0, &mut row);
        assert_eq!(pixel(&row, 0).b, 0xFF);
    }

    #[test]
    fn mode_four_looks_its_bytes_up_in_the_palette() {
        let mut vram = vec![0u8; 0x1_8000];
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 2 * 2, GREEN);
        vram[0] = 2;

        let mut row = row();
        render_scanline(4, 0, &vram, &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).g, 0xFF);
    }

    #[test]
    fn mode_four_index_zero_shows_the_backdrop_rather_than_being_drawn() {
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, BLUE); // the backdrop
        put(&mut palette, 2, RED);

        let mut vram = vec![0u8; 0x1_8000];
        vram[1] = 1;

        let mut row = row();
        render_scanline(4, 0, &vram, &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).b, 0xFF, "index 0 left the backdrop showing");
        assert_eq!(pixel(&row, 1).r, 0xFF);
    }

    #[test]
    fn the_frame_bit_selects_the_second_buffer_in_mode_four() {
        // The point of halving the colour depth: draw into the hidden frame and flip a bit.
        let mut vram = vec![0u8; 0x1_8000];
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 2, RED);
        put(&mut palette, 4, GREEN);
        vram[0] = 1; // frame 0
        vram[0xA000] = 2; // frame 1

        let mut row = row();
        render_scanline(4, 0, &vram, &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).r, 0xFF);
        render_scanline(4, 0, &vram, &palette, 0xA000, &mut row);
        assert_eq!(pixel(&row, 0).g, 0xFF);
    }

    #[test]
    fn mode_five_draws_a_smaller_picture_and_leaves_the_rest_as_backdrop() {
        // Mode 5 buys double buffering by shrinking the picture rather than by dropping colour
        // depth, and the area around it is not black — it is the backdrop like anywhere else.
        let mut vram = vec![0u8; 0x1_8000];
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, BLUE);
        put(&mut vram, 0, RED);

        let mut row = row();
        render_scanline(5, 0, &vram, &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).r, 0xFF);
        assert_eq!(
            pixel(&row, MODE5_WIDTH as usize).b,
            0xFF,
            "outside the picture, the backdrop shows"
        );
    }

    #[test]
    fn mode_five_leaves_lines_below_its_picture_as_backdrop() {
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, BLUE);
        let mut row = row();
        render_scanline(
            5,
            MODE5_HEIGHT,
            &vec![0xFF; 0x1_8000],
            &palette,
            0,
            &mut row,
        );
        assert_eq!(pixel(&row, 0).b, 0xFF);
    }

    #[test]
    fn the_backdrop_shows_wherever_a_mode_does_not_reach() {
        // Mode 3 is deliberately absent: it covers every pixel of every visible line and has no
        // transparent index, so the backdrop is never visible in it. Asserting otherwise would
        // be asserting a bug.
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, GREEN);

        let mut row = row();
        render_scanline(4, 0, &vec![0u8; 0x1_8000], &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).g, 0xFF, "mode 4 index 0 is transparent");

        render_scanline(5, 0, &vec![0u8; 0x1_8000], &palette, 0, &mut row);
        assert_eq!(
            pixel(&row, SCREEN_WIDTH as usize - 1).g,
            0xFF,
            "mode 5 leaves the sides of the screen uncovered"
        );
    }

    #[test]
    fn mode_three_covers_every_visible_pixel() {
        // Its counterpart to the test above: no transparency, so a zero in VRAM is black rather
        // than a hole showing the backdrop.
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, GREEN);
        let mut row = row();
        render_scanline(3, 0, &vec![0u8; 0x1_8000], &palette, 0, &mut row);
        assert_eq!(pixel(&row, SCREEN_WIDTH as usize - 1), Rgba8::BLACK);
    }

    #[test]
    fn a_line_past_the_bottom_of_the_screen_draws_nothing() {
        let mut row = row();
        render_scanline(
            3,
            SCREEN_HEIGHT,
            &vec![0xFF; 0x1_8000],
            &[0; 0x400],
            0,
            &mut row,
        );
        assert_eq!(pixel(&row, 0), Rgba8::BLACK, "the backdrop, not VRAM");
    }

    #[test]
    fn a_tile_mode_number_draws_nothing_here() {
        // Modes 0-2 are the tile pipeline's business; this module must not guess at them.
        let mut row = row();
        let mut palette = vec![0u8; 0x400];
        put(&mut palette, 0, RED);
        render_scanline(0, 0, &vec![0xFF; 0x1_8000], &palette, 0, &mut row);
        assert_eq!(pixel(&row, 0).r, 0xFF, "backdrop only");
    }

    #[test]
    fn forced_blank_shows_white_rather_than_the_backdrop() {
        // It is how a game hides a mid-frame rewrite of VRAM, so it must not depend on palette
        // contents that the rewrite may be in the middle of changing.
        let mut framebuffer = Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT);
        render_forced_blank(&mut framebuffer);
        assert_eq!(framebuffer.pixel(0, 0), Rgba8::WHITE);
        assert_eq!(
            framebuffer.pixel(SCREEN_WIDTH - 1, SCREEN_HEIGHT - 1),
            Rgba8::WHITE
        );
    }
}
