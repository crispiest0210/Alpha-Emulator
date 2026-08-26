//! The 3D core: the command FIFO, the geometry engine, and the software rasteriser.
//!
//! Prompt 13 calls this the single largest scope item in the project and asks for geometry and
//! texturing correctness ahead of the rarer effects. It is built the way everything else here was,
//! as three units with their own tests: [`matrix`] for the four stacks, [`geometry`] for command
//! decode and vertex assembly, [`render`] for scan conversion and texturing. This module is the
//! register interface that feeds them and the double buffer that hands the result to engine A.
//!
//! # Two ways in, one command stream
//!
//! Software drives the engine either by writing packed commands to `GXFIFO` — four one-byte
//! opcodes in a word, then all their parameters in order — or by writing parameters straight to a
//! per-command port at `0x0400_0400 + opcode * 4`. Both funnel into the same `(opcode, params)`
//! execution, because they are the same commands and a second path would drift.
//!
//! The FIFO is a state machine over [`geometry::parameter_count`], and that table is the thing
//! that must not be wrong: one bad entry desynchronises everything after it, and the symptom is
//! geometry that is correct up to a point and garbage afterwards.
//!
//! # `SWAP_BUFFERS` is a frame boundary, not a command
//!
//! `SWAP_BUFFERS` does not take effect where it appears. It marks the display list complete and
//! the swap happens at the next vertical blank, which is what lets a game build the next frame's
//! list while the current one is still being scanned out. [`Gpu3d::on_vblank`] is where the
//! rendering actually happens, so a frame is rasterised once rather than per scanline.
//!
//! # `system-nds` has no `wgpu` dependency and must not gain one
//!
//! Prompt 13 is explicit. If the software rasteriser ever needs to be replaced, the replacement is
//! `frontend-native` consuming [`geometry::DisplayList`] — which is already a plain description of
//! triangles with no rendering in it — rather than this crate calling a graphics API.

pub mod geometry;
pub mod matrix;
pub mod render;

use core_common::{Savable, StateError, StateReader, StateWriter};
use geometry::{parameter_count, Geometry};
use render::Framebuffer3d;
use std::collections::VecDeque;

use crate::vram::Vram;

/// The packed command FIFO, which is also where the ARM7's sound registers live — on the other
/// core, and the bus routes on that.
pub const GXFIFO: u32 = 0x0400_0400;
/// The per-command ports run from `MTX_MODE` up.
pub const PORT_BASE: u32 = 0x0400_0440;
pub const PORT_END: u32 = 0x0400_05D0;

pub mod reg {
    pub const DISP3DCNT: u32 = 0x0400_0060;
    pub const EDGE_COLOR: u32 = 0x0400_0330;
    pub const ALPHA_TEST_REF: u32 = 0x0400_0340;
    pub const CLEAR_COLOR: u32 = 0x0400_0350;
    pub const CLEAR_DEPTH: u32 = 0x0400_0354;
    pub const FOG_COLOR: u32 = 0x0400_0358;
    pub const TOON_TABLE: u32 = 0x0400_0380;
    pub const GXSTAT: u32 = 0x0400_0600;
    pub const RAM_COUNT: u32 = 0x0400_0604;
    pub const POS_RESULT: u32 = 0x0400_0620;
    pub const VEC_RESULT: u32 = 0x0400_0630;
}

/// The 3D core.
pub struct Gpu3d {
    pub geometry: Geometry,
    pub framebuffer: Framebuffer3d,

    disp3dcnt: u16,
    clear_color: u32,
    clear_depth: u32,
    /// `GXSTAT`'s interrupt mode, bits 30-31: 0 never, 1 when the FIFO is less than half full,
    /// 2 when it is empty. See [`Self::fifo_irq_pending`].
    gxstat_irq: u8,

    /// Opcodes waiting for their parameters, from a packed `GXFIFO` word.
    ///
    /// Unbounded, deliberately. Hardware's FIFO is 256 entries deep and stalls the CPU when full;
    /// stalling would mean the bus could block a CPU mid-instruction, which nothing in this
    /// project's `Bus` contract can express. Reporting the FIFO as never full means a game's
    /// "wait until there is room" loop exits immediately, which is correct behaviour arrived at
    /// by a route hardware does not take — recorded here rather than presented as accuracy.
    pending: VecDeque<u8>,
    params: Vec<u32>,
    /// A command being fed through its own port rather than through the FIFO.
    port: Option<(u8, Vec<u32>)>,

    /// Stored and unused: the toon table, edge colours, and the fog block. Each belongs to an
    /// effect [`render`] does not implement, and holding the writes means a game that sets them
    /// once at boot does not produce an unmapped-write warning on every frame.
    toon: Box<[u16; 32]>,
    edge_color: [u16; 8],
    fog_color: u32,
    alpha_test_ref: u8,
}

impl Default for Gpu3d {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpu3d {
    pub fn new() -> Self {
        Self {
            geometry: Geometry::new(),
            framebuffer: Framebuffer3d::new(),
            disp3dcnt: 0,
            clear_color: 0,
            clear_depth: 0x7FFF,
            gxstat_irq: 0,
            pending: VecDeque::new(),
            params: Vec::new(),
            port: None,
            toon: Box::new([0; 32]),
            edge_color: [0; 8],
            fog_color: 0,
            alpha_test_ref: 0,
        }
    }

    /// Whether this address belongs to the 3D core, from the ARM9's point of view.
    pub fn owns(addr: u32) -> bool {
        (GXFIFO..GXFIFO + 0x40).contains(&addr)
            || (PORT_BASE..PORT_END).contains(&addr)
            || (reg::GXSTAT..reg::GXSTAT + 0x20).contains(&addr)
            || (reg::POS_RESULT..reg::VEC_RESULT + 12).contains(&addr)
            || (reg::EDGE_COLOR..reg::EDGE_COLOR + 16).contains(&addr)
            || (reg::TOON_TABLE..reg::TOON_TABLE + 64).contains(&addr)
            || matches!(
                addr & !3,
                reg::DISP3DCNT
                    | reg::ALPHA_TEST_REF
                    | reg::CLEAR_COLOR
                    | reg::CLEAR_DEPTH
                    | reg::FOG_COLOR
            )
    }

    /// Whether the 3D layer should be composited at all.
    pub fn enabled(&self) -> bool {
        self.disp3dcnt & 1 != 0
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        if (GXFIFO..GXFIFO + 0x40).contains(&addr) {
            self.push_fifo(value);
            return true;
        }
        if (PORT_BASE..PORT_END).contains(&addr) {
            let opcode = ((addr - GXFIFO) / 4) as u8;
            self.push_port(opcode, value);
            return true;
        }
        match addr & !3 {
            reg::DISP3DCNT => self.disp3dcnt = value as u16,
            reg::CLEAR_COLOR => self.clear_color = value,
            reg::CLEAR_DEPTH => self.clear_depth = value & 0x7FFF,
            reg::ALPHA_TEST_REF => self.alpha_test_ref = value as u8,
            reg::FOG_COLOR => self.fog_color = value,
            reg::GXSTAT => self.gxstat_irq = (value >> 30) as u8,
            _ if (reg::EDGE_COLOR..reg::EDGE_COLOR + 16).contains(&addr) => {
                let index = ((addr - reg::EDGE_COLOR) / 2) as usize;
                self.edge_color[index] = value as u16;
                self.edge_color[index + 1] = (value >> 16) as u16;
            }
            _ if (reg::TOON_TABLE..reg::TOON_TABLE + 64).contains(&addr) => {
                let index = ((addr - reg::TOON_TABLE) / 2) as usize;
                self.toon[index] = value as u16;
                self.toon[index + 1] = (value >> 16) as u16;
            }
            _ => return false,
        }
        true
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        Some(match addr & !3 {
            reg::DISP3DCNT => self.disp3dcnt as u32,
            reg::CLEAR_COLOR => self.clear_color,
            reg::CLEAR_DEPTH => self.clear_depth,
            reg::GXSTAT => self.gxstat(),
            reg::RAM_COUNT => {
                (self.geometry.polygon_count() as u32 & 0x1FFF)
                    | ((self.geometry.vertex_count() as u32 & 0x1FFF) << 16)
            }
            _ if (reg::POS_RESULT..reg::POS_RESULT + 16).contains(&addr) => {
                self.geometry.pos_result[((addr - reg::POS_RESULT) / 4) as usize] as u32
            }
            _ if (reg::VEC_RESULT..reg::VEC_RESULT + 12).contains(&addr) => {
                // The vector test result is 16-bit per component, packed two to a word by the
                // caller reading halfwords; a word read gets the component and zero above it.
                self.geometry.vec_result[((addr - reg::VEC_RESULT) / 4) as usize] as u32 & 0xFFFF
            }
            _ if (reg::EDGE_COLOR..reg::EDGE_COLOR + 16).contains(&addr) => 0,
            _ if (reg::TOON_TABLE..reg::TOON_TABLE + 64).contains(&addr) => 0,
            _ => return None,
        })
    }

    /// Whether the geometry FIFO's interrupt condition currently holds.
    ///
    /// # Why this is always the condition, and why that is not a shortcut
    ///
    /// Commands here are executed as they arrive rather than queued, so the FIFO is empty at every
    /// moment software can look at it — which makes both of the conditions `GXSTAT` can select
    /// permanently true. This says so instead of saying nothing.
    ///
    /// Saying nothing is what it did before, and it cost a real game its title screen. The DS's
    /// geometry interrupt is how a driver learns that the hardware has taken the display list it
    /// handed over: Pokemon Platinum sets a flag, sends its list, and spins until the interrupt
    /// handler clears the flag again. With the FIFO reported empty and the interrupt never raised,
    /// the one thing the machine *had* to tell the game was the one thing it never said, and the
    /// game waited at a title screen that was otherwise drawn correctly.
    ///
    /// The interrupt is a level, not an edge — the condition holds for as long as it holds — so
    /// this is asked once per quantum rather than latched at a transition. That is also why
    /// software turns the mode back off in its handler: on hardware, leaving it on with an empty
    /// FIFO re-enters the handler forever, and it does here too.
    pub fn fifo_irq_pending(&self) -> bool {
        // 1 is "less than half full" and 2 is "empty"; an empty FIFO satisfies both.
        matches!(self.gxstat_irq, 1 | 2)
    }

    /// `GXSTAT`, assembled from the pieces that own each field.
    fn gxstat(&self) -> u32 {
        let mut value = 0u32;
        value |= (self.geometry.matrices.stack_pointer() as u32 & 0x1F) << 8;
        if self.geometry.matrices.overflow || self.geometry.overflow {
            value |= 1 << 15;
        }
        // The FIFO is reported as empty and less than half full, always. See the note on `pending`.
        value |= 1 << 25;
        value |= 1 << 26;
        value |= (self.gxstat_irq as u32) << 30;
        value
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        // The command ports and the FIFO are word-only; a halfword write to one is a driver bug
        // rather than a narrower command.
        if (GXFIFO..PORT_END).contains(&addr) {
            return true;
        }
        let current = self.read32(addr & !3).unwrap_or(0);
        let spliced = if addr & 2 == 0 {
            (current & 0xFFFF_0000) | value as u32
        } else {
            (current & 0xFFFF) | ((value as u32) << 16)
        };
        self.write32(addr & !3, spliced)
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        let word = self.read32(addr & !3)?;
        Some(if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        })
    }

    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        if (GXFIFO..PORT_END).contains(&addr) {
            return true;
        }
        let current = self.read16(addr & !1).unwrap_or(0);
        let spliced = if addr & 1 == 0 {
            (current & 0xFF00) | value as u16
        } else {
            (current & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(addr & !1, spliced)
    }

    pub fn read8(&self, addr: u32) -> Option<u8> {
        let word = self.read32(addr & !3)?;
        Some((word >> ((addr & 3) * 8)) as u8)
    }

    /// A word written to `GXFIFO`: either four packed opcodes or one parameter.
    fn push_fifo(&mut self, value: u32) {
        if self.pending.is_empty() {
            for i in 0..4 {
                self.pending.push_back((value >> (i * 8)) as u8);
            }
            self.run_ready();
            return;
        }
        self.params.push(value);
        self.run_ready();
    }

    /// Execute every queued command whose parameters have all arrived.
    fn run_ready(&mut self) {
        while let Some(&opcode) = self.pending.front() {
            let count = parameter_count(opcode).unwrap_or(0) as usize;
            if self.params.len() < count {
                return;
            }
            self.pending.pop_front();
            let params: Vec<u32> = self.params.drain(..count).collect();
            self.geometry.execute(opcode, &params);
        }
        // Every queued opcode ran, so any parameters left over belonged to nothing.
        self.params.clear();
    }

    /// A word written to a command's own port.
    fn push_port(&mut self, opcode: u8, value: u32) {
        let count = parameter_count(opcode).unwrap_or(0) as usize;
        if count == 0 {
            self.geometry.execute(opcode, &[]);
            return;
        }
        let entry = self.port.get_or_insert_with(|| (opcode, Vec::new()));
        // A write to a different port abandons whatever was part-way through, which is what a
        // driver that interleaves two commands would see.
        if entry.0 != opcode {
            *entry = (opcode, Vec::new());
        }
        entry.1.push(value);
        if entry.1.len() >= count {
            let (opcode, params) = self.port.take().expect("just inserted");
            self.geometry.execute(opcode, &params);
        }
    }

    /// Swap and rasterise, if `SWAP_BUFFERS` asked for it.
    ///
    /// Called at vertical blank, which is when hardware performs the swap — not where the command
    /// appeared in the list.
    pub fn on_vblank(&mut self, vram: &Vram) {
        if !self.geometry.swap_pending {
            return;
        }
        let list = self.geometry.take_display_list();
        render::render(
            &list,
            vram,
            self.clear_color,
            self.clear_depth,
            &mut self.framebuffer,
        );
    }

    pub fn reset(&mut self) {
        self.geometry.reset();
        self.framebuffer = Framebuffer3d::new();
        self.disp3dcnt = 0;
        self.clear_color = 0;
        self.clear_depth = 0x7FFF;
        self.pending.clear();
        self.params.clear();
        self.port = None;
    }
}

impl Savable for Gpu3d {
    fn save(&self, w: &mut StateWriter) {
        self.geometry.save(w);
        w.write_u16(self.disp3dcnt);
        w.write_u32(self.clear_color);
        w.write_u32(self.clear_depth);
        w.write_u8(self.gxstat_irq);
        w.write_u32(self.fog_color);
        w.write_u8(self.alpha_test_ref);
        for value in self.toon.iter() {
            w.write_u16(*value);
        }
        for value in self.edge_color {
            w.write_u16(value);
        }
        // The half-received command: opcodes already popped off `GXFIFO` but still waiting on
        // parameters, and a command being fed through its own port instead of the FIFO. Dropping
        // this used to be excused as "the emulator's state, not the machine's" — but the machine
        // really does hold it: hardware's FIFO is the same 256-entry queue whether it is read
        // through the packed port or the per-command ones, and a game that has written the first
        // half of `MTX_MULT_4X4`'s sixteen words has committed those words to hardware already.
        // Restoring without them means the second half of that matrix arrives with no first half
        // to complete, and is silently interpreted as a new, wrong command instead.
        w.write_u64(self.pending.len() as u64);
        for opcode in &self.pending {
            w.write_u8(*opcode);
        }
        w.write_u64(self.params.len() as u64);
        for param in &self.params {
            w.write_u32(*param);
        }
        w.write_bool(self.port.is_some());
        if let Some((opcode, params)) = &self.port {
            w.write_u8(*opcode);
            w.write_u64(params.len() as u64);
            for param in params {
                w.write_u32(*param);
            }
        }
        // The rendered framebuffer. See `Framebuffer3d`'s own doc comment for why this is not the
        // "regenerated every frame" case it looks like at first.
        self.framebuffer.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.geometry.load(r)?;
        self.disp3dcnt = r.read_u16()?;
        self.clear_color = r.read_u32()?;
        self.clear_depth = r.read_u32()?;
        self.gxstat_irq = r.read_u8()?;
        self.fog_color = r.read_u32()?;
        self.alpha_test_ref = r.read_u8()?;
        for value in self.toon.iter_mut() {
            *value = r.read_u16()?;
        }
        for value in &mut self.edge_color {
            *value = r.read_u16()?;
        }

        let pending_count = r.read_u64()? as usize;
        // Hardware's own FIFO holds 256 entries; a claim well past that is a corrupt blob, not a
        // machine mid-way through an enormous burst.
        if pending_count > 0x400 {
            return Err(StateError::Malformed(format!(
                "the geometry FIFO claims {pending_count} queued opcodes"
            )));
        }
        self.pending.clear();
        for _ in 0..pending_count {
            self.pending.push_back(r.read_u8()?);
        }

        let params_count = r.read_u64()? as usize;
        if params_count > 0x400 {
            return Err(StateError::Malformed(format!(
                "the geometry FIFO claims {params_count} queued parameters"
            )));
        }
        self.params.clear();
        for _ in 0..params_count {
            self.params.push(r.read_u32()?);
        }

        self.port = if r.read_bool()? {
            let opcode = r.read_u8()?;
            let count = r.read_u64()? as usize;
            // 32 is the largest parameter count any command takes (`SHININESS`'s table), so more
            // than that is a corrupt blob rather than a real in-progress command.
            if count > 32 {
                return Err(StateError::Malformed(format!(
                    "a port command claims {count} parameters, more than any command takes"
                )));
            }
            let mut params = Vec::with_capacity(count);
            for _ in 0..count {
                params.push(r.read_u32()?);
            }
            Some((opcode, params))
        } else {
            None
        };

        self.framebuffer.load(r)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
