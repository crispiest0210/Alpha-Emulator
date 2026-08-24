//! The software rasteriser: scan conversion, the depth buffer, and texture sampling.
//!
//! # Fixed point, and why not floats
//!
//! Every interpolation here is `i64` fixed point. Floats would be easier to read and are what a
//! graphics tutorial would use, and they were rejected for one reason: the framebuffer this
//! produces is hashed by the accuracy harness and compared across machines, and float rounding is
//! not guaranteed identical across targets. A renderer whose output depends on the host is a
//! renderer whose determinism tests are meaningless.
//!
//! # Perspective correction is not optional
//!
//! Attributes are interpolated as `attribute / w` alongside `1 / w`, and divided at each pixel.
//! Interpolating them directly is visibly wrong the moment a textured polygon is seen at an angle
//! — the texture swims, in the way early-90s console 3D did — and it is wrong in a way that looks
//! like a texture-coordinate bug rather than like missing perspective correction.
//!
//! # What is deferred
//!
//! Prompt 13 asks for geometry and texturing correctness first and for the rest to be documented.
//! Deferred: **fog**, **edge marking**, **anti-aliasing**, **shadow polygons** (mode 3, which
//! render as ordinary polygons), and the **toon and highlight tables** (mode 2 falls back to
//! modulation). Each is a visual refinement on top of a picture that is otherwise the right shape,
//! which is the order prompt 13 asks for.

use super::geometry::{DisplayList, Polygon, ScreenVertex};
use crate::video::{SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::vram::{Vram, VramSpace};
use core_common::{Savable, StateError, StateReader, StateWriter};

pub const PIXELS: usize = (SCREEN_WIDTH * SCREEN_HEIGHT) as usize;

/// Fractional bits used for the reciprocal of `w`.
const INV_W_SHIFT: u32 = 22;

/// One frame of 3D output, as engine A composites it.
pub struct Framebuffer3d {
    /// 15-bit BGR per pixel.
    pub color: Box<[u16]>,
    /// 0-31 per pixel; zero is a pixel the 3D engine did not draw.
    pub alpha: Box<[u8]>,
    depth: Box<[u32]>,
    polygon_id: Box<[u8]>,
}

impl Default for Framebuffer3d {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer3d {
    pub fn new() -> Self {
        Self {
            color: vec![0; PIXELS].into_boxed_slice(),
            alpha: vec![0; PIXELS].into_boxed_slice(),
            depth: vec![0x00FF_FFFF; PIXELS].into_boxed_slice(),
            polygon_id: vec![0; PIXELS].into_boxed_slice(),
        }
    }

    #[inline]
    pub fn color_at(&self, x: u32, y: u32) -> u16 {
        self.color[(y * SCREEN_WIDTH + x) as usize]
    }

    #[inline]
    pub fn alpha_at(&self, x: u32, y: u32) -> u8 {
        self.alpha[(y * SCREEN_WIDTH + x) as usize]
    }

    /// Reset to the clear colour and depth, which is what a frame starts from.
    pub fn clear(&mut self, clear_color: u32, clear_depth: u32) {
        let color = (clear_color & 0x7FFF) as u16;
        let alpha = ((clear_color >> 16) & 0x1F) as u8;
        let id = ((clear_color >> 24) & 0x3F) as u8;
        // The clear depth register is 15 bits and expands into the 24-bit buffer with its low
        // bits set, so a polygon at exactly the clear depth is in front of it rather than
        // z-fighting with it.
        let depth = ((clear_depth & 0x7FFF) * 0x200 + 0x1FF).min(0x00FF_FFFF);
        self.color.fill(color);
        self.alpha.fill(alpha);
        self.depth.fill(depth);
        self.polygon_id.fill(id);
    }
}

/// The picture engine A composites from, saved in full.
///
/// Unlike the 2D backgrounds and sprites, this is not something a fresh render regenerates every
/// frame: [`super::Gpu3d::on_vblank`] only rewrites it when a swap is pending, so a game that goes
/// several frames — or several minutes, sitting on a menu — without submitting new geometry keeps
/// showing exactly this buffer. A save state taken during that stretch has to carry the picture
/// itself, or reloading it shows whatever pixels happened to be in memory rather than what the
/// screen actually held.
///
/// That costs roughly 384 KiB per save — more than any other single piece of this crate's state —
/// but the alternative is a save-state format that answers "what is on screen right now" wrong.
/// `crates/system-nds/src/vram.rs` already carries a comparable amount of raw framebuffer-adjacent
/// data for the same reason: a value nothing else can reconstruct has to be written down.
impl Savable for Framebuffer3d {
    fn save(&self, w: &mut StateWriter) {
        for value in self.color.iter() {
            w.write_u16(*value);
        }
        for value in self.alpha.iter() {
            w.write_u8(*value);
        }
        for value in self.depth.iter() {
            w.write_u32(*value);
        }
        for value in self.polygon_id.iter() {
            w.write_u8(*value);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for value in self.color.iter_mut() {
            *value = r.read_u16()?;
        }
        for value in self.alpha.iter_mut() {
            *value = r.read_u8()?;
        }
        for value in self.depth.iter_mut() {
            *value = r.read_u32()?;
        }
        for value in self.polygon_id.iter_mut() {
            *value = r.read_u8()?;
        }
        Ok(())
    }
}

/// Everything interpolated across a polygon, premultiplied by `1/w` where perspective demands it.
#[derive(Debug, Clone, Copy, Default)]
struct Attributes {
    depth: i64,
    inv_w: i64,
    /// Colour and texture coordinates, each already divided by `w`.
    color: [i64; 3],
    texcoord: [i64; 2],
}

impl Attributes {
    fn of(v: &ScreenVertex) -> Self {
        let inv_w = (1i64 << INV_W_SHIFT) / (v.w.max(1) as i64);
        Self {
            depth: v.depth as i64,
            inv_w,
            color: [
                v.color[0] as i64 * inv_w,
                v.color[1] as i64 * inv_w,
                v.color[2] as i64 * inv_w,
            ],
            texcoord: [v.texcoord[0] as i64 * inv_w, v.texcoord[1] as i64 * inv_w],
        }
    }

    /// Linear blend, with `num/den` as the position between `self` and `other`.
    fn lerp(&self, other: &Self, num: i64, den: i64) -> Self {
        let mix = |a: i64, b: i64| a + (b - a) * num / den;
        Self {
            depth: mix(self.depth, other.depth),
            inv_w: mix(self.inv_w, other.inv_w),
            color: [
                mix(self.color[0], other.color[0]),
                mix(self.color[1], other.color[1]),
                mix(self.color[2], other.color[2]),
            ],
            texcoord: [
                mix(self.texcoord[0], other.texcoord[0]),
                mix(self.texcoord[1], other.texcoord[1]),
            ],
        }
    }

    /// Undo the `1/w` premultiplication.
    fn resolve(&self) -> ([u8; 3], [i32; 2]) {
        let w = self.inv_w.max(1);
        (
            [
                (self.color[0] / w).clamp(0, 63) as u8,
                (self.color[1] / w).clamp(0, 63) as u8,
                (self.color[2] / w).clamp(0, 63) as u8,
            ],
            [(self.texcoord[0] / w) as i32, (self.texcoord[1] / w) as i32],
        )
    }
}

/// Where one polygon edge crosses a scanline.
#[derive(Debug, Clone, Copy)]
struct Crossing {
    x: i32,
    attributes: Attributes,
}

/// Render a display list.
pub fn render(
    list: &DisplayList,
    vram: &Vram,
    clear_color: u32,
    clear_depth: u32,
    out: &mut Framebuffer3d,
) {
    out.clear(clear_color, clear_depth);
    for polygon in &list.polygons {
        render_polygon(polygon, list, vram, out);
    }
}

fn render_polygon(polygon: &Polygon, list: &DisplayList, vram: &Vram, out: &mut Framebuffer3d) {
    let vertices: Vec<&ScreenVertex> = polygon
        .vertices
        .iter()
        .filter_map(|i| list.vertices.get(*i))
        .collect();
    if vertices.len() < 3 {
        return;
    }
    let attributes: Vec<Attributes> = vertices.iter().map(|v| Attributes::of(v)).collect();

    let top = vertices.iter().map(|v| v.y).min().unwrap_or(0).max(0);
    let bottom = vertices
        .iter()
        .map(|v| v.y)
        .max()
        .unwrap_or(0)
        .min(SCREEN_HEIGHT as i32 - 1);

    let mut crossings: Vec<Crossing> = Vec::with_capacity(4);
    for y in top..=bottom {
        crossings.clear();
        for i in 0..vertices.len() {
            let a = vertices[i];
            let b = vertices[(i + 1) % vertices.len()];
            // A horizontal edge contributes no crossing; its endpoints are already produced by
            // the two edges either side of it, and counting it produces a duplicate span.
            if a.y == b.y {
                continue;
            }
            let (top_v, bottom_v, top_a, bottom_a) = if a.y < b.y {
                (a, b, attributes[i], attributes[(i + 1) % vertices.len()])
            } else {
                (b, a, attributes[(i + 1) % vertices.len()], attributes[i])
            };
            if y < top_v.y || y >= bottom_v.y {
                continue;
            }
            let den = (bottom_v.y - top_v.y) as i64;
            let num = (y - top_v.y) as i64;
            crossings.push(Crossing {
                x: top_v.x + ((bottom_v.x - top_v.x) as i64 * num / den) as i32,
                attributes: top_a.lerp(&bottom_a, num, den),
            });
        }
        if crossings.len() < 2 {
            continue;
        }
        crossings.sort_by_key(|c| c.x);
        let left = crossings[0];
        let right = crossings[crossings.len() - 1];
        draw_span(polygon, &left, &right, y, vram, out);
    }
}

fn draw_span(
    polygon: &Polygon,
    left: &Crossing,
    right: &Crossing,
    y: i32,
    vram: &Vram,
    out: &mut Framebuffer3d,
) {
    let span = (right.x - left.x).max(0) as i64;
    let start = left.x.max(0);
    let end = right.x.min(SCREEN_WIDTH as i32 - 1);
    let alpha = polygon.alpha();
    // Alpha 0 means fully opaque on the DS, not fully transparent. Reading it the other way makes
    // every ordinary polygon invisible, which is the single most confusing way for a 3D engine to
    // fail — the geometry is all there and none of it is on screen.
    let polygon_alpha = if alpha == 0 { 31 } else { alpha };
    let translucent = polygon_alpha < 31;

    for x in start..=end {
        let num = (x - left.x) as i64;
        let attributes = if span == 0 {
            left.attributes
        } else {
            left.attributes.lerp(&right.attributes, num, span)
        };
        let index = (y as u32 * SCREEN_WIDTH + x as u32) as usize;

        let depth = attributes.depth.clamp(0, 0x00FF_FFFF) as u32;
        let passes = if polygon.depth_equal() {
            depth.abs_diff(out.depth[index]) <= 0x200
        } else {
            depth < out.depth[index]
        };
        if !passes {
            continue;
        }

        let (vertex_color, texcoord) = attributes.resolve();
        let Some((color, texel_alpha)) =
            shade(polygon, vertex_color, texcoord, polygon_alpha, vram)
        else {
            continue;
        };
        if texel_alpha == 0 {
            continue;
        }

        if texel_alpha >= 31 {
            out.color[index] = color;
            out.alpha[index] = 31;
            out.depth[index] = depth;
            out.polygon_id[index] = polygon.polygon_id();
        } else {
            // A translucent polygon does not draw over another fragment of the same polygon ID,
            // which is how a game stops a transparent model blending with itself.
            if out.polygon_id[index] == polygon.polygon_id() && out.alpha[index] < 31 {
                continue;
            }
            out.color[index] = blend(color, out.color[index], texel_alpha);
            out.alpha[index] = out.alpha[index].max(texel_alpha);
            out.polygon_id[index] = polygon.polygon_id();
            if !translucent || polygon.writes_depth_translucent() {
                out.depth[index] = depth;
            }
        }
    }
}

/// Combine the vertex colour with the texture, if any.
///
/// Returns `None` when the pixel should not be drawn at all.
fn shade(
    polygon: &Polygon,
    vertex_color: [u8; 3],
    texcoord: [i32; 2],
    polygon_alpha: u8,
    vram: &Vram,
) -> Option<(u16, u8)> {
    let format = (polygon.tex_param >> 26) & 7;
    if format == 0 {
        return Some((pack(vertex_color), polygon_alpha));
    }
    let (texel, texel_alpha) = sample_texture(polygon, texcoord, vram)?;

    // Decal mode replaces the vertex colour where the texture is opaque rather than modulating.
    // Modes 2 and 3 fall back to modulation; see the module docs.
    let color = if polygon.mode() == 1 {
        if texel_alpha == 0 {
            pack(vertex_color)
        } else {
            texel
        }
    } else {
        modulate(texel, vertex_color)
    };
    let alpha = if polygon.mode() == 1 {
        polygon_alpha
    } else {
        ((texel_alpha as u32 * polygon_alpha as u32) / 31) as u8
    };
    Some((color, alpha))
}

/// Fetch one texel, honouring repeat, flip, and the seven texture formats.
fn sample_texture(polygon: &Polygon, texcoord: [i32; 2], vram: &Vram) -> Option<(u16, u8)> {
    let param = polygon.tex_param;
    let base = (param & 0xFFFF) << 3;
    let width = 8u32 << ((param >> 20) & 7);
    let height = 8u32 << ((param >> 23) & 7);
    let format = (param >> 26) & 7;
    let color0_transparent = param & (1 << 29) != 0;

    let s = wrap(
        texcoord[0] >> 4,
        width,
        param & (1 << 16) != 0,
        param & (1 << 18) != 0,
    );
    let t = wrap(
        texcoord[1] >> 4,
        height,
        param & (1 << 17) != 0,
        param & (1 << 19) != 0,
    );
    let index = t * width + s;

    // The palette base is in 16-byte units for every format except the 4-colour one, which uses
    // 8. Using one unit for both puts every 4-colour texture's palette twice as far away as it
    // is, and the symptom is a texture drawn in somebody else's colours.
    let palette_base = if format == 2 {
        polygon.palette_base << 3
    } else {
        polygon.palette_base << 4
    };
    let palette = |entry: u32| vram.read16(VramSpace::TexturePalette, palette_base + entry * 2);

    Some(match format {
        // A3I5: five index bits and three of alpha.
        1 => {
            let byte = vram.read8(VramSpace::Texture, base + index) as u32;
            let alpha = ((byte >> 5) & 7) * 4 + ((byte >> 5) & 7) / 2;
            (palette(byte & 0x1F), alpha as u8)
        }
        // Four colours, two bits per texel.
        2 => {
            let byte = vram.read8(VramSpace::Texture, base + index / 4) as u32;
            let entry = (byte >> ((index % 4) * 2)) & 3;
            (
                palette(entry),
                opaque_unless(entry == 0 && color0_transparent),
            )
        }
        3 => {
            let byte = vram.read8(VramSpace::Texture, base + index / 2) as u32;
            let entry = if index.is_multiple_of(2) {
                byte & 0xF
            } else {
                byte >> 4
            };
            (
                palette(entry),
                opaque_unless(entry == 0 && color0_transparent),
            )
        }
        4 => {
            let entry = vram.read8(VramSpace::Texture, base + index) as u32;
            (
                palette(entry),
                opaque_unless(entry == 0 && color0_transparent),
            )
        }
        5 => compressed_texel(base, s, t, width, palette_base, vram),
        // A5I3: three index bits and five of alpha, the other way round from format 1.
        6 => {
            let byte = vram.read8(VramSpace::Texture, base + index) as u32;
            (palette(byte & 7), ((byte >> 3) & 0x1F) as u8)
        }
        _ => {
            let value = vram.read16(VramSpace::Texture, base + index * 2);
            (value & 0x7FFF, opaque_unless(value & 0x8000 == 0))
        }
    })
}

/// One texel of a 4x4-block compressed texture.
///
/// The block's two-bit indices and its palette *pointer* live in two different places: the
/// indices follow the texture's base address, and the pointers live in the other half of the same
/// 128 KiB slot. That split is the whole trick of the format, and looking for the pointer next to
/// the data produces textures that are the right shape in entirely the wrong colours.
fn compressed_texel(
    base: u32,
    s: u32,
    t: u32,
    width: u32,
    palette_base: u32,
    vram: &Vram,
) -> (u16, u8) {
    let blocks_wide = (width / 4).max(1);
    let block = (t / 4) * blocks_wide + (s / 4);
    let data = vram.read32(VramSpace::Texture, base + block * 4);
    let entry = (data >> (((t % 4) * 4 + (s % 4)) * 2)) & 3;

    // The pointer table sits in the upper half of whichever 128 KiB slot the data is in.
    let slot = base & !0x1_FFFF;
    let extra_slot = if slot < 0x4_0000 { 0x2_0000 } else { 0x4_0000 };
    let pointer_addr = extra_slot + ((base & 0x1_FFFF) >> 1) + block * 2;
    let pointer = vram.read16(VramSpace::Texture, pointer_addr) as u32;
    let mode = (pointer >> 14) & 3;
    let palette = palette_base + (pointer & 0x3FFF) * 4;
    let color = |i: u32| vram.read16(VramSpace::TexturePalette, palette + i * 2);

    match (mode, entry) {
        (_, 0) => (color(0), 31),
        (_, 1) => (color(1), 31),
        (0, 2) | (2, 2) => (color(2), 31),
        (1, 2) => (average(color(0), color(1), 1, 1), 31),
        (3, 2) => (average(color(0), color(1), 5, 3), 31),
        (0, _) => (0, 0),
        (1, _) => (0, 0),
        (2, _) => (color(3), 31),
        _ => (average(color(0), color(1), 3, 5), 31),
    }
}

/// A weighted blend of two 15-bit colours, per channel.
fn average(a: u16, b: u16, wa: u32, wb: u32) -> u16 {
    let mut out = 0u16;
    for shift in [0, 5, 10] {
        let ca = ((a >> shift) & 0x1F) as u32;
        let cb = ((b >> shift) & 0x1F) as u32;
        out |= (((ca * wa + cb * wb) / (wa + wb)).min(31) as u16) << shift;
    }
    out
}

fn opaque_unless(transparent: bool) -> u8 {
    if transparent {
        0
    } else {
        31
    }
}

/// Repeat, flip, or clamp one texture coordinate.
fn wrap(value: i32, size: u32, repeat: bool, flip: bool) -> u32 {
    let size = size.max(1) as i32;
    if !repeat {
        return value.clamp(0, size - 1) as u32;
    }
    let period = if flip { size * 2 } else { size };
    let folded = value.rem_euclid(period);
    if flip && folded >= size {
        (period - 1 - folded) as u32
    } else {
        folded as u32
    }
}

fn pack(color: [u8; 3]) -> u16 {
    ((color[0] as u16 >> 1) & 0x1F)
        | (((color[1] as u16 >> 1) & 0x1F) << 5)
        | (((color[2] as u16 >> 1) & 0x1F) << 10)
}

/// Multiply a texel by a 6-bit vertex colour, which is what texture mode 0 does.
fn modulate(texel: u16, vertex: [u8; 3]) -> u16 {
    let mut out = 0u16;
    for (i, shift) in [0, 5, 10].into_iter().enumerate() {
        let t = ((texel >> shift) & 0x1F) as u32;
        // The vertex colour is 6 bits and the texel 5, so the product is over 63 rather than 31.
        let value = (t * (vertex[i] as u32 + 1)) >> 6;
        out |= (value.min(31) as u16) << shift;
    }
    out
}

/// Blend a translucent fragment over what is already there.
fn blend(top: u16, bottom: u16, alpha: u8) -> u16 {
    let a = alpha as u32 + 1;
    let mut out = 0u16;
    for shift in [0, 5, 10] {
        let t = ((top >> shift) & 0x1F) as u32;
        let b = ((bottom >> shift) & 0x1F) as u32;
        out |= (((t * a + b * (32 - a)) / 32).min(31) as u16) << shift;
    }
    out
}
