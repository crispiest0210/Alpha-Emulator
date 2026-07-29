//! Game Boy Color system assembly.
//!
//! # Where the CGB actually lives, and why
//!
//! Prompt 11 asks for reuse of `system-gb` over a parallel implementation, and says to document
//! the choice. The choice is that **a Game Boy Color is not a second machine**. It is the Game
//! Boy with more memory banks, a second palette path, per-tile attributes, a faster clock, and
//! a DMA engine — and every one of those is a branch inside a component `system-gb` already
//! owns. So [`GbcSystem`] is a thin cap over [`GbSystem`], and the deltas are parameters:
//! [`GbSystem::with_model`] takes a [`Model`], and the components branch on it.
//!
//! The register blocks that a DMG has no concept of — palette RAM, `KEY1`, VRAM DMA — live in
//! `system_gb::cgb`, not here. They have to: the CPU reaches them through the bus, mid-frame,
//! and `system-gbc` depends on `system-gb`, so anything defined here would be invisible from
//! the bus. The alternative was a trait in `system-gb` for this crate to implement, which was
//! rejected — it would have exactly one implementor, its shape dictated entirely by that
//! implementor, and it would put dynamic dispatch in the I/O write path to buy nothing but a
//! file location. That module's docs carry the full reasoning.
//!
//! What is genuinely this crate's is what belongs to the *machine* rather than to any one
//! component: choosing the model from the cartridge header, and the boot path that recolours a
//! DMG cartridge running on colour hardware.
//!
//! # Boot, and what is deliberately missing
//!
//! A real CGB boot ROM picks a compatibility palette by hashing the cartridge title against a
//! table of around thirty hand-assigned palettes, so Tetris comes up blue and Zelda comes up
//! green. That table lives inside a copyrighted boot ROM, and this project does not vendor one
//! — see the note in `system_gb::system`. Without a supplied boot ROM the machine installs a
//! neutral greyscale instead, which reproduces DMG output faithfully and is honest about being
//! the fallback rather than guessing at colours the hardware would have chosen. Supply a real
//! boot ROM and it runs, and does what it does.

//! # Status
//!
//! The machine is assembled and runs: colour rendering with per-tile attributes and CGB sprite
//! priority, the `KEY1` speed switch driven by `STOP`, both VRAM DMA modes, banked VRAM and
//! WRAM, and a save state that survives a speed change. What is not verified is accuracy
//! against CGB test ROMs — `cgb-acid2` and the Mooneye CGB suite are not in the corpus yet, so
//! everything here is checked against hardware documentation and unit tests rather than
//! against a reference implementation.

#![deny(unsafe_code)]

use core_common::{
    AudioSample, CartridgeError, FrameOutput, Framebuffer, InputState, Savable, StateError,
    StateReader, StateWriter, System,
};
use system_gb::cgb::palettes;
use system_gb::{GbSystem, GbcCompatibilityShades};

pub use system_gb::cgb::{
    rgb555_to_rgba8, CgbPalettes, CgbState, Hdma, SpeedSwitch, TileAttributes,
};
pub use system_gb::GbModel as Model;

/// The Game Boy Color.
///
/// A newtype over [`GbSystem`] rather than a struct of its own: everything a CGB adds is
/// already inside that machine, gated on [`Model`]. Making this a wrapper keeps one frame loop,
/// one save-state format, and one place where a timing fix has to land.
pub struct GbcSystem {
    inner: GbSystem,
}

impl GbcSystem {
    /// Load a cartridge on Game Boy Color hardware.
    ///
    /// The header decides whether this runs as a colour machine or in DMG-compatibility mode;
    /// both are CGB hardware, and the difference is what the game is allowed to assume. See
    /// [`Model::for_cartridge`].
    pub fn new(rom: Vec<u8>, boot_rom: Option<Vec<u8>>) -> Result<Self, CartridgeError> {
        // 0x0143 is the CGB flag. Read before the move into `with_model`, and defensively:
        // a ROM too short to contain a header is rejected there, not here.
        let flag = rom.get(0x0143).copied().unwrap_or(0);
        let model = Model::for_cartridge(flag, true);
        let mut inner = GbSystem::with_model(rom, boot_rom, model)?;

        if model == Model::CgbInDmgMode {
            install_compatibility_palette(&mut inner);
        }
        Ok(Self { inner })
    }

    /// The machine underneath, for a debugger or a test that needs at its components.
    pub fn inner(&self) -> &GbSystem {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut GbSystem {
        &mut self.inner
    }
}

/// Give a DMG cartridge the greyscale a DMG would have shown.
///
/// Both palette banks, every palette: a DMG game writes `BGP`/`OBP0`/`OBP1` and expects four
/// shades, and in compatibility mode those registers no longer reach the screen. Without this
/// the game would draw through whatever palette RAM powered up as, which is all-ones white —
/// an invisible picture rather than a monochrome one.
fn install_compatibility_palette(system: &mut GbSystem) {
    for palette in 0..8 {
        for (index, shade) in GbcCompatibilityShades::GREYSCALE.iter().enumerate() {
            system
                .bus_mut()
                .cgb
                .palettes
                .set_colour(false, palette, index as u8, *shade);
            system
                .bus_mut()
                .cgb
                .palettes
                .set_colour(true, palette, index as u8, *shade);
        }
    }
    // Leave the index registers where a fresh machine has them, so a save state taken before
    // the first game write matches one taken after a reset.
    let _ = palettes::reg::BCPS;
}

impl System for GbcSystem {
    fn id(&self) -> &'static str {
        // Distinct from `system-gb`'s "gb" even though the machine underneath is shared: a
        // save state carries this in its header, and loading a CGB state into a DMG session
        // would restore banks and palettes the DMG has nowhere to put.
        "gbc"
    }

    fn display_name(&self) -> &'static str {
        "Game Boy Color"
    }

    fn state_version(&self) -> u32 {
        self.inner.state_version()
    }

    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError> {
        self.inner.load_cartridge(rom)?;
        if Model::for_cartridge(rom.get(0x0143).copied().unwrap_or(0), true) == Model::CgbInDmgMode
        {
            install_compatibility_palette(&mut self.inner);
        }
        Ok(())
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.inner.save_ram()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        self.inner.load_save_ram(data)
    }

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.inner.step_frame(input)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn framebuffer(&self) -> &Framebuffer {
        self.inner.framebuffer()
    }

    fn take_audio_samples(&mut self) -> &[AudioSample] {
        self.inner.take_audio_samples()
    }
}

impl Savable for GbcSystem {
    fn save(&self, w: &mut StateWriter) {
        self.inner.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.inner.load(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dmg_cartridge_on_cgb_hardware_is_its_own_mode() {
        assert_eq!(Model::for_cartridge(0x00, true), Model::CgbInDmgMode);
        assert_eq!(Model::for_cartridge(0x00, false), Model::Dmg);
    }

    #[test]
    fn both_cgb_header_flags_select_full_colour_mode() {
        // 0x80 is "enhanced for CGB, still runs on a DMG" and 0xC0 is "CGB only"; on CGB
        // hardware they are the same thing.
        assert_eq!(Model::for_cartridge(0x80, true), Model::Cgb);
        assert_eq!(Model::for_cartridge(0xC0, true), Model::Cgb);
    }

    #[test]
    fn a_cgb_cartridge_in_a_dmg_still_runs_as_a_dmg() {
        // The 0x80 flag exists precisely so these cartridges boot on original hardware.
        assert_eq!(Model::for_cartridge(0x80, false), Model::Dmg);
    }

    #[test]
    fn compatibility_mode_has_the_hardware_but_not_the_tile_attributes() {
        let m = Model::CgbInDmgMode;
        assert!(m.has_cgb_hardware(), "banking and KEY1 are present");
        assert!(m.uses_colour_palettes(), "the boot ROM recoloured the game");
        assert!(
            !m.uses_tile_attributes(),
            "VRAM bank 1 holds no attribute map, so reading one would decode garbage"
        );
    }

    #[test]
    fn a_dmg_has_none_of_it() {
        let m = Model::Dmg;
        assert!(!m.has_cgb_hardware());
        assert!(!m.uses_colour_palettes());
        assert!(!m.uses_tile_attributes());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use core_common::{Bus, Rgba8};
    use ppu_tile2d::PaletteSource;
    use system_gb::GbPpu;

    /// A tile map cell pointing at tile 1, with tile 1 filled with colour index 1.
    fn ppu_showing_colour_one() -> (GbPpu, Vec<u8>, Vec<u8>) {
        let mut vram = vec![0u8; 0x4000];
        // Tile 1's pixel data: every pixel colour index 1 (low bitplane set, high clear).
        for row in 0..8 {
            vram[0x10 + row * 2] = 0xFF;
        }
        // Tile map entry (0,0) -> tile 1.
        vram[0x1800] = 1;

        let mut ppu = GbPpu::new();
        // LCD on, background on, tile data at 0x8000.
        ppu.lcdc = 0x91;
        (ppu, vram, vec![0u8; 0xA0])
    }

    #[test]
    fn the_same_pixels_resolve_to_grey_on_a_dmg_and_to_colour_through_cgb_palette_ram() {
        // This is the whole point of keeping the scanline buffer indexed until the line is
        // done: one renderer, two lookups. If the PPU had resolved to RGBA during the fetch,
        // colour would have meant a second renderer.
        let (mut ppu, vram, oam) = ppu_showing_colour_one();

        ppu.render_scanline(0, &vram, &oam);
        let dmg = ppu.framebuffer().pixel(0, 0);
        assert_eq!(dmg.r, dmg.g, "a DMG pixel is grey");
        assert_eq!(dmg.g, dmg.b);

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F); // background palette 0, colour 1: red
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0),
            Rgba8 {
                r: 0xFF,
                g: 0,
                b: 0,
                a: 0xFF
            },
            "the same indexed pixel came out red through CGB palette RAM"
        );
    }

    #[test]
    fn clearing_lcdc_bit_zero_blanks_a_dmg_but_not_a_cgb() {
        // The bit keeps its position across the two machines and changes its job. Treating the
        // CGB case as a blank would black out the screen instead of merely reordering layers.
        let (mut ppu, vram, oam) = ppu_showing_colour_one();
        ppu.lcdc = 0x90; // background bit cleared

        ppu.render_scanline(0, &vram, &oam);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0),
            ppu_tile2d::DMG_SHADES[0],
            "a DMG blanks to white"
        );

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "a CGB still draws the background"
        );
    }

    #[test]
    fn compatibility_mode_draws_through_palette_ram_but_reads_no_attributes() {
        // The combination that makes the third variant necessary: a recoloured picture from a
        // tile map that never had an attribute byte written beside it.
        let m = Model::CgbInDmgMode;
        assert!(m.uses_colour_palettes());
        assert!(!m.uses_tile_attributes());
        assert!(!m.bg_enable_blanks_background(), "it is CGB hardware");
    }

    #[test]
    fn the_attribute_byte_picks_a_palette_per_tile() {
        // Without this the whole background resolves through palette 0, which looks like
        // colour is working right up until a game uses more than one palette on a line.
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        // Tile (1,0) also points at tile 1, so both cells draw identical pixels.
        vram[0x1801] = 1;
        // Bank 1, same offsets: cell 0 keeps palette 0, cell 1 takes palette 3.
        vram[0x2000 + 0x1800] = 0;
        vram[0x2000 + 0x1801] = 3;

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F); // palette 0 colour 1: red
        palettes.set_colour(false, 3, 1, 0x7C00); // palette 3 colour 1: blue

        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(ppu.framebuffer().pixel(0, 0).r, 0xFF, "first tile is red");
        assert_eq!(ppu.framebuffer().pixel(8, 0).b, 0xFF, "second tile is blue");
        assert_eq!(ppu.framebuffer().pixel(8, 0).r, 0x00);
    }

    #[test]
    fn compatibility_mode_ignores_whatever_is_in_the_second_bank() {
        // The reason CgbInDmgMode exists. A DMG cartridge never writes bank 1, so anything
        // read from there is uninitialised memory — and decoding it as palette and flip bits
        // would corrupt a picture that is otherwise correct.
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        vram[0x2000 + 0x1800] = 0xFF; // palette 7, bank 1, both flips, priority

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        palettes.set_colour(false, 7, 1, 0x7C00);

        ppu.render_scanline_with(Model::CgbInDmgMode, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "still palette 0, not the 7 the stale byte names"
        );
    }

    #[test]
    fn a_tile_can_take_its_pixels_from_the_second_bank() {
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        // Tile 2 in bank 1, colour index 1 everywhere.
        for row in 0..8 {
            vram[0x2000 + 0x20 + row * 2] = 0xFF;
        }
        vram[0x1800] = 2; // the map names tile 2
        vram[0x2000 + 0x1800] = 0x08; // attribute: bank 1

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "tile data came from bank 1"
        );
    }

    #[test]
    fn a_background_tile_can_demand_to_be_drawn_over_a_sprite() {
        // End to end: the attribute byte's priority bit reaches the sprite compositor. The
        // same scene under the DMG rule puts the sprite in front, because a DMG tile map has
        // no way to ask.
        let (mut ppu, mut vram, mut oam) = ppu_showing_colour_one();
        vram[0x2000 + 0x1800] = 0x80; // background tile asks for priority

        // A sprite at (0,0) using tile 1, which is opaque colour 1, and *not* marked as
        // "behind background" — so only the tile's bit can put it behind.
        oam[0] = 16; // y = 0
        oam[1] = 8; // x = 0
        oam[2] = 1;
        oam[3] = 0x00;
        ppu.lcdc |= 0x02; // sprites on

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F); // background red
        palettes.set_colour(true, 0, 1, 0x03E0); // sprite green

        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "the tile's priority bit kept the background in front"
        );

        // Clearing LCDC bit 0 waves the contest away and the sprite comes forward.
        ppu.lcdc &= !0x01;
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).g,
            0xFF,
            "master priority off puts every sprite in front"
        );
    }

    // -- The assembled machine ---------------------------------------------

    /// A CGB cartridge whose entry point runs `program`.
    fn cgb_rom(program: &[u8], cgb_flag: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0143] = cgb_flag;
        rom[0x0100..0x0100 + program.len()].copy_from_slice(program);
        rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
        rom
    }

    fn cgb(program: &[u8]) -> GbcSystem {
        GbcSystem::new(cgb_rom(program, 0xC0), None).expect("the synthetic ROM is valid")
    }

    #[test]
    fn the_header_flag_decides_which_machine_the_cartridge_gets() {
        let colour = GbcSystem::new(cgb_rom(&[0x18, 0xFE], 0xC0), None).unwrap();
        assert!(colour.inner().bus().memory.model.uses_tile_attributes());

        let mono = GbcSystem::new(cgb_rom(&[0x18, 0xFE], 0x00), None).unwrap();
        assert!(
            !mono.inner().bus().memory.model.uses_tile_attributes(),
            "a DMG cartridge gets compatibility mode"
        );
        assert!(
            mono.inner().bus().memory.model.has_cgb_hardware(),
            "but it is still colour hardware"
        );
    }

    #[test]
    fn a_dmg_cartridge_is_recoloured_to_greyscale_at_boot() {
        // Palette RAM powers up all-ones white. Without the compatibility palette a DMG game
        // would draw white on white — an invisible picture rather than a monochrome one.
        let mono = GbcSystem::new(cgb_rom(&[0x18, 0xFE], 0x00), None).unwrap();
        let palettes = &mono.inner().bus().cgb.palettes;
        assert_eq!(palettes.lookup_bg(0, 0), core_common::Rgba8::WHITE);
        assert_eq!(palettes.lookup_bg(0, 3), core_common::Rgba8::BLACK);
        assert_eq!(
            palettes.lookup_sprite(7, 3),
            core_common::Rgba8::BLACK,
            "every palette in both banks, not just the first"
        );
    }

    #[test]
    fn a_colour_cartridge_keeps_the_palette_ram_it_powered_up_with() {
        let colour = cgb(&[0x18, 0xFE]);
        assert_eq!(
            colour.inner().bus().cgb.palettes.lookup_bg(0, 3),
            core_common::Rgba8::WHITE,
            "a CGB game writes its own palettes and must not find them pre-filled"
        );
    }

    #[test]
    fn stop_switches_speed_when_key1_is_armed() {
        // `3E 01` LD A,1; `E0 4D` LDH (KEY1),A; `10 00` STOP; `18 FE` spin.
        let mut system = cgb(&[0x3E, 0x01, 0xE0, 0x4D, 0x10, 0x00, 0x18, 0xFE]);
        assert!(!system.inner().bus().cgb.speed.is_double_speed());

        system.step_frame(InputState::default());
        assert!(
            system.inner().bus().cgb.speed.is_double_speed(),
            "the armed STOP switched the clock"
        );
        assert!(
            !system.inner().cpu().is_stopped(),
            "and execution resumed rather than entering low-power mode"
        );
    }

    #[test]
    fn an_unarmed_stop_is_still_low_power_mode() {
        // The same instruction, the opposite meaning, decided entirely by KEY1 bit 0.
        let mut system = cgb(&[0x10, 0x00, 0x18, 0xFE]);
        system.step_frame(InputState::default());
        assert!(system.inner().cpu().is_stopped());
        assert!(!system.inner().bus().cgb.speed.is_double_speed());
    }

    #[test]
    fn double_speed_runs_the_cpu_twice_as_far_per_frame() {
        // The point of the mode, and the thing that breaks if the halving is modelled by
        // shortening scheduled intervals instead: a frame must still take a frame.
        let program = &[0x3E, 0x01, 0xE0, 0x4D, 0x10, 0x00, 0x3C, 0x18, 0xFD];
        let mut fast = cgb(program);
        let mut slow = cgb(&[0x3C, 0x18, 0xFD]);

        let before = fast.inner().bus().timing.now();
        fast.step_frame(InputState::default());
        let fast_elapsed = fast.inner().bus().timing.now().get() - before.get();

        let before = slow.inner().bus().timing.now();
        slow.step_frame(InputState::default());
        let slow_elapsed = slow.inner().bus().timing.now().get() - before.get();

        assert!(fast.inner().bus().cgb.speed.is_double_speed());
        // Frames are the same length in real time whatever the CPU is doing.
        let difference = fast_elapsed.abs_diff(slow_elapsed);
        assert!(
            difference * 20 < slow_elapsed,
            "a double-speed frame took {fast_elapsed} cycles against {slow_elapsed}"
        );
    }

    #[test]
    fn a_general_purpose_vram_transfer_moves_the_bytes_immediately() {
        let mut system = cgb(&[0x18, 0xFE]);
        // Source: work RAM at 0xC000, filled with a recognisable pattern.
        for offset in 0..32u16 {
            system
                .inner_mut()
                .bus_mut()
                .write8(0xC000 + offset as u32, offset as u8);
        }
        let bus = system.inner_mut().bus_mut();
        bus.write8(0xFF51, 0xC0); // source high
        bus.write8(0xFF52, 0x00); // source low
        bus.write8(0xFF53, 0x00); // destination high, inside VRAM
        bus.write8(0xFF54, 0x00); // destination low
        bus.write8(0xFF55, 0x01); // two blocks, general purpose

        for offset in 0..32u32 {
            assert_eq!(
                bus.read8(0x8000 + offset),
                offset as u8,
                "byte {offset} of the transfer"
            );
        }
        assert_eq!(bus.read8(0xFF55), 0xFF, "and the transfer reports finished");
    }

    #[test]
    fn an_hblank_transfer_moves_one_block_per_line_rather_than_all_at_once() {
        let mut system = cgb(&[0x18, 0xFE]);
        for offset in 0..64u16 {
            system
                .inner_mut()
                .bus_mut()
                .write8(0xC000 + offset as u32, 0xA5);
        }
        let bus = system.inner_mut().bus_mut();
        bus.write8(0xFF51, 0xC0);
        bus.write8(0xFF52, 0x00);
        bus.write8(0xFF53, 0x00);
        bus.write8(0xFF54, 0x00);
        bus.write8(0xFF55, 0x83); // four blocks, HBlank mode

        assert_eq!(
            bus.read8(0x8000),
            0x00,
            "nothing has moved yet — this is what makes it usable mid-frame"
        );
        assert!(system.inner().bus().cgb.hdma.is_hblank_pending());

        system.step_frame(InputState::default());
        assert_eq!(
            system.inner_mut().bus_mut().read8(0x8000),
            0xA5,
            "the horizontal blanks in one frame were more than enough for four blocks"
        );
        assert!(!system.inner().bus().cgb.hdma.is_hblank_pending());
    }

    #[test]
    fn a_colour_machine_round_trips_through_a_save_state() {
        let mut system = cgb(&[0x3E, 0x01, 0xE0, 0x4D, 0x10, 0x00, 0x3C, 0x18, 0xFD]);
        system.step_frame(InputState::default());
        let state = system.save_state();

        let mut restored = cgb(&[0x3E, 0x01, 0xE0, 0x4D, 0x10, 0x00, 0x3C, 0x18, 0xFD]);
        restored.load_state(&state).expect("the state is valid");
        assert_eq!(
            restored.inner().bus().cgb.speed,
            system.inner().bus().cgb.speed,
            "the clock speed is machine state and has to survive"
        );

        system.step_frame(InputState::default());
        restored.step_frame(InputState::default());
        assert_eq!(
            restored.framebuffer().as_bytes(),
            system.framebuffer().as_bytes()
        );
    }
}
