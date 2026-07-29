//! Where the emulated picture goes in the window, and where a mouse click lands in it.
//!
//! Pure arithmetic over sizes — no `wgpu`, no `egui`, no window. Prompt 14 asks specifically that
//! the coordinate mapping be unit-tested "since [it is a] pure function independent of the actual
//! window", and that is only possible if it is kept that way. Every function here is total: given
//! any window size, including degenerate ones a compositor really does hand you during a resize
//! or a minimise, it returns a usable layout rather than a division by zero.
//!
//! # Dual screens
//!
//! The emulation core produces the Nintendo DS as one framebuffer of 256×384 — two 256×192 screens
//! stacked with no gap. The gap belongs here: it is a presentation choice that depends on how big
//! the window is, and a framebuffer with a hole in it would force the compositor to know about a
//! decision that has nothing to do with emulation.
//!
//! Splitting the framebuffer in two is also what makes touch input possible, since the mapping
//! from a window position to a DS touch coordinate is exactly the inverse of the bottom screen's
//! placement.

use core_common::TouchPoint;
use frontend_core::ScalingMode;

/// A rectangle in logical points, matching what `egui` works in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// A region of the source framebuffer, in emulated pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One screen: where to read from, and where to draw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenView {
    pub source: SourceRect,
    pub dest: Rect,
}

/// The complete placement for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// One entry per physical screen, top to bottom.
    pub screens: Vec<ScreenView>,
    /// Which entry accepts touch input, if any. The DS's lower screen; nothing on a Game Boy.
    pub touch_screen: Option<usize>,
    /// Emulated pixels per logical point, after the scaling mode has had its say.
    pub scale: f32,
}

impl Layout {
    /// An empty layout, for when there is no frame to show.
    pub fn none() -> Self {
        Self {
            screens: Vec::new(),
            touch_screen: None,
            scale: 1.0,
        }
    }

    /// Translate a window position into a touch coordinate.
    ///
    /// `None` when this machine has no touch screen, or when the position is outside it. Clamping
    /// to the edge instead would be wrong in a way that matters: a stylus lifted off the screen
    /// and a stylus pressed against its border are different inputs, and the DS's own touch
    /// controller reports them differently.
    pub fn touch_at(&self, x: f32, y: f32) -> Option<TouchPoint> {
        let screen = self.screens.get(self.touch_screen?)?;
        if !screen.dest.contains(x, y) {
            return None;
        }
        // Guard the division: a zero-sized destination happens when the window is collapsed to
        // nothing, and a click cannot arrive in a rectangle of no area anyway.
        if screen.dest.width <= 0.0 || screen.dest.height <= 0.0 {
            return None;
        }
        let fraction_x = (x - screen.dest.x) / screen.dest.width;
        let fraction_y = (y - screen.dest.y) / screen.dest.height;
        let touch_x = (fraction_x * screen.source.width as f32) as u32;
        let touch_y = (fraction_y * screen.source.height as f32) as u32;
        Some(TouchPoint {
            // The subtraction cannot underflow: `contains` rejected an empty source region's
            // rectangle above, since a zero-width source means a zero-width destination.
            x: touch_x.min(screen.source.width.saturating_sub(1)) as u16,
            y: touch_y.min(screen.source.height.saturating_sub(1)) as u16,
        })
    }

    /// The whole picture's bounding box, for drawing a border or a background.
    pub fn bounds(&self) -> Option<Rect> {
        let first = self.screens.first()?;
        let last = self.screens.last()?;
        Some(Rect::new(
            first.dest.x,
            first.dest.y,
            first.dest.width,
            last.dest.y + last.dest.height - first.dest.y,
        ))
    }
}

/// Work out where to draw a framebuffer inside the space available.
///
/// `gap_pixels` is in *emulated* pixels and so scales with the picture, which is what keeps the
/// gap looking the same at every window size.
pub fn compute(
    framebuffer: (u32, u32),
    dual_screen: bool,
    gap_pixels: u32,
    available: Rect,
    mode: ScalingMode,
) -> Layout {
    let (fb_width, fb_height) = framebuffer;
    if fb_width == 0 || fb_height == 0 {
        return Layout::none();
    }

    // A dual-screen framebuffer with an odd height cannot be halved evenly. Rather than silently
    // dropping a row, treat it as a single screen: it is not a DS framebuffer, whatever it claims.
    let dual_screen = dual_screen && fb_height % 2 == 0;
    let gap = if dual_screen { gap_pixels } else { 0 };

    let content_width = fb_width as f32;
    let content_height = (fb_height + gap) as f32;

    let fit = (available.width / content_width).min(available.height / content_height);
    let scale = match mode {
        // Below one, an integer scale would be zero and nothing would be drawn at all. One
        // emulated pixel per screen pixel with the edges clipped by the window is a far better
        // answer to a tiny window than a blank one.
        ScalingMode::IntegerNearest => fit.floor().max(1.0),
        ScalingMode::Nearest | ScalingMode::Linear => fit,
    };
    // A non-finite or non-positive scale comes from a zero-sized window, which really does happen
    // between a minimise and the resize event that follows it.
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        return Layout::none();
    };

    let drawn_width = content_width * scale;
    let drawn_height = content_height * scale;
    let left = available.x + (available.width - drawn_width) / 2.0;
    let top = available.y + (available.height - drawn_height) / 2.0;

    let mut screens = Vec::with_capacity(if dual_screen { 2 } else { 1 });
    if dual_screen {
        let half = fb_height / 2;
        let screen_height = half as f32 * scale;
        screens.push(ScreenView {
            source: SourceRect {
                x: 0,
                y: 0,
                width: fb_width,
                height: half,
            },
            dest: Rect::new(left, top, drawn_width, screen_height),
        });
        screens.push(ScreenView {
            source: SourceRect {
                x: 0,
                y: half,
                width: fb_width,
                height: half,
            },
            dest: Rect::new(
                left,
                top + screen_height + gap as f32 * scale,
                drawn_width,
                screen_height,
            ),
        });
    } else {
        screens.push(ScreenView {
            source: SourceRect {
                x: 0,
                y: 0,
                width: fb_width,
                height: fb_height,
            },
            dest: Rect::new(left, top, drawn_width, drawn_height),
        });
    }

    Layout {
        touch_screen: dual_screen.then_some(1),
        screens,
        scale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: (u32, u32) = (160, 144);
    const DS: (u32, u32) = (256, 384);

    fn window(width: f32, height: f32) -> Rect {
        Rect::new(0.0, 0.0, width, height)
    }

    #[test]
    fn a_single_screen_is_centred_and_keeps_its_aspect_ratio() {
        // 800×600 fits 160×144 at 4.166; the height is the binding constraint.
        let layout = compute(GB, false, 0, window(800.0, 600.0), ScalingMode::Nearest);
        let dest = layout.screens[0].dest;

        assert_eq!(layout.screens.len(), 1);
        assert!((dest.height - 600.0).abs() < 0.01, "fills the short axis");
        assert!(
            (dest.width / dest.height - 160.0 / 144.0).abs() < 0.001,
            "aspect ratio distorted: {dest:?}"
        );
        assert!(
            (dest.x + dest.width / 2.0 - 400.0).abs() < 0.01,
            "not horizontally centred: {dest:?}"
        );
    }

    #[test]
    fn a_wide_window_letterboxes_on_the_left_and_right() {
        let layout = compute(GB, false, 0, window(1920.0, 300.0), ScalingMode::Nearest);
        let dest = layout.screens[0].dest;
        assert!(dest.x > 0.0, "should be inset horizontally: {dest:?}");
        assert!((dest.y - 0.0).abs() < 0.01, "should fill vertically");
    }

    #[test]
    fn integer_scaling_picks_a_whole_multiple_and_leaves_a_border() {
        // 800/160 = 5.0, 600/144 = 4.16 — so 4×, and 640×576 inside 800×600.
        let layout = compute(
            GB,
            false,
            0,
            window(800.0, 600.0),
            ScalingMode::IntegerNearest,
        );
        let dest = layout.screens[0].dest;

        assert_eq!(layout.scale, 4.0);
        assert_eq!(dest.width, 640.0);
        assert_eq!(dest.height, 576.0);
        assert_eq!(dest.x, 80.0, "centred: (800 - 640) / 2");
        assert_eq!(dest.y, 12.0, "centred: (600 - 576) / 2");
    }

    #[test]
    fn integer_scaling_never_scales_to_zero() {
        // A window smaller than one emulated pixel per screen pixel. Flooring would give 0× and
        // draw nothing at all, which looks exactly like a broken renderer.
        let layout = compute(
            GB,
            false,
            0,
            window(80.0, 72.0),
            ScalingMode::IntegerNearest,
        );
        assert_eq!(layout.scale, 1.0);
        assert_eq!(layout.screens[0].dest.width, 160.0);
    }

    #[test]
    fn a_zero_sized_window_produces_no_layout_rather_than_a_division_by_zero() {
        // A minimised window really does report this, and it arrives before the resize that
        // corrects it.
        assert_eq!(
            compute(GB, false, 0, window(0.0, 0.0), ScalingMode::Nearest),
            Layout::none()
        );
        assert_eq!(
            compute(GB, false, 0, window(100.0, 0.0), ScalingMode::Nearest),
            Layout::none()
        );
    }

    #[test]
    fn an_empty_framebuffer_produces_no_layout() {
        assert_eq!(
            compute((0, 0), false, 0, window(800.0, 600.0), ScalingMode::Nearest),
            Layout::none()
        );
    }

    #[test]
    fn a_single_screen_has_no_touch_input() {
        let layout = compute(GB, false, 0, window(800.0, 600.0), ScalingMode::Nearest);
        assert_eq!(layout.touch_screen, None);
        assert_eq!(layout.touch_at(400.0, 300.0), None);
    }

    // --- dual screen ------------------------------------------------------------------------

    #[test]
    fn a_dual_screen_framebuffer_splits_into_two_halves_with_a_gap() {
        let layout = compute(DS, true, 8, window(512.0, 1000.0), ScalingMode::Nearest);
        assert_eq!(layout.screens.len(), 2);

        let (top, bottom) = (layout.screens[0], layout.screens[1]);
        assert_eq!(
            top.source,
            SourceRect {
                x: 0,
                y: 0,
                width: 256,
                height: 192
            }
        );
        assert_eq!(
            bottom.source,
            SourceRect {
                x: 0,
                y: 192,
                width: 256,
                height: 192
            },
            "the lower screen reads the lower half of the framebuffer"
        );
        assert_eq!(top.dest.width, bottom.dest.width);
        assert_eq!(top.dest.height, bottom.dest.height);

        let gap = bottom.dest.y - (top.dest.y + top.dest.height);
        assert!(
            (gap - 8.0 * layout.scale).abs() < 0.01,
            "the gap must scale with the picture, got {gap} at scale {}",
            layout.scale
        );
    }

    #[test]
    fn the_gap_is_counted_when_fitting_so_the_picture_never_overflows() {
        let available = window(512.0, 400.0);
        let layout = compute(DS, true, 8, available, ScalingMode::Nearest);
        let bounds = layout.bounds().unwrap();
        assert!(
            bounds.y + bounds.height <= available.height + 0.01,
            "the two screens plus the gap must fit: {bounds:?}"
        );
    }

    #[test]
    fn the_lower_screen_is_the_touch_screen() {
        let layout = compute(DS, true, 8, window(512.0, 1000.0), ScalingMode::Nearest);
        assert_eq!(layout.touch_screen, Some(1));

        let bottom = layout.screens[1].dest;
        // Dead centre of the lower screen is the centre of the DS's touch area.
        let touch = layout
            .touch_at(
                bottom.x + bottom.width / 2.0,
                bottom.y + bottom.height / 2.0,
            )
            .expect("the centre of the touch screen is a touch");
        assert_eq!(touch, TouchPoint { x: 128, y: 96 });
    }

    #[test]
    fn a_click_on_the_upper_screen_is_not_a_touch() {
        let layout = compute(DS, true, 8, window(512.0, 1000.0), ScalingMode::Nearest);
        let top = layout.screens[0].dest;
        assert_eq!(
            layout.touch_at(top.x + top.width / 2.0, top.y + top.height / 2.0),
            None,
            "the DS's upper screen is not a touch screen"
        );
    }

    #[test]
    fn a_click_in_the_gap_or_the_letterbox_is_not_a_touch() {
        let layout = compute(DS, true, 8, window(512.0, 1000.0), ScalingMode::Nearest);
        let top = layout.screens[0].dest;
        let bottom = layout.screens[1].dest;
        let mid_gap = (top.y + top.height + bottom.y) / 2.0;
        assert_eq!(layout.touch_at(bottom.x + 10.0, mid_gap), None);
        assert_eq!(layout.touch_at(-5.0, bottom.y + 10.0), None);
        assert_eq!(layout.touch_at(10_000.0, bottom.y + 10.0), None);
    }

    #[test]
    fn touch_coordinates_span_the_whole_screen_without_ever_going_out_of_range() {
        let layout = compute(DS, true, 8, window(700.0, 900.0), ScalingMode::Nearest);
        let bottom = layout.screens[1].dest;

        let top_left = layout.touch_at(bottom.x, bottom.y).unwrap();
        assert_eq!(top_left, TouchPoint { x: 0, y: 0 });

        // The last pixel *inside* the rectangle. A half-open rectangle means the far edge itself
        // is outside, which is what `contains` enforces.
        let inside_bottom_right = layout
            .touch_at(
                bottom.x + bottom.width - 0.01,
                bottom.y + bottom.height - 0.01,
            )
            .unwrap();
        assert_eq!(inside_bottom_right, TouchPoint { x: 255, y: 191 });

        assert_eq!(
            layout.touch_at(bottom.x + bottom.width, bottom.y),
            None,
            "the far edge is outside a half-open rectangle"
        );
    }

    #[test]
    fn every_position_inside_the_touch_screen_maps_into_range() {
        // A sweep, because an off-by-one in the clamp would only show at one pixel and would be
        // an out-of-bounds touch coordinate reaching the emulated hardware.
        let layout = compute(DS, true, 8, window(733.0, 941.0), ScalingMode::Nearest);
        let dest = layout.screens[1].dest;
        for i in 0..500 {
            let fraction = i as f32 / 500.0;
            let point = layout
                .touch_at(
                    dest.x + dest.width * fraction,
                    dest.y + dest.height * fraction,
                )
                .expect("inside the rectangle");
            assert!(point.x < 256, "x out of range: {point:?}");
            assert!(point.y < 192, "y out of range: {point:?}");
        }
    }

    #[test]
    fn an_odd_height_framebuffer_is_not_treated_as_dual_screen() {
        // 385 rows cannot be two equal screens. Halving it would silently drop a row of pixels
        // from one of them, which is the sort of thing that shows up as a mysterious one-pixel
        // offset months later.
        let layout = compute(
            (256, 385),
            true,
            8,
            window(512.0, 1000.0),
            ScalingMode::Nearest,
        );
        assert_eq!(layout.screens.len(), 1);
        assert_eq!(layout.touch_screen, None);
        assert_eq!(layout.screens[0].source.height, 385);
    }

    #[test]
    fn a_layout_inside_an_offset_viewport_stays_inside_it() {
        // The picture is drawn below a menu bar, so the available area does not start at the
        // origin. Getting this wrong puts the game under the chrome.
        let available = Rect::new(0.0, 32.0, 800.0, 568.0);
        let layout = compute(GB, false, 0, available, ScalingMode::Nearest);
        let dest = layout.screens[0].dest;
        assert!(dest.y >= 32.0, "drawn over the menu bar: {dest:?}");
        assert!(dest.y + dest.height <= 600.01, "overflows: {dest:?}");
    }

    #[test]
    fn bounds_covers_both_screens_and_the_gap_between_them() {
        let layout = compute(DS, true, 8, window(512.0, 1000.0), ScalingMode::Nearest);
        let bounds = layout.bounds().unwrap();
        let top = layout.screens[0].dest;
        let bottom = layout.screens[1].dest;
        assert_eq!(bounds.y, top.y);
        assert!((bounds.y + bounds.height - (bottom.y + bottom.height)).abs() < 0.01);
    }

    #[test]
    fn linear_and_nearest_place_the_picture_identically() {
        // The two differ only in how the GPU samples the texture. If they ever placed it
        // differently, switching filter modes would move the picture, which would be a bug in
        // this function rather than a rendering choice.
        let a = compute(GB, false, 0, window(801.0, 599.0), ScalingMode::Nearest);
        let b = compute(GB, false, 0, window(801.0, 599.0), ScalingMode::Linear);
        assert_eq!(a, b);
    }
}
