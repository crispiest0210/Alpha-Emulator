//! [`DebugTarget`]: the one surface a debugger sees into a running machine through.
//!
//! # Why this is not just `CpuIntrospect`
//!
//! [`CpuIntrospect`](crate::CpuIntrospect) describes a CPU. A debugger needs a *machine*: the
//! registers, but also the memory around them, the instruction at the program counter, and the
//! names of the regions an address might be in. Those last three are the system's knowledge, not
//! the core's — only the system knows that `0x8000` is video RAM on a Game Boy and cartridge ROM on
//! a Game Boy Advance, and only the system knows which of its two disassemblers applies at the
//! current PC.
//!
//! # Why the debugger does not get the bus
//!
//! The tempting shape is `System::bus(&mut self) -> &mut dyn Bus`. It is rejected for the same
//! reason [`System`](crate::System) exposes no internals at all: the predecessor implemented save
//! states by reaching into a third-party core's private object graph, and every subsequent bug came
//! from something else having done the same. A handle to the live bus also hands the debugger
//! `read8`, which on a Game Boy has side effects — reading `0xFF44` mid-frame is fine, but reading
//! the joypad register latches, and a memory view that scrolls past MMIO would silently change what
//! the game sees.
//!
//! So the read path is [`peek8`](DebugTarget::peek8), which returns `Option<u8>` and is allowed —
//! required — to answer `None` where a side-effect-free read is not possible. A hex viewer showing
//! `--` for two bytes is correct; a hex viewer that perturbed the machine to avoid showing `--`
//! would be a debugger that changes the bug it is being used to find.
//!
//! # Cost
//!
//! Nothing here is called during emulation. [`System::debug`](crate::System::debug) returns `None`
//! by default, so a system that does not implement any of it pays one null check per debugger
//! request, which happens a few times a second at most.

use crate::{DisasmInstruction, RegisterValue};

/// Whether an access read or wrote.
///
/// Defined here rather than in `debugger` because the *bus* has to name it, and a system crate may
/// not depend on `debugger`. `debugger` re-exports this, so there is one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    Read,
    Write,
}

/// One byte-wide bus access, as it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub addr: u32,
    pub kind: AccessKind,
    pub value: u8,
}

/// Entries one [`AccessLog`] holds.
///
/// Sized for the widest single instruction in the workspace: an ARM `ldm`/`stm` can move sixteen
/// registers, which is sixty-four bytes, plus its own fetch. 128 leaves room without making the log
/// large enough to matter — it lives inside the bus, so it is paid for in cache footprint whether it
/// is armed or not.
const CAPACITY: usize = 128;

/// A bus's record of the accesses one instruction made.
///
/// # Why the bus records instead of the debugger checking
///
/// Watchpoints are the one thing the session's stepping trick cannot do. Execution breakpoints work
/// by checking the program counter *between* calls to
/// [`step_instruction`](crate::System::step_instruction), so no system crate learns that breakpoints
/// exist. A watchpoint has to see each access, and only the bus does.
///
/// What the bus gets is deliberately as dumb as possible: it records, it does not decide. It has no
/// idea what a watchpoint is, holds no addresses to compare against, and cannot stop execution. The
/// session drains the log after each instruction and asks `debugger`'s registry about each entry, so
/// the policy stays above the systems exactly as it does for execution breakpoints.
///
/// # What it costs when nothing is watching
///
/// One load and one branch per bus access, from [`record`](Self::record) returning immediately while
/// [`is_armed`](Self::is_armed) is false. That is not nothing and it is not claimed to be: prompt 15
/// asks for the claim to be *verified* with prompt 18's profiling rather than asserted, and it has
/// not been. What can be said is that the branch is perfectly predicted — it is false for the entire
/// lifetime of an ordinary session — and that arming happens only when a watchpoint exists.
///
/// # What it does not see
///
/// Accesses that never reach the bus. A PPU fetching tiles reads VRAM directly, and so does DMA on
/// the Game Boy family; a watchpoint on VRAM sees the CPU's writes to it and not the PPU's reads
/// from it. That matches what hardware watchpoints do — they watch the CPU bus — but it is a real
/// limitation and a watchpoint that never fires on a DMA-written address is not broken.
#[derive(Debug, Clone)]
pub struct AccessLog {
    armed: bool,
    entries: Vec<Access>,
    /// Set when an instruction made more accesses than the log can hold.
    ///
    /// Reported rather than ignored: silently dropping accesses would make a watchpoint that
    /// *should* have fired look like one that was never hit, which is the worst failure a debugger
    /// can have.
    overflowed: bool,
}

impl Default for AccessLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessLog {
    pub fn new() -> Self {
        Self {
            armed: false,
            entries: Vec::new(),
            overflowed: false,
        }
    }

    /// Start or stop recording. Clears whatever was held.
    pub fn set_armed(&mut self, armed: bool) {
        self.armed = armed;
        self.entries.clear();
        self.entries.shrink_to_fit();
        if armed {
            self.entries.reserve(CAPACITY);
        }
        self.overflowed = false;
    }

    #[inline(always)]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Record one byte-wide access.
    ///
    /// Byte-wide, always, even for a halfword or word transfer: a watchpoint covers an address
    /// *range*, and recording a word store as one entry at its base address would mean a watchpoint
    /// on the third byte of a structure never fired for the store that overwrote it.
    #[inline(always)]
    pub fn record(&mut self, addr: u32, kind: AccessKind, value: u8) {
        if !self.armed {
            return;
        }
        if self.entries.len() == CAPACITY {
            self.overflowed = true;
            return;
        }
        self.entries.push(Access { addr, kind, value });
    }

    /// Take everything recorded since the last drain.
    pub fn drain(&mut self) -> impl Iterator<Item = Access> + '_ {
        self.overflowed = false;
        self.entries.drain(..)
    }

    /// Whether the last drain lost accesses to the capacity limit.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A named span of a machine's address space, for a memory viewer's jump list.
///
/// Static because these are properties of the hardware, not of a session: a Game Boy's video RAM is
/// at `0x8000` on every Game Boy that will ever exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugRegion {
    pub name: &'static str,
    pub start: u32,
    /// Inclusive end, so a region reaching `0xFFFF_FFFF` is expressible.
    pub end: u32,
}

impl DebugRegion {
    pub const fn new(name: &'static str, start: u32, end: u32) -> Self {
        Self { name, start, end }
    }

    pub const fn contains(&self, addr: u32) -> bool {
        addr >= self.start && addr <= self.end
    }

    pub const fn len(&self) -> u64 {
        (self.end as u64 - self.start as u64) + 1
    }

    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Everything a debugger can learn about a live machine.
///
/// Object-safe and flat on purpose. A supertrait tower would be tidier to write and worse to use:
/// the consumer is an `egui` panel that wants seven facts, and `&mut dyn DebugTarget` is what lets
/// the session hand it those facts without knowing which machine it is talking to.
pub trait DebugTarget {
    /// The register file, in a stable display order.
    fn registers(&self) -> Vec<RegisterValue>;

    /// The *architectural* program counter — the address of the next instruction, not a pipeline
    /// register reading two instructions ahead. Breakpoint comparison and the disassembly
    /// highlight both depend on this being the former.
    fn program_counter(&self) -> u32;

    /// Jump execution elsewhere.
    fn set_program_counter(&mut self, pc: u32);

    /// Condition flags rendered compactly, e.g. `"Z-H-"`. Empty when there is nothing to show.
    fn flags_summary(&self) -> String;

    /// Whether the core is halted waiting for an interrupt, which a debugger must distinguish from
    /// "running but making no visible progress".
    fn is_halted(&self) -> bool;

    /// Read one byte **without side effects**, or `None` where that is not possible.
    ///
    /// `None` is a real answer and must be shown as one. See the module docs.
    fn peek8(&self, addr: u32) -> Option<u8>;

    /// Decode the instruction at `addr`, using whichever disassembler the machine is currently in
    /// the mode for — ARM or Thumb on a GBA, which the system knows and the caller cannot.
    ///
    /// Reads through [`peek8`](Self::peek8), so a disassembly view can never perturb MMIO.
    fn disassemble(&self, addr: u32) -> Option<DisasmInstruction>;

    /// Named regions of the address space, for a jump list.
    fn regions(&self) -> &'static [DebugRegion];

    /// How many hex digits an address of this machine takes: 4 for a Game Boy, 8 for a GBA.
    ///
    /// Presentation, but it belongs here because it is a fact about the hardware. A Game Boy
    /// address printed as `0000C000` is harder to read than `C000` and invites the reader to
    /// wonder what the leading zeroes mean.
    fn address_digits(&self) -> u8;
}

// ---------------------------------------------------------------------------
// PPU introspection: palettes, tiles, sprites, and the registers that place them
// ---------------------------------------------------------------------------
//
// A second, optional debugger extension, the same shape as `DebugTarget` and for the same
// reason `System::debug` gives one: "why does the picture look wrong" is a different question
// from "why did the CPU do that", and answering it needs the PPU's own data — palettes, tile
// data, OAM, and the registers that place them — none of which a CPU-only `DebugTarget` has
// any reason to carry.
//
// # Why the decoded output lives here, not the decoding
//
// [`PpuSnapshot`] is built entirely from this crate's own types (`Rgba8`, plain integers), so
// it can live in `core-common` without this crate depending on `ppu-tile2d` or any system
// crate — that dependency would run backwards, since those crates already depend on this one.
// The *decoding* — turning raw tile bytes into an `Rgba8` bitmap using `ppu-tile2d`'s pixel
// math — happens on the implementing system's side of the trait, exactly as
// [`DebugTarget::disassemble`] decodes with a CPU crate's disassembler and hands back this
// crate's plain [`DisasmInstruction`].

use crate::Rgba8;

/// A background or object layer that a debugger can isolate.
///
/// Windows are deliberately not members of this enum: a window does not draw a picture of its
/// own, so "solo this window" does not mean anything the way "solo this background" does. A
/// window can only be force-hidden — see [`LayerOverrides::win_hidden`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebugLayer {
    Bg0,
    Bg1,
    Bg2,
    Bg3,
    Obj,
}

/// A debugging override on what the PPU draws, layered on top of the machine's real registers.
///
/// # Why this must never touch save states or emulation state
///
/// This is a lens over the picture, not a change to the machine underneath it. A system that
/// implements [`PpuDebugTarget`] must apply it only at the last moment — inside the per-pixel
/// compositing decision — and must exclude it from its `Savable` implementation. Two
/// consequences follow, and both are load-bearing: a game's own logic can never observe that a
/// layer is hidden (nothing it can read is affected), and a save state written with an override
/// active restores to the same bytes as one written without it. `testing/golden/gba.toml`'s
/// hashes stay meaningful only because of the first property, and a save file stays portable
/// only because of the second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayerOverrides {
    /// Force each background layer off, independent of `DISPCNT`'s enable bits.
    pub bg_hidden: [bool; 4],
    /// Force every sprite off, independent of `DISPCNT`'s object enable bit.
    pub obj_hidden: bool,
    /// Force window 0 or window 1's masking off, so every layer draws as though that window did
    /// not exist. The object window is not included: it is a property of individual sprites
    /// (`GraphicsMode::ObjectWindow`) rather than a switch this override layer can meaningfully
    /// flip on its own.
    pub win_hidden: [bool; 2],
    /// When set, only this layer (and the backdrop) draws — every other background and the
    /// sprite layer are hidden regardless of `bg_hidden`/`obj_hidden`. Windows are unaffected:
    /// soloing a background does not imply anything about which regions a window still masks.
    pub solo: Option<DebugLayer>,
}

impl LayerOverrides {
    /// Whether background layer `index` should draw, given `DISPCNT`'s own enable bit for it.
    #[inline]
    pub fn bg_visible(&self, index: usize) -> bool {
        if self.bg_hidden[index] {
            return false;
        }
        match self.solo {
            None => true,
            Some(DebugLayer::Bg0) => index == 0,
            Some(DebugLayer::Bg1) => index == 1,
            Some(DebugLayer::Bg2) => index == 2,
            Some(DebugLayer::Bg3) => index == 3,
            Some(DebugLayer::Obj) => false,
        }
    }

    /// Whether sprites should draw at all.
    #[inline]
    pub fn obj_visible(&self) -> bool {
        if self.obj_hidden {
            return false;
        }
        matches!(self.solo, None | Some(DebugLayer::Obj))
    }

    /// Whether window `index` (0 or 1) still masks other layers.
    #[inline]
    pub fn win_visible(&self, index: usize) -> bool {
        !self.win_hidden[index]
    }
}

/// A tile's bit depth, for the tile/VRAM viewer.
///
/// Mirrors the two GBA-relevant variants of `ppu_tile2d::BitDepth` (its third, `Two`, is the
/// Game Boy's two-bitplane format and never applies here) — kept as a separate type rather than
/// reused directly because `core-common` cannot depend on `ppu-tile2d` without the dependency
/// running backwards; see the module docs above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileBitDepth {
    /// 16 colours, 32 bytes per 8x8 tile.
    Four,
    /// 256 colours, 64 bytes per 8x8 tile.
    Eight,
}

/// What the tile/VRAM viewer is asking to see.
///
/// Bounded the same way [`DebugTarget`]'s memory viewer is: decoding every tile in VRAM to
/// `Rgba8` on every poll would be tens of thousands of pixels the panel is not displaying,
/// several times a second, whether or not the tile view is scrolled there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PpuDebugRequest {
    /// Byte offset into VRAM's character data where the requested tiles start.
    pub tile_char_base: usize,
    /// How many consecutive tiles to decode, from `tile_char_base`.
    pub tile_count: usize,
    pub tile_depth: TileBitDepth,
    /// Which of the sixteen 16-colour palette banks to decode a 4bpp tile against. Ignored for
    /// 8bpp tiles, which are not banked.
    pub tile_palette_bank: u8,
}

impl Default for PpuDebugRequest {
    fn default() -> Self {
        Self {
            tile_char_base: 0,
            // 512 tiles is one whole 16 KiB character block at 4bpp — enough to fill a viewer
            // without decoding VRAM that is not being looked at.
            tile_count: 512,
            tile_depth: TileBitDepth::Four,
            tile_palette_bank: 0,
        }
    }
}

impl PpuDebugRequest {
    /// Clamp to a size a debugger poll can afford to decode several times a second.
    pub fn clamped(mut self) -> Self {
        self.tile_count = self.tile_count.min(1024);
        self
    }
}

/// One decoded palette entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteSwatch {
    pub color: Rgba8,
    /// The raw 15-bit BGR value, for a hover readout — the format the hardware and every GBA
    /// reference document actually describes, which `color` alone cannot reconstruct exactly
    /// (alpha is synthesised, not stored).
    pub raw: u16,
}

/// One decoded 8x8 tile, row-major from the top-left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileBitmap {
    pub pixels: [Rgba8; 64],
}

/// One row of the OAM viewer's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OamRow {
    pub index: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub priority: u8,
    pub palette: u8,
    pub tile: u16,
    /// Which of the affine matrices this entry uses, when its mode is affine.
    pub affine_index: Option<usize>,
    pub graphics_mode: &'static str,
    pub mode: &'static str,
    /// Whether this sprite covers the scanline the PPU is about to draw (or just drew, for a
    /// paused machine) — the fact that answers "is this even one of the sprites in play right
    /// now" without the reader cross-referencing `y`/`height` against `VCOUNT` by hand.
    pub on_current_scanline: bool,
}

/// One background layer's registers, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BgRegisters {
    pub control: u16,
    pub enabled: bool,
    pub priority: u8,
    pub char_base: u32,
    pub screen_base: u32,
    /// 4 or 8.
    pub bpp: u8,
    pub size_tiles: (u32, u32),
    /// Read from the system's own stored scroll state, not from the bus — `BGxHOFS`/`BGxVOFS`
    /// are write-only, and a bus read of a write-only register answers zero. A debugger that
    /// read them the ordinary way would show every layer parked at (0,0) no matter how far a
    /// game had actually scrolled it.
    pub scroll_x: u16,
    pub scroll_y: u16,
    pub mosaic: bool,
}

/// One window's rectangle and which layers it lets through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowRegisters {
    pub enabled: bool,
    pub left: u8,
    pub right: u8,
    pub top: u8,
    pub bottom: u8,
    /// Low six bits of this window's `WININ`/`WINOUT` half: which of BG0-3, OBJ, and the colour
    /// effect are let through inside it.
    pub layers_in: u8,
}

/// Every PPU register a debugger view names, decoded rather than left as hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PpuRegisters {
    pub dispcnt: u16,
    pub mode: u16,
    pub forced_blank: bool,
    pub obj_1d_mapping: bool,
    pub dispstat: u16,
    pub vcount: u16,
    pub backgrounds: [BgRegisters; 4],
    /// Window 0 and window 1.
    pub windows: [WindowRegisters; 2],
    /// The "outside all windows" layer bits — always in effect wherever no window claims a
    /// pixel, so it has no rectangle of its own to go with `windows` above.
    pub winout: u8,
    /// The object window's layer bits, from `WINOUT`'s upper half.
    pub obj_window_layers: u8,
    pub bldcnt: u16,
    pub bldalpha: u16,
    /// Also write-only on hardware; read from the system's stored value for the same reason
    /// `scroll_x`/`scroll_y` above are.
    pub bldy: u16,
}

/// One PPU debugger poll, all five views' data at once.
///
/// Bundled into one snapshot rather than five separate requests because they are cheap next to
/// the tile decode that dominates the cost, and because a debugger reading them a moment apart
/// could show a palette and a tile view that each describe a different instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpuSnapshot {
    /// All 256 background palette entries, in index order.
    pub bg_palette: Vec<PaletteSwatch>,
    /// All 256 sprite palette entries, in index order.
    pub sprite_palette: Vec<PaletteSwatch>,
    /// The tiles named by the request that produced this snapshot.
    pub tiles: Vec<TileBitmap>,
    /// All 128 OAM entries, in table order.
    pub oam: Vec<OamRow>,
    pub registers: PpuRegisters,
    /// The overrides in effect when this snapshot was taken, echoed back so the panel's toggle
    /// state and the machine's actual state cannot silently drift apart.
    pub overrides: LayerOverrides,
}

/// PPU introspection, when a system offers it.
///
/// `&self` throughout except the override setter: every one of these views is read-only, and
/// keeping them so is what makes capturing a snapshot safe to do from a live, running machine —
/// the same reasoning [`DebugTarget::peek8`] rests on.
pub trait PpuDebugTarget {
    fn ppu_snapshot(&self, request: &PpuDebugRequest) -> PpuSnapshot;

    /// Replace the layer overrides in effect. See [`LayerOverrides`] for what this must and
    /// must not be allowed to affect.
    fn set_layer_overrides(&mut self, overrides: LayerOverrides);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_hidden_is_the_default_and_hides_nothing() {
        let overrides = LayerOverrides::default();
        for i in 0..4 {
            assert!(overrides.bg_visible(i));
        }
        assert!(overrides.obj_visible());
        assert!(overrides.win_visible(0));
        assert!(overrides.win_visible(1));
    }

    #[test]
    fn hiding_one_background_leaves_the_others_and_obj_alone() {
        let overrides = LayerOverrides {
            bg_hidden: [false, true, false, false],
            ..Default::default()
        };
        assert!(overrides.bg_visible(0));
        assert!(!overrides.bg_visible(1));
        assert!(overrides.bg_visible(2));
        assert!(overrides.bg_visible(3));
        assert!(overrides.obj_visible());
    }

    #[test]
    fn hiding_obj_leaves_every_background_alone() {
        let overrides = LayerOverrides {
            obj_hidden: true,
            ..Default::default()
        };
        assert!(!overrides.obj_visible());
        for i in 0..4 {
            assert!(overrides.bg_visible(i));
        }
    }

    #[test]
    fn soloing_a_background_hides_every_other_background_and_obj() {
        let overrides = LayerOverrides {
            solo: Some(DebugLayer::Bg2),
            ..Default::default()
        };
        assert!(!overrides.bg_visible(0));
        assert!(!overrides.bg_visible(1));
        assert!(overrides.bg_visible(2), "the soloed layer still draws");
        assert!(!overrides.bg_visible(3));
        assert!(!overrides.obj_visible());
    }

    #[test]
    fn soloing_obj_hides_every_background() {
        let overrides = LayerOverrides {
            solo: Some(DebugLayer::Obj),
            ..Default::default()
        };
        assert!(overrides.obj_visible());
        for i in 0..4 {
            assert!(
                !overrides.bg_visible(i),
                "BG{i} must not draw while OBJ is soloed"
            );
        }
    }

    #[test]
    fn hide_wins_over_solo_for_the_same_layer() {
        // Asking for a layer both soloed and hidden is a contradictory request, and hidden is
        // the more specific, more recently-set-feeling instruction — it wins rather than being
        // silently overridden by solo.
        let overrides = LayerOverrides {
            bg_hidden: [false, false, true, false],
            solo: Some(DebugLayer::Bg2),
            ..Default::default()
        };
        assert!(!overrides.bg_visible(2));
    }

    #[test]
    fn windows_are_independent_of_background_and_obj_overrides() {
        let overrides = LayerOverrides {
            win_hidden: [true, false],
            solo: Some(DebugLayer::Bg0),
            ..Default::default()
        };
        assert!(!overrides.win_visible(0));
        assert!(overrides.win_visible(1));
    }

    #[test]
    fn a_ppu_debug_request_clamps_an_absurd_tile_count() {
        let request = PpuDebugRequest {
            tile_count: usize::MAX,
            ..PpuDebugRequest::default()
        }
        .clamped();
        assert_eq!(request.tile_count, 1024);
    }

    #[test]
    fn a_region_covers_its_inclusive_end() {
        let vram = DebugRegion::new("VRAM", 0x8000, 0x9FFF);
        assert!(vram.contains(0x8000));
        assert!(vram.contains(0x9FFF), "the end is inside the region");
        assert!(!vram.contains(0xA000));
        assert_eq!(vram.len(), 0x2000);
    }

    #[test]
    fn a_region_reaching_the_top_of_the_address_space_does_not_overflow() {
        // The obvious `end - start + 1` in u32 wraps to zero here, which would report the largest
        // possible region as empty.
        let all = DebugRegion::new("everything", 0, u32::MAX);
        assert_eq!(all.len(), 0x1_0000_0000);
        assert!(all.contains(u32::MAX));
    }

    #[test]
    fn a_single_byte_region_is_one_byte_long() {
        let register = DebugRegion::new("IE", 0xFFFF, 0xFFFF);
        assert_eq!(register.len(), 1);
        assert!(register.contains(0xFFFF));
    }
}
