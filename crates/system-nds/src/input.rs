//! The keypad, the two extra buttons, and the touchscreen behind the ARM7's SPI bus.
//!
//! # Ten buttons on both cores, two more on one
//!
//! `KEYINPUT` holds the ten buttons the Game Boy Advance also had and is readable by both cores.
//! X, Y, the lid, and the pen-down line are on `EXTKEYIN`, which only the ARM7 can see — so a game
//! that wants X or Y has to ask the ARM7 for it over IPC, which is why so much DS homebrew has an
//! ARM7 half that does nothing but forward input.
//!
//! Every bit is **active low**: a set bit is a released button. Initialising the register to zero
//! means every button held from power-on, which looks like a stuck controller.
//!
//! # The touchscreen is a serial ADC, not a register
//!
//! There is no "touch X" register. The ARM7 selects the touchscreen controller on its SPI bus,
//! shifts out a command naming a measurement channel, and shifts back a 12-bit conversion in two
//! bytes. Software then maps that raw reading to screen coordinates using calibration data it
//! reads out of the firmware.
//!
//! This project has no firmware image, so the numbers on both ends of that mapping are ours to
//! choose. [`RAW_PER_PIXEL`] is the choice: the controller reports `coordinate * 16`, and the
//! system assembly writes a matching linear calibration into the firmware user-settings block that
//! direct boot fabricates. The two must agree, which is why the constant lives here and the
//! comment saying so lives next to the writer.
//!
//! # The other two devices on the bus
//!
//! The firmware flash is [`crate::firmware`], which this owns and forwards to. It used to read as
//! `0xFF` — an absent chip — on the reasoning that direct boot made it unnecessary; see that
//! module for the retail game that reads it anyway and hangs.
//!
//! The power-management chip is not implemented: it accepts writes and reads back zero. What it
//! controls is the backlight, the power LED, and the amplifier — outputs this machine has no
//! equivalent of — and its one input, the battery level, has no meaningful answer on a machine
//! that is not running on a battery.

use crate::firmware::Firmware;
use crate::Core;
use core_common::{Buttons, InputState, Savable, StateError, StateReader, StateWriter};

pub mod reg {
    pub const KEYINPUT: u32 = 0x0400_0130;
    pub const KEYCNT: u32 = 0x0400_0132;
    /// ARM7 only.
    pub const EXTKEYIN: u32 = 0x0400_0136;
    /// ARM7 only.
    pub const SPICNT: u32 = 0x0400_01C0;
    pub const SPIDATA: u32 = 0x0400_01C2;
}

/// Raw ADC counts per screen pixel.
///
/// See the module docs: this and the fabricated calibration block are two halves of one decision.
pub const RAW_PER_PIXEL: u16 = 16;

/// The SPI device `SPICNT` bits 8-9 select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpiDevice {
    PowerManagement,
    Firmware,
    Touchscreen,
    Reserved,
}

impl SpiDevice {
    fn from_bits(spicnt: u16) -> Self {
        match (spicnt >> 8) & 3 {
            0 => SpiDevice::PowerManagement,
            1 => SpiDevice::Firmware,
            2 => SpiDevice::Touchscreen,
            _ => SpiDevice::Reserved,
        }
    }
}

/// Keypad state and the SPI bus the touchscreen and the firmware flash hang off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// Active low, so all ones is nothing held.
    keyinput: u16,
    keycnt: u16,
    extkeyin: u16,
    spicnt: u16,
    spidata: u8,
    /// The conversion the touchscreen controller is part-way through shifting out.
    tsc_output: u16,
    /// How many bytes of that conversion have been read.
    tsc_position: u8,
    touch: Option<(u16, u16)>,
    /// The firmware flash, which is the second device on this bus.
    pub firmware: Firmware,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    pub fn new() -> Self {
        Self {
            keyinput: 0x03FF,
            keycnt: 0,
            // Bits 0 and 1 are X and Y released; bit 6 set is the pen up; bit 7 set is the lid
            // open. The unused bits read as one.
            extkeyin: 0x007F,
            spicnt: 0,
            spidata: 0,
            tsc_output: 0,
            tsc_position: 0,
            touch: None,
            firmware: Firmware::new(),
        }
    }

    /// Apply a frame's worth of input.
    pub fn set_input(&mut self, input: InputState) {
        let mut keys = 0x03FFu16;
        for (bit, button) in [
            Buttons::A,
            Buttons::B,
            Buttons::SELECT,
            Buttons::START,
            Buttons::RIGHT,
            Buttons::LEFT,
            Buttons::UP,
            Buttons::DOWN,
            Buttons::R,
            Buttons::L,
        ]
        .into_iter()
        .enumerate()
        {
            if input.buttons.contains(button) {
                keys &= !(1 << bit);
            }
        }
        self.keyinput = keys;

        let mut ext = 0x007Fu16;
        if input.buttons.contains(Buttons::X) {
            ext &= !(1 << 0);
        }
        if input.buttons.contains(Buttons::Y) {
            ext &= !(1 << 1);
        }

        self.touch = input.touch.map(|point| {
            // The frontend hands over coordinates already in the bottom screen's space; see
            // `frontend_core`'s dual-screen layout, which is written and tested against exactly
            // this 256x192 area.
            (point.x.min(255), point.y.min(191))
        });
        if self.touch.is_some() {
            // The pen-down line is active low too.
            ext &= !(1 << 6);
        }
        self.extkeyin = ext;
    }

    /// Whether the keypad interrupt condition holds.
    ///
    /// Bit 14 selects the condition: clear means "any of the selected keys", set means "all of
    /// them". The all-of-them form is how a game watches for a specific chord such as the soft
    /// reset combination, and treating it as "any" fires that on the first key of the chord.
    pub fn irq_pending(&self) -> bool {
        if self.keycnt & (1 << 15) == 0 {
            return false;
        }
        let selected = self.keycnt & 0x03FF;
        let pressed = !self.keyinput & 0x03FF;
        if self.keycnt & (1 << 14) != 0 {
            selected != 0 && pressed & selected == selected
        } else {
            pressed & selected != 0
        }
    }

    pub fn owns(core: Core, addr: u32) -> bool {
        match addr & !1 {
            reg::KEYINPUT | reg::KEYCNT => true,
            reg::EXTKEYIN | reg::SPICNT | reg::SPIDATA => core == Core::Arm7,
            _ => false,
        }
    }

    pub fn read16(&mut self, core: Core, addr: u32) -> Option<u16> {
        match addr & !1 {
            reg::KEYINPUT => Some(self.keyinput),
            reg::KEYCNT => Some(self.keycnt),
            reg::EXTKEYIN if core == Core::Arm7 => Some(self.extkeyin),
            reg::SPICNT if core == Core::Arm7 => Some(self.spicnt),
            reg::SPIDATA if core == Core::Arm7 => Some(self.spidata as u16),
            _ => None,
        }
    }

    pub fn write16(&mut self, core: Core, addr: u32, value: u16) -> bool {
        match addr & !1 {
            // KEYINPUT and EXTKEYIN are the pins; writing them does nothing.
            reg::KEYINPUT => true,
            reg::KEYCNT => {
                self.keycnt = value;
                true
            }
            reg::EXTKEYIN if core == Core::Arm7 => true,
            reg::SPICNT if core == Core::Arm7 => {
                self.spicnt = value & 0xCF03;
                true
            }
            reg::SPIDATA if core == Core::Arm7 => {
                self.transfer(value as u8);
                true
            }
            _ => false,
        }
    }

    pub fn read8(&mut self, core: Core, addr: u32) -> Option<u8> {
        let value = self.read16(core, addr & !1)?;
        Some(if addr & 1 == 0 {
            value as u8
        } else {
            (value >> 8) as u8
        })
    }

    pub fn write8(&mut self, core: Core, addr: u32, value: u8) -> bool {
        let Some(current) = self.read16(core, addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (current & 0xFF00) | value as u16
        } else {
            (current & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(core, addr & !1, spliced)
    }

    /// Shift one byte through the SPI bus and latch what came back.
    fn transfer(&mut self, byte: u8) {
        // Bit 11 held low ends the transfer and deselects the device, which is what resets the
        // controller's byte counter between conversions and what frames one flash command.
        let keep_selected = self.spicnt & (1 << 11) != 0;
        let device = SpiDevice::from_bits(self.spicnt);
        self.spidata = match device {
            SpiDevice::Touchscreen => self.touchscreen_transfer(byte),
            SpiDevice::Firmware => self.firmware.transfer(byte),
            SpiDevice::PowerManagement | SpiDevice::Reserved => 0,
        };
        if !keep_selected {
            self.tsc_position = 0;
            // The flash's framing is the select line and nothing else: a command runs until the
            // chip is deselected. Leaving it selected across the release is how a driver that
            // reads two blocks in a row ends up with the second one continuing the first.
            self.firmware.deselect();
        }
    }

    /// The touchscreen controller's half of a transfer.
    ///
    /// A command byte has bit 7 set and names a channel; the controller answers it with a zero
    /// and then hands the 12-bit conversion back over the next two bytes, high seven bits first.
    fn touchscreen_transfer(&mut self, byte: u8) -> u8 {
        if byte & 0x80 != 0 {
            let channel = (byte >> 4) & 7;
            self.tsc_output = self.conversion(channel);
            self.tsc_position = 0;
            return 0;
        }
        self.tsc_position = self.tsc_position.saturating_add(1);
        match self.tsc_position {
            1 => (self.tsc_output >> 5) as u8,
            _ => (self.tsc_output << 3) as u8,
        }
    }

    /// What the controller measures on a channel.
    ///
    /// Channel 1 is Y and channel 5 is X — not the order anyone expects, and swapping them
    /// produces a touchscreen that works perfectly along the diagonal and nowhere else.
    fn conversion(&self, channel: u8) -> u16 {
        let Some((x, y)) = self.touch else {
            // With the pen up, the position channels read zero and the pressure channels read
            // their extremes, which is how software detects a release without `EXTKEYIN`.
            return match channel {
                3 => 0,
                4 => 0x0FFF,
                _ => 0,
            };
        };
        match channel {
            1 => y * RAW_PER_PIXEL,
            5 => x * RAW_PER_PIXEL,
            // Z1 and Z2, the pressure channels. Any pair implying a firm press will do.
            3 => 0x0400,
            4 => 0x0800,
            _ => 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Savable for Input {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.keyinput);
        w.write_u16(self.keycnt);
        w.write_u16(self.extkeyin);
        w.write_u16(self.spicnt);
        w.write_u8(self.spidata);
        w.write_u16(self.tsc_output);
        w.write_u8(self.tsc_position);
        match self.touch {
            Some((x, y)) => {
                w.write_bool(true);
                w.write_u16(x);
                w.write_u16(y);
            }
            None => {
                w.write_bool(false);
                w.write_u16(0);
                w.write_u16(0);
            }
        }
        self.firmware.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.keyinput = r.read_u16()?;
        self.keycnt = r.read_u16()?;
        self.extkeyin = r.read_u16()?;
        self.spicnt = r.read_u16()?;
        self.spidata = r.read_u8()?;
        self.tsc_output = r.read_u16()?;
        self.tsc_position = r.read_u8()?;
        let touching = r.read_bool()?;
        let x = r.read_u16()?;
        let y = r.read_u16()?;
        self.touch = touching.then_some((x, y));
        self.firmware.load(r)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
