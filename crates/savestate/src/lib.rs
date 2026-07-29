//! Versioned binary save-state format and the [`Savable`] trait.
//!
//! # Why `Savable` lives here and not in `core-common`
//!
//! Prompt 02 left the placement of [`Savable`] to whichever crate avoids a circular
//! dependency. `savestate` needs nothing from `core-common` — it is pure serialization with
//! no notion of a CPU, bus, or scheduler — whereas `core-common`'s `Cpu`/`Bus`/`System`
//! traits all want a `Savable` supertrait bound. So the dependency runs
//! `core-common -> savestate`, and this crate is a leaf.
//!
//! # The rule this crate exists to enforce
//!
//! Every stateful struct implements [`Savable`] **at the moment it is written**. Save-state
//! fidelity must never depend on one module reaching into another module's private fields;
//! that is exactly what the predecessor project did, and it is why it needed a "warm reboot
//! after every load" workaround that still corrupted tile data.
//!
//! # Format
//!
//! All integers are little-endian. A complete state blob is:
//!
//! ```text
//! magic       [u8; 8]              b"ALPHAST1"
//! system_id   len-prefixed UTF-8   e.g. "gb", "gba"
//! version     u32                  system-defined, bumped on any layout change
//! payload     ...                  the system's own `Savable::save` output
//! ```
//!
//! There is deliberately no self-describing schema: a version mismatch is a hard error, not
//! a best-effort partial load. Silently loading a state that does not match the emulator's
//! current layout is how you get corruption that looks like an emulation bug.

#![deny(unsafe_code)]

pub mod rewind;

pub use rewind::{RewindBuffer, Snapshot};

use thiserror::Error;

pub const MAGIC: [u8; 8] = *b"ALPHAST1";

/// Everything that can go wrong reading a save state.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum StateError {
    #[error("save state truncated: needed {needed} more byte(s) at offset {offset}")]
    UnexpectedEof { offset: usize, needed: usize },

    #[error("not an Alpha Emulator save state (bad magic)")]
    BadMagic,

    #[error("save state is for system {found:?}, but this is {expected:?}")]
    WrongSystem { expected: String, found: String },

    #[error("save state version {found} is not supported (this build writes version {expected})")]
    VersionMismatch { expected: u32, found: u32 },

    #[error("save state is malformed: {0}")]
    Malformed(String),

    #[error("{0} bytes of trailing data after the state payload")]
    TrailingData(usize),
}

// ---------------------------------------------------------------------------
// Writer / reader
// ---------------------------------------------------------------------------

/// Append-only byte sink that [`Savable::save`] writes into.
///
/// Writes are infallible: the target is an in-memory buffer, so there is no I/O error to
/// propagate and `save` therefore does not return a `Result`.
#[derive(Debug, Default, Clone)]
pub struct StateWriter {
    buf: Vec<u8>,
}

impl StateWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Length-prefixed byte blob, for data whose size is not statically known.
    pub fn write_blob(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        self.write_bytes(bytes);
    }

    pub fn write_str(&mut self, s: &str) {
        self.write_blob(s.as_bytes());
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_i8(&mut self, v: i8) {
        self.write_u8(v as u8);
    }

    pub fn write_i16(&mut self, v: i16) {
        self.write_u16(v as u16);
    }

    pub fn write_i32(&mut self, v: i32) {
        self.write_u32(v as u32);
    }

    pub fn write_i64(&mut self, v: i64) {
        self.write_u64(v as u64);
    }

    pub fn write_f32(&mut self, v: f32) {
        self.write_u32(v.to_bits());
    }

    pub fn write_f64(&mut self, v: f64) {
        self.write_u64(v.to_bits());
    }
}

/// Cursor over a state blob that [`Savable::load`] reads from.
#[derive(Debug, Clone)]
pub struct StateReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> StateReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StateError> {
        if self.remaining() < n {
            return Err(StateError::UnexpectedEof {
                offset: self.pos,
                needed: n - self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), StateError> {
        let n = out.len();
        out.copy_from_slice(self.take(n)?);
        Ok(())
    }

    pub fn read_blob(&mut self) -> Result<&'a [u8], StateError> {
        let len = self.read_u64()? as usize;
        self.take(len)
    }

    pub fn read_string(&mut self) -> Result<String, StateError> {
        let bytes = self.read_blob()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| StateError::Malformed(format!("invalid UTF-8: {e}")))
    }

    pub fn read_u8(&mut self) -> Result<u8, StateError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_bool(&mut self) -> Result<bool, StateError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(StateError::Malformed(format!(
                "expected a bool (0 or 1), found {other}"
            ))),
        }
    }

    pub fn read_u16(&mut self) -> Result<u16, StateError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn read_u32(&mut self) -> Result<u32, StateError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u64(&mut self) -> Result<u64, StateError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_i8(&mut self) -> Result<i8, StateError> {
        Ok(self.read_u8()? as i8)
    }

    pub fn read_i16(&mut self) -> Result<i16, StateError> {
        Ok(self.read_u16()? as i16)
    }

    pub fn read_i32(&mut self) -> Result<i32, StateError> {
        Ok(self.read_u32()? as i32)
    }

    pub fn read_i64(&mut self) -> Result<i64, StateError> {
        Ok(self.read_u64()? as i64)
    }

    pub fn read_f32(&mut self) -> Result<f32, StateError> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    pub fn read_f64(&mut self) -> Result<f64, StateError> {
        Ok(f64::from_bits(self.read_u64()?))
    }
}

// ---------------------------------------------------------------------------
// Savable
// ---------------------------------------------------------------------------

/// Explicit, versioned serialization for one piece of emulated state.
///
/// # Contract for implementers
///
/// - `save` and `load` must be exact inverses. If `load` cannot reconstruct a field, that
///   field must not exist — derive it or store it, never guess it.
/// - Write **every** field that affects emulated behavior. A field omitted here is a
///   divergence bug that surfaces as "the game desyncs a few frames after loading", which is
///   far harder to debug than a serialization error.
/// - Do not write caches that are recomputable from other saved state, but *do* invalidate
///   them in `load`.
/// - Field order is the format. Adding, removing, or reordering a field is a breaking change
///   that requires bumping the owning system's state version.
/// - `load` may leave `self` partially modified when it returns `Err`. Callers must treat a
///   failed load as "this instance is now garbage, reset it" — see [`decode_state`], which is
///   written with that in mind.
pub trait Savable {
    fn save(&self, w: &mut StateWriter);
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError>;
}

/// A [`Savable`] that can also be constructed straight out of a reader.
///
/// Needed for container types: `Vec<T>::load` has to materialize elements that do not exist
/// yet. Blanket-implemented for anything that is `Savable + Default`, which is why most
/// implementers never name this trait directly.
pub trait SavableInit: Savable + Sized {
    fn load_new(r: &mut StateReader) -> Result<Self, StateError>;
}

impl<T: Savable + Default> SavableInit for T {
    fn load_new(r: &mut StateReader) -> Result<Self, StateError> {
        let mut v = T::default();
        v.load(r)?;
        Ok(v)
    }
}

macro_rules! impl_savable_primitive {
    ($ty:ty, $write:ident, $read:ident) => {
        impl Savable for $ty {
            fn save(&self, w: &mut StateWriter) {
                w.$write(*self);
            }
            fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
                *self = r.$read()?;
                Ok(())
            }
        }
    };
}

impl_savable_primitive!(u8, write_u8, read_u8);
impl_savable_primitive!(u16, write_u16, read_u16);
impl_savable_primitive!(u32, write_u32, read_u32);
impl_savable_primitive!(u64, write_u64, read_u64);
impl_savable_primitive!(i8, write_i8, read_i8);
impl_savable_primitive!(i16, write_i16, read_i16);
impl_savable_primitive!(i32, write_i32, read_i32);
impl_savable_primitive!(i64, write_i64, read_i64);
impl_savable_primitive!(f32, write_f32, read_f32);
impl_savable_primitive!(f64, write_f64, read_f64);
impl_savable_primitive!(bool, write_bool, read_bool);

impl Savable for String {
    fn save(&self, w: &mut StateWriter) {
        w.write_str(self);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = r.read_string()?;
        Ok(())
    }
}

impl<T: SavableInit> Savable for Option<T> {
    fn save(&self, w: &mut StateWriter) {
        match self {
            Some(v) => {
                w.write_bool(true);
                v.save(w);
            }
            None => w.write_bool(false),
        }
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = if r.read_bool()? {
            Some(T::load_new(r)?)
        } else {
            None
        };
        Ok(())
    }
}

impl<T: SavableInit> Savable for Vec<T> {
    fn save(&self, w: &mut StateWriter) {
        w.write_u64(self.len() as u64);
        for v in self {
            v.save(w);
        }
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let len = r.read_u64()? as usize;
        // A corrupt length must not cause a huge speculative allocation. One byte is the
        // smallest any element can serialize to, so the bytes left in the reader bound the
        // element count.
        if len > r.remaining() {
            return Err(StateError::Malformed(format!(
                "vector length {len} exceeds {} remaining bytes",
                r.remaining()
            )));
        }
        self.clear();
        self.reserve(len);
        for _ in 0..len {
            self.push(T::load_new(r)?);
        }
        Ok(())
    }
}

impl<T: Savable, const N: usize> Savable for [T; N] {
    fn save(&self, w: &mut StateWriter) {
        for v in self {
            v.save(w);
        }
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for v in self.iter_mut() {
            v.load(r)?;
        }
        Ok(())
    }
}

impl<T: Savable> Savable for Box<T> {
    fn save(&self, w: &mut StateWriter) {
        (**self).save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        (**self).load(r)
    }
}

/// Specialized so the common case — a large RAM array — round-trips as one bulk copy rather
/// than a per-byte call, and so a size change between builds is caught explicitly.
impl Savable for Box<[u8]> {
    fn save(&self, w: &mut StateWriter) {
        w.write_blob(self);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.len() {
            return Err(StateError::Malformed(format!(
                "memory region is {} bytes in this build, {} in the save state",
                self.len(),
                bytes.len()
            )));
        }
        self.copy_from_slice(bytes);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Container
// ---------------------------------------------------------------------------

/// Wrap a value's state in the versioned container described in the module docs.
pub fn encode_state<T: Savable + ?Sized>(system_id: &str, version: u32, value: &T) -> Vec<u8> {
    let mut w = StateWriter::with_capacity(1 << 16);
    w.write_bytes(&MAGIC);
    w.write_str(system_id);
    w.write_u32(version);
    value.save(&mut w);
    w.into_inner()
}

/// Read a container written by [`encode_state`] back into `value`.
///
/// The header is fully validated before a single byte reaches `value.load`, so a state for
/// the wrong system or version cannot partially clobber a running emulator. Once the payload
/// starts loading, a mid-payload error does leave `value` inconsistent — callers must reset
/// on `Err`, per the [`Savable`] contract.
pub fn decode_state<T: Savable + ?Sized>(
    system_id: &str,
    version: u32,
    data: &[u8],
    value: &mut T,
) -> Result<(), StateError> {
    let mut r = StateReader::new(data);
    let mut magic = [0u8; 8];
    r.read_bytes(&mut magic)?;
    if magic != MAGIC {
        return Err(StateError::BadMagic);
    }

    let found = r.read_string()?;
    if found != system_id {
        return Err(StateError::WrongSystem {
            expected: system_id.to_string(),
            found,
        });
    }

    let found_version = r.read_u32()?;
    if found_version != version {
        return Err(StateError::VersionMismatch {
            expected: version,
            found: found_version,
        });
    }

    value.load(&mut r)?;
    if !r.is_empty() {
        return Err(StateError::TrailingData(r.remaining()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Clone)]
    struct Toy {
        counter: u32,
        flag: bool,
        name: String,
        ram: Box<[u8]>,
        history: Vec<u16>,
        pending: Option<i32>,
        regs: [u8; 4],
    }

    impl Savable for Toy {
        fn save(&self, w: &mut StateWriter) {
            self.counter.save(w);
            self.flag.save(w);
            self.name.save(w);
            self.ram.save(w);
            self.history.save(w);
            self.pending.save(w);
            self.regs.save(w);
        }
        fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
            self.counter.load(r)?;
            self.flag.load(r)?;
            self.name.load(r)?;
            self.ram.load(r)?;
            self.history.load(r)?;
            self.pending.load(r)?;
            self.regs.load(r)?;
            Ok(())
        }
    }

    fn sample() -> Toy {
        Toy {
            counter: 0xDEAD_BEEF,
            flag: true,
            name: "toy".into(),
            ram: vec![1, 2, 3, 4, 5].into_boxed_slice(),
            history: vec![10, 20, 30],
            pending: Some(-7),
            regs: [9, 8, 7, 6],
        }
    }

    fn blank() -> Toy {
        Toy {
            ram: vec![0; 5].into_boxed_slice(),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_every_primitive_shape() {
        let original = sample();
        let blob = encode_state("toy", 1, &original);

        let mut restored = blank();
        decode_state("toy", 1, &blob, &mut restored).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut blob = encode_state("toy", 1, &sample());
        blob[0] = b'X';
        assert_eq!(
            decode_state("toy", 1, &blob, &mut blank()),
            Err(StateError::BadMagic)
        );
    }

    #[test]
    fn rejects_state_from_another_system() {
        let blob = encode_state("gba", 1, &sample());
        assert!(matches!(
            decode_state("gb", 1, &blob, &mut blank()),
            Err(StateError::WrongSystem { .. })
        ));
    }

    #[test]
    fn rejects_version_mismatch_without_touching_the_target() {
        let blob = encode_state("toy", 1, &sample());
        let mut restored = blank();
        assert_eq!(
            decode_state("toy", 2, &blob, &mut restored),
            Err(StateError::VersionMismatch {
                expected: 2,
                found: 1
            })
        );
        // Header validation happens before any payload load, so nothing was clobbered.
        assert_eq!(restored, blank());
    }

    #[test]
    fn rejects_truncated_state() {
        let blob = encode_state("toy", 1, &sample());
        let err = decode_state("toy", 1, &blob[..blob.len() - 3], &mut blank()).unwrap_err();
        assert!(matches!(err, StateError::UnexpectedEof { .. }));
    }

    #[test]
    fn rejects_trailing_data() {
        let mut blob = encode_state("toy", 1, &sample());
        blob.push(0xFF);
        assert_eq!(
            decode_state("toy", 1, &blob, &mut blank()),
            Err(StateError::TrailingData(1))
        );
    }

    #[test]
    fn rejects_memory_region_size_change() {
        let blob = encode_state("toy", 1, &sample());
        // A build whose RAM is a different size must refuse the state rather than silently
        // truncating it.
        let mut restored = Toy {
            ram: vec![0; 4].into_boxed_slice(),
            ..Default::default()
        };
        assert!(matches!(
            decode_state("toy", 1, &blob, &mut restored),
            Err(StateError::Malformed(_))
        ));
    }

    #[test]
    fn corrupt_vector_length_does_not_allocate_wildly() {
        let mut w = StateWriter::new();
        w.write_u64(u64::MAX);
        let blob = w.into_inner();
        let mut v: Vec<u16> = Vec::new();
        assert!(matches!(
            v.load(&mut StateReader::new(&blob)),
            Err(StateError::Malformed(_))
        ));
    }
}
