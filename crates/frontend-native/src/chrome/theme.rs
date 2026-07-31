//! The application's visual theme.
//!
//! # Why a palette rather than scattered colours
//!
//! Every colour the interface uses comes from one [`Palette`], and the widget styles are derived
//! from it rather than written out. That is not tidiness for its own sake: an interface whose
//! colours are chosen at each call site drifts, and the drift is invisible until two panels sit
//! next to each other and disagree. Swapping to a different look is then editing one constant.
//!
//! # The default is the DS Lite
//!
//! Off-white shell, cool silver trim, a soft blue accent, and generous corner rounding. The screen
//! area is deliberately near-black rather than taking the shell colour, because that is what the
//! hardware's bezel does and because a light surround washes out an emulated screen that is itself
//! mostly dark.
//!
//! [`Palette::GBA`], [`Palette::DS_PHAT`], and [`Palette::GAME_BOY`] are the other three hardware
//! looks, each one constant away from being the default.

use egui::{Color32, CornerRadius, Stroke, Visuals};
use frontend_core::ThemeChoice;

/// Every colour the interface draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The shell: panel and window backgrounds.
    pub shell: Color32,
    /// A recessed area — text fields, list backgrounds.
    pub recess: Color32,
    /// A raised control at rest, and its two interaction states.
    pub control: Color32,
    pub control_hover: Color32,
    pub control_active: Color32,
    /// Trim: the hairline between one surface and the next.
    pub trim: Color32,
    /// Body text, and the dimmer variant for secondary information.
    pub text: Color32,
    pub text_dim: Color32,
    /// The accent, used for selection, focus, and links.
    pub accent: Color32,
    /// Text drawn on top of the accent.
    pub on_accent: Color32,
    /// Behind the emulated screens. The bezel.
    pub bezel: Color32,
    /// Whether this palette is a light one, which decides shadow and text-contrast choices.
    pub light: bool,
}

impl Palette {
    /// Nintendo DS Lite: polar white shell, cool silver trim, the power indicator's blue.
    pub const DS_LITE: Palette = Palette {
        shell: Color32::from_rgb(0xF2, 0xF3, 0xF5),
        recess: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        control: Color32::from_rgb(0xE4, 0xE7, 0xEA),
        control_hover: Color32::from_rgb(0xD6, 0xDB, 0xE0),
        control_active: Color32::from_rgb(0xC3, 0xCB, 0xD2),
        trim: Color32::from_rgb(0xC9, 0xCD, 0xD2),
        text: Color32::from_rgb(0x2E, 0x33, 0x38),
        text_dim: Color32::from_rgb(0x6B, 0x74, 0x7D),
        accent: Color32::from_rgb(0x3C, 0x7D, 0xC4),
        on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        // The DS Lite's bezel is a true gloss black, and the screens sit inside it.
        bezel: Color32::from_rgb(0x0D, 0x0F, 0x11),
        light: true,
    };

    /// Game Boy Advance: the indigo-violet shell and its lighter button grey.
    pub const GBA: Palette = Palette {
        shell: Color32::from_rgb(0x2A, 0x22, 0x4A),
        recess: Color32::from_rgb(0x1B, 0x16, 0x33),
        control: Color32::from_rgb(0x3B, 0x31, 0x63),
        control_hover: Color32::from_rgb(0x4B, 0x3F, 0x7A),
        control_active: Color32::from_rgb(0x5C, 0x4E, 0x92),
        trim: Color32::from_rgb(0x4A, 0x3E, 0x78),
        text: Color32::from_rgb(0xE8, 0xE5, 0xF2),
        text_dim: Color32::from_rgb(0xA5, 0x9F, 0xC0),
        accent: Color32::from_rgb(0xB0, 0x9A, 0xE0),
        on_accent: Color32::from_rgb(0x1B, 0x16, 0x33),
        bezel: Color32::from_rgb(0x0B, 0x09, 0x16),
        light: false,
    };

    /// The original DS: titanium grey with the charge indicator's amber.
    pub const DS_PHAT: Palette = Palette {
        shell: Color32::from_rgb(0x2B, 0x2E, 0x31),
        recess: Color32::from_rgb(0x1C, 0x1F, 0x21),
        control: Color32::from_rgb(0x3A, 0x3E, 0x42),
        control_hover: Color32::from_rgb(0x4A, 0x4F, 0x54),
        control_active: Color32::from_rgb(0x5B, 0x61, 0x67),
        trim: Color32::from_rgb(0x4C, 0x51, 0x56),
        text: Color32::from_rgb(0xE4, 0xE6, 0xE8),
        text_dim: Color32::from_rgb(0x9A, 0xA0, 0xA6),
        accent: Color32::from_rgb(0xE8, 0xA3, 0x3D),
        on_accent: Color32::from_rgb(0x1C, 0x1F, 0x21),
        bezel: Color32::from_rgb(0x0E, 0x10, 0x11),
        light: false,
    };

    /// The original Game Boy's LCD: olive-green field, dark green ink.
    pub const GAME_BOY: Palette = Palette {
        shell: Color32::from_rgb(0x9B, 0xBC, 0x0F),
        recess: Color32::from_rgb(0x8B, 0xAC, 0x0F),
        control: Color32::from_rgb(0x8B, 0xAC, 0x0F),
        control_hover: Color32::from_rgb(0x7A, 0x9B, 0x0E),
        control_active: Color32::from_rgb(0x30, 0x62, 0x30),
        trim: Color32::from_rgb(0x30, 0x62, 0x30),
        text: Color32::from_rgb(0x0F, 0x38, 0x0F),
        text_dim: Color32::from_rgb(0x30, 0x62, 0x30),
        accent: Color32::from_rgb(0x0F, 0x38, 0x0F),
        on_accent: Color32::from_rgb(0x9B, 0xBC, 0x0F),
        bezel: Color32::from_rgb(0x0F, 0x38, 0x0F),
        light: true,
    };
}

impl From<ThemeChoice> for Palette {
    /// The colours a setting names.
    ///
    /// `frontend-core` owns the *choice* because it is a user setting stored with the rest of
    /// them; the colours live here because that crate may not depend on a UI framework. This is
    /// the one function joining the two.
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::DsLite => Palette::DS_LITE,
            ThemeChoice::Gba => Palette::GBA,
            ThemeChoice::DsPhat => Palette::DS_PHAT,
            ThemeChoice::GameBoy => Palette::GAME_BOY,
        }
    }
}

/// How round a corner is. The DS Lite is a notably rounded piece of hardware, and this is the one
/// number that carries most of that impression.
const ROUNDING: u8 = 6;

/// Build the egui visuals for a palette.
///
/// Derived rather than hand-written per widget so that a colour cannot be forgotten in one state
/// and set in another — the failure that makes a hovered button jump to a colour nothing else in
/// the interface uses.
pub fn visuals(palette: Palette) -> Visuals {
    let mut visuals = if palette.light {
        Visuals::light()
    } else {
        Visuals::dark()
    };

    visuals.panel_fill = palette.shell;
    visuals.window_fill = palette.shell;
    visuals.faint_bg_color = palette.control;
    visuals.extreme_bg_color = palette.recess;
    visuals.window_stroke = Stroke::new(1.0, palette.trim);
    visuals.window_corner_radius = CornerRadius::same(ROUNDING);

    let radius = CornerRadius::same(ROUNDING);
    let widgets = &mut visuals.widgets;

    widgets.noninteractive.bg_fill = palette.shell;
    widgets.noninteractive.weak_bg_fill = palette.shell;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.trim);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.noninteractive.corner_radius = radius;

    widgets.inactive.bg_fill = palette.control;
    widgets.inactive.weak_bg_fill = palette.control;
    widgets.inactive.bg_stroke = Stroke::new(1.0, palette.trim);
    widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.inactive.corner_radius = radius;

    widgets.hovered.bg_fill = palette.control_hover;
    widgets.hovered.weak_bg_fill = palette.control_hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0, palette.accent);
    widgets.hovered.fg_stroke = Stroke::new(1.5, palette.text);
    widgets.hovered.corner_radius = radius;

    widgets.active.bg_fill = palette.control_active;
    widgets.active.weak_bg_fill = palette.control_active;
    widgets.active.bg_stroke = Stroke::new(1.0, palette.accent);
    widgets.active.fg_stroke = Stroke::new(1.5, palette.text);
    widgets.active.corner_radius = radius;

    widgets.open.bg_fill = palette.control_hover;
    widgets.open.weak_bg_fill = palette.control_hover;
    widgets.open.bg_stroke = Stroke::new(1.0, palette.trim);
    widgets.open.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.open.corner_radius = radius;

    visuals.selection.bg_fill = palette.accent;
    visuals.selection.stroke = Stroke::new(1.0, palette.on_accent);
    visuals.hyperlink_color = palette.accent;
    visuals.warn_fg_color = Color32::from_rgb(0xC7, 0x7A, 0x18);
    visuals.error_fg_color = Color32::from_rgb(0xC4, 0x3D, 0x3D);
    visuals.weak_text_alpha = 0.75;

    visuals
}

/// Apply a palette to a context, including the spacing that goes with it.
pub fn apply(ctx: &egui::Context, palette: Palette) {
    ctx.set_visuals(visuals(palette));
    // `all_styles_mut` rather than one theme's, because egui keeps a light and a dark style and
    // the spacing below belongs to both — the palette decides the colours, not the metrics.
    ctx.all_styles_mut(|style| {
        // A little more air than egui's default. The hardware this is dressed as has generously
        // spaced controls, and a cramped panel reads as a developer tool rather than as a console.
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(10);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.indent = 18.0;
        style.spacing.slider_width = 160.0;
        style.spacing.interact_size.y = 24.0;
        style.visuals.striped = true;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_keeps_its_text_readable_against_its_shell() {
        // The one property a palette cannot get wrong. Checked as a luminance gap rather than by
        // eye, because "looks fine on my monitor" is how a theme ships unreadable on someone
        // else's.
        for (name, palette) in [
            ("DS Lite", Palette::DS_LITE),
            ("GBA", Palette::GBA),
            ("DS Phat", Palette::DS_PHAT),
            ("Game Boy", Palette::GAME_BOY),
        ] {
            let gap = (luminance(palette.text) - luminance(palette.shell)).abs();
            assert!(
                gap > 0.35,
                "{name}: text and shell are too close ({gap:.2})"
            );

            let dim_gap = (luminance(palette.text_dim) - luminance(palette.shell)).abs();
            assert!(
                dim_gap > 0.15,
                "{name}: dim text is too close ({dim_gap:.2})"
            );

            let accent_gap = (luminance(palette.accent) - luminance(palette.on_accent)).abs();
            assert!(
                accent_gap > 0.25,
                "{name}: text on the accent is too close ({accent_gap:.2})"
            );
        }
    }

    #[test]
    fn the_light_flag_agrees_with_which_way_round_the_ink_is() {
        // `light` picks which egui base the visuals start from, so what it has to mean is "dark
        // ink on a lighter field" — not "the shell is bright". The Game Boy's LCD green sits at
        // the middle of the luminance range and is still unambiguously a light theme, which is
        // why this checks the *relationship* rather than an absolute threshold.
        for (name, palette) in [
            ("DS Lite", Palette::DS_LITE),
            ("GBA", Palette::GBA),
            ("DS Phat", Palette::DS_PHAT),
            ("Game Boy", Palette::GAME_BOY),
        ] {
            let shell = luminance(palette.shell);
            let text = luminance(palette.text);
            if palette.light {
                assert!(text < shell, "{name} is light, so its ink must be darker");
            } else {
                assert!(text > shell, "{name} is dark, so its ink must be lighter");
            }
            // And a control has to sit between the two rather than outside them, or a button
            // reads as a hole in the panel.
            let control = luminance(palette.control);
            let (low, high) = if palette.light {
                (text, shell.max(luminance(palette.recess)))
            } else {
                (shell.min(luminance(palette.recess)), text)
            };
            assert!(
                control >= low && control <= high,
                "{name}: a control at {control:.2} is outside {low:.2}..{high:.2}"
            );
        }
    }

    #[test]
    fn the_bezel_is_darker_than_the_shell_on_every_palette() {
        // The emulated screen sits on the bezel. A bezel lighter than the shell would make the
        // picture the brightest thing on screen surrounded by something brighter still.
        for palette in [
            Palette::DS_LITE,
            Palette::GBA,
            Palette::DS_PHAT,
            Palette::GAME_BOY,
        ] {
            assert!(luminance(palette.bezel) <= luminance(palette.shell));
        }
    }

    #[test]
    fn the_visuals_carry_the_palettes_colours_rather_than_egui_defaults() {
        let visuals = visuals(Palette::DS_LITE);
        assert_eq!(visuals.panel_fill, Palette::DS_LITE.shell);
        assert_eq!(visuals.selection.bg_fill, Palette::DS_LITE.accent);
        assert_eq!(visuals.widgets.inactive.bg_fill, Palette::DS_LITE.control);
        // Every interaction state is set, so none falls back to a colour nothing else uses.
        assert_ne!(
            visuals.widgets.hovered.bg_fill,
            visuals.widgets.inactive.bg_fill
        );
        assert_ne!(
            visuals.widgets.active.bg_fill,
            visuals.widgets.hovered.bg_fill
        );
    }

    /// Perceived brightness, 0 to 1. The usual sRGB coefficients.
    fn luminance(color: Color32) -> f32 {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }
}
