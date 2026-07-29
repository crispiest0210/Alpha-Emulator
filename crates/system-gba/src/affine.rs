//! Affine transformation: rotated and scaled backgrounds and sprites.
//!
//! # Fixed point, and two different formats
//!
//! The matrix components are 8.8 — sixteen bits, eight of them fractional. The background
//! reference point is 20.8, twenty-eight bits in a thirty-two bit register with the top four
//! discarded. Mixing them up scales a background by 256 or shrinks it to nothing, and because
//! both are "8 fractional bits" it is easy to assume they are the same width.
//!
//! # A background's reference point is not recomputed each line
//!
//! It *accumulates*. At the end of every scanline the internal position advances by `pb` and
//! `pd`, and the next line starts from there rather than from the register. That is what makes
//! a mid-frame write to `BG2PB` bend the picture from that line down instead of shifting the
//! whole layer, which is how games do perspective floors. Recomputing from the register each
//! line looks correct on a static screen and wrong the moment anything animates.
//!
//! A write to the reference-point register *does* reload the internal position — that is how a
//! game resets the effect at the top of a frame.
//!
//! # A sprite's transform runs the other way
//!
//! A background maps screen position to texture position by accumulating forward. A sprite maps
//! an offset from the centre of its on-screen box back into its own tile data, so the matrix is
//! applied to a coordinate relative to the centre rather than accumulated across the line.

use core_common::{Savable, StateError, StateReader, StateWriter};

use crate::objects::AffineMatrix;

/// Base address of `BG2PA`. The two affine layers have eight bytes of matrix each, followed by
/// eight bytes of reference point.
pub const BG2_BASE: u32 = 0x0400_0020;
pub const BG3_BASE: u32 = 0x0400_0030;

/// Fractional bits in every value here.
pub const FRACTIONAL_BITS: u32 = 8;

/// A 20.8 fixed-point coordinate, sign-extended from the 28 bits the register holds.
#[inline]
pub fn sign_extend_28(value: u32) -> i32 {
    let value = value & 0x0FFF_FFFF;
    if value & 0x0800_0000 != 0 {
        (value | 0xF000_0000) as i32
    } else {
        value as i32
    }
}

/// One affine background layer's registers and its running position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AffineBackground {
    pub matrix: AffineMatrix,
    /// The reference point as the game wrote it, in 20.8 fixed point.
    pub reference_x: i32,
    pub reference_y: i32,
    /// Where the next scanline starts from. Advances by `pb`/`pd` per line and is reloaded by a
    /// write to the reference-point registers.
    current_x: i32,
    current_y: i32,
}

impl AffineBackground {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reload the running position from the registers, as the start of a frame does.
    pub fn begin_frame(&mut self) {
        self.current_x = self.reference_x;
        self.current_y = self.reference_y;
    }

    /// Advance to the next scanline.
    ///
    /// By `pb` and `pd`, not by one: those are the matrix's contribution *per line*, and using
    /// anything else turns a rotation back into a plain scroll.
    pub fn advance_line(&mut self) {
        self.current_x = self.current_x.wrapping_add(self.matrix.pb as i32);
        self.current_y = self.current_y.wrapping_add(self.matrix.pd as i32);
    }

    /// The texture coordinate for a pixel at `x` along the current scanline.
    ///
    /// Returned in whole pixels: the fractional part has done its job by the time it is added
    /// up, and keeping it would only push the rounding into every caller.
    pub fn texture_at(&self, x: u32) -> (i32, i32) {
        let step = x as i32;
        let tx = self.current_x.wrapping_add(self.matrix.pa as i32 * step);
        let ty = self.current_y.wrapping_add(self.matrix.pc as i32 * step);
        (tx >> FRACTIONAL_BITS, ty >> FRACTIONAL_BITS)
    }

    pub fn write32(&mut self, offset: u32, value: u32) {
        match offset {
            // Writing a reference point reloads the running position: this is how a game resets
            // the effect at the top of a frame.
            0x8 => {
                self.reference_x = sign_extend_28(value);
                self.current_x = self.reference_x;
            }
            0xC => {
                self.reference_y = sign_extend_28(value);
                self.current_y = self.reference_y;
            }
            _ => {
                self.write16(offset, value as u16);
                self.write16(offset + 2, (value >> 16) as u16);
            }
        }
    }

    pub fn write16(&mut self, offset: u32, value: u16) {
        match offset {
            0x0 => self.matrix.pa = value as i16,
            0x2 => self.matrix.pb = value as i16,
            0x4 => self.matrix.pc = value as i16,
            0x6 => self.matrix.pd = value as i16,
            // A halfword write to a reference point touches only that half, and still reloads.
            0x8 => {
                self.reference_x =
                    sign_extend_28((self.reference_x as u32 & 0xFFFF_0000) | value as u32);
                self.current_x = self.reference_x;
            }
            0xA => {
                self.reference_x =
                    sign_extend_28((self.reference_x as u32 & 0xFFFF) | ((value as u32) << 16));
                self.current_x = self.reference_x;
            }
            0xC => {
                self.reference_y =
                    sign_extend_28((self.reference_y as u32 & 0xFFFF_0000) | value as u32);
                self.current_y = self.reference_y;
            }
            0xE => {
                self.reference_y =
                    sign_extend_28((self.reference_y as u32 & 0xFFFF) | ((value as u32) << 16));
                self.current_y = self.reference_y;
            }
            _ => {}
        }
    }
}

/// Map a point inside a sprite's on-screen box back to a texture coordinate.
///
/// `offset_x`/`offset_y` are measured from the centre of the *screen* box, which for a
/// double-size sprite is twice the sprite's own size. `half_width`/`half_height` are half the
/// sprite's true size, so the result lands inside its tile data.
///
/// Runs the opposite way from a background: a background accumulates forward across the line,
/// while a sprite transforms a coordinate relative to its centre. Using the background's
/// accumulation for a sprite produces a shear that grows with screen position.
pub fn transform_object_pixel(
    matrix: &AffineMatrix,
    offset_x: i32,
    offset_y: i32,
    half_width: i32,
    half_height: i32,
) -> (i32, i32) {
    let tx = (matrix.pa as i32 * offset_x + matrix.pb as i32 * offset_y) >> FRACTIONAL_BITS;
    let ty = (matrix.pc as i32 * offset_x + matrix.pd as i32 * offset_y) >> FRACTIONAL_BITS;
    (tx + half_width, ty + half_height)
}

/// The identity transform, as the hardware's fixed-point format spells it.
pub const IDENTITY: AffineMatrix = AffineMatrix {
    pa: 1 << FRACTIONAL_BITS,
    pb: 0,
    pc: 0,
    pd: 1 << FRACTIONAL_BITS,
};

impl Savable for AffineBackground {
    fn save(&self, w: &mut StateWriter) {
        self.matrix.save(w);
        w.write_u32(self.reference_x as u32);
        w.write_u32(self.reference_y as u32);
        w.write_u32(self.current_x as u32);
        w.write_u32(self.current_y as u32);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.matrix.load(r)?;
        self.reference_x = r.read_u32()? as i32;
        self.reference_y = r.read_u32()? as i32;
        self.current_x = r.read_u32()? as i32;
        self.current_y = r.read_u32()? as i32;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_layer() -> AffineBackground {
        let mut layer = AffineBackground::new();
        layer.matrix = IDENTITY;
        layer
    }

    #[test]
    fn the_identity_matrix_maps_each_pixel_to_itself() {
        let layer = identity_layer();
        for x in [0u32, 1, 100, 239] {
            assert_eq!(layer.texture_at(x), (x as i32, 0), "pixel {x}");
        }
    }

    #[test]
    fn a_reference_point_is_twenty_eight_bits_and_signs_from_bit_twenty_seven() {
        // The matrix components are 16-bit and the reference point is 28-bit, and both have
        // eight fractional bits — which is exactly why they get confused.
        assert_eq!(sign_extend_28(0x0000_0100), 256);
        assert_eq!(sign_extend_28(0x0FFF_FF00), -256);
        assert_eq!(sign_extend_28(0x0800_0000), -134_217_728);
        // The top four bits of the register are discarded rather than contributing.
        assert_eq!(sign_extend_28(0xF000_0100), 256);
    }

    #[test]
    fn doubling_pa_halves_the_apparent_size() {
        // Scaling is inverse: a larger step through the texture fits more of it in the same
        // number of screen pixels.
        let mut layer = identity_layer();
        layer.matrix.pa = 2 << FRACTIONAL_BITS;
        assert_eq!(layer.texture_at(10), (20, 0));
    }

    #[test]
    fn the_reference_point_accumulates_across_lines_rather_than_being_recomputed() {
        // A background's position advances by pb and pd at the end of every line. Recomputing
        // from the register each line looks right on a static screen and wrong the moment
        // anything animates.
        let mut layer = identity_layer();
        layer.matrix.pb = 1 << FRACTIONAL_BITS;
        layer.matrix.pd = 2 << FRACTIONAL_BITS;
        layer.begin_frame();

        assert_eq!(layer.texture_at(0), (0, 0));
        layer.advance_line();
        assert_eq!(layer.texture_at(0), (1, 2));
        layer.advance_line();
        assert_eq!(layer.texture_at(0), (2, 4));
    }

    #[test]
    fn changing_the_matrix_mid_frame_bends_the_picture_from_that_line_down() {
        // This is how a game draws a perspective floor: the accumulated position is kept and
        // only the rate changes.
        let mut layer = identity_layer();
        layer.matrix.pd = 1 << FRACTIONAL_BITS;
        layer.begin_frame();
        layer.advance_line();
        layer.advance_line();
        assert_eq!(layer.texture_at(0).1, 2);

        layer.matrix.pd = 4 << FRACTIONAL_BITS;
        layer.advance_line();
        assert_eq!(
            layer.texture_at(0).1,
            6,
            "carried on from 2 at the new rate, rather than restarting"
        );
    }

    #[test]
    fn writing_a_reference_point_reloads_the_running_position() {
        // How a game resets the effect at the top of a frame.
        let mut layer = identity_layer();
        layer.matrix.pd = 1 << FRACTIONAL_BITS;
        layer.begin_frame();
        layer.advance_line();
        layer.advance_line();
        assert_eq!(layer.texture_at(0).1, 2);

        layer.write32(0xC, 0);
        assert_eq!(layer.texture_at(0).1, 0);
    }

    #[test]
    fn a_halfword_write_touches_only_its_half_of_the_reference_point() {
        let mut layer = identity_layer();
        layer.write32(0x8, 0x0000_1234);
        layer.write16(0xA, 0x0001);
        assert_eq!(layer.reference_x, 0x0001_1234);
    }

    #[test]
    fn beginning_a_frame_returns_to_the_registers() {
        let mut layer = identity_layer();
        layer.write32(0x8, 0x0000_0500);
        layer.matrix.pb = 1 << FRACTIONAL_BITS;
        layer.advance_line();
        layer.advance_line();
        layer.begin_frame();
        assert_eq!(layer.texture_at(0), (5, 0));
    }

    #[test]
    fn the_matrix_registers_decode_into_their_components() {
        let mut layer = AffineBackground::new();
        layer.write16(0x0, 0x0100);
        layer.write16(0x2, 0xFF00);
        layer.write16(0x4, 0x0080);
        layer.write16(0x6, 0x0200);
        assert_eq!(
            layer.matrix,
            AffineMatrix {
                pa: 256,
                pb: -256,
                pc: 128,
                pd: 512,
            }
        );
    }

    #[test]
    fn an_identity_sprite_transform_maps_a_centre_offset_back_to_the_same_place() {
        // Offset zero from the centre is the centre of the sprite's own tile data.
        let (x, y) = transform_object_pixel(&IDENTITY, 0, 0, 8, 8);
        assert_eq!((x, y), (8, 8));

        let (x, y) = transform_object_pixel(&IDENTITY, -8, -8, 8, 8);
        assert_eq!((x, y), (0, 0), "the top-left corner");
    }

    #[test]
    fn a_quarter_turn_sprite_matrix_swaps_the_axes() {
        // pa=0, pb=1, pc=-1, pd=0 is a rotation, and the give-away that it is right is that a
        // horizontal offset comes back as a vertical one.
        let matrix = AffineMatrix {
            pa: 0,
            pb: 1 << FRACTIONAL_BITS,
            pc: -(1 << FRACTIONAL_BITS),
            pd: 0,
        };
        let (x, y) = transform_object_pixel(&matrix, 4, 0, 8, 8);
        assert_eq!((x, y), (8, 4), "a step across became a step down");
    }

    #[test]
    fn a_sprite_transform_does_not_accumulate_across_the_line() {
        // Using the background's accumulation for a sprite produces a shear that grows with
        // screen position, which is why these are two functions rather than one.
        let matrix = AffineMatrix {
            pa: 2 << FRACTIONAL_BITS,
            ..IDENTITY
        };
        assert_eq!(transform_object_pixel(&matrix, 1, 0, 0, 0).0, 2);
        assert_eq!(transform_object_pixel(&matrix, 2, 0, 0, 0).0, 4);
        assert_eq!(
            transform_object_pixel(&matrix, 3, 0, 0, 0).0,
            6,
            "each offset stands alone"
        );
    }

    #[test]
    fn affine_state_round_trips_mid_frame() {
        use savestate::{decode_state, encode_state};
        let mut layer = identity_layer();
        layer.write32(0x8, 0x0000_0700);
        layer.matrix.pb = 3 << FRACTIONAL_BITS;
        layer.begin_frame();
        layer.advance_line();
        layer.advance_line();

        let bytes = encode_state("gba-affine", 1, &layer);
        let mut restored = AffineBackground::new();
        decode_state("gba-affine", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored, layer);
        assert_eq!(
            restored.texture_at(0),
            layer.texture_at(0),
            "including the accumulated position, not just the registers"
        );
    }
}
