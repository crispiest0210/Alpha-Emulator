//! The top-level per-platform handle: [`System`].
//!
//! This is the entire surface the frontends, the debugger's session layer, and the accuracy
//! harness consume. Everything a frontend needs to run a game is here, and nothing a frontend
//! does not need is. In particular, there is no way to reach a system's CPU, PPU, or bus
//! through this trait — the predecessor project's save states were implemented by reaching
//! into a third-party core's private object graph, and refusing to expose that path is a
//! deliberate structural choice, not an oversight.

use crate::debug::DebugTarget;
use crate::{AudioSample, Cycles, Framebuffer, InputState};
use savestate::{decode_state, encode_state, Savable, StateError};
use thiserror::Error;

/// Why a ROM or save file could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CartridgeError {
    #[error("ROM is {len} bytes, need at least {min}")]
    TooSmall { len: usize, min: usize },

    #[error("ROM size {len} is not valid for this system")]
    BadSize { len: usize },

    #[error("invalid cartridge header: {0}")]
    BadHeader(String),

    #[error("unsupported mapper {code:#04X} ({name})")]
    UnsupportedMapper { code: u8, name: String },

    #[error("this ROM is for {found}, not {expected}")]
    WrongSystem {
        expected: &'static str,
        found: String,
    },

    #[error("save data is {found} bytes, but this cartridge has {expected} bytes of save RAM")]
    SaveSizeMismatch { expected: usize, found: usize },

    #[error("this cartridge has no battery-backed save")]
    NoSaveRam,

    #[error("{0}")]
    Other(String),
}

/// What the frontend learns from running one frame, beyond the framebuffer and audio.
///
/// Kept to things a frontend must *act* on. Statistics and internal state belong in the
/// debugger's interfaces, not in a struct returned sixty times a second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameOutput {
    /// Emulated cycles consumed by this frame, in the system's base clock units. Frames are
    /// not all the same length — a frame ending mid-instruction carries the overshoot into
    /// the next one — so this is reported rather than assumed.
    pub cycles_elapsed: Cycles,

    /// The game wrote to battery-backed save RAM during this frame.
    ///
    /// The frontend uses this to schedule a debounced flush to disk, so a save is durable
    /// shortly after the game writes it rather than only at clean shutdown. Emulators that
    /// only flush on exit lose saves whenever they crash or are force-quit.
    pub save_ram_dirty: bool,

    /// The emulated machine requested power-off or entered a terminal stopped state.
    pub stopped: bool,
}

/// One emulated machine.
///
/// # Contract for implementers
///
/// - [`step_frame`](System::step_frame) runs until exactly one video frame has been produced,
///   leaving [`framebuffer`](System::framebuffer) holding it. It must always return, even
///   with no cartridge loaded — a frontend calling `step_frame` on an empty system should get
///   a blank frame, not a hang or a panic.
/// - `input` applies for the whole frame. Sub-frame input polling is the system's business if
///   it needs it; the interface intentionally does not expose it, because no frontend can
///   produce meaningful sub-frame input from a 60 Hz event loop.
/// - Audio and video are produced at the system's native rate, resampled to
///   [`AUDIO_SAMPLE_RATE`](crate::AUDIO_SAMPLE_RATE) on the audio side.
/// - `save_state`/`load_state` have working default implementations built on
///   [`id`](System::id) and [`state_version`](System::state_version). Implement [`Savable`]
///   for your system and you get both, correctly versioned, for free — which is the point: it
///   should be *harder* to skip save-state support than to include it.
pub trait System: Savable {
    /// Stable short identifier, e.g. `"gb"`, `"gbc"`, `"gba"`, `"nds"`.
    ///
    /// Written into save states and used to reject a state from the wrong system, so it must
    /// never change once released.
    fn id(&self) -> &'static str;

    /// Human-readable name for the UI, e.g. `"Game Boy Advance"`.
    fn display_name(&self) -> &'static str;

    /// Save-state layout version for this system.
    ///
    /// Bump on **any** change to what the system's `Savable` impl writes, including changes
    /// in a subsystem it delegates to. Old states are then rejected with a clear error rather
    /// than loaded as garbage.
    fn state_version(&self) -> u32;

    /// Apply input without running anything.
    ///
    /// [`step_frame`](System::step_frame) calls this first, so the two can never disagree about what
    /// applying input means. It is separate because the debugger needs it on its own: while
    /// single-stepping there is no frame to pass input to, and without this the joypad would read as
    /// it did on whatever full frame ran last.
    ///
    /// Required rather than defaulted. A no-op default would mean a system that forgot to implement
    /// it silently ignores the controller, which is a bug with no error message.
    fn set_input(&mut self, input: InputState);

    /// Run one video frame.
    ///
    /// `input` applies for the whole frame; implementations pass it to
    /// [`set_input`](System::set_input) before running anything.
    fn step_frame(&mut self, input: InputState) -> FrameOutput;

    /// Run exactly one CPU instruction, returning the cycles it took.
    ///
    /// Required, not defaulted: there is no honest default. It is what a debugger single-steps
    /// with, and it is also how execution breakpoints are checked without putting a hook inside
    /// every system's hot loop — the session steps instruction by instruction *only while a
    /// debugger is attached*, so ordinary play pays nothing at all. That trade is why this is on
    /// the trait rather than being an inherent method on each system, which is what it was.
    ///
    /// Frame boundaries are not respected: stepping past the end of a frame is a normal thing for
    /// a debugger to do, and [`framebuffer`](System::framebuffer) simply holds a partially drawn
    /// picture until the frame completes.
    fn step_instruction(&mut self) -> Cycles;

    /// Return to power-on state, keeping the loaded cartridge.
    fn reset(&mut self);

    /// Parse and install a cartridge, then reset.
    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError>;

    /// The most recently completed frame.
    fn framebuffer(&self) -> &Framebuffer;

    /// Audio produced since the previous call, at [`AUDIO_SAMPLE_RATE`](crate::AUDIO_SAMPLE_RATE).
    ///
    /// Takes `&mut self` and drains: each sample is returned exactly once. Implementations
    /// typically swap an internal accumulation buffer with a returned one, so no allocation
    /// happens per frame.
    fn take_audio_samples(&mut self) -> &[AudioSample];

    /// Debugger inspection, when this system offers it.
    ///
    /// `None` by default, so a system can be written and run long before it has anything to
    /// introspect — which is the state the Nintendo DS is in. A frontend shows the debugger panel
    /// as unavailable rather than empty.
    fn debug(&mut self) -> Option<&mut dyn DebugTarget> {
        None
    }

    /// Battery-backed save contents, or `None` when the cartridge has no save memory.
    fn save_ram(&self) -> Option<&[u8]>;

    /// Restore battery-backed save contents from disk.
    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError>;

    /// Serialize the complete machine state.
    ///
    /// The default wraps this system's [`Savable`] output in the versioned container from the
    /// `savestate` crate. Overriding it is almost always a mistake.
    fn save_state(&self) -> Vec<u8> {
        encode_state(self.id(), self.state_version(), self)
    }

    /// Restore a state produced by [`save_state`](System::save_state).
    ///
    /// A state from a different system or state version is rejected before any of it is
    /// applied. A *corrupt* state can still fail partway through, leaving the machine
    /// inconsistent — callers must [`reset`](System::reset) on error rather than continuing
    /// to run, which is exactly what the frontend's load path does.
    fn load_state(&mut self, data: &[u8]) -> Result<(), StateError> {
        // Read both before taking the `&mut self` borrow below. `id` returns `&'static str`,
        // so nothing borrowed from `self` outlives these lines.
        let id = self.id();
        let version = self.state_version();
        decode_state(id, version, data, self)
    }
}
