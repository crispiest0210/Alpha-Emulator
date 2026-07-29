//! The GBA keypad: `KEYINPUT` and `KEYCNT`.
//!
//! # Active low, like every Nintendo handheld
//!
//! A *set* bit means the button is *up*. A fresh keypad reads `0x03FF`, not zero. Getting this
//! backwards makes a game behave as though every button is held from the moment it boots, which
//! looks like an input-routing bug rather than an inverted comparison.
//!
//! # The interrupt is a condition, not an event
//!
//! `KEYCNT` selects a set of buttons and a rule: raise the interrupt when *any* of them is
//! pressed, or only when *all* of them are. The all-of-them form is how a game implements a
//! soft-reset combination without polling. Treating it as "any" would fire that reset the
//! moment a player touched one of the four keys.

use core_common::{Buttons, Savable, StateError, StateReader, StateWriter};

pub const KEYINPUT: u32 = 0x0400_0130;
pub const KEYCNT: u32 = 0x0400_0132;

/// The ten bits `KEYINPUT` reports, in hardware order.
///
/// The GBA has no X or Y, so those two are simply absent from the mapping rather than folded
/// onto something else — a frontend binding them gets nothing, which is correct.
const BUTTON_ORDER: [Buttons; 10] = [
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
];

/// Every bit that exists. The six above them read as zero.
const PRESENT: u16 = 0x03FF;

mod control {
    /// Which buttons take part.
    pub const SELECTION: u16 = 0x03FF;
    pub const IRQ_ENABLE: u16 = 1 << 14;
    /// Set requires *all* the selected buttons; clear requires any one of them.
    pub const REQUIRE_ALL: u16 = 1 << 15;
    pub const MASK: u16 = SELECTION | IRQ_ENABLE | REQUIRE_ALL;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keypad {
    /// Active low: a set bit is a button that is *up*.
    state: u16,
    control: u16,
}

impl Default for Keypad {
    fn default() -> Self {
        Self::new()
    }
}

impl Keypad {
    pub fn new() -> Self {
        Self {
            state: PRESENT,
            control: 0,
        }
    }

    pub fn owns(addr: u32) -> bool {
        (KEYINPUT..KEYCNT + 2).contains(&addr)
    }

    /// Apply what the frontend reported.
    pub fn set_input(&mut self, buttons: Buttons) {
        let mut state = PRESENT;
        for (bit, button) in BUTTON_ORDER.iter().enumerate() {
            if buttons.contains(*button) {
                state &= !(1 << bit);
            }
        }
        // Left and right, or up and down, cannot both be held on real hardware — the membrane
        // is one contact per axis. Games do not all cope with the impossible combination, so it
        // is resolved here rather than left for each of them to mishandle differently.
        state = resolve_opposites(state, 4, 5);
        state = resolve_opposites(state, 6, 7);
        self.state = state;
    }

    /// Whether the configured combination is being held.
    pub fn interrupt_requested(&self) -> bool {
        if self.control & control::IRQ_ENABLE == 0 {
            return false;
        }
        let selected = self.control & control::SELECTION;
        if selected == 0 {
            return false;
        }
        // `state` is active low, so a pressed button is a *clear* bit.
        let pressed = !self.state & PRESENT;
        if self.control & control::REQUIRE_ALL != 0 {
            pressed & selected == selected
        } else {
            pressed & selected != 0
        }
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        Some(match addr {
            KEYINPUT => self.state,
            KEYCNT => self.control,
            _ => return None,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        match addr {
            // `KEYINPUT` is the buttons themselves; a write cannot press one.
            KEYINPUT => {}
            KEYCNT => self.control = value & control::MASK,
            _ => return None,
        }
        Some(())
    }
}

/// Drop the lower-numbered of two opposing directions when both are held.
///
/// Which one survives is arbitrary — hardware cannot produce the combination at all — but it
/// has to be *deterministic*, because a replay that resolved it differently on playback would
/// diverge.
fn resolve_opposites(state: u16, first: u32, second: u32) -> u16 {
    let both_pressed = state & (1 << first) == 0 && state & (1 << second) == 0;
    if both_pressed {
        state | (1 << first)
    } else {
        state
    }
}

impl Savable for Keypad {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.state);
        w.write_u16(self.control);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.state = r.read_u16()?;
        self.control = r.read_u16()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_keypad_reads_every_button_as_up() {
        // Active low. Reading zero here would make a game behave as though every button is held
        // from the moment it boots.
        let keypad = Keypad::new();
        assert_eq!(keypad.read16(KEYINPUT), Some(0x03FF));
    }

    #[test]
    fn a_pressed_button_clears_its_bit() {
        let mut keypad = Keypad::new();
        keypad.set_input(Buttons::A);
        assert_eq!(keypad.read16(KEYINPUT), Some(0x03FE));

        keypad.set_input(Buttons::A | Buttons::B);
        assert_eq!(keypad.read16(KEYINPUT), Some(0x03FC));
    }

    #[test]
    fn every_button_lands_on_the_bit_the_hardware_uses() {
        for (bit, button) in BUTTON_ORDER.iter().enumerate() {
            let mut keypad = Keypad::new();
            keypad.set_input(*button);
            assert_eq!(
                keypad.read16(KEYINPUT),
                Some(PRESENT & !(1 << bit)),
                "{button:?} should be bit {bit}"
            );
        }
    }

    #[test]
    fn the_buttons_the_gba_does_not_have_do_nothing() {
        // No X or Y. A frontend binding them gets nothing, which is correct — folding them onto
        // another button would give a player a control that hardware does not have.
        let mut keypad = Keypad::new();
        keypad.set_input(Buttons::X | Buttons::Y);
        assert_eq!(keypad.read16(KEYINPUT), Some(PRESENT));
    }

    #[test]
    fn opposing_directions_cannot_both_be_held() {
        // The membrane is one contact per axis, so hardware cannot produce this. Games do not
        // all cope with it, so it is resolved here rather than differently in each of them.
        let mut keypad = Keypad::new();
        keypad.set_input(Buttons::LEFT | Buttons::RIGHT);
        let state = keypad.read16(KEYINPUT).unwrap();
        let pressed = !state & PRESENT;
        assert_eq!(pressed.count_ones(), 1, "only one horizontal direction");

        keypad.set_input(Buttons::UP | Buttons::DOWN);
        let pressed = !keypad.read16(KEYINPUT).unwrap() & PRESENT;
        assert_eq!(pressed.count_ones(), 1);
    }

    #[test]
    fn resolving_opposites_is_deterministic() {
        // Arbitrary which one survives, but a replay that resolved it differently on playback
        // would diverge.
        let mut first = Keypad::new();
        let mut second = Keypad::new();
        first.set_input(Buttons::LEFT | Buttons::RIGHT);
        second.set_input(Buttons::RIGHT | Buttons::LEFT);
        assert_eq!(first.read16(KEYINPUT), second.read16(KEYINPUT));
    }

    #[test]
    fn releasing_a_button_sets_its_bit_again() {
        let mut keypad = Keypad::new();
        keypad.set_input(Buttons::START);
        keypad.set_input(Buttons::empty());
        assert_eq!(keypad.read16(KEYINPUT), Some(PRESENT));
    }

    #[test]
    fn no_interrupt_is_requested_without_the_enable_bit() {
        let mut keypad = Keypad::new();
        keypad.write16(KEYCNT, control::SELECTION);
        keypad.set_input(Buttons::A);
        assert!(!keypad.interrupt_requested());
    }

    #[test]
    fn the_any_form_fires_on_one_selected_button() {
        let mut keypad = Keypad::new();
        keypad.write16(KEYCNT, control::IRQ_ENABLE | 0b11); // A or B
        keypad.set_input(Buttons::B);
        assert!(keypad.interrupt_requested());

        keypad.set_input(Buttons::START);
        assert!(!keypad.interrupt_requested(), "not a selected button");
    }

    #[test]
    fn the_all_form_waits_for_the_whole_combination() {
        // How a game implements a soft-reset combination without polling. Treating it as "any"
        // would fire the reset the moment a player touched one of the four keys.
        let mut keypad = Keypad::new();
        let combination = 0b11 | (1 << 2) | (1 << 3); // A, B, Select, Start
        keypad.write16(
            KEYCNT,
            control::IRQ_ENABLE | control::REQUIRE_ALL | combination,
        );

        keypad.set_input(Buttons::A);
        assert!(!keypad.interrupt_requested(), "one of four is not all four");

        keypad.set_input(Buttons::A | Buttons::B | Buttons::SELECT);
        assert!(!keypad.interrupt_requested(), "three of four");

        keypad.set_input(Buttons::A | Buttons::B | Buttons::SELECT | Buttons::START);
        assert!(keypad.interrupt_requested());
    }

    #[test]
    fn an_empty_selection_never_fires() {
        // Otherwise the all-form's "every selected button is pressed" is vacuously true and the
        // interrupt fires forever.
        let mut keypad = Keypad::new();
        keypad.write16(KEYCNT, control::IRQ_ENABLE | control::REQUIRE_ALL);
        assert!(!keypad.interrupt_requested());
    }

    #[test]
    fn keyinput_cannot_be_written() {
        let mut keypad = Keypad::new();
        keypad.write16(KEYINPUT, 0);
        assert_eq!(
            keypad.read16(KEYINPUT),
            Some(PRESENT),
            "a write cannot press a button"
        );
    }

    #[test]
    fn control_reads_back_with_its_unused_bits_clear() {
        let mut keypad = Keypad::new();
        keypad.write16(KEYCNT, 0xFFFF);
        assert_eq!(keypad.read16(KEYCNT), Some(control::MASK));
    }

    #[test]
    fn the_block_claims_its_two_registers_and_no_more() {
        assert!(Keypad::owns(KEYINPUT));
        assert!(Keypad::owns(KEYCNT + 1));
        assert!(!Keypad::owns(KEYINPUT - 1));
        assert!(!Keypad::owns(KEYCNT + 2));
    }

    #[test]
    fn keypad_state_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut keypad = Keypad::new();
        keypad.set_input(Buttons::A | Buttons::UP);
        keypad.write16(KEYCNT, control::IRQ_ENABLE | 0b11);

        let bytes = encode_state("gba-keypad", 1, &keypad);
        let mut restored = Keypad::new();
        decode_state("gba-keypad", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored, keypad);
    }
}
