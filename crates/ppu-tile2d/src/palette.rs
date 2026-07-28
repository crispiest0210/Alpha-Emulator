//! Palette lookup, abstracted over the three formats these systems use.
//!
//! A trait rather than one hardcoded format, because the three generations store colour
//! genuinely differently: the DMG has no colour at all and indexes a fixed four-shade ramp
//! through a remapping register, while the GBC and GBA hold 15-bit BGR in palette RAM.

use core_common::{Rgba8, Savable, StateError, StateReader, StateWriter};

/// Where a composited pixel's colour comes from.
pub trait PaletteSource {
    /// Colour for a background pixel.
    fn lookup_bg(&self, palette: u8, color: u8) -> Rgba8;
    /// Colour for a sprite pixel.
    ///
    /// Separate from the background lookup because every one of these systems has separate
    /// sprite palettes, and on the DMG they are separate *registers* with different
    /// transparency rules rather than merely a different region of one palette RAM.
    fn lookup_sprite(&self, palette: u8, color: u8) -> Rgba8;
    /// The colour shown where nothing was drawn.
    fn backdrop(&self) -> Rgba8 {
        self.lookup_bg(0, 0)
    }
}

/// The four shades a DMG can display, from lightest to darkest.
///
/// The greenish cast of the original hardware is a property of the LCD, not the console, so
/// these are neutral greys. A frontend that wants the green look applies it as a filter,
/// which keeps the emulated output honest and the styling a presentation choice.
pub const DMG_SHADES: [Rgba8; 4] = [
    Rgba8::rgb(255, 255, 255),
    Rgba8::rgb(170, 170, 170),
    Rgba8::rgb(85, 85, 85),
    Rgba8::rgb(0, 0, 0),
];

/// The DMG's palette registers: `BGP`, `OBP0`, and `OBP1`.
///
/// Each packs four 2-bit shade numbers, one per colour index. The indirection is the point —
/// games animate palettes by rewriting these registers, producing fades and flashes without
/// touching a single tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonochromePalette {
    /// `BGP` at `0xFF47`.
    pub bgp: u8,
    /// `OBP0` and `OBP1` at `0xFF48` and `0xFF49`.
    pub obp: [u8; 2],
}

impl Default for MonochromePalette {
    fn default() -> Self {
        Self::new()
    }
}

impl MonochromePalette {
    /// Powers on as the identity mapping: index 0 is the lightest shade.
    pub const fn new() -> Self {
        Self {
            bgp: 0b11_10_01_00,
            obp: [0b11_10_01_00; 2],
        }
    }

    /// Extract the shade a register assigns to `color`.
    #[inline]
    fn shade(register: u8, color: u8) -> Rgba8 {
        DMG_SHADES[((register >> ((color & 3) * 2)) & 3) as usize]
    }
}

impl PaletteSource for MonochromePalette {
    #[inline]
    fn lookup_bg(&self, _palette: u8, color: u8) -> Rgba8 {
        Self::shade(self.bgp, color)
    }

    #[inline]
    fn lookup_sprite(&self, palette: u8, color: u8) -> Rgba8 {
        // Index 0 is transparent for sprites and never reaches a lookup, so the low two bits
        // of OBP0/OBP1 are unused on hardware. Nothing here depends on that, but it explains
        // why the two registers are otherwise identical in shape to BGP.
        Self::shade(self.obp[(palette & 1) as usize], color)
    }
}

impl Savable for MonochromePalette {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.bgp);
        w.write_u8(self.obp[0]);
        w.write_u8(self.obp[1]);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.bgp = r.read_u8()?;
        self.obp[0] = r.read_u8()?;
        self.obp[1] = r.read_u8()?;
        Ok(())
    }
}

/// Convert one 15-bit BGR colour to 8-bit RGBA.
///
/// Each channel is scaled by replicating its top bits into the low ones rather than shifting
/// and leaving zeros, so full-scale input reaches 255 instead of 248. Getting that wrong
/// makes every bright colour slightly dim, uniformly, which is hard to notice and hard to
/// unsee once pointed out.
#[inline]
pub fn bgr555_to_rgba(value: u16) -> Rgba8 {
    let expand = |c: u16| ((c << 3) | (c >> 2)) as u8;
    Rgba8::rgb(
        expand(value & 0x1F),
        expand((value >> 5) & 0x1F),
        expand((value >> 10) & 0x1F),
    )
}

/// Palette RAM holding 15-bit BGR entries: the GBC's CGB palettes and the GBA's palette RAM.
///
/// Backgrounds and sprites occupy separate halves, which is why the constructor takes the
/// sprite half's byte offset rather than assuming one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bgr555Palette {
    bytes: Box<[u8]>,
    /// Byte offset where the sprite palettes begin.
    sprite_base: usize,
    /// Colours per palette: 4 on the GBC, 16 or 256 on the GBA.
    colors_per_palette: usize,
}

impl Bgr555Palette {
    pub fn new(size: usize, sprite_base: usize, colors_per_palette: usize) -> Self {
        Self {
            bytes: vec![0; size].into_boxed_slice(),
            sprite_base,
            colors_per_palette,
        }
    }

    /// The GBC layout: 64 bytes of background palettes then 64 of sprite palettes, eight
    /// palettes of four colours each.
    pub fn cgb() -> Self {
        Self::new(128, 64, 4)
    }

    /// The GBA layout: 512 bytes split evenly, sixteen palettes of sixteen colours.
    pub fn gba() -> Self {
        Self::new(1024, 512, 16)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    #[inline]
    pub fn read(&self, offset: usize) -> u8 {
        self.bytes.get(offset).copied().unwrap_or(0)
    }

    #[inline]
    pub fn write(&mut self, offset: usize, value: u8) {
        if let Some(slot) = self.bytes.get_mut(offset) {
            *slot = value;
        }
    }

    #[inline]
    fn entry(&self, base: usize, palette: u8, color: u8) -> Rgba8 {
        let index = base + (palette as usize * self.colors_per_palette + color as usize) * 2;
        if index + 1 >= self.bytes.len() {
            return Rgba8::BLACK;
        }
        bgr555_to_rgba(u16::from_le_bytes([
            self.bytes[index],
            self.bytes[index + 1],
        ]))
    }
}

impl PaletteSource for Bgr555Palette {
    #[inline]
    fn lookup_bg(&self, palette: u8, color: u8) -> Rgba8 {
        self.entry(0, palette, color)
    }

    #[inline]
    fn lookup_sprite(&self, palette: u8, color: u8) -> Rgba8 {
        self.entry(self.sprite_base, palette, color)
    }
}

impl Savable for Bgr555Palette {
    fn save(&self, w: &mut StateWriter) {
        w.write_blob(&self.bytes);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.bytes.len() {
            return Err(StateError::Malformed(format!(
                "palette RAM is {} bytes in this build, {} in the state",
                self.bytes.len(),
                bytes.len()
            )));
        }
        self.bytes.copy_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_monochrome_palette_is_the_identity_mapping() {
        let p = MonochromePalette::new();
        for color in 0..4u8 {
            assert_eq!(p.lookup_bg(0, color), DMG_SHADES[color as usize]);
        }
    }

    #[test]
    fn a_rewritten_register_remaps_every_index_at_once() {
        // This is how games fade the screen: one register write, no tile data touched.
        let mut p = MonochromePalette::new();
        p.bgp = 0b00_00_00_00; // everything to the lightest shade
        for color in 0..4u8 {
            assert_eq!(p.lookup_bg(0, color), DMG_SHADES[0]);
        }

        p.bgp = 0b00_01_10_11; // inverted
        assert_eq!(p.lookup_bg(0, 0), DMG_SHADES[3]);
        assert_eq!(p.lookup_bg(0, 3), DMG_SHADES[0]);
    }

    #[test]
    fn the_two_sprite_palettes_are_selected_independently_of_the_background() {
        let mut p = MonochromePalette::new();
        p.bgp = 0b11_11_11_11;
        p.obp[0] = 0b00_01_10_11;
        p.obp[1] = 0b11_10_01_00;

        assert_eq!(p.lookup_bg(0, 0), DMG_SHADES[3]);
        assert_eq!(p.lookup_sprite(0, 0), DMG_SHADES[3]);
        assert_eq!(p.lookup_sprite(1, 0), DMG_SHADES[0]);
        assert_eq!(p.lookup_sprite(1, 3), DMG_SHADES[3]);
    }

    #[test]
    fn bgr555_expands_each_channel_to_full_range() {
        assert_eq!(bgr555_to_rgba(0x0000), Rgba8::rgb(0, 0, 0));
        // All five bits set in every channel must reach 255, not 248.
        assert_eq!(bgr555_to_rgba(0x7FFF), Rgba8::rgb(255, 255, 255));
        // Pure red is the low five bits; the format is BGR, not RGB.
        assert_eq!(bgr555_to_rgba(0x001F), Rgba8::rgb(255, 0, 0));
        assert_eq!(bgr555_to_rgba(0x03E0), Rgba8::rgb(0, 255, 0));
        assert_eq!(bgr555_to_rgba(0x7C00), Rgba8::rgb(0, 0, 255));
    }

    #[test]
    fn palette_ram_separates_background_and_sprite_halves() {
        let mut p = Bgr555Palette::cgb();
        // Background palette 1, colour 2 lives at (1*4 + 2) * 2 = 12.
        p.write(12, 0x1F);
        p.write(13, 0x00);
        assert_eq!(p.lookup_bg(1, 2), Rgba8::rgb(255, 0, 0));

        // The same index in the sprite half is a different entry entirely.
        assert_eq!(p.lookup_sprite(1, 2), Rgba8::BLACK);
        p.write(64 + 12, 0x00);
        p.write(64 + 13, 0x7C);
        assert_eq!(p.lookup_sprite(1, 2), Rgba8::rgb(0, 0, 255));
        assert_eq!(p.lookup_bg(1, 2), Rgba8::rgb(255, 0, 0), "unaffected");
    }

    #[test]
    fn the_gba_layout_uses_sixteen_colour_palettes() {
        let mut p = Bgr555Palette::gba();
        // Palette 2, colour 5 is at (2*16 + 5) * 2 = 74.
        p.write(74, 0xFF);
        p.write(75, 0x7F);
        assert_eq!(p.lookup_bg(2, 5), Rgba8::rgb(255, 255, 255));
    }

    #[test]
    fn an_out_of_range_entry_reads_black_rather_than_panicking() {
        let p = Bgr555Palette::cgb();
        assert_eq!(p.lookup_bg(200, 200), Rgba8::BLACK);
    }

    #[test]
    fn the_backdrop_is_the_first_background_entry() {
        let mut p = Bgr555Palette::cgb();
        p.write(0, 0xE0);
        p.write(1, 0x03);
        assert_eq!(p.backdrop(), p.lookup_bg(0, 0));
        assert_eq!(p.backdrop(), Rgba8::rgb(0, 255, 0));
    }

    #[test]
    fn palettes_round_trip_through_a_save_state() {
        let mut mono = MonochromePalette::new();
        mono.bgp = 0x1B;
        mono.obp[1] = 0x2D;
        let mut w = StateWriter::new();
        mono.save(&mut w);
        let blob = w.into_inner();
        let mut restored = MonochromePalette::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, mono);

        let mut color = Bgr555Palette::cgb();
        color.write(20, 0xAB);
        let mut w = StateWriter::new();
        color.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Bgr555Palette::cgb();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, color);
    }
}
