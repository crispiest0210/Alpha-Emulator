//! The Game Boy memory map — and the reference pattern the other three systems follow.
//!
//! # The map (Pan Docs)
//!
//! ```text
//! 0000-3FFF  cartridge ROM, bank 0            -> Mapper
//! 4000-7FFF  cartridge ROM, switchable bank   -> Mapper
//! 8000-9FFF  VRAM (2 banks on CGB)
//! A000-BFFF  cartridge RAM                    -> Mapper
//! C000-CFFF  WRAM bank 0
//! D000-DFFF  WRAM banks 1-7 (1 only on DMG)
//! E000-FDFF  echo RAM: a mirror of C000-DDFF
//! FE00-FE9F  OAM
//! FEA0-FEFF  unusable
//! FF00-FF7F  I/O registers
//! FF80-FFFE  HRAM
//! FFFF       IE
//! ```
//!
//! # Why this is a `match`, not a `RegionMap`
//!
//! `core-common` provides [`RegionMap`](core_common::RegionMap) for composing a bus out of
//! independent spans, and it is the right tool when regions are numerous, uniform, and
//! genuinely independent. This map is none of those things: it has eleven regions, four of
//! them are windows onto something else (echo RAM, the two cartridge windows), and the
//! boundaries are compile-time constants that never move. A `match` on the top nibble is both
//! faster — no binary search on the hottest path in the emulator — and clearer about the
//! aliasing, which a flat list of regions would hide.
//!
//! `RegionMap` earns its keep on the GBA and DS, whose maps are sparser and whose regions
//! really are independent. Choosing per system is the point of having both.
//!
//! # Open bus
//!
//! Unmapped reads return `0xFF` here, and that is a *decision*, not a default: the Game Boy's
//! data bus floats high, so an unmapped read sees all ones. [`Bus::open_bus8`] is a required
//! method precisely so each system has to state this rather than inheriting a zero.

use cart_common::Mapper;
use core_common::{Bus, Savable, StateError, StateReader, StateWriter};

pub const VRAM_BANK_SIZE: usize = 0x2000;
pub const WRAM_BANK_SIZE: usize = 0x1000;
pub const OAM_SIZE: usize = 0xA0;
pub const HRAM_SIZE: usize = 0x7F;
pub const IO_SIZE: usize = 0x80;

/// Which machine is being emulated.
///
/// The CGB adds a second VRAM bank and seven extra WRAM banks. Both are the same map with
/// more banks, so one type covers both rather than `system-gbc` duplicating it — and the same
/// reasoning extends past the memory map, which is why the palette and attribute questions
/// below are answered here too rather than by a parallel enum in that crate.
///
/// Three variants, not two: a CGB running a DMG cartridge is its own machine. It has the
/// CGB's banked memory and speed switch, but the boot ROM has recoloured a game that never
/// asked to be recoloured. Collapsing it into `Dmg` would lose the banking; collapsing it into
/// `Cgb` would have the PPU read an attribute map that was never written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GbModel {
    /// Original hardware.
    Dmg,
    /// CGB hardware running a CGB-aware cartridge.
    Cgb,
    /// CGB hardware running an unmodified DMG cartridge.
    CgbInDmgMode,
}

impl GbModel {
    pub const fn vram_banks(self) -> usize {
        match self {
            GbModel::Dmg => 1,
            // Compatibility mode included: the second bank is physically there, and the boot
            // ROM uses it even when the game does not.
            GbModel::Cgb | GbModel::CgbInDmgMode => 2,
        }
    }

    /// Bank 0 plus the switchable banks.
    pub const fn wram_banks(self) -> usize {
        match self {
            GbModel::Dmg => 2,
            GbModel::Cgb | GbModel::CgbInDmgMode => 8,
        }
    }

    /// Whether the CGB register blocks respond at all.
    ///
    /// True in compatibility mode as well: the hardware is present and the registers answer,
    /// which is how the boot ROM installs its compatibility palette in the first place. What
    /// differs there is that the *game* never touches them.
    pub const fn has_cgb_hardware(self) -> bool {
        matches!(self, GbModel::Cgb | GbModel::CgbInDmgMode)
    }

    /// Whether the picture comes from CGB palette RAM rather than `BGP`/`OBP0`/`OBP1`.
    pub const fn uses_colour_palettes(self) -> bool {
        matches!(self, GbModel::Cgb | GbModel::CgbInDmgMode)
    }

    /// Whether the background map has a second attribute byte in VRAM bank 1.
    ///
    /// False in compatibility mode: the game writes a DMG tile map and leaves bank 1 alone, so
    /// reading attributes there would decode uninitialised memory as palette and flip bits.
    pub const fn uses_tile_attributes(self) -> bool {
        matches!(self, GbModel::Cgb)
    }

    /// Whether `LCDC` bit 0 blanks the background.
    ///
    /// It does on a DMG. On a CGB the bit keeps its position but changes its job: the
    /// background always draws, and clearing the bit instead drops background priority so
    /// every sprite comes to the front. A game uses that for a cutscene without editing its
    /// tile maps — and treating it as a blank would black out the screen instead.
    pub const fn bg_enable_blanks_background(self) -> bool {
        matches!(self, GbModel::Dmg)
    }

    /// Pick the model for a cartridge, from the CGB flag at `0x0143` of its header.
    ///
    /// `0x80` means "enhanced for CGB but still runs on a DMG" and `0xC0` means "CGB only";
    /// both run in full CGB mode on CGB hardware. Anything else is a DMG cartridge, which on
    /// CGB hardware means compatibility mode.
    pub const fn for_cartridge(cgb_flag: u8, on_cgb_hardware: bool) -> Self {
        if !on_cgb_hardware {
            return GbModel::Dmg;
        }
        match cgb_flag {
            0x80 | 0xC0 => GbModel::Cgb,
            _ => GbModel::CgbInDmgMode,
        }
    }
}

/// Registers this module owns, as opposed to the PPU/APU/timer registers that live in the I/O
/// block and are given behavior by the system assembly.
pub mod io {
    /// Interrupt Flag.
    pub const IF: u16 = 0xFF0F;
    /// CGB VRAM bank select.
    pub const VBK: u16 = 0xFF4F;
    /// Boot ROM disable. Write-once: any write unmaps the boot ROM permanently.
    pub const BANK: u16 = 0xFF50;
    /// CGB WRAM bank select.
    pub const SVBK: u16 = 0xFF70;
    /// Interrupt Enable.
    pub const IE: u16 = 0xFFFF;
}

/// The Game Boy bus.
///
/// Owns everything on the main board and delegates the two cartridge windows to a
/// [`Mapper`]. The I/O block is plain storage at this layer; the system assembly intercepts
/// the addresses that have behavior (PPU, APU, timer, joypad) before they reach here.
pub struct GbBus {
    pub model: GbModel,
    pub mapper: Box<dyn Mapper>,

    vram: Box<[u8]>,
    wram: Box<[u8]>,
    oam: Box<[u8]>,
    hram: Box<[u8]>,
    /// Raw storage for `FF00`-`FF7F`. Addresses with real behavior are handled above this.
    io: Box<[u8]>,

    vram_bank: usize,
    /// WRAM bank for `D000`-`DFFF`. Bank 0 is not selectable — writing 0 selects bank 1.
    wram_bank: usize,

    pub interrupt_flags: u8,
    pub interrupt_enable: u8,

    /// The boot ROM, mapped over the bottom of the address space until disabled.
    boot_rom: Option<Box<[u8]>>,
    boot_rom_enabled: bool,

    /// The value the data bus last carried, which is what an unmapped read *could* return on
    /// some hardware. The Game Boy floats high instead, so this is kept for the debugger
    /// rather than used for open-bus reads.
    last_bus_value: u8,
}

impl GbBus {
    pub fn new(model: GbModel, mapper: Box<dyn Mapper>) -> Self {
        Self {
            model,
            mapper,
            vram: vec![0; VRAM_BANK_SIZE * model.vram_banks()].into_boxed_slice(),
            wram: vec![0; WRAM_BANK_SIZE * model.wram_banks()].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            hram: vec![0; HRAM_SIZE].into_boxed_slice(),
            io: vec![0; IO_SIZE].into_boxed_slice(),
            vram_bank: 0,
            wram_bank: 1,
            interrupt_flags: 0,
            interrupt_enable: 0,
            boot_rom: None,
            boot_rom_enabled: false,
            last_bus_value: 0xFF,
        }
    }

    /// Map a boot ROM over the bottom of the address space.
    ///
    /// Treated as a real dependency loaded before the machine starts, not fetched
    /// asynchronously while it runs — the predecessor project raced its BIOS fetch against
    /// CPU startup and crashed on reload, and taking the ROM up front by value makes that
    /// shape impossible to express here.
    pub fn install_boot_rom(&mut self, rom: Vec<u8>) {
        self.boot_rom = Some(rom.into_boxed_slice());
        self.boot_rom_enabled = true;
    }

    pub fn boot_rom_enabled(&self) -> bool {
        self.boot_rom_enabled
    }

    /// Whether `addr` is currently answered by the boot ROM rather than the cartridge.
    ///
    /// The CGB boot ROM is larger and leaves a hole at `0x0100`-`0x01FF` so the cartridge
    /// header remains visible to it.
    fn boot_rom_covers(&self, addr: u16) -> bool {
        let Some(rom) = &self.boot_rom else {
            return false;
        };
        if !self.boot_rom_enabled {
            return false;
        }
        match self.model {
            GbModel::Dmg => (addr as usize) < rom.len().min(0x100),
            // Compatibility mode runs the same physical CGB boot ROM — it is the boot ROM that
            // *decides* the machine is in compatibility mode, so it is mapped either way.
            GbModel::Cgb | GbModel::CgbInDmgMode => {
                (addr < 0x0100 || (0x0200..0x0900).contains(&addr)) && (addr as usize) < rom.len()
            }
        }
    }

    #[inline]
    fn vram_index(&self, addr: u16) -> usize {
        self.vram_bank * VRAM_BANK_SIZE + (addr as usize - 0x8000)
    }

    /// WRAM is two windows: a fixed bank 0 and a switchable one.
    #[inline]
    fn wram_index(&self, addr: u16) -> usize {
        // Echo RAM folds onto the same storage, so normalize it first.
        let addr = if (0xE000..=0xFDFF).contains(&addr) {
            addr - 0x2000
        } else {
            addr
        };
        if addr < 0xD000 {
            addr as usize - 0xC000
        } else {
            self.wram_bank * WRAM_BANK_SIZE + (addr as usize - 0xD000)
        }
    }

    /// VRAM and OAM together, for the PPU, which reads both while compositing a line.
    ///
    /// Returned as one borrow so the renderer does not have to take two overlapping ones.
    pub fn vram_and_oam(&self) -> (&[u8], &[u8]) {
        (&self.vram, &self.oam)
    }

    /// Return the board to power-on state, keeping the cartridge in the slot.
    ///
    /// The mapper is deliberately untouched: resetting a console does not eject the game, and
    /// it certainly does not wipe battery-backed save RAM.
    pub fn reset(&mut self) {
        self.vram.fill(0);
        self.wram.fill(0);
        self.oam.fill(0);
        self.hram.fill(0);
        self.io.fill(0);
        self.vram_bank = 0;
        self.wram_bank = 1;
        self.interrupt_flags = 0;
        self.interrupt_enable = 0;
        self.boot_rom_enabled = self.boot_rom.is_some();
        self.last_bus_value = 0xFF;
    }

    /// Direct access for the PPU, which reads VRAM and OAM without going through the CPU bus.
    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        &mut self.vram
    }

    pub fn oam(&self) -> &[u8] {
        &self.oam
    }

    pub fn oam_mut(&mut self) -> &mut [u8] {
        &mut self.oam
    }

    /// Raw I/O storage, for the system assembly to read registers it does not intercept.
    pub fn io_raw(&self) -> &[u8] {
        &self.io
    }

    pub fn io_raw_mut(&mut self) -> &mut [u8] {
        &mut self.io
    }

    pub fn vram_bank(&self) -> usize {
        self.vram_bank
    }

    pub fn wram_bank(&self) -> usize {
        self.wram_bank
    }

    /// Request an interrupt by setting its bit in `IF`.
    pub fn request_interrupt(&mut self, bit: u8) {
        self.interrupt_flags |= 1 << bit;
    }

    fn read_io(&mut self, addr: u16) -> u8 {
        match addr {
            io::IF => {
                // The top three bits of IF are not implemented and read as ones.
                0xE0 | (self.interrupt_flags & 0x1F)
            }
            io::VBK if self.model == GbModel::Cgb => 0xFE | self.vram_bank as u8,
            io::SVBK if self.model == GbModel::Cgb => 0xF8 | self.wram_bank as u8,
            io::BANK => 0xFE | (!self.boot_rom_enabled as u8),
            _ => self.io[(addr - 0xFF00) as usize],
        }
    }

    fn write_io(&mut self, addr: u16, value: u8) {
        match addr {
            io::IF => self.interrupt_flags = value & 0x1F,
            io::VBK if self.model == GbModel::Cgb => {
                self.vram_bank = (value & 1) as usize;
            }
            io::SVBK if self.model == GbModel::Cgb => {
                // Bank 0 is not selectable through this register: writing 0 selects bank 1.
                let bank = (value & 0x07) as usize;
                self.wram_bank = if bank == 0 { 1 } else { bank };
            }
            io::BANK => {
                // Write-once. Once the boot ROM is unmapped nothing can bring it back, which
                // is why this ignores the value entirely.
                if value != 0 {
                    self.boot_rom_enabled = false;
                }
            }
            _ => self.io[(addr - 0xFF00) as usize] = value,
        }
    }
}

impl Bus for GbBus {
    fn read8(&mut self, addr: u32) -> u8 {
        let addr = addr as u16;
        let value = match addr {
            0x0000..=0x7FFF => {
                if self.boot_rom_covers(addr) {
                    self.boot_rom.as_ref().unwrap()[addr as usize]
                } else {
                    self.mapper.read(addr)
                }
            }
            0x8000..=0x9FFF => self.vram[self.vram_index(addr)],
            0xA000..=0xBFFF => self.mapper.read(addr),
            0xC000..=0xDFFF => self.wram[self.wram_index(addr)],
            // Echo RAM. Nintendo's manuals said not to use it; plenty of games do anyway,
            // which is why it is a real mirror rather than an error.
            0xE000..=0xFDFF => self.wram[self.wram_index(addr)],
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize],
            // The unusable region reads as zero on a DMG rather than floating high.
            0xFEA0..=0xFEFF => 0x00,
            0xFF00..=0xFF7F => self.read_io(addr),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize],
            io::IE => self.interrupt_enable,
        };
        self.last_bus_value = value;
        value
    }

    fn write8(&mut self, addr: u32, value: u8) {
        let addr = addr as u16;
        self.last_bus_value = value;
        match addr {
            // Writes into the ROM window are bank-switching register writes, and reach the
            // mapper even while the boot ROM is shadowing reads from the same addresses.
            0x0000..=0x7FFF => self.mapper.write(addr, value),
            0x8000..=0x9FFF => {
                let index = self.vram_index(addr);
                self.vram[index] = value;
            }
            0xA000..=0xBFFF => self.mapper.write(addr, value),
            // WRAM and the echo region are contiguous and resolve to the same storage.
            0xC000..=0xFDFF => {
                let index = self.wram_index(addr);
                self.wram[index] = value;
            }
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize] = value,
            0xFEA0..=0xFEFF => {}
            0xFF00..=0xFF7F => self.write_io(addr, value),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = value,
            io::IE => self.interrupt_enable = value,
        }
    }

    /// The Game Boy's data bus floats high, so an unmapped read sees all ones.
    ///
    /// Every address in the 16-bit space is decoded by something here, so this is only
    /// reached through the wide accessors straddling the end of the address space.
    fn open_bus8(&self, _addr: u32) -> u8 {
        0xFF
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        let addr = addr as u16;
        match addr {
            0x0000..=0x7FFF if self.boot_rom_covers(addr) => {
                Some(self.boot_rom.as_ref().unwrap()[addr as usize])
            }
            // ROM is peekable because bank selection is pure address arithmetic; cartridge RAM is
            // not, because a mapper read there can be answering an RTC register or a Flash command.
            // `Mapper::peek` draws that line, and it is the line that lets a debugger disassemble
            // ROM at all — which is most of what a debugger is for.
            0x0000..=0x7FFF => self.mapper.peek(addr),
            0xA000..=0xBFFF => None,
            0x8000..=0x9FFF => Some(self.vram[self.vram_index(addr)]),
            0xC000..=0xFDFF => Some(self.wram[self.wram_index(addr)]),
            0xFE00..=0xFE9F => Some(self.oam[(addr - 0xFE00) as usize]),
            0xFEA0..=0xFEFF => Some(0x00),
            // Reading some I/O registers has side effects, so a debugger must not.
            0xFF00..=0xFF7F => None,
            0xFF80..=0xFFFE => Some(self.hram[(addr - 0xFF80) as usize]),
            io::IE => Some(self.interrupt_enable),
        }
    }
}

impl Savable for GbBus {
    fn save(&self, w: &mut StateWriter) {
        // The cartridge ROM and the boot ROM are not saved: they come from files, not from
        // the machine. The mapper's own `save` covers its banking state and cartridge RAM.
        self.mapper.save(w);
        w.write_blob(&self.vram);
        w.write_blob(&self.wram);
        w.write_blob(&self.oam);
        w.write_blob(&self.hram);
        w.write_blob(&self.io);
        w.write_u32(self.vram_bank as u32);
        w.write_u32(self.wram_bank as u32);
        w.write_u8(self.interrupt_flags);
        w.write_u8(self.interrupt_enable);
        w.write_bool(self.boot_rom_enabled);
        w.write_u8(self.last_bus_value);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.mapper.load(r)?;

        let restore = |target: &mut Box<[u8]>, name: &str, r: &mut StateReader| {
            let bytes = r.read_blob()?;
            if bytes.len() != target.len() {
                return Err(StateError::Malformed(format!(
                    "{name} is {} bytes in this build, {} in the state",
                    target.len(),
                    bytes.len()
                )));
            }
            target.copy_from_slice(bytes);
            Ok(())
        };
        restore(&mut self.vram, "VRAM", r)?;
        restore(&mut self.wram, "WRAM", r)?;
        restore(&mut self.oam, "OAM", r)?;
        restore(&mut self.hram, "HRAM", r)?;
        restore(&mut self.io, "I/O", r)?;

        self.vram_bank = r.read_u32()? as usize % self.model.vram_banks();
        self.wram_bank = (r.read_u32()? as usize).clamp(1, self.model.wram_banks() - 1);
        self.interrupt_flags = r.read_u8()?;
        self.interrupt_enable = r.read_u8()?;
        self.boot_rom_enabled = r.read_bool()?;
        self.last_bus_value = r.read_u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cart_common::{create_mapper, GbHeader};

    fn rom(cartridge_type: u8, ram_size_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0134..0x0139].copy_from_slice(b"TEST\0");
        rom[0x0147] = cartridge_type;
        rom[0x0148] = 0x00;
        rom[0x0149] = ram_size_code;
        rom[0x014D] = GbHeader::header_checksum(&rom);
        rom
    }

    fn bus(model: GbModel) -> GbBus {
        let rom = rom(0x03, 0x02); // MBC1 + RAM + battery
        let header = GbHeader::parse(&rom).unwrap();
        GbBus::new(model, create_mapper(rom, &header).unwrap())
    }

    #[test]
    fn every_region_of_the_map_is_addressable() {
        let mut bus = bus(GbModel::Dmg);

        // VRAM
        bus.write8(0x8000, 0x11);
        bus.write8(0x9FFF, 0x12);
        assert_eq!(bus.read8(0x8000), 0x11);
        assert_eq!(bus.read8(0x9FFF), 0x12);

        // WRAM, both windows
        bus.write8(0xC000, 0x21);
        bus.write8(0xDFFF, 0x22);
        assert_eq!(bus.read8(0xC000), 0x21);
        assert_eq!(bus.read8(0xDFFF), 0x22);

        // OAM
        bus.write8(0xFE00, 0x31);
        bus.write8(0xFE9F, 0x32);
        assert_eq!(bus.read8(0xFE00), 0x31);
        assert_eq!(bus.read8(0xFE9F), 0x32);

        // HRAM
        bus.write8(0xFF80, 0x41);
        bus.write8(0xFFFE, 0x42);
        assert_eq!(bus.read8(0xFF80), 0x41);
        assert_eq!(bus.read8(0xFFFE), 0x42);

        // IE sits alone above HRAM.
        bus.write8(0xFFFF, 0x1F);
        assert_eq!(bus.read8(0xFFFF), 0x1F);
    }

    #[test]
    fn echo_ram_mirrors_work_ram_in_both_directions() {
        let mut bus = bus(GbModel::Dmg);

        // A write to WRAM appears in echo RAM 0x2000 lower.
        bus.write8(0xC000, 0xAA);
        assert_eq!(bus.read8(0xE000), 0xAA);

        // And a write to echo RAM appears in WRAM.
        bus.write8(0xE123, 0xBB);
        assert_eq!(bus.read8(0xC123), 0xBB);

        // The mirror ends at 0xFDFF, which maps to 0xDDFF.
        bus.write8(0xDDFF, 0xCC);
        assert_eq!(bus.read8(0xFDFF), 0xCC);

        // 0xFE00 is OAM, not more echo RAM.
        bus.write8(0xDE00, 0x99);
        bus.write8(0xFE00, 0x11);
        assert_eq!(bus.read8(0xDE00), 0x99, "OAM must not alias WRAM");
        assert_eq!(bus.read8(0xFE00), 0x11);
    }

    #[test]
    fn the_unusable_region_reads_as_zero_and_swallows_writes() {
        let mut bus = bus(GbModel::Dmg);
        bus.write8(0xFEA0, 0xFF);
        assert_eq!(bus.read8(0xFEA0), 0x00);
        assert_eq!(bus.read8(0xFEFF), 0x00);
    }

    #[test]
    fn cartridge_windows_reach_the_mapper() {
        let mut bus = bus(GbModel::Dmg);
        // A write into the ROM window is a bank-switch register write.
        bus.write8(0x2000, 0x01);

        // Cartridge RAM needs enabling first, exactly as on hardware.
        assert_eq!(
            bus.read8(0xA000),
            0xFF,
            "disabled cartridge RAM is open bus"
        );
        bus.write8(0x0000, 0x0A);
        bus.write8(0xA000, 0x77);
        assert_eq!(bus.read8(0xA000), 0x77);
    }

    #[test]
    fn interrupt_registers_read_back_with_their_unimplemented_bits_set() {
        let mut bus = bus(GbModel::Dmg);
        bus.write8(0xFF0F, 0x05);
        assert_eq!(
            bus.read8(0xFF0F),
            0xE5,
            "the top three bits of IF are not implemented and read as ones"
        );

        bus.request_interrupt(0);
        assert_eq!(bus.read8(0xFF0F) & 0x01, 0x01);
    }

    #[test]
    fn the_boot_rom_shadows_the_cartridge_until_it_is_disabled() {
        let mut bus = bus(GbModel::Dmg);
        let mut boot = vec![0u8; 0x100];
        boot[0] = 0x31; // a recognizable first instruction
        bus.install_boot_rom(boot);

        assert!(bus.boot_rom_enabled());
        assert_eq!(bus.read8(0x0000), 0x31, "the boot ROM answers");
        assert_eq!(bus.read8(0x0100), 0x00, "but only below 0x100");

        // Any nonzero write to 0xFF50 unmaps it, permanently.
        bus.write8(0xFF50, 0x01);
        assert!(!bus.boot_rom_enabled());
        assert_eq!(bus.read8(0x0000), 0x00, "the cartridge is visible again");

        bus.write8(0xFF50, 0x00);
        assert!(!bus.boot_rom_enabled(), "unmapping cannot be undone");
    }

    #[test]
    fn rom_window_writes_reach_the_mapper_even_while_the_boot_rom_shadows_reads() {
        // The boot ROM only intercepts reads; the cartridge's bank registers stay wired up.
        let mut bus = bus(GbModel::Dmg);
        bus.install_boot_rom(vec![0xAA; 0x100]);
        bus.write8(0x0000, 0x0A); // enable cartridge RAM through the shadowed window
        bus.write8(0xA000, 0x5A);
        assert_eq!(bus.read8(0xA000), 0x5A);
    }

    #[test]
    fn cgb_vram_banking_selects_between_two_banks() {
        let mut bus = bus(GbModel::Cgb);
        bus.write8(0x8000, 0x11);

        bus.write8(0xFF4F, 0x01);
        assert_eq!(bus.vram_bank(), 1);
        assert_eq!(bus.read8(0x8000), 0x00, "bank 1 is separate storage");
        bus.write8(0x8000, 0x22);

        bus.write8(0xFF4F, 0x00);
        assert_eq!(bus.read8(0x8000), 0x11);
        // Only bit 0 exists; the rest read as ones.
        assert_eq!(bus.read8(0xFF4F), 0xFE);
    }

    #[test]
    fn cgb_wram_banking_leaves_bank_zero_fixed_and_cannot_select_it() {
        let mut bus = bus(GbModel::Cgb);
        bus.write8(0xC000, 0xAA); // always bank 0

        for bank in 1..8u8 {
            bus.write8(0xFF70, bank);
            bus.write8(0xD000, 0x10 + bank);
        }
        for bank in 1..8u8 {
            bus.write8(0xFF70, bank);
            assert_eq!(bus.read8(0xD000), 0x10 + bank, "bank {bank}");
            assert_eq!(bus.read8(0xC000), 0xAA, "bank 0 stays fixed");
        }

        // Writing 0 selects bank 1, not bank 0.
        bus.write8(0xFF70, 0x00);
        assert_eq!(bus.wram_bank(), 1);
        assert_eq!(bus.read8(0xD000), 0x11);
    }

    #[test]
    fn a_dmg_ignores_the_cgb_banking_registers() {
        let mut bus = bus(GbModel::Dmg);
        bus.write8(0xFF4F, 0x01);
        bus.write8(0xFF70, 0x03);
        assert_eq!(bus.vram_bank(), 0);
        assert_eq!(bus.wram_bank(), 1);
    }

    #[test]
    fn wide_accesses_compose_little_endian_across_the_map() {
        let mut bus = bus(GbModel::Dmg);
        bus.write16(0xC000, 0x1234);
        assert_eq!(bus.read8(0xC000), 0x34);
        assert_eq!(bus.read8(0xC001), 0x12);
        assert_eq!(bus.read16(0xC000), 0x1234);
    }

    #[test]
    fn peeking_is_safe_where_it_can_be_and_refused_where_it_cannot() {
        let mut bus = bus(GbModel::Dmg);
        bus.write8(0xC000, 0x42);
        assert_eq!(bus.peek8(0xC000), Some(0x42));
        assert_eq!(bus.peek8(0xE000), Some(0x42), "echo RAM peeks too");
        assert_eq!(bus.peek8(0xFF40), None, "I/O may have side effects");
        assert_eq!(bus.peek8(0xFFFF), Some(0));
        // Cartridge ROM *is* peekable — bank selection is pure address arithmetic, and a debugger
        // that cannot disassemble ROM is not a debugger. Cartridge RAM is not, because a mapper read
        // there can be answering an RTC register or a Flash command.
        assert!(bus.peek8(0x0000).is_some(), "ROM peeks");
        assert_eq!(
            bus.peek8(0xA000),
            None,
            "cartridge RAM may have side effects"
        );
    }

    #[test]
    fn the_whole_map_round_trips_through_a_save_state() {
        let mut bus = bus(GbModel::Cgb);
        bus.write8(0x0000, 0x0A);
        bus.write8(0xA000, 0x11);
        bus.write8(0xFF4F, 0x01);
        bus.write8(0x8000, 0x22);
        bus.write8(0xFF70, 0x05);
        bus.write8(0xD000, 0x33);
        bus.write8(0xFE00, 0x44);
        bus.write8(0xFF80, 0x55);
        bus.write8(0xFFFF, 0x1F);
        bus.write8(0xFF0F, 0x03);

        let mut w = StateWriter::new();
        bus.save(&mut w);
        let blob = w.into_inner();

        let mut restored = self::bus(GbModel::Cgb);
        restored.load(&mut StateReader::new(&blob)).unwrap();

        assert_eq!(restored.vram_bank(), 1);
        assert_eq!(restored.read8(0x8000), 0x22);
        assert_eq!(restored.wram_bank(), 5);
        assert_eq!(restored.read8(0xD000), 0x33);
        assert_eq!(restored.read8(0xA000), 0x11, "cartridge RAM and its enable");
        assert_eq!(restored.read8(0xFE00), 0x44);
        assert_eq!(restored.read8(0xFF80), 0x55);
        assert_eq!(restored.read8(0xFFFF), 0x1F);
        assert_eq!(restored.read8(0xFF0F) & 0x1F, 0x03);
    }
}
