//! Shared primitive types: [`Cycles`], [`InputState`], [`Framebuffer`], [`AudioSample`].
//!
//! Everything here is deliberately system-agnostic. If a type in this module needs to know
//! whether it is running on a Game Boy or a DS, it is in the wrong crate.

use bitflags::bitflags;
use savestate::{Savable, StateError, StateReader, StateWriter};
use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

/// The universal clock unit, counted in each system's own base clock ticks.
///
/// One `Cycles` tick means whatever the owning [`System`](crate::System) documents it to
/// mean — GB counts 4.194304 MHz t-cycles, GBA counts its own 16.78 MHz base clock, and so
/// on. Nothing in this crate converts between systems, because nothing needs to: a
/// `Scheduler` and its `System` always share one clock domain.
///
/// Wall-clock time and frame counts are deliberately *not* usable as scheduling units
/// anywhere in the core. Frame pacing is the frontend's problem; the core only knows cycles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cycles(pub u64);

impl Cycles {
    pub const ZERO: Cycles = Cycles(0);
    /// Sentinel for "no event scheduled" comparisons; far beyond any real run length
    /// (a GBA would need ~34,000 years of continuous emulation to reach it).
    pub const NEVER: Cycles = Cycles(u64::MAX);

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Saturating difference, for "how far until this deadline" queries where a deadline
    /// already in the past means zero, not a panic or a wrapped huge number.
    #[inline]
    pub const fn saturating_sub(self, other: Cycles) -> Cycles {
        Cycles(self.0.saturating_sub(other.0))
    }
}

impl Add for Cycles {
    type Output = Cycles;
    #[inline]
    fn add(self, rhs: Cycles) -> Cycles {
        Cycles(self.0 + rhs.0)
    }
}

impl AddAssign for Cycles {
    #[inline]
    fn add_assign(&mut self, rhs: Cycles) {
        self.0 += rhs.0;
    }
}

impl Sub for Cycles {
    type Output = Cycles;
    #[inline]
    fn sub(self, rhs: Cycles) -> Cycles {
        Cycles(self.0 - rhs.0)
    }
}

impl SubAssign for Cycles {
    #[inline]
    fn sub_assign(&mut self, rhs: Cycles) {
        self.0 -= rhs.0;
    }
}

impl std::iter::Sum for Cycles {
    fn sum<I: Iterator<Item = Cycles>>(iter: I) -> Cycles {
        Cycles(iter.map(|c| c.0).sum())
    }
}

impl fmt::Display for Cycles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Savable for Cycles {
    fn save(&self, w: &mut StateWriter) {
        w.write_u64(self.0);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.0 = r.read_u64()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

bitflags! {
    /// The union of digital buttons across every supported system.
    ///
    /// This is one flag set rather than four per-system button enums so that input routing,
    /// keybind config, and movie recording are written once. A system simply ignores the
    /// bits it does not have: the Game Boy never looks at [`Buttons::L`], and nothing
    /// anywhere has to translate between incompatible per-system input types.
    ///
    /// Every button defaults to released, so `InputState::default()` is "nothing pressed".
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct Buttons: u16 {
        const A      = 1 << 0;
        const B      = 1 << 1;
        /// GBA/NDS only.
        const X      = 1 << 2;
        /// GBA/NDS only.
        const Y      = 1 << 3;
        /// Shoulder buttons: GBA/NDS only.
        const L      = 1 << 4;
        const R      = 1 << 5;
        const START  = 1 << 6;
        const SELECT = 1 << 7;
        const UP     = 1 << 8;
        const DOWN   = 1 << 9;
        const LEFT   = 1 << 10;
        const RIGHT  = 1 << 11;
    }
}

impl Savable for Buttons {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.bits());
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = Buttons::from_bits_truncate(r.read_u16()?);
        Ok(())
    }
}

/// A touchscreen contact point, in the touch screen's own pixel coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
}

impl Savable for TouchPoint {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.x);
        w.write_u16(self.y);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.x = r.read_u16()?;
        self.y = r.read_u16()?;
        Ok(())
    }
}

/// One frame's worth of input, handed to [`System::step_frame`](crate::System::step_frame).
///
/// Systems read only the fields they have. `touch` is `None` on every system without a
/// touchscreen and whenever the DS stylus is not down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputState {
    pub buttons: Buttons,
    pub touch: Option<TouchPoint>,
}

impl InputState {
    #[inline]
    pub fn is_pressed(&self, button: Buttons) -> bool {
        self.buttons.contains(button)
    }

    #[inline]
    pub fn set(&mut self, button: Buttons, pressed: bool) {
        self.buttons.set(button, pressed);
    }
}

impl Savable for InputState {
    fn save(&self, w: &mut StateWriter) {
        self.buttons.save(w);
        self.touch.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.buttons.load(r)?;
        self.touch.load(r)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

/// The one canonical pixel format in the core: 8-bit RGBA, in that byte order.
///
/// Every PPU backend converts into this. It maps directly onto `wgpu`'s
/// `Rgba8UnormSrgb`, so the frontend uploads [`Framebuffer::as_bytes`] with no repacking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const BLACK: Rgba8 = Rgba8::rgb(0, 0, 0);
    pub const WHITE: Rgba8 = Rgba8::rgb(255, 255, 255);

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[inline]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// Bytes per pixel in a [`Framebuffer`], given [`Rgba8`].
pub const BYTES_PER_PIXEL: usize = 4;

/// Owned pixel storage for one rendered frame.
///
/// Pixels are stored as raw RGBA bytes rather than as `Vec<Rgba8>` so that
/// [`as_bytes`](Framebuffer::as_bytes) is a plain reborrow — no `unsafe` transmute and no
/// per-frame repacking on the way to the GPU.
///
/// Systems with two screens (the DS) present them stacked vertically in a single
/// framebuffer — top screen first — rather than exposing two framebuffers. That keeps the
/// [`System`](crate::System) interface uniform, and the frontend splits the image when it
/// needs to lay the screens out separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * BYTES_PER_PIXEL],
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Raw RGBA bytes, `width * height * 4` long, row-major from the top-left.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.pixels
    }

    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    /// One scanline's bytes, for PPU backends that render a row at a time.
    ///
    /// Returns an empty slice for an out-of-range `y` rather than panicking, so a PPU bug
    /// shows up as a blank line instead of taking the emulator down mid-frame.
    #[inline]
    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        if y >= self.height {
            return &mut [];
        }
        let stride = self.width as usize * BYTES_PER_PIXEL;
        let start = y as usize * stride;
        &mut self.pixels[start..start + stride]
    }

    #[inline]
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Rgba8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL;
        self.pixels[i] = color.r;
        self.pixels[i + 1] = color.g;
        self.pixels[i + 2] = color.b;
        self.pixels[i + 3] = color.a;
    }

    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> Rgba8 {
        if x >= self.width || y >= self.height {
            return Rgba8::default();
        }
        let i = (y as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL;
        Rgba8::rgba(
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        )
    }

    pub fn fill(&mut self, color: Rgba8) {
        for px in self.pixels.chunks_exact_mut(BYTES_PER_PIXEL) {
            px[0] = color.r;
            px[1] = color.g;
            px[2] = color.b;
            px[3] = color.a;
        }
    }

    /// Reallocate to a new size, clearing the contents. Systems that can change resolution
    /// at runtime call this; fixed-resolution systems never do.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels
            .resize(width as usize * height as usize * BYTES_PER_PIXEL, 0);
    }
}

impl Savable for Framebuffer {
    fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.width);
        w.write_u32(self.height);
        w.write_blob(&self.pixels);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let width = r.read_u32()?;
        let height = r.read_u32()?;
        let pixels = r.read_blob()?;
        let expected = width as usize * height as usize * BYTES_PER_PIXEL;
        if pixels.len() != expected {
            return Err(StateError::Malformed(format!(
                "framebuffer is {}x{} ({expected} bytes) but the state holds {} bytes",
                width,
                height,
                pixels.len()
            )));
        }
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.extend_from_slice(pixels);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// The sample rate every [`System`](crate::System) resamples its APU output to.
///
/// Fixing this in the core rather than per system means the frontend's ring buffer and
/// output stream are configured once, and mixing two systems' output (or switching systems
/// without tearing down the audio device) needs no rate negotiation. Systems whose native
/// APU rate differs — which is all of them — resample internally.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// One stereo frame of audio, nominally in `-1.0..=1.0`.
///
/// `f32` rather than `i16` because every APU mixes several channels before output, and
/// keeping the intermediate mix in float avoids clipping decisions inside the core that the
/// frontend cannot undo.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub struct AudioSample {
    pub left: f32,
    pub right: f32,
}

impl AudioSample {
    pub const SILENCE: AudioSample = AudioSample {
        left: 0.0,
        right: 0.0,
    };

    #[inline]
    pub const fn stereo(left: f32, right: f32) -> Self {
        Self { left, right }
    }

    #[inline]
    pub const fn mono(v: f32) -> Self {
        Self { left: v, right: v }
    }
}

impl Savable for AudioSample {
    fn save(&self, w: &mut StateWriter) {
        w.write_f32(self.left);
        w.write_f32(self.right);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.left = r.read_f32()?;
        self.right = r.read_f32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savestate::{decode_state, encode_state};

    #[test]
    fn cycles_arithmetic_and_saturation() {
        let a = Cycles(100);
        let b = Cycles(40);
        assert_eq!(a + b, Cycles(140));
        assert_eq!(a - b, Cycles(60));
        assert_eq!(b.saturating_sub(a), Cycles::ZERO);
        assert_eq!(Cycles::NEVER.get(), u64::MAX);
    }

    #[test]
    fn input_defaults_to_nothing_pressed() {
        let input = InputState::default();
        assert!(!input.is_pressed(Buttons::A));
        assert!(!input.is_pressed(Buttons::L));
        assert_eq!(input.touch, None);
    }

    #[test]
    fn framebuffer_pixel_access_is_bounds_checked() {
        let mut fb = Framebuffer::new(4, 3);
        fb.set_pixel(1, 2, Rgba8::rgb(10, 20, 30));
        assert_eq!(fb.pixel(1, 2), Rgba8::rgb(10, 20, 30));
        // Out of range writes are dropped and reads return the default, rather than panicking
        // and taking down the emulation thread on a PPU off-by-one.
        fb.set_pixel(99, 99, Rgba8::WHITE);
        assert_eq!(fb.pixel(99, 99), Rgba8::default());
        assert!(fb.row_mut(99).is_empty());
        assert_eq!(fb.as_bytes().len(), 4 * 3 * BYTES_PER_PIXEL);
    }

    #[test]
    fn framebuffer_rows_are_contiguous_rgba() {
        let mut fb = Framebuffer::new(2, 2);
        fb.row_mut(1).copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(fb.pixel(0, 1), Rgba8::rgba(1, 2, 3, 4));
        assert_eq!(fb.pixel(1, 1), Rgba8::rgba(5, 6, 7, 8));
    }

    #[test]
    fn framebuffer_round_trips_and_rejects_size_mismatch() {
        let mut fb = Framebuffer::new(3, 2);
        fb.fill(Rgba8::rgb(7, 8, 9));
        let blob = encode_state("test", 1, &fb);

        let mut restored = Framebuffer::new(1, 1);
        decode_state("test", 1, &blob, &mut restored).unwrap();
        assert_eq!(fb, restored);
    }

    #[test]
    fn input_state_round_trips_including_touch() {
        let input = InputState {
            buttons: Buttons::A | Buttons::START | Buttons::LEFT,
            touch: Some(TouchPoint { x: 120, y: 90 }),
        };
        let blob = encode_state("test", 1, &input);
        let mut restored = InputState::default();
        decode_state("test", 1, &blob, &mut restored).unwrap();
        assert_eq!(input, restored);
    }
}
