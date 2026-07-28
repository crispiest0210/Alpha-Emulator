//! The joypad register at `0xFF00`.
//!
//! Two things about it catch people out. The button bits are **active low** — a pressed
//! button reads as zero — and the register multiplexes two groups of four buttons through
//! one nibble, selected by two other bits that are *also* active low.

use core_common::{Buttons, InputState, Savable, StateError, StateReader, StateWriter};

pub const JOYP: u16 = 0xFF00;

/// Clear to select the direction pad.
const SELECT_DIRECTIONS: u8 = 1 << 4;
/// Clear to select the action buttons.
const SELECT_ACTIONS: u8 = 1 << 5;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Joypad {
    /// Which buttons the player is holding.
    pressed: Buttons,
    /// The two selection bits, as the game last wrote them.
    select: u8,
}

impl Joypad {
    pub fn new() -> Self {
        Self {
            pressed: Buttons::empty(),
            // Both groups deselected at power-on.
            select: SELECT_DIRECTIONS | SELECT_ACTIONS,
        }
    }

    /// Update the held buttons, returning true if a joypad interrupt should fire.
    ///
    /// The interrupt is raised on a high-to-low transition of any *selected* line — that is,
    /// on a press, not a release, and only for the group the game currently has selected.
    /// Games use it to wake from `STOP`.
    pub fn set_input(&mut self, input: InputState) -> bool {
        let before = self.selected_lines();
        self.pressed = input.buttons;
        let after = self.selected_lines();
        // Active low, so a newly pressed button is a bit that went from one to zero.
        before & !after != 0
    }

    /// The low nibble, active low, for whichever group is selected.
    fn selected_lines(&self) -> u8 {
        let mut lines = 0x0F;
        if self.select & SELECT_DIRECTIONS == 0 {
            lines &= !self.direction_bits();
        }
        if self.select & SELECT_ACTIONS == 0 {
            lines &= !self.action_bits();
        }
        lines
    }

    fn direction_bits(&self) -> u8 {
        (self.pressed.contains(Buttons::RIGHT) as u8)
            | ((self.pressed.contains(Buttons::LEFT) as u8) << 1)
            | ((self.pressed.contains(Buttons::UP) as u8) << 2)
            | ((self.pressed.contains(Buttons::DOWN) as u8) << 3)
    }

    fn action_bits(&self) -> u8 {
        (self.pressed.contains(Buttons::A) as u8)
            | ((self.pressed.contains(Buttons::B) as u8) << 1)
            | ((self.pressed.contains(Buttons::SELECT) as u8) << 2)
            | ((self.pressed.contains(Buttons::START) as u8) << 3)
    }

    pub fn read(&self) -> u8 {
        // Bits 6 and 7 are unused and read as ones.
        0xC0 | (self.select & 0x30) | self.selected_lines()
    }

    /// Only the two selection bits are writable; the button lines are inputs.
    pub fn write(&mut self, value: u8) {
        self.select = value & 0x30;
    }
}

impl Savable for Joypad {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.pressed.bits());
        w.write_u8(self.select);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.pressed = Buttons::from_bits_truncate(r.read_u16()?);
        self.select = r.read_u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(buttons: Buttons) -> InputState {
        InputState {
            buttons,
            touch: None,
        }
    }

    #[test]
    fn nothing_selected_reads_all_lines_high() {
        let mut pad = Joypad::new();
        pad.set_input(held(Buttons::A | Buttons::RIGHT));
        // With neither group selected the nibble reads as though nothing is pressed.
        assert_eq!(pad.read() & 0x0F, 0x0F);
    }

    #[test]
    fn a_pressed_button_reads_as_zero() {
        // Active low, which is the detail most often inverted.
        let mut pad = Joypad::new();
        pad.write(!SELECT_ACTIONS); // select the action group
        pad.set_input(held(Buttons::A));
        assert_eq!(pad.read() & 0x01, 0, "A is bit 0, and pressed means clear");

        pad.set_input(held(Buttons::empty()));
        assert_eq!(pad.read() & 0x01, 0x01);
    }

    #[test]
    fn the_two_groups_share_one_nibble() {
        let mut pad = Joypad::new();
        pad.set_input(held(Buttons::A | Buttons::DOWN));

        pad.write(!SELECT_ACTIONS);
        assert_eq!(pad.read() & 0x0F, 0b1110, "A only");

        pad.write(!SELECT_DIRECTIONS);
        assert_eq!(pad.read() & 0x0F, 0b0111, "Down only");
    }

    #[test]
    fn selecting_both_groups_merges_them() {
        let mut pad = Joypad::new();
        pad.set_input(held(Buttons::A | Buttons::DOWN));
        pad.write(0x00); // both selection bits clear
        assert_eq!(pad.read() & 0x0F, 0b0110, "A and Down together");
    }

    #[test]
    fn the_unused_high_bits_read_as_ones() {
        let pad = Joypad::new();
        assert_eq!(pad.read() & 0xC0, 0xC0);
    }

    #[test]
    fn only_the_selection_bits_are_writable() {
        let mut pad = Joypad::new();
        pad.write(0x00);
        assert_eq!(pad.read() & 0x30, 0x00);
        pad.write(0xFF);
        assert_eq!(pad.read() & 0x30, 0x30);
    }

    #[test]
    fn a_press_in_the_selected_group_raises_an_interrupt() {
        let mut pad = Joypad::new();
        pad.write(!SELECT_ACTIONS);
        assert!(pad.set_input(held(Buttons::A)), "a press interrupts");
        assert!(
            !pad.set_input(held(Buttons::A)),
            "holding it does not interrupt again"
        );
        assert!(!pad.set_input(held(Buttons::empty())), "nor does releasing");
    }

    #[test]
    fn a_press_in_a_deselected_group_raises_nothing() {
        let mut pad = Joypad::new();
        pad.write(!SELECT_ACTIONS); // actions selected, directions not
        assert!(!pad.set_input(held(Buttons::UP)));
        assert!(pad.set_input(held(Buttons::UP | Buttons::B)));
    }

    #[test]
    fn the_joypad_round_trips_through_a_save_state() {
        let mut pad = Joypad::new();
        pad.write(!SELECT_DIRECTIONS);
        pad.set_input(held(Buttons::LEFT | Buttons::START));

        let mut w = StateWriter::new();
        pad.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Joypad::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, pad);
        assert_eq!(restored.read(), pad.read());
    }
}
