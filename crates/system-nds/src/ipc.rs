//! Inter-processor communication: `IPCSYNC` and the two 16-word FIFOs.
//!
//! This is the hardware the two cores actually talk through, and it is the reason prompt 13 can
//! specify cooperative interleaving rather than threads: every synchronization primitive the DS
//! gives software is a register one core writes and the other polls or takes an interrupt from.
//! Nothing here is a memory barrier or an atomic, so nothing here needs one.
//!
//! # One object, two sides
//!
//! There is exactly one `Ipc` in the machine, not one per core. The ARM9's send FIFO *is* the
//! ARM7's receive FIFO — the same sixteen words — and `IPCSYNC` is a pair of nibbles each core
//! writes and the other reads. Giving each core its own object and synchronizing them would
//! recreate the thing the hardware already does in one place.
//!
//! # Interrupts are latched, not delivered
//!
//! A FIFO push can raise an interrupt on the *other* core, and this module has no access to
//! either interrupt controller. So it latches what it wants raised and the system assembly drains
//! it with [`Ipc::take_pending`] after each access. That keeps this a unit that can be tested on
//! its own, which is the whole point of building it before the machine around it.
//!
//! # Edges, not levels
//!
//! Both FIFO interrupts are edge-triggered and the edges are easy to get backwards. The receive
//! interrupt fires when the receive FIFO goes from empty to non-empty — not while it is
//! non-empty. The send interrupt fires when the send FIFO goes from non-empty to empty. A game
//! that pushes two words in a row gets one interrupt, and an implementation that raises on level
//! floods the other core with interrupts it never returns from.

use crate::Core;
use core_common::{Savable, StateError, StateReader, StateWriter};

/// Words each direction holds. Both FIFOs are this deep on hardware.
pub const FIFO_DEPTH: usize = 16;

/// Which of this module's interrupts a core has pending.
///
/// Returned rather than raised: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IpcIrqs {
    /// The other core strobed `IPCSYNC` bit 13 and this core has bit 14 set.
    pub sync: bool,
    /// This core's send FIFO went empty with its send-empty interrupt enabled.
    pub send_empty: bool,
    /// This core's receive FIFO went non-empty with its receive interrupt enabled.
    pub recv_not_empty: bool,
}

impl IpcIrqs {
    pub fn any(self) -> bool {
        self.sync || self.send_empty || self.recv_not_empty
    }
}

/// A 16-word queue, stored as a ring so a save state is a fixed size regardless of how full it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fifo {
    words: [u32; FIFO_DEPTH],
    head: u8,
    len: u8,
}

impl Default for Fifo {
    fn default() -> Self {
        Self {
            words: [0; FIFO_DEPTH],
            head: 0,
            len: 0,
        }
    }
}

impl Fifo {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len as usize == FIFO_DEPTH
    }

    fn push(&mut self, word: u32) -> bool {
        if self.is_full() {
            return false;
        }
        let tail = (self.head as usize + self.len as usize) % FIFO_DEPTH;
        self.words[tail] = word;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<u32> {
        if self.is_empty() {
            return None;
        }
        let word = self.words[self.head as usize];
        self.head = ((self.head as usize + 1) % FIFO_DEPTH) as u8;
        self.len -= 1;
        Some(word)
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

/// Per-core control state. Everything here is written by one core and mostly read by it too;
/// the cross-core parts are the `IPCSYNC` output nibble and the FIFO the other side drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Side {
    /// The four bits this core presents to the other through `IPCSYNC`.
    sync_out: u8,
    /// Whether a strobe from the other core raises an interrupt here.
    sync_irq_enable: bool,
    fifo_enable: bool,
    send_empty_irq: bool,
    recv_not_empty_irq: bool,
    /// Set by a send into a full FIFO or a receive from an empty one, and cleared only by writing
    /// a 1 to bit 14. It is sticky on purpose: software checks it after a burst, not per word.
    error: bool,
    /// The last word this core successfully received, which is what a read of an empty FIFO
    /// returns. Hardware repeats the last word rather than returning zero, and software that
    /// mis-tracks its own queue depth then reads a plausible value instead of an obvious one.
    last_recv: u32,
    pending: IpcIrqs,
}

/// `IPCSYNC`, `IPCFIFOCNT`, and the two FIFOs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ipc {
    sides: [Side; 2],
    /// Indexed by the *receiving* core: `queues[Core::Arm7]` is what the ARM9 has sent.
    queues: [Fifo; 2],
}

impl Ipc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take and clear whatever this core should be interrupted for.
    pub fn take_pending(&mut self, core: Core) -> IpcIrqs {
        std::mem::take(&mut self.sides[core as usize].pending)
    }

    /// `IPCSYNC` as this core reads it: the other core's nibble low, its own high.
    pub fn read_sync(&self, core: Core) -> u32 {
        let me = &self.sides[core as usize];
        let them = &self.sides[core.other() as usize];
        (them.sync_out as u32) | ((me.sync_out as u32) << 8) | ((me.sync_irq_enable as u32) << 14)
    }

    /// Write `IPCSYNC`. Bit 13 is a strobe, not a stored bit — it raises an interrupt on the
    /// other core if that core has enabled one, and is not readable afterwards.
    pub fn write_sync(&mut self, core: Core, value: u32) {
        {
            let me = &mut self.sides[core as usize];
            me.sync_out = ((value >> 8) & 0x0F) as u8;
            me.sync_irq_enable = value & (1 << 14) != 0;
        }
        if value & (1 << 13) != 0 {
            let them = &mut self.sides[core.other() as usize];
            if them.sync_irq_enable {
                them.pending.sync = true;
            }
        }
    }

    /// `IPCFIFOCNT` as this core reads it.
    pub fn read_control(&self, core: Core) -> u16 {
        let me = &self.sides[core as usize];
        let send = &self.queues[core.other() as usize];
        let recv = &self.queues[core as usize];
        (send.is_empty() as u16)
            | ((send.is_full() as u16) << 1)
            | ((me.send_empty_irq as u16) << 2)
            | ((recv.is_empty() as u16) << 8)
            | ((recv.is_full() as u16) << 9)
            | ((me.recv_not_empty_irq as u16) << 10)
            | ((me.error as u16) << 14)
            | ((me.fifo_enable as u16) << 15)
    }

    /// Write `IPCFIFOCNT`.
    ///
    /// Three of the sixteen bits are not storage: bit 3 clears this core's send FIFO, bit 14
    /// acknowledges the error flag, and enabling either interrupt while its condition already
    /// holds raises it immediately — which is how software arms the send-empty interrupt at all,
    /// since the FIFO is empty at the moment it wants to be told about.
    pub fn write_control(&mut self, core: Core, value: u16) {
        let send_irq = value & (1 << 2) != 0;
        let recv_irq = value & (1 << 10) != 0;

        if value & (1 << 3) != 0 {
            self.queues[core.other() as usize].clear();
        }
        let send_empty = self.queues[core.other() as usize].is_empty();
        let recv_has_data = !self.queues[core as usize].is_empty();

        let me = &mut self.sides[core as usize];
        // Rising edge only: re-writing the register with the bit already set does not re-raise.
        if send_irq && !me.send_empty_irq && send_empty {
            me.pending.send_empty = true;
        }
        if recv_irq && !me.recv_not_empty_irq && recv_has_data {
            me.pending.recv_not_empty = true;
        }
        me.send_empty_irq = send_irq;
        me.recv_not_empty_irq = recv_irq;
        me.fifo_enable = value & (1 << 15) != 0;
        if value & (1 << 14) != 0 {
            me.error = false;
        }
    }

    /// Push a word into this core's send FIFO — which is the other core's receive FIFO.
    pub fn send(&mut self, core: Core, word: u32) {
        if !self.sides[core as usize].fifo_enable {
            // Disabled FIFOs swallow sends without recording an error. The word is simply not
            // stored, which is why software enables the FIFO before its first push.
            return;
        }
        let was_empty = self.queues[core.other() as usize].is_empty();
        if !self.queues[core.other() as usize].push(word) {
            self.sides[core as usize].error = true;
            return;
        }
        if was_empty {
            let them = &mut self.sides[core.other() as usize];
            if them.recv_not_empty_irq {
                them.pending.recv_not_empty = true;
            }
        }
    }

    /// Pop a word from this core's receive FIFO.
    ///
    /// Reading an empty FIFO is not an error software is prevented from making: it sets the error
    /// flag and returns the last word that *was* read.
    pub fn receive(&mut self, core: Core) -> u32 {
        if !self.sides[core as usize].fifo_enable {
            // A disabled FIFO does not pop. It repeats the last word this core actually
            // received, even when words are queued behind it — enabling the FIFO later then
            // finds them still there.
            return self.sides[core as usize].last_recv;
        }
        match self.queues[core as usize].pop() {
            Some(word) => {
                self.sides[core as usize].last_recv = word;
                if self.queues[core as usize].is_empty() {
                    let them = &mut self.sides[core.other() as usize];
                    if them.send_empty_irq {
                        them.pending.send_empty = true;
                    }
                }
                word
            }
            None => {
                let me = &mut self.sides[core as usize];
                me.error = true;
                me.last_recv
            }
        }
    }

    /// The word at the head of this core's receive FIFO, without popping. For the debugger.
    pub fn peek_receive(&self, core: Core) -> u32 {
        let queue = &self.queues[core as usize];
        if queue.is_empty() {
            self.sides[core as usize].last_recv
        } else {
            queue.words[queue.head as usize]
        }
    }

    /// How many words are waiting for this core. Tests and the debugger only.
    pub fn receive_len(&self, core: Core) -> usize {
        self.queues[core as usize].len as usize
    }
}

impl Savable for Ipc {
    fn save(&self, w: &mut StateWriter) {
        for side in &self.sides {
            w.write_u8(side.sync_out);
            w.write_bool(side.sync_irq_enable);
            w.write_bool(side.fifo_enable);
            w.write_bool(side.send_empty_irq);
            w.write_bool(side.recv_not_empty_irq);
            w.write_bool(side.error);
            w.write_u32(side.last_recv);
            w.write_bool(side.pending.sync);
            w.write_bool(side.pending.send_empty);
            w.write_bool(side.pending.recv_not_empty);
        }
        for queue in &self.queues {
            for word in queue.words {
                w.write_u32(word);
            }
            w.write_u8(queue.head);
            w.write_u8(queue.len);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for side in &mut self.sides {
            side.sync_out = r.read_u8()?;
            side.sync_irq_enable = r.read_bool()?;
            side.fifo_enable = r.read_bool()?;
            side.send_empty_irq = r.read_bool()?;
            side.recv_not_empty_irq = r.read_bool()?;
            side.error = r.read_bool()?;
            side.last_recv = r.read_u32()?;
            side.pending.sync = r.read_bool()?;
            side.pending.send_empty = r.read_bool()?;
            side.pending.recv_not_empty = r.read_bool()?;
        }
        for queue in &mut self.queues {
            for word in &mut queue.words {
                *word = r.read_u32()?;
            }
            queue.head = r.read_u8()?;
            queue.len = r.read_u8()?;
            if queue.len as usize > FIFO_DEPTH || queue.head as usize >= FIFO_DEPTH {
                return Err(StateError::Malformed(
                    "IPC FIFO ring indices are out of range".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
