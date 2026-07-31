//! The 3D engine's four matrix stacks.
//!
//! # Fixed point, not floating point
//!
//! Every matrix element is 20.12 signed fixed point, and multiplication is done in 64 bits and
//! shifted back down by 12. That is what the hardware does, and it is not interchangeable with
//! `f32`: a DS matrix has 32 bits of precision spread deliberately across a range software knows
//! about, and floats would move the rounding of every vertex by a fraction of a pixel in a way
//! that accumulates across a matrix stack thirty deep. Using floats here would also make the
//! save-state format depend on the host's rounding mode.
//!
//! # Four stacks with four different depths
//!
//! - **Projection**, one level deep. Push and pop toggle between two matrices.
//! - **Position** and **direction**, 31 levels, pushed and popped *together* by one pointer. The
//!   direction matrix is the position matrix without its translation, and it exists so a normal
//!   can be transformed without picking up the translation a position would.
//! - **Texture**, one level.
//!
//! Which stack a command acts on is set by `MTX_MODE`, and mode 2 is the trap: it multiplies the
//! *position and direction matrices both*, while modes 0, 1, and 3 touch exactly one. A renderer
//! that treats mode 2 as "position only" lights everything from the wrong direction, which looks
//! like a lighting bug rather than like a matrix bug.
//!
//! # Overflow does not panic
//!
//! Push past the top or pop past the bottom and hardware sets an error bit in `GXSTAT` and carries
//! on. Software does it — a stack overflow in a game's display list is a bug it has already
//! shipped around — so the stack saturates and records the error rather than trapping.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// Fractional bits in the fixed-point format the geometry engine works in.
pub const FRACTION_BITS: u32 = 12;
pub const ONE: i32 = 1 << FRACTION_BITS;

/// A 4x4 matrix in column-major order, which is the order the hardware's load commands supply.
///
/// Column-major matters: `MTX_LOAD_4x4` sends sixteen parameters that fill columns, so storing
/// them row-major transposes every matrix a game loads and produces geometry that is mirrored
/// through the diagonal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matrix(pub [i32; 16]);

impl Default for Matrix {
    fn default() -> Self {
        Self::identity()
    }
}

impl Matrix {
    pub const fn identity() -> Self {
        let mut m = [0i32; 16];
        m[0] = ONE;
        m[5] = ONE;
        m[10] = ONE;
        m[15] = ONE;
        Self(m)
    }

    /// Element at `(row, column)`.
    #[inline]
    pub fn at(&self, row: usize, column: usize) -> i32 {
        self.0[column * 4 + row]
    }

    /// The plain matrix product `self * other`.
    ///
    /// The order matters and is easy to invert. `MTX_MULT_*` computes `current * parameter`, so
    /// the *most recently multiplied* matrix is the one nearest the vertex and therefore applies
    /// first — translate then scale scales the vertex before moving it. Getting this backwards
    /// produces a scene that is correctly shaped and wrongly placed, with every object's
    /// transform composed in reverse, and it survives casual inspection because a single
    /// transform looks fine either way.
    pub fn multiply(&self, other: &Matrix) -> Matrix {
        let mut out = [0i32; 16];
        for column in 0..4 {
            for row in 0..4 {
                let mut sum = 0i64;
                for k in 0..4 {
                    sum += self.at(row, k) as i64 * other.at(k, column) as i64;
                }
                out[column * 4 + row] = (sum >> FRACTION_BITS) as i32;
            }
        }
        Matrix(out)
    }

    /// Transform a point, returning the four clip-space components.
    ///
    /// `w` is returned rather than divided out here: the geometry engine clips in homogeneous
    /// coordinates and only divides at the viewport transform, and dividing early throws away the
    /// information clipping needs.
    pub fn transform(&self, x: i32, y: i32, z: i32, w: i32) -> [i32; 4] {
        let mut out = [0i32; 4];
        for (row, slot) in out.iter_mut().enumerate() {
            let sum = self.at(row, 0) as i64 * x as i64
                + self.at(row, 1) as i64 * y as i64
                + self.at(row, 2) as i64 * z as i64
                + self.at(row, 3) as i64 * w as i64;
            *slot = (sum >> FRACTION_BITS) as i32;
        }
        out
    }

    /// Multiply in a translation, which is what `MTX_TRANS` does.
    pub fn translated(&self, x: i32, y: i32, z: i32) -> Matrix {
        let mut m = Matrix::identity();
        m.0[12] = x;
        m.0[13] = y;
        m.0[14] = z;
        self.multiply(&m)
    }

    /// Multiply in a scale, which is what `MTX_SCALE` does.
    ///
    /// Note that `MTX_SCALE` applies to the position matrix even in mode 2 — the direction matrix
    /// is deliberately left alone, because scaling a normal would change its length.
    pub fn scaled(&self, x: i32, y: i32, z: i32) -> Matrix {
        let mut m = Matrix::identity();
        m.0[0] = x;
        m.0[5] = y;
        m.0[10] = z;
        self.multiply(&m)
    }

    /// Build a matrix from a 4x3 parameter list: three columns of three, then the translation.
    pub fn from_4x3(values: &[i32]) -> Matrix {
        let mut m = Matrix::identity();
        for column in 0..4 {
            for row in 0..3 {
                m.0[column * 4 + row] = values.get(column * 3 + row).copied().unwrap_or(0);
            }
        }
        m
    }

    /// Build a matrix from a 3x3 parameter list, leaving the translation at zero.
    pub fn from_3x3(values: &[i32]) -> Matrix {
        let mut m = Matrix::identity();
        for column in 0..3 {
            for row in 0..3 {
                m.0[column * 4 + row] = values.get(column * 3 + row).copied().unwrap_or(0);
            }
        }
        m
    }

    pub fn from_4x4(values: &[i32]) -> Matrix {
        let mut m = [0i32; 16];
        for (slot, value) in m.iter_mut().zip(values) {
            *slot = *value;
        }
        Matrix(m)
    }
}

/// Which matrix `MTX_*` commands act on, as set by `MTX_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixMode {
    Projection,
    /// The position matrix alone.
    Position,
    /// The position *and* direction matrices together. See the module docs.
    PositionAndDirection,
    Texture,
}

impl MatrixMode {
    pub fn from_bits(bits: u32) -> Self {
        match bits & 3 {
            0 => MatrixMode::Projection,
            1 => MatrixMode::Position,
            2 => MatrixMode::PositionAndDirection,
            _ => MatrixMode::Texture,
        }
    }
}

/// The position/direction stack depth. Hardware has 32 entries and reports an error at 31.
const POSITION_DEPTH: usize = 32;

/// All four matrices and their stacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixStack {
    pub mode: MatrixMode,
    pub projection: Matrix,
    pub position: Matrix,
    pub direction: Matrix,
    pub texture: Matrix,

    projection_saved: Matrix,
    texture_saved: Matrix,
    position_stack: Box<[Matrix; POSITION_DEPTH]>,
    direction_stack: Box<[Matrix; POSITION_DEPTH]>,
    /// The shared stack pointer for the position and direction stacks.
    pointer: u8,
    /// Whether the projection stack's single level is occupied.
    projection_pushed: bool,
    /// Set by a push past the top or a pop past the bottom, and reported through `GXSTAT`.
    pub overflow: bool,
    /// The clip matrix — projection times position — cached because every vertex needs it and it
    /// changes far less often than vertices arrive.
    clip: Matrix,
    clip_valid: bool,
}

impl Default for MatrixStack {
    fn default() -> Self {
        Self::new()
    }
}

impl MatrixStack {
    pub fn new() -> Self {
        Self {
            mode: MatrixMode::Projection,
            projection: Matrix::identity(),
            position: Matrix::identity(),
            direction: Matrix::identity(),
            texture: Matrix::identity(),
            projection_saved: Matrix::identity(),
            texture_saved: Matrix::identity(),
            position_stack: Box::new([Matrix::identity(); POSITION_DEPTH]),
            direction_stack: Box::new([Matrix::identity(); POSITION_DEPTH]),
            pointer: 0,
            projection_pushed: false,
            overflow: false,
            clip: Matrix::identity(),
            clip_valid: true,
        }
    }

    pub fn set_mode(&mut self, bits: u32) {
        self.mode = MatrixMode::from_bits(bits);
    }

    pub fn stack_pointer(&self) -> u8 {
        self.pointer
    }

    /// Projection times position: what a vertex is actually multiplied by.
    pub fn clip_matrix(&mut self) -> &Matrix {
        if !self.clip_valid {
            self.clip = self.projection.multiply(&self.position);
            self.clip_valid = true;
        }
        &self.clip
    }

    fn invalidate(&mut self) {
        self.clip_valid = false;
    }

    /// `MTX_PUSH`.
    pub fn push(&mut self) {
        match self.mode {
            MatrixMode::Projection => {
                if self.projection_pushed {
                    self.overflow = true;
                }
                self.projection_saved = self.projection;
                self.projection_pushed = true;
            }
            MatrixMode::Texture => self.texture_saved = self.texture,
            _ => {
                // Hardware flags an overflow at 31 but still stores, wrapping the pointer into
                // the 32nd slot. Saturating without the flag would hide a real display-list bug.
                if self.pointer >= 31 {
                    self.overflow = true;
                }
                let slot = (self.pointer & 31) as usize;
                self.position_stack[slot] = self.position;
                self.direction_stack[slot] = self.direction;
                self.pointer = self.pointer.saturating_add(1).min(31);
            }
        }
    }

    /// `MTX_POP`, whose parameter is a *signed six-bit* offset — a game pops several levels at
    /// once, and reading it as unsigned turns a pop of one into a pop of sixty-three.
    pub fn pop(&mut self, offset: i32) {
        match self.mode {
            MatrixMode::Projection => {
                if !self.projection_pushed {
                    self.overflow = true;
                }
                self.projection = self.projection_saved;
                self.projection_pushed = false;
                self.invalidate();
            }
            MatrixMode::Texture => self.texture = self.texture_saved,
            _ => {
                let target = self.pointer as i32 - offset;
                if !(0..=31).contains(&target) {
                    self.overflow = true;
                }
                self.pointer = target.clamp(0, 31) as u8;
                let slot = self.pointer as usize;
                self.position = self.position_stack[slot];
                self.direction = self.direction_stack[slot];
                self.invalidate();
            }
        }
    }

    /// `MTX_STORE`, which writes to an absolute slot rather than the stack pointer.
    pub fn store(&mut self, slot: u32) {
        match self.mode {
            MatrixMode::Projection => {
                self.projection_saved = self.projection;
                self.projection_pushed = true;
            }
            MatrixMode::Texture => self.texture_saved = self.texture,
            _ => {
                let slot = (slot & 31) as usize;
                self.position_stack[slot] = self.position;
                self.direction_stack[slot] = self.direction;
            }
        }
    }

    /// `MTX_RESTORE`.
    pub fn restore(&mut self, slot: u32) {
        match self.mode {
            MatrixMode::Projection => {
                self.projection = self.projection_saved;
                self.invalidate();
            }
            MatrixMode::Texture => self.texture = self.texture_saved,
            _ => {
                let slot = (slot & 31) as usize;
                self.position = self.position_stack[slot];
                self.direction = self.direction_stack[slot];
                self.invalidate();
            }
        }
    }

    /// `MTX_IDENTITY`.
    pub fn load_identity(&mut self) {
        self.apply(|_| Matrix::identity());
    }

    /// `MTX_LOAD_4x4` and `MTX_LOAD_4x3`.
    ///
    /// Named `load_matrix` rather than `load` because an inherent method silently shadows a trait
    /// method of the same name, and `Savable::load` is the other one — a trap this project has
    /// already paid for once on the SM83 core's `is_halted`.
    pub fn load_matrix(&mut self, matrix: Matrix) {
        self.apply(|_| matrix);
    }

    /// `MTX_MULT_*`.
    pub fn multiply(&mut self, matrix: Matrix) {
        self.apply(|current| current.multiply(&matrix));
    }

    /// `MTX_TRANS`.
    pub fn translate(&mut self, x: i32, y: i32, z: i32) {
        self.apply(|current| current.translated(x, y, z));
    }

    /// `MTX_SCALE`, which does *not* touch the direction matrix even in mode 2.
    pub fn scale(&mut self, x: i32, y: i32, z: i32) {
        match self.mode {
            MatrixMode::Projection => self.projection = self.projection.scaled(x, y, z),
            MatrixMode::Texture => self.texture = self.texture.scaled(x, y, z),
            _ => self.position = self.position.scaled(x, y, z),
        }
        self.invalidate();
    }

    /// Apply `f` to whichever matrices the current mode names.
    fn apply(&mut self, f: impl Fn(&Matrix) -> Matrix) {
        match self.mode {
            MatrixMode::Projection => self.projection = f(&self.projection),
            MatrixMode::Texture => self.texture = f(&self.texture),
            MatrixMode::Position => self.position = f(&self.position),
            MatrixMode::PositionAndDirection => {
                self.position = f(&self.position);
                self.direction = f(&self.direction);
            }
        }
        self.invalidate();
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Savable for MatrixStack {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.mode as u8);
        for matrix in [
            &self.projection,
            &self.position,
            &self.direction,
            &self.texture,
            &self.projection_saved,
            &self.texture_saved,
        ] {
            for value in matrix.0 {
                w.write_i32(value);
            }
        }
        for stack in [&self.position_stack, &self.direction_stack] {
            for matrix in stack.iter() {
                for value in matrix.0 {
                    w.write_i32(value);
                }
            }
        }
        w.write_u8(self.pointer);
        w.write_bool(self.projection_pushed);
        w.write_bool(self.overflow);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.mode = MatrixMode::from_bits(r.read_u8()? as u32);
        let read_matrix = |r: &mut StateReader| -> Result<Matrix, StateError> {
            let mut m = [0i32; 16];
            for value in &mut m {
                *value = r.read_i32()?;
            }
            Ok(Matrix(m))
        };
        self.projection = read_matrix(r)?;
        self.position = read_matrix(r)?;
        self.direction = read_matrix(r)?;
        self.texture = read_matrix(r)?;
        self.projection_saved = read_matrix(r)?;
        self.texture_saved = read_matrix(r)?;
        for i in 0..POSITION_DEPTH {
            self.position_stack[i] = read_matrix(r)?;
        }
        for i in 0..POSITION_DEPTH {
            self.direction_stack[i] = read_matrix(r)?;
        }
        self.pointer = r.read_u8()?.min(31);
        self.projection_pushed = r.read_bool()?;
        self.overflow = r.read_bool()?;
        self.clip_valid = false;
        Ok(())
    }
}
