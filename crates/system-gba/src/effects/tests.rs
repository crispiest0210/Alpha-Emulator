use super::*;

const NONE: [bool; 3] = [false; 3];
const WIN0: [bool; 3] = [true, false, false];
const BOTH: [bool; 3] = [true, true, false];

/// `left..right` horizontally, `top..bottom` vertically.
fn bounds(left: u16, right: u16, top: u16, bottom: u16) -> (u16, u16) {
    ((left << 8) | right, (top << 8) | bottom)
}

#[test]
fn with_no_window_enabled_every_layer_draws() {
    // A game that never uses windows relies on the registers not being consulted at all.
    let effects = Effects::new();
    assert_eq!(effects.visible_layers(0, 0, NONE, false), u16::MAX);
    assert_eq!(effects.visible_layers(200, 100, NONE, false), u16::MAX);
}

#[test]
fn a_window_selects_a_layer_set_rather_than_clipping() {
    // Inside a window a *different set of layers* draws. A game shows only the background and
    // sprites belonging to a status bar there; treating it as a clip rectangle would blank the
    // status bar rather than filter it.
    let mut effects = Effects::new();
    let (h, v) = bounds(10, 50, 5, 20);
    effects.write16(reg::WIN0H, h);
    effects.write16(reg::WIN0V, v);
    effects.write16(reg::WININ, Layer::Bg0.bit() | Layer::Object.bit());
    effects.write16(reg::WINOUT, Layer::Bg1.bit());

    let inside = effects.visible_layers(20, 10, WIN0, false);
    assert_eq!(inside, Layer::Bg0.bit() | Layer::Object.bit());

    let outside = effects.visible_layers(200, 10, WIN0, false);
    assert_eq!(outside, Layer::Bg1.bit());
}

#[test]
fn a_pixel_outside_the_vertical_bounds_is_outside_the_window() {
    let mut effects = Effects::new();
    let (h, v) = bounds(0, 240, 5, 20);
    effects.write16(reg::WIN0H, h);
    effects.write16(reg::WIN0V, v);
    effects.write16(reg::WININ, Layer::Bg0.bit());
    effects.write16(reg::WINOUT, Layer::Bg1.bit());

    assert_eq!(
        effects.visible_layers(100, 10, WIN0, false),
        Layer::Bg0.bit()
    );
    assert_eq!(
        effects.visible_layers(100, 30, WIN0, false),
        Layer::Bg1.bit()
    );
}

#[test]
fn a_boundary_whose_end_is_smaller_wraps_around_the_screen() {
    // Hardware does not treat this as empty. Games use it for a band that straddles the edge,
    // so clamping to empty loses the effect entirely.
    let mut effects = Effects::new();
    let (h, v) = bounds(200, 40, 0, 160);
    effects.write16(reg::WIN0H, h);
    effects.write16(reg::WIN0V, v);
    effects.write16(reg::WININ, Layer::Bg0.bit());
    effects.write16(reg::WINOUT, Layer::Bg1.bit());

    assert_eq!(
        effects.visible_layers(220, 10, WIN0, false),
        Layer::Bg0.bit()
    );
    assert_eq!(
        effects.visible_layers(20, 10, WIN0, false),
        Layer::Bg0.bit()
    );
    assert_eq!(
        effects.visible_layers(100, 10, WIN0, false),
        Layer::Bg1.bit(),
        "and the middle is outside"
    );
}

#[test]
fn window_zero_wins_over_window_one_where_they_overlap() {
    // The fixed order is what lets a game nest a small window inside a larger one.
    let mut effects = Effects::new();
    let (h, v) = bounds(0, 100, 0, 100);
    effects.write16(reg::WIN0H, h);
    effects.write16(reg::WIN0V, v);
    effects.write16(reg::WIN1H, h);
    effects.write16(reg::WIN1V, v);
    effects.write16(reg::WININ, Layer::Bg0.bit() | (Layer::Bg1.bit() << 8));

    assert_eq!(
        effects.visible_layers(50, 50, BOTH, false),
        Layer::Bg0.bit()
    );
}

#[test]
fn window_one_applies_where_window_zero_does_not_reach() {
    let mut effects = Effects::new();
    let (h0, v0) = bounds(0, 50, 0, 50);
    let (h1, v1) = bounds(0, 200, 0, 150);
    effects.write16(reg::WIN0H, h0);
    effects.write16(reg::WIN0V, v0);
    effects.write16(reg::WIN1H, h1);
    effects.write16(reg::WIN1V, v1);
    effects.write16(reg::WININ, Layer::Bg0.bit() | (Layer::Bg1.bit() << 8));
    effects.write16(reg::WINOUT, Layer::Bg3.bit());

    assert_eq!(
        effects.visible_layers(20, 20, BOTH, false),
        Layer::Bg0.bit()
    );
    assert_eq!(
        effects.visible_layers(100, 100, BOTH, false),
        Layer::Bg1.bit()
    );
    assert_eq!(
        effects.visible_layers(220, 100, BOTH, false),
        Layer::Bg3.bit()
    );
}

#[test]
fn the_object_window_is_checked_last() {
    let mut effects = Effects::new();
    let (h, v) = bounds(0, 100, 0, 100);
    effects.write16(reg::WIN0H, h);
    effects.write16(reg::WIN0V, v);
    effects.write16(reg::WININ, Layer::Bg0.bit());
    effects.write16(reg::WINOUT, Layer::Bg3.bit() | (Layer::Bg2.bit() << 8));

    let enabled = [true, false, true];
    assert_eq!(
        effects.visible_layers(50, 50, enabled, true),
        Layer::Bg0.bit(),
        "window 0 wins even where the object window also covers"
    );
    assert_eq!(
        effects.visible_layers(200, 50, enabled, true),
        Layer::Bg2.bit(),
        "and outside it the object window applies"
    );
}

#[test]
fn each_layer_has_the_bit_the_hardware_gives_it() {
    // One index serves WININ, WINOUT, and BLDCNT, which is why they share this.
    assert_eq!(Layer::Bg0.bit(), 1 << 0);
    assert_eq!(Layer::Bg3.bit(), 1 << 3);
    assert_eq!(Layer::Object.bit(), 1 << 4);
    assert_eq!(Layer::Backdrop.bit(), 1 << 5);
    for index in 0..4 {
        assert_eq!(Layer::background(index).bit(), 1 << index);
    }
}

#[test]
fn the_four_blend_modes_decode_from_the_same_two_bits() {
    let mut effects = Effects::new();
    for (setting, expected) in [
        (0, BlendMode::None),
        (1, BlendMode::Alpha),
        (2, BlendMode::Brighten),
        (3, BlendMode::Darken),
    ] {
        effects.write16(reg::BLDCNT, setting << 6);
        assert_eq!(effects.blend_mode(), expected);
    }
}

#[test]
fn the_two_target_sets_are_separate() {
    // A game switching modes does not have to rewrite which layers take part, which is why they
    // share one register with the mode.
    let mut effects = Effects::new();
    effects.write16(reg::BLDCNT, Layer::Bg0.bit() | (Layer::Bg1.bit() << 8));
    assert!(effects.is_first_target(Layer::Bg0));
    assert!(!effects.is_first_target(Layer::Bg1));
    assert!(effects.is_second_target(Layer::Bg1));
    assert!(!effects.is_second_target(Layer::Bg0));
}

#[test]
fn no_blend_leaves_the_pixel_alone() {
    let effects = Effects::new();
    let top = Rgba8::rgb(10, 20, 30);
    assert_eq!(effects.blend(BlendMode::None, top, Rgba8::WHITE), top);
}

#[test]
fn an_alpha_blend_mixes_the_two_layers_by_their_weights() {
    let mut effects = Effects::new();
    // Half of each.
    effects.write16(reg::BLDALPHA, 8 | (8 << 8));
    let result = effects.blend(
        BlendMode::Alpha,
        Rgba8::rgb(200, 0, 0),
        Rgba8::rgb(0, 200, 0),
    );
    assert_eq!(result.r, 100);
    assert_eq!(result.g, 100);
}

#[test]
fn the_weights_saturate_above_one_rather_than_clamping_to_it() {
    // Both can exceed 16/16, which a game uses to brighten while blending. Clamping to 1.0 would
    // lose that.
    let mut effects = Effects::new();
    effects.write16(reg::BLDALPHA, 16 | (16 << 8));
    let result = effects.blend(
        BlendMode::Alpha,
        Rgba8::rgb(200, 0, 0),
        Rgba8::rgb(200, 0, 0),
    );
    assert_eq!(result.r, 255, "saturated rather than wrapped");
}

#[test]
fn brighten_takes_a_pixel_toward_white_and_darken_toward_black() {
    let mut effects = Effects::new();
    effects.write16(reg::BLDY, 16); // fully
    let grey = Rgba8::rgb(128, 128, 128);
    assert_eq!(effects.blend(BlendMode::Brighten, grey, grey), Rgba8::WHITE);
    assert_eq!(effects.blend(BlendMode::Darken, grey, grey), Rgba8::BLACK);
}

#[test]
fn a_zero_brightness_leaves_the_pixel_untouched() {
    let effects = Effects::new();
    let colour = Rgba8::rgb(10, 200, 90);
    assert_eq!(effects.blend(BlendMode::Brighten, colour, colour), colour);
    assert_eq!(effects.blend(BlendMode::Darken, colour, colour), colour);
}

#[test]
fn the_write_only_registers_read_back_as_zero() {
    let mut effects = Effects::new();
    for addr in [reg::WIN0H, reg::WIN1V, reg::MOSAIC, reg::BLDY] {
        effects.write16(addr, 0xFFFF);
        assert_eq!(effects.read16(addr), Some(0), "{addr:#010X}");
    }
    // But these do read back.
    effects.write16(reg::WININ, 0x0102);
    assert_eq!(effects.read16(reg::WININ), Some(0x0102));
}

#[test]
fn the_registers_read_back_with_their_unused_bits_clear() {
    let mut effects = Effects::new();
    effects.write16(reg::WININ, 0xFFFF);
    assert_eq!(effects.read16(reg::WININ), Some(0x3F3F));
    effects.write16(reg::BLDCNT, 0xFFFF);
    assert_eq!(effects.read16(reg::BLDCNT), Some(0x3FFF));
    effects.write16(reg::BLDALPHA, 0xFFFF);
    assert_eq!(effects.read16(reg::BLDALPHA), Some(0x1F1F));
}

#[test]
fn the_block_claims_its_registers_and_no_others() {
    assert!(Effects::owns(reg::WIN0H));
    assert!(Effects::owns(reg::BLDY + 1));
    assert!(!Effects::owns(reg::WIN0H - 1));
    assert!(!Effects::owns(reg::BLDY + 2));
}

#[test]
fn effect_state_round_trips() {
    use savestate::{decode_state, encode_state};
    let mut effects = Effects::new();
    effects.write16(reg::WIN0H, 0x1040);
    effects.write16(reg::WININ, 0x1F0F);
    effects.write16(reg::BLDCNT, 0x00C1);
    effects.write16(reg::BLDALPHA, 0x0808);
    effects.write16(reg::BLDY, 4);

    let bytes = encode_state("gba-effects", 1, &effects);
    let mut restored = Effects::new();
    decode_state("gba-effects", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, effects);
}
