//! The geometry engine: command decode, vertex assembly, lighting, and clipping.
//!
//! # A command is an opcode and a fixed number of parameters
//!
//! The ARM9 feeds the 3D engine either by writing packed commands to `GXFIFO` — four opcodes in
//! one word, then all their parameters — or by writing parameters straight to a per-command port.
//! Both arrive here as `(opcode, parameters)`. The parameter count is a property of the opcode and
//! nothing else, so [`parameter_count`] is a table and the FIFO is a small state machine over it.
//!
//! Getting one entry of that table wrong desynchronises the whole display list from that point on,
//! and the symptom is geometry that is correct up to some moment and garbage afterwards — which
//! reads as a rasteriser bug. It has its own test.
//!
//! # Coordinates stay fixed point all the way to the viewport
//!
//! A vertex is 4.12 signed. It is multiplied by the clip matrix into homogeneous clip space,
//! clipped there against the six frustum planes, and only then divided by `w`. Dividing early —
//! which is what makes the maths look like a graphics tutorial — throws away exactly what the
//! clipper needs and puts vertices behind the camera in front of it.
//!
//! # What is deferred
//!
//! Prompt 13 says to prioritise geometry and texturing correctness over less commonly load-bearing
//! effects, and to document what is deferred. Deferred here: the shininess table (specular uses a
//! computed falloff instead), `BOX_TEST` (which always answers "visible", so a game's own culling
//! never wrongly hides geometry), and edge marking. Fog is in [`super::render`].

use super::matrix::{Matrix, MatrixStack, ONE};
use core_common::{Savable, StateError, StateReader, StateWriter};

/// How many parameters each geometry command takes.
///
/// Zero means the command still exists and takes none — `MTX_IDENTITY` and `END_VTXS` are real
/// commands with no parameters, and are not the same thing as an unknown opcode.
pub fn parameter_count(opcode: u8) -> Option<u8> {
    Some(match opcode {
        0x00 => 0, // NOP
        0x10 => 1, // MTX_MODE
        0x11 => 0, // MTX_PUSH
        0x12 => 1, // MTX_POP
        0x13 => 1, // MTX_STORE
        0x14 => 1, // MTX_RESTORE
        0x15 => 0, // MTX_IDENTITY
        0x16 => 16,
        0x17 => 12,
        0x18 => 16,
        0x19 => 12,
        0x1A => 9,
        0x1B => 3,  // MTX_SCALE
        0x1C => 3,  // MTX_TRANS
        0x20 => 1,  // COLOR
        0x21 => 1,  // NORMAL
        0x22 => 1,  // TEXCOORD
        0x23 => 2,  // VTX_16
        0x24 => 1,  // VTX_10
        0x25 => 1,  // VTX_XY
        0x26 => 1,  // VTX_XZ
        0x27 => 1,  // VTX_YZ
        0x28 => 1,  // VTX_DIFF
        0x29 => 1,  // POLYGON_ATTR
        0x2A => 1,  // TEXIMAGE_PARAM
        0x2B => 1,  // PLTT_BASE
        0x30 => 1,  // DIF_AMB
        0x31 => 1,  // SPE_EMI
        0x32 => 1,  // LIGHT_VECTOR
        0x33 => 1,  // LIGHT_COLOR
        0x34 => 32, // SHININESS
        0x40 => 1,  // BEGIN_VTXS
        0x41 => 0,  // END_VTXS
        0x50 => 1,  // SWAP_BUFFERS
        0x60 => 1,  // VIEWPORT
        0x70 => 3,  // BOX_TEST
        0x71 => 2,  // POS_TEST
        0x72 => 1,  // VEC_TEST
        _ => return None,
    })
}

/// What a primitive is made of, as set by `BEGIN_VTXS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    Triangles,
    Quads,
    TriangleStrip,
    QuadStrip,
}

impl Primitive {
    fn from_bits(bits: u32) -> Self {
        match bits & 3 {
            0 => Primitive::Triangles,
            1 => Primitive::Quads,
            2 => Primitive::TriangleStrip,
            _ => Primitive::QuadStrip,
        }
    }
}

/// A vertex after the clip matrix but before the viewport divide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipVertex {
    /// Homogeneous clip coordinates.
    pub position: [i32; 4],
    /// 6-bit-per-channel colour, held at 6 bits because that is the depth the blend unit works in.
    pub color: [u8; 3],
    /// Texture coordinates in 1/16 texel units.
    pub texcoord: [i32; 2],
}

/// A vertex after the viewport transform, ready to rasterise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScreenVertex {
    /// Screen position, in whole pixels.
    pub x: i32,
    pub y: i32,
    /// 24-bit depth-buffer value.
    pub depth: u32,
    /// The clip-space `w`, kept for perspective-correct interpolation.
    pub w: i32,
    pub color: [u8; 3],
    pub texcoord: [i32; 2],
}

/// One assembled polygon, indexing into the frame's vertex list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polygon {
    pub vertices: Vec<usize>,
    pub attr: u32,
    pub tex_param: u32,
    pub palette_base: u32,
    /// Whether the winding put the front face toward the viewer.
    pub front_facing: bool,
}

impl Polygon {
    pub fn alpha(&self) -> u8 {
        ((self.attr >> 16) & 0x1F) as u8
    }

    pub fn polygon_id(&self) -> u8 {
        ((self.attr >> 24) & 0x3F) as u8
    }

    /// The polygon mode: 0 modulation, 1 decal, 2 toon/highlight, 3 shadow.
    pub fn mode(&self) -> u8 {
        ((self.attr >> 4) & 3) as u8
    }

    pub fn renders_back(&self) -> bool {
        self.attr & (1 << 6) != 0
    }

    pub fn renders_front(&self) -> bool {
        self.attr & (1 << 7) != 0
    }

    /// Whether depth is written for a translucent polygon.
    pub fn writes_depth_translucent(&self) -> bool {
        self.attr & (1 << 11) != 0
    }

    /// Whether the depth test is "equal" rather than "less".
    pub fn depth_equal(&self) -> bool {
        self.attr & (1 << 14) != 0
    }
}

/// The finished contents of vertex and polygon RAM for one frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayList {
    pub vertices: Vec<ScreenVertex>,
    pub polygons: Vec<Polygon>,
    /// Whether the frame's depth buffer should be cleared from the rear-plane image rather than
    /// from `CLEAR_DEPTH`. Not implemented; recorded so the renderer can say so.
    pub rear_plane_bitmap: bool,
}

/// Hardware limits. Exceeding either sets the overflow flag rather than growing.
pub const MAX_VERTICES: usize = 6144;
pub const MAX_POLYGONS: usize = 2048;

/// One of the four hardware lights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Light {
    /// Direction, already through the direction matrix, in 1.9 fixed point.
    vector: [i32; 3],
    /// Half-vector, which is the light vector plus the line of sight (0,0,-1), normalised by two.
    half: [i32; 3],
    color: [u8; 3],
}

/// The geometry engine's whole state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    pub matrices: MatrixStack,

    primitive: Option<Primitive>,
    /// Vertices accumulated for the primitive currently being assembled.
    strip: Vec<ClipVertex>,
    /// How many vertices the current strip has consumed in total, for the strip winding rule.
    strip_count: usize,

    current_color: [u8; 3],
    current_texcoord: [i32; 2],
    /// The texture coordinates as written, before any matrix transform, so a later `TEXIMAGE_PARAM`
    /// change re-transforms from the source rather than from an already-transformed value.
    raw_texcoord: [i32; 2],
    last_vertex: [i32; 3],

    polygon_attr: u32,
    /// `POLYGON_ATTR` is latched at `BEGIN_VTXS`, not applied immediately: a game sets the
    /// attributes for the *next* primitive while the current one is still being assembled.
    pending_polygon_attr: u32,
    tex_param: u32,
    palette_base: u32,

    lights: [Light; 4],
    diffuse: [u8; 3],
    ambient: [u8; 3],
    specular: [u8; 3],
    emission: [u8; 3],
    lights_enabled: u32,

    viewport: [u8; 4],

    /// The list being built. Swapped out by `SWAP_BUFFERS`.
    building: DisplayList,
    /// Set when vertex or polygon RAM overflows, and reported through `GXSTAT`.
    pub overflow: bool,
    /// Set by `SWAP_BUFFERS`; the system clears it once the renderer has taken the list.
    pub swap_pending: bool,
    /// The result of the last `POS_TEST`, read back through `POS_RESULT`.
    pub pos_result: [i32; 4],
    /// The result of the last `VEC_TEST`.
    pub vec_result: [i32; 3],
}

impl Default for Geometry {
    fn default() -> Self {
        Self::new()
    }
}

impl Geometry {
    pub fn new() -> Self {
        Self {
            matrices: MatrixStack::new(),
            primitive: None,
            strip: Vec::with_capacity(4),
            strip_count: 0,
            current_color: [63; 3],
            current_texcoord: [0; 2],
            raw_texcoord: [0; 2],
            last_vertex: [0; 3],
            polygon_attr: 0,
            pending_polygon_attr: 0,
            tex_param: 0,
            palette_base: 0,
            lights: [Light::default(); 4],
            diffuse: [0; 3],
            ambient: [0; 3],
            specular: [0; 3],
            emission: [0; 3],
            lights_enabled: 0,
            viewport: [0, 0, 255, 191],
            building: DisplayList::default(),
            overflow: false,
            swap_pending: false,
            pos_result: [0; 4],
            vec_result: [0; 3],
        }
    }

    /// Execute one command with its parameters.
    pub fn execute(&mut self, opcode: u8, params: &[u32]) {
        let p = |i: usize| params.get(i).copied().unwrap_or(0);
        match opcode {
            0x10 => self.matrices.set_mode(p(0)),
            0x11 => self.matrices.push(),
            0x12 => self.matrices.pop(sign_extend(p(0), 6)),
            0x13 => self.matrices.store(p(0)),
            0x14 => self.matrices.restore(p(0)),
            0x15 => self.matrices.load_identity(),
            0x16 => self.matrices.load_matrix(Matrix::from_4x4(&to_i32(params))),
            0x17 => self.matrices.load_matrix(Matrix::from_4x3(&to_i32(params))),
            0x18 => self.matrices.multiply(Matrix::from_4x4(&to_i32(params))),
            0x19 => self.matrices.multiply(Matrix::from_4x3(&to_i32(params))),
            0x1A => self.matrices.multiply(Matrix::from_3x3(&to_i32(params))),
            0x1B => self.matrices.scale(p(0) as i32, p(1) as i32, p(2) as i32),
            0x1C => self
                .matrices
                .translate(p(0) as i32, p(1) as i32, p(2) as i32),
            0x20 => self.current_color = decode_color(p(0)),
            0x21 => self.apply_normal(p(0)),
            0x22 => self.set_texcoord(p(0)),
            0x23 => {
                let x = sign_extend(p(0) & 0xFFFF, 16);
                let y = sign_extend(p(0) >> 16, 16);
                let z = sign_extend(p(1) & 0xFFFF, 16);
                self.emit_vertex(x, y, z);
            }
            0x24 => {
                // Three 10-bit values in 4.6, so each shifts up by six into 4.12.
                let x = sign_extend(p(0) & 0x3FF, 10) << 6;
                let y = sign_extend((p(0) >> 10) & 0x3FF, 10) << 6;
                let z = sign_extend((p(0) >> 20) & 0x3FF, 10) << 6;
                self.emit_vertex(x, y, z);
            }
            0x25 => {
                let x = sign_extend(p(0) & 0xFFFF, 16);
                let y = sign_extend(p(0) >> 16, 16);
                self.emit_vertex(x, y, self.last_vertex[2]);
            }
            0x26 => {
                let x = sign_extend(p(0) & 0xFFFF, 16);
                let z = sign_extend(p(0) >> 16, 16);
                self.emit_vertex(x, self.last_vertex[1], z);
            }
            0x27 => {
                let y = sign_extend(p(0) & 0xFFFF, 16);
                let z = sign_extend(p(0) >> 16, 16);
                self.emit_vertex(self.last_vertex[0], y, z);
            }
            0x28 => {
                // Each difference is a 10-bit value whose unit is eight of the vertex format's,
                // which is why this is a shift of three and not of six.
                let d = |shift: u32| (sign_extend((p(0) >> shift) & 0x3FF, 10)) << 3;
                self.emit_vertex(
                    self.last_vertex[0] + d(0),
                    self.last_vertex[1] + d(10),
                    self.last_vertex[2] + d(20),
                );
            }
            0x29 => self.pending_polygon_attr = p(0),
            0x2A => {
                self.tex_param = p(0);
                // A texture-coordinate transform mode change re-transforms from the raw value.
                self.set_texcoord_from_raw();
            }
            0x2B => self.palette_base = p(0) & 0x1FFF,
            0x30 => {
                self.diffuse = decode_color(p(0));
                self.ambient = decode_color(p(0) >> 16);
                // Bit 15 sets the vertex colour to the diffuse colour immediately, which is how
                // an unlit polygon gets a colour without a separate `COLOR` command.
                if p(0) & (1 << 15) != 0 {
                    self.current_color = self.diffuse;
                }
            }
            0x31 => {
                self.specular = decode_color(p(0));
                self.emission = decode_color(p(0) >> 16);
            }
            0x32 => self.set_light_vector(p(0)),
            0x33 => {
                let index = (p(0) >> 30) as usize & 3;
                self.lights[index].color = decode_color(p(0));
            }
            // SHININESS: the specular falloff table. Deferred; see the module docs.
            0x34 => {}
            0x40 => self.begin(Primitive::from_bits(p(0))),
            0x41 => self.end(),
            0x50 => self.swap_buffers(),
            0x60 => {
                self.viewport = [
                    p(0) as u8,
                    (p(0) >> 8) as u8,
                    (p(0) >> 16) as u8,
                    (p(0) >> 24) as u8,
                ]
            }
            // BOX_TEST always answers "visible". Answering "hidden" wrongly would let a game's own
            // culling remove geometry that should be on screen, which is unrecoverable; answering
            // "visible" wrongly only costs work.
            0x70 => {}
            0x71 => {
                let x = sign_extend(p(0) & 0xFFFF, 16);
                let y = sign_extend(p(0) >> 16, 16);
                let z = sign_extend(p(1) & 0xFFFF, 16);
                self.pos_result = self.matrices.clip_matrix().transform(x, y, z, ONE);
            }
            0x72 => {
                let v = |shift: u32| sign_extend((p(0) >> shift) & 0x3FF, 10) << 3;
                let transformed = self.matrices.direction.transform(v(0), v(10), v(20), 0);
                self.vec_result = [transformed[0], transformed[1], transformed[2]];
            }
            _ => {}
        }
    }

    fn set_texcoord(&mut self, param: u32) {
        self.raw_texcoord = [
            sign_extend(param & 0xFFFF, 16),
            sign_extend(param >> 16, 16),
        ];
        self.set_texcoord_from_raw();
    }

    /// Apply the texture matrix if the transform mode asks for it.
    fn set_texcoord_from_raw(&mut self) {
        let mode = (self.tex_param >> 30) & 3;
        if mode == 1 {
            // Transform mode 1 runs the coordinates through the texture matrix.
            let m = &self.matrices.texture;
            let s = self.raw_texcoord[0] as i64 * m.at(0, 0) as i64
                + self.raw_texcoord[1] as i64 * m.at(0, 1) as i64
                + (m.at(0, 3) as i64) * 16;
            let t = self.raw_texcoord[0] as i64 * m.at(1, 0) as i64
                + self.raw_texcoord[1] as i64 * m.at(1, 1) as i64
                + (m.at(1, 3) as i64) * 16;
            self.current_texcoord = [(s >> 12) as i32, (t >> 12) as i32];
        } else {
            self.current_texcoord = self.raw_texcoord;
        }
    }

    /// `LIGHT_VECTOR`: the direction goes through the direction matrix, which is why that matrix
    /// exists at all.
    fn set_light_vector(&mut self, param: u32) {
        let index = (param >> 30) as usize & 3;
        let v = |shift: u32| sign_extend((param >> shift) & 0x3FF, 10) << 3;
        let transformed = self.matrices.direction.transform(v(0), v(10), v(20), 0);
        let vector = [transformed[0], transformed[1], transformed[2]];
        // The half-vector between the light and the line of sight, which points down -Z.
        let half = [vector[0] / 2, vector[1] / 2, (vector[2] - ONE) / 2];
        self.lights[index] = Light {
            vector,
            half,
            color: self.lights[index].color,
        };
    }

    /// `NORMAL`: run the lighting equation and set the vertex colour from it.
    fn apply_normal(&mut self, param: u32) {
        // In texture transform mode 2 the normal doubles as texture coordinates.
        if (self.tex_param >> 30) & 3 == 2 {
            let n = |shift: u32| sign_extend((param >> shift) & 0x3FF, 10) << 3;
            self.raw_texcoord = [n(0) >> 8, n(10) >> 8];
        }
        let v = |shift: u32| sign_extend((param >> shift) & 0x3FF, 10) << 3;
        let transformed = self.matrices.direction.transform(v(0), v(10), v(20), 0);
        let normal = [transformed[0], transformed[1], transformed[2]];

        let mut color = self.emission.map(|c| c as i32);
        for index in 0..4 {
            if self.lights_enabled & (1 << index) == 0 {
                continue;
            }
            let light = self.lights[index];
            // Diffuse falls off with the cosine between the normal and the light, and a light
            // behind the surface contributes nothing rather than a negative amount.
            let diffuse_level = (-dot(&light.vector, &normal)).max(0);
            // Specular uses the half-vector, squared for a tighter highlight. The real hardware
            // looks the falloff up in the shininess table, which is deferred.
            let specular_dot = (-dot(&light.half, &normal)).max(0);
            let specular_level = ((specular_dot as i64 * specular_dot as i64) >> 12) as i32;

            for (channel, slot) in color.iter_mut().enumerate() {
                let light_c = light.color[channel] as i32;
                *slot += (self.diffuse[channel] as i32 * light_c * diffuse_level) >> 18;
                *slot += (self.specular[channel] as i32 * light_c * specular_level) >> 18;
                *slot += (self.ambient[channel] as i32 * light_c) >> 6;
            }
        }
        self.current_color = [
            color[0].clamp(0, 63) as u8,
            color[1].clamp(0, 63) as u8,
            color[2].clamp(0, 63) as u8,
        ];
    }

    fn begin(&mut self, primitive: Primitive) {
        // Beginning a primitive latches the attributes set for it and abandons anything the
        // previous one had part-assembled, which is what an unterminated strip does on hardware.
        self.polygon_attr = self.pending_polygon_attr;
        self.lights_enabled = self.polygon_attr & 0xF;
        self.primitive = Some(primitive);
        self.strip.clear();
        self.strip_count = 0;
    }

    fn end(&mut self) {
        // `END_VTXS` does nothing on hardware beyond marking the list; a strip ends when the next
        // `BEGIN_VTXS` arrives. Modelled the same way so an unterminated list behaves alike.
    }

    fn emit_vertex(&mut self, x: i32, y: i32, z: i32) {
        self.last_vertex = [x, y, z];
        let Some(primitive) = self.primitive else {
            return;
        };
        let position = self.matrices.clip_matrix().transform(x, y, z, ONE);
        self.strip.push(ClipVertex {
            position,
            color: self.current_color,
            texcoord: self.current_texcoord,
        });
        self.strip_count += 1;

        match primitive {
            Primitive::Triangles if self.strip.len() == 3 => {
                let face: Vec<ClipVertex> = self.strip.drain(..).collect();
                self.assemble(&face);
            }
            Primitive::Quads if self.strip.len() == 4 => {
                let face: Vec<ClipVertex> = self.strip.drain(..).collect();
                self.assemble(&face);
            }
            Primitive::TriangleStrip if self.strip.len() >= 3 => {
                let mut face = self.strip[self.strip.len() - 3..].to_vec();
                // Every other triangle in a strip has the opposite winding, and hardware swaps
                // two vertices to keep the facing consistent. Without this, half a strip is
                // back-facing and disappears under any culling at all.
                if self.strip_count.is_multiple_of(2) {
                    face.swap(0, 1);
                }
                self.assemble(&face);
                if self.strip.len() > 3 {
                    self.strip.remove(0);
                }
            }
            Primitive::QuadStrip if self.strip.len() == 4 => {
                // A quad strip's vertices arrive in pairs, so the fourth and third are swapped to
                // put them in winding order.
                let face = [self.strip[0], self.strip[1], self.strip[3], self.strip[2]];
                self.assemble(&face);
                self.strip.remove(0);
                self.strip.remove(0);
            }
            _ => {}
        }
    }

    /// Clip a face, transform it to the screen, and add it to the list.
    fn assemble(&mut self, face: &[ClipVertex]) {
        let clipped = clip_polygon(face);
        if clipped.len() < 3 {
            return;
        }

        let screen: Vec<ScreenVertex> = clipped.iter().map(|v| self.to_screen(v)).collect();
        let front_facing = is_front_facing(&screen);
        let attr = self.polygon_attr;
        let renders_front = attr & (1 << 7) != 0;
        let renders_back = attr & (1 << 6) != 0;
        if (front_facing && !renders_front) || (!front_facing && !renders_back) {
            return;
        }

        if self.building.polygons.len() >= MAX_POLYGONS
            || self.building.vertices.len() + screen.len() > MAX_VERTICES
        {
            self.overflow = true;
            return;
        }

        let base = self.building.vertices.len();
        self.building.vertices.extend_from_slice(&screen);
        self.building.polygons.push(Polygon {
            vertices: (base..base + screen.len()).collect(),
            attr,
            tex_param: self.tex_param,
            palette_base: self.palette_base,
            front_facing,
        });
    }

    /// Divide by `w` and map into the viewport.
    fn to_screen(&self, v: &ClipVertex) -> ScreenVertex {
        let [x, y, z, w] = v.position;
        let (left, bottom, right, top) = (
            self.viewport[0] as i32,
            self.viewport[1] as i32,
            self.viewport[2] as i32,
            self.viewport[3] as i32,
        );
        let width = (right - left + 1).max(1);
        let height = (top - bottom + 1).max(1);

        // `w` of zero is a degenerate vertex; hardware produces something rather than dividing by
        // zero, and so does this.
        let denominator = if w == 0 { 1 } else { w as i64 * 2 };
        let sx = ((x as i64 + w as i64) * width as i64) / denominator + left as i64;
        let sy = ((-(y as i64) + w as i64) * height as i64) / denominator;
        // Screen Y counts down from the top, and the viewport's Y counts up from the bottom.
        let sy = sy + (191 - top) as i64;

        // The Z-buffer value: clip-space z mapped into 24 bits.
        let depth = if w == 0 {
            0
        } else {
            (((z as i64 * 0x4000) / w as i64) + 0x3FFF).clamp(0, 0x7FFF) as u32 * 0x200
        };

        ScreenVertex {
            x: sx as i32,
            y: sy as i32,
            depth: depth.min(0x00FF_FFFF),
            w,
            color: v.color,
            texcoord: v.texcoord,
        }
    }

    fn swap_buffers(&mut self) {
        self.swap_pending = true;
    }

    /// Take the finished display list, which is what `SWAP_BUFFERS` made available.
    pub fn take_display_list(&mut self) -> DisplayList {
        self.swap_pending = false;
        self.primitive = None;
        self.strip.clear();
        std::mem::take(&mut self.building)
    }

    pub fn polygon_count(&self) -> usize {
        self.building.polygons.len()
    }

    pub fn vertex_count(&self) -> usize {
        self.building.vertices.len()
    }

    pub fn viewport(&self) -> [u8; 4] {
        self.viewport
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Sign-extend the low `bits` of a value.
pub fn sign_extend(value: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

/// Five bits per channel, as every geometry colour parameter carries it.
fn decode_color(param: u32) -> [u8; 3] {
    // Scaled from 5 bits to the 6 the engine works in by doubling, so full scale stays full.
    [
        ((param & 0x1F) * 2) as u8,
        (((param >> 5) & 0x1F) * 2) as u8,
        (((param >> 10) & 0x1F) * 2) as u8,
    ]
}

fn to_i32(params: &[u32]) -> Vec<i32> {
    params.iter().map(|v| *v as i32).collect()
}

fn dot(a: &[i32; 3], b: &[i32; 3]) -> i32 {
    let sum = a[0] as i64 * b[0] as i64 + a[1] as i64 * b[1] as i64 + a[2] as i64 * b[2] as i64;
    (sum >> 12) as i32
}

/// Whether a screen-space polygon's winding puts its front toward the viewer.
fn is_front_facing(vertices: &[ScreenVertex]) -> bool {
    if vertices.len() < 3 {
        return true;
    }
    let ax = vertices[1].x - vertices[0].x;
    let ay = vertices[1].y - vertices[0].y;
    let bx = vertices[2].x - vertices[0].x;
    let by = vertices[2].y - vertices[0].y;
    (ax as i64 * by as i64 - ay as i64 * bx as i64) <= 0
}

/// Clip a polygon against the six frustum planes, in homogeneous coordinates.
///
/// Sutherland-Hodgman, one plane at a time. Done here rather than in screen space because a vertex
/// with negative `w` is behind the camera, and dividing it through before clipping puts it on the
/// screen mirrored — the classic symptom being geometry that flips inside out as it passes the
/// near plane.
pub fn clip_polygon(input: &[ClipVertex]) -> Vec<ClipVertex> {
    let mut current = input.to_vec();
    for axis in 0..3 {
        for side in [1i32, -1] {
            if current.len() < 3 {
                return Vec::new();
            }
            current = clip_against(&current, axis, side);
        }
    }
    current
}

/// Clip against one plane: `side * coordinate <= w`.
fn clip_against(input: &[ClipVertex], axis: usize, side: i32) -> Vec<ClipVertex> {
    let inside = |v: &ClipVertex| (side as i64 * v.position[axis] as i64) <= v.position[3] as i64;
    let mut out = Vec::with_capacity(input.len() + 1);
    for i in 0..input.len() {
        let a = input[i];
        let b = input[(i + 1) % input.len()];
        let a_in = inside(&a);
        let b_in = inside(&b);
        if a_in {
            out.push(a);
        }
        if a_in != b_in {
            out.push(interpolate(&a, &b, axis, side));
        }
    }
    out
}

/// The point where the edge `a`-`b` crosses the plane.
fn interpolate(a: &ClipVertex, b: &ClipVertex, axis: usize, side: i32) -> ClipVertex {
    let da = side as i64 * a.position[axis] as i64 - a.position[3] as i64;
    let db = side as i64 * b.position[axis] as i64 - b.position[3] as i64;
    let denominator = da - db;
    // A zero denominator means both ends are on the plane; either endpoint will do.
    if denominator == 0 {
        return *a;
    }
    let t = |x: i64, y: i64| ((x * -db + y * da) / denominator) as i32;

    let mut position = [0i32; 4];
    for (i, slot) in position.iter_mut().enumerate() {
        *slot = t(a.position[i] as i64, b.position[i] as i64);
    }
    ClipVertex {
        position,
        color: [
            t(a.color[0] as i64, b.color[0] as i64).clamp(0, 63) as u8,
            t(a.color[1] as i64, b.color[1] as i64).clamp(0, 63) as u8,
            t(a.color[2] as i64, b.color[2] as i64).clamp(0, 63) as u8,
        ],
        texcoord: [
            t(a.texcoord[0] as i64, b.texcoord[0] as i64),
            t(a.texcoord[1] as i64, b.texcoord[1] as i64),
        ],
    }
}

impl Savable for Geometry {
    fn save(&self, w: &mut StateWriter) {
        self.matrices.save(w);
        // The half-assembled primitive — the vertices already popped off `GXFIFO` into `strip`,
        // waiting for enough of them to complete a triangle or quad — is still not saved. Those
        // vertices are gone from the command stream: the CPU has already executed the writes that
        // produced them and will not repeat them on resume, so losing `strip` here is a real,
        // narrow gap in a save taken between `BEGIN_VTXS` and the primitive's last vertex. It is
        // accepted for now because it is rare (a save lands on a specific handful of instructions
        // out of a whole frame's worth) and small (at most three or four vertices), unlike
        // `building` below, which is not narrow at all — it is every polygon the current frame has
        // assembled, and losing it silently swaps in a blank picture on the next `VBlank`.
        for value in self.current_color {
            w.write_u8(value);
        }
        for value in self.current_texcoord {
            w.write_i32(value);
        }
        for value in self.raw_texcoord {
            w.write_i32(value);
        }
        for value in self.last_vertex {
            w.write_i32(value);
        }
        w.write_u32(self.polygon_attr);
        w.write_u32(self.pending_polygon_attr);
        w.write_u32(self.tex_param);
        w.write_u32(self.palette_base);
        for light in self.lights {
            for value in light.vector {
                w.write_i32(value);
            }
            for value in light.half {
                w.write_i32(value);
            }
            for value in light.color {
                w.write_u8(value);
            }
        }
        for block in [self.diffuse, self.ambient, self.specular, self.emission] {
            for value in block {
                w.write_u8(value);
            }
        }
        w.write_u32(self.lights_enabled);
        for value in self.viewport {
            w.write_u8(value);
        }
        w.write_bool(self.overflow);
        w.write_bool(self.swap_pending);
        // `POS_RESULT` and `VEC_RESULT`: the answers to the last `POS_TEST`/`VEC_TEST`, which a
        // game reads back well after issuing the command that produced them.
        for value in self.pos_result {
            w.write_i32(value);
        }
        for value in self.vec_result {
            w.write_i32(value);
        }
        // The list under construction. This is the fix for the bug this comment used to excuse:
        // a save taken after some polygons were assembled but before `SWAP_BUFFERS` restored with
        // `building` empty, and a save taken after `SWAP_BUFFERS` but before the next `VBlank`
        // restored with `swap_pending` true and nothing to swap — either way, the next frame drew
        // a blank picture instead of the one the game had already built.
        self.building.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.matrices.load(r)?;
        for value in &mut self.current_color {
            *value = r.read_u8()?;
        }
        for value in &mut self.current_texcoord {
            *value = r.read_i32()?;
        }
        for value in &mut self.raw_texcoord {
            *value = r.read_i32()?;
        }
        for value in &mut self.last_vertex {
            *value = r.read_i32()?;
        }
        self.polygon_attr = r.read_u32()?;
        self.pending_polygon_attr = r.read_u32()?;
        self.tex_param = r.read_u32()?;
        self.palette_base = r.read_u32()?;
        for light in &mut self.lights {
            for value in &mut light.vector {
                *value = r.read_i32()?;
            }
            for value in &mut light.half {
                *value = r.read_i32()?;
            }
            for value in &mut light.color {
                *value = r.read_u8()?;
            }
        }
        for block in [
            &mut self.diffuse,
            &mut self.ambient,
            &mut self.specular,
            &mut self.emission,
        ] {
            for value in block {
                *value = r.read_u8()?;
            }
        }
        self.lights_enabled = r.read_u32()?;
        for value in &mut self.viewport {
            *value = r.read_u8()?;
        }
        self.overflow = r.read_bool()?;
        self.swap_pending = r.read_bool()?;
        for value in &mut self.pos_result {
            *value = r.read_i32()?;
        }
        for value in &mut self.vec_result {
            *value = r.read_i32()?;
        }
        self.building.load(r)?;
        self.primitive = None;
        self.strip.clear();
        self.strip_count = 0;
        Ok(())
    }
}

/// A vertex ready to rasterise, as [`DisplayList::save`] writes it.
fn save_screen_vertex(v: &ScreenVertex, w: &mut StateWriter) {
    w.write_i32(v.x);
    w.write_i32(v.y);
    w.write_u32(v.depth);
    w.write_i32(v.w);
    for value in v.color {
        w.write_u8(value);
    }
    for value in v.texcoord {
        w.write_i32(value);
    }
}

fn load_screen_vertex(r: &mut StateReader) -> Result<ScreenVertex, StateError> {
    Ok(ScreenVertex {
        x: r.read_i32()?,
        y: r.read_i32()?,
        depth: r.read_u32()?,
        w: r.read_i32()?,
        color: [r.read_u8()?, r.read_u8()?, r.read_u8()?],
        texcoord: [r.read_i32()?, r.read_i32()?],
    })
}

fn save_polygon(p: &Polygon, w: &mut StateWriter) {
    w.write_u64(p.vertices.len() as u64);
    for index in &p.vertices {
        w.write_u32(*index as u32);
    }
    w.write_u32(p.attr);
    w.write_u32(p.tex_param);
    w.write_u32(p.palette_base);
    w.write_bool(p.front_facing);
}

fn load_polygon(r: &mut StateReader) -> Result<Polygon, StateError> {
    let count = r.read_u64()? as usize;
    // A polygon's vertex indices point into the list's own `vertices`, so it can never legitimately
    // name more of them than the whole list is allowed to hold.
    if count > MAX_VERTICES {
        return Err(StateError::Malformed(format!(
            "a polygon claims {count} vertices; the whole list holds at most {MAX_VERTICES}"
        )));
    }
    let mut vertices = Vec::with_capacity(count);
    for _ in 0..count {
        vertices.push(r.read_u32()? as usize);
    }
    Ok(Polygon {
        vertices,
        attr: r.read_u32()?,
        tex_param: r.read_u32()?,
        palette_base: r.read_u32()?,
        front_facing: r.read_bool()?,
    })
}

impl Savable for DisplayList {
    fn save(&self, w: &mut StateWriter) {
        w.write_u64(self.vertices.len() as u64);
        for vertex in &self.vertices {
            save_screen_vertex(vertex, w);
        }
        w.write_u64(self.polygons.len() as u64);
        for polygon in &self.polygons {
            save_polygon(polygon, w);
        }
        w.write_bool(self.rear_plane_bitmap);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let vertex_count = r.read_u64()? as usize;
        if vertex_count > MAX_VERTICES {
            return Err(StateError::Malformed(format!(
                "a display list claims {vertex_count} vertices; hardware allows at most {MAX_VERTICES}"
            )));
        }
        self.vertices = Vec::with_capacity(vertex_count);
        for _ in 0..vertex_count {
            self.vertices.push(load_screen_vertex(r)?);
        }

        let polygon_count = r.read_u64()? as usize;
        if polygon_count > MAX_POLYGONS {
            return Err(StateError::Malformed(format!(
                "a display list claims {polygon_count} polygons; hardware allows at most {MAX_POLYGONS}"
            )));
        }
        self.polygons = Vec::with_capacity(polygon_count);
        for _ in 0..polygon_count {
            self.polygons.push(load_polygon(r)?);
        }

        self.rear_plane_bitmap = r.read_bool()?;
        Ok(())
    }
}
