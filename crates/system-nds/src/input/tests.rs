use super::*;
use core_common::TouchPoint;

const ARM9: Core = Core::Arm9;
const ARM7: Core = Core::Arm7;

fn pressed(buttons: Buttons) -> InputState {
    InputState {
        buttons,
        touch: None,
    }
}

fn touched(x: u16, y: u16) -> InputState {
    InputState {
        buttons: Buttons::empty(),
        touch: Some(TouchPoint { x, y }),
    }
}

#[test]
fn nothing_is_held_at_power_on() {
    // Zeroing these registers means every button held from the moment the machine starts, which
    // presents as a stuck controller rather than as an initialisation bug.
    let mut input = Input::new();
    assert_eq!(input.read16(ARM9, reg::KEYINPUT), Some(0x03FF));
    assert_eq!(input.read16(ARM7, reg::EXTKEYIN), Some(0x007F));
}

#[test]
fn the_ten_shared_buttons_map_to_their_bits() {
    let cases = [
        (Buttons::A, 0),
        (Buttons::B, 1),
        (Buttons::SELECT, 2),
        (Buttons::START, 3),
        (Buttons::RIGHT, 4),
        (Buttons::LEFT, 5),
        (Buttons::UP, 6),
        (Buttons::DOWN, 7),
        (Buttons::R, 8),
        (Buttons::L, 9),
    ];
    for (button, bit) in cases {
        let mut input = Input::new();
        input.set_input(pressed(button));
        assert_eq!(
            input.read16(ARM9, reg::KEYINPUT),
            Some(0x03FF & !(1 << bit)),
            "{button:?}"
        );
    }
}

#[test]
fn x_and_y_are_on_the_register_only_the_arm7_can_see() {
    let mut input = Input::new();
    input.set_input(pressed(Buttons::X | Buttons::Y));
    assert_eq!(input.read16(ARM7, reg::EXTKEYIN), Some(0x007C));
    // Not in KEYINPUT at all, on either core.
    assert_eq!(input.read16(ARM9, reg::KEYINPUT), Some(0x03FF));
    assert_eq!(input.read16(ARM9, reg::EXTKEYIN), None);
    assert!(!Input::owns(ARM9, reg::EXTKEYIN));
    assert!(Input::owns(ARM7, reg::EXTKEYIN));
    assert!(Input::owns(ARM9, reg::KEYINPUT));
}

#[test]
fn a_touch_pulls_the_pen_down_line() {
    let mut input = Input::new();
    assert_ne!(input.read16(ARM7, reg::EXTKEYIN).unwrap() & (1 << 6), 0);
    input.set_input(touched(10, 20));
    assert_eq!(input.read16(ARM7, reg::EXTKEYIN).unwrap() & (1 << 6), 0);
    input.set_input(pressed(Buttons::empty()));
    assert_ne!(input.read16(ARM7, reg::EXTKEYIN).unwrap() & (1 << 6), 0);
}

#[test]
fn the_keypad_interrupt_distinguishes_any_from_all() {
    let mut input = Input::new();
    // Enable, "any", watching A and B.
    input.write16(ARM7, reg::KEYCNT, (1 << 15) | 0b11);
    assert!(!input.irq_pending());
    input.set_input(pressed(Buttons::A));
    assert!(input.irq_pending());

    // "All" needs the whole chord, which is how a game watches for a soft-reset combination.
    input.write16(ARM7, reg::KEYCNT, (1 << 15) | (1 << 14) | 0b11);
    assert!(!input.irq_pending(), "only one of the two");
    input.set_input(pressed(Buttons::A | Buttons::B));
    assert!(input.irq_pending());

    // And nothing at all fires with the enable bit clear.
    input.write16(ARM7, reg::KEYCNT, 0b11);
    assert!(!input.irq_pending());
}

#[test]
fn the_key_registers_are_pins_and_ignore_writes() {
    let mut input = Input::new();
    input.set_input(pressed(Buttons::A));
    input.write16(ARM9, reg::KEYINPUT, 0xFFFF);
    assert_eq!(input.read16(ARM9, reg::KEYINPUT), Some(0x03FE));
}

/// Run the conversion a touchscreen driver runs: command, then two data bytes.
fn read_channel(input: &mut Input, channel: u8) -> u16 {
    // Select the touchscreen and hold it selected across the whole conversion.
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (2 << 8));
    input.write16(ARM7, reg::SPIDATA, (0x80 | (channel << 4)) as u16);
    input.write16(ARM7, reg::SPIDATA, 0);
    let high = input.read16(ARM7, reg::SPIDATA).unwrap();
    input.write16(ARM7, reg::SPIDATA, 0);
    let low = input.read16(ARM7, reg::SPIDATA).unwrap();
    // Exactly the reconstruction a driver performs.
    ((high & 0x7F) << 5) | ((low & 0xF8) >> 3)
}

#[test]
fn the_touchscreen_reports_a_position_over_the_serial_bus() {
    let mut input = Input::new();
    input.set_input(touched(100, 50));
    assert_eq!(read_channel(&mut input, 5), 100 * RAW_PER_PIXEL, "X");
    assert_eq!(read_channel(&mut input, 1), 50 * RAW_PER_PIXEL, "Y");
}

#[test]
fn channel_one_is_y_and_channel_five_is_x() {
    // Swapping them gives a touchscreen that works perfectly along the diagonal and nowhere
    // else, which is a genuinely confusing symptom.
    let mut input = Input::new();
    input.set_input(touched(200, 10));
    assert_ne!(read_channel(&mut input, 5), read_channel(&mut input, 1));
    assert_eq!(read_channel(&mut input, 5), 200 * RAW_PER_PIXEL);
}

#[test]
fn the_extremes_of_the_screen_survive_the_twelve_bit_conversion() {
    let mut input = Input::new();
    for (x, y) in [(0, 0), (255, 191), (255, 0), (0, 191)] {
        input.set_input(touched(x, y));
        assert_eq!(read_channel(&mut input, 5), x * RAW_PER_PIXEL);
        assert_eq!(read_channel(&mut input, 1), y * RAW_PER_PIXEL);
    }
}

#[test]
fn a_touch_outside_the_screen_is_clamped_rather_than_wrapped() {
    let mut input = Input::new();
    input.set_input(touched(9999, 9999));
    assert_eq!(read_channel(&mut input, 5), 255 * RAW_PER_PIXEL);
    assert_eq!(read_channel(&mut input, 1), 191 * RAW_PER_PIXEL);
}

#[test]
fn the_position_channels_read_zero_with_the_pen_up() {
    let mut input = Input::new();
    assert_eq!(read_channel(&mut input, 5), 0);
    assert_eq!(read_channel(&mut input, 1), 0);
}

/// Run the conversion the DS's own SDK runs: one command byte per sample, each riding in the
/// slot where the previous sample's low byte comes back.
///
/// ```text
/// send: cmd  00h  cmd  00h  cmd  00h
/// recv: 00h  hi1  lo1  hi2  lo2  hi3
/// ```
fn read_channel_chained(input: &mut Input, channel: u8, samples: usize) -> Vec<u16> {
    let command = (0x80 | (channel << 4)) as u16;
    let send = |input: &mut Input, byte: u16| {
        input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (2 << 8));
        input.write16(ARM7, reg::SPIDATA, byte);
        input.read16(ARM7, reg::SPIDATA).unwrap()
    };
    send(input, command);
    let mut values = Vec::with_capacity(samples);
    for _ in 0..samples {
        let high = send(input, 0);
        let low = send(input, command);
        values.push(((high & 0x7F) << 5) | ((low & 0xF8) >> 3));
    }
    values
}

#[test]
fn a_chained_conversion_hands_back_its_low_bits() {
    // Odd coordinates, so the bottom five bits of the reading are the ones that decide them: at
    // sixteen counts per pixel an answer missing them is an answer rounded down to an even pixel.
    let mut input = Input::new();
    input.set_input(touched(101, 51));
    assert_eq!(
        read_channel_chained(&mut input, 5, 3),
        vec![101 * RAW_PER_PIXEL; 3],
        "X, three times over"
    );
    assert_eq!(
        read_channel_chained(&mut input, 1, 3),
        vec![51 * RAW_PER_PIXEL; 3],
        "Y, three times over"
    );
}

#[test]
fn deselecting_the_device_restarts_the_byte_counter() {
    let mut input = Input::new();
    input.set_input(touched(100, 50));
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (2 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x80 | (5 << 4));
    input.write16(ARM7, reg::SPIDATA, 0);
    let first = input.read16(ARM7, reg::SPIDATA).unwrap();

    // Dropping bit 11 makes this byte the last of the sequence: it still transfers, and the
    // controller is deselected afterwards.
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (2 << 8));
    input.write16(ARM7, reg::SPIDATA, 0);
    let low = input.read16(ARM7, reg::SPIDATA).unwrap();
    assert_ne!(low, first, "that was the second data byte");

    // A byte sent now starts a fresh sequence rather than continuing the old one, which is what
    // lets a driver abandon a conversion part-way and try again.
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (2 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x80 | (5 << 4));
    input.write16(ARM7, reg::SPIDATA, 0);
    assert_eq!(input.read16(ARM7, reg::SPIDATA), Some(first));
}

#[test]
fn the_firmware_answers_a_read_out_of_its_own_image() {
    // The bus used to answer every firmware byte with 0xFF. See `crate::firmware` for the retail
    // game that reads the flash directly and hangs when it does. This checks the join: bytes
    // clocked at the firmware device reach the chip and its answer comes back through SPIDATA.
    let mut input = Input::new();
    let expected = input.firmware.image()[4];
    for byte in [0x03, 0x00, 0x00, 0x04] {
        input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (1 << 8));
        input.write16(ARM7, reg::SPIDATA, byte);
    }
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (1 << 8));
    input.write16(ARM7, reg::SPIDATA, 0);
    assert_eq!(input.read16(ARM7, reg::SPIDATA), Some(expected as u16));
}

#[test]
fn releasing_the_chipselect_ends_the_firmware_command() {
    // Bit 11 is the select line, and the flash's only framing. A driver that reads two blocks in
    // a row gets the second from where the first stopped without this.
    let mut input = Input::new();
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (1 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x03);
    // No hold: this byte transfers and then deselects.
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x00);
    // Which makes the next byte an opcode again, not the second address byte.
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (1 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x05);
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (1 << 8));
    input.write16(ARM7, reg::SPIDATA, 0);
    // The status register, which is what opcode 5 asks for, and not image data.
    assert_eq!(input.read16(ARM7, reg::SPIDATA), Some(0));
}

#[test]
fn the_power_management_chip_is_absent_but_does_not_hang() {
    // It accepts writes and reads back zero: what it controls — backlight, power LED, amplifier —
    // has no equivalent here, and its one input is a battery this machine does not run on.
    let mut input = Input::new();
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11));
    input.write16(ARM7, reg::SPIDATA, 0x00);
    assert_eq!(input.read16(ARM7, reg::SPIDATA), Some(0));
}

#[test]
fn the_spi_bus_is_not_in_the_arm9s_map() {
    let mut input = Input::new();
    assert!(!Input::owns(ARM9, reg::SPICNT));
    assert_eq!(input.read16(ARM9, reg::SPIDATA), None);
    assert!(!input.write16(ARM9, reg::SPICNT, 0xFFFF));
}

#[test]
fn byte_accesses_reach_the_registers() {
    let mut input = Input::new();
    input.set_input(pressed(Buttons::L));
    assert_eq!(input.read8(ARM9, reg::KEYINPUT), Some(0xFF));
    assert_eq!(input.read8(ARM9, reg::KEYINPUT + 1), Some(0x01));
    input.write8(ARM7, reg::KEYCNT, 0b11);
    input.write8(ARM7, reg::KEYCNT + 1, 0x80);
    assert!(!input.irq_pending());
    input.set_input(pressed(Buttons::A));
    assert!(input.irq_pending());
}

#[test]
fn input_round_trips_through_a_save_state_mid_conversion() {
    use savestate::{decode_state, encode_state};

    let mut input = Input::new();
    input.set_input(touched(77, 88));
    input.write16(ARM7, reg::SPICNT, (1 << 15) | (1 << 11) | (2 << 8));
    input.write16(ARM7, reg::SPIDATA, 0x80 | (5 << 4));
    input.write16(ARM7, reg::SPIDATA, 0);
    let high = input.read16(ARM7, reg::SPIDATA).unwrap();

    let blob = encode_state("nds", 1, &input);
    let mut restored = Input::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    // The half-finished conversion continues where it left off.
    restored.write16(ARM7, reg::SPIDATA, 0);
    let low = restored.read16(ARM7, reg::SPIDATA).unwrap();
    assert_eq!(
        ((high & 0x7F) << 5) | ((low & 0xF8) >> 3),
        77 * RAW_PER_PIXEL
    );
    assert_eq!(restored.read16(ARM7, reg::EXTKEYIN).unwrap() & (1 << 6), 0);
}
