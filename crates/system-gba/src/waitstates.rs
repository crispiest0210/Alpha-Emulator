//! Memory access timing: `WAITCNT` and the per-region cost of a bus access.
//!
//! # Why this is not a fixed cost per access
//!
//! Prompt 12 is explicit that wait states are real, cycle-count-affecting, and checkable by test
//! ROMs, and must not be approximated. The reason is that a GBA's memory is wildly uneven: IWRAM
//! answers in one cycle on a 32-bit bus, EWRAM takes three on a 16-bit one, and the cartridge
//! takes whatever the game configured — so the *same* loop runs at very different speeds
//! depending on which of the three ROM windows it was linked into. Games choose a window
//! deliberately, and a flat cost erases the choice.
//!
//! # Sequential and non-sequential are different numbers
//!
//! A ROM access that follows on from the previous address is faster than one that jumps, because
//! the cartridge bus keeps its address latched. That is why a tight loop that walks forward
//! through ROM is much faster than one that chases pointers, and why this takes an `Access`
//! rather than only an address.
//!
//! # A 32-bit access to a 16-bit bus is two accesses
//!
//! EWRAM, palette RAM, VRAM, OAM, and the cartridge are all 16 bits wide. A word access to any
//! of them costs two halfword accesses, the second of which is always sequential.
//!
//! # The prefetch buffer answers a sequential code fetch before it is asked
//!
//! `WAITCNT` bit 14 turns on a small buffer that walks ahead of the CPU through ROM while nothing
//! else needs the cartridge bus, so that a sequential *instruction* fetch — not a data access, and
//! not one that jumps — usually finds its word already there and costs one cycle rather than the
//! window's configured wait states. Games that link their hot code into a slow ROM window and turn
//! this on depend on that difference; leaving it unmodelled overcharges exactly the code a real
//! cartridge would have made cheap.
//!
//! The buffer is a single bit of state, `prefetch_primed`, rather than a queue with a depth: it is
//! `true` exactly when the previous access was a code fetch with the bit set, which
//! is enough to answer "is the next sequential fetch already there" without modelling how many
//! halfwords ahead a real buffer has actually gotten. [`WaitControl::cost`] takes `&mut self` and
//! an `is_fetch` flag for this reason — an access that is not a code fetch, whatever else is true
//! about it, breaks the run and pays full price, and the *next* access does too until another
//! sequential fetch re-primes it.

use core_common::{Savable, StateError, StateReader, StateWriter};

use crate::memory::Region;

pub const WAITCNT: u32 = 0x0400_0204;
/// Bit 14: the game pak prefetch buffer.
const PREFETCH_ENABLE: u16 = 1 << 14;

/// Whether an access follows on from the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The address continues from the last one, so the cartridge bus keeps its latch.
    Sequential,
    NonSequential,
}

/// First-access wait states, indexed by the two-bit `WAITCNT` setting.
const FIRST_ACCESS: [u32; 4] = [4, 3, 2, 8];
/// Second (sequential) access wait states for each ROM window.
const SEQUENTIAL_0: [u32; 2] = [2, 1];
const SEQUENTIAL_1: [u32; 2] = [4, 1];
const SEQUENTIAL_2: [u32; 2] = [8, 1];
/// SRAM has one setting and no sequential form.
const SRAM_WAIT: [u32; 4] = [4, 3, 2, 8];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WaitControl {
    value: u16,
    /// Whether the prefetch buffer is currently ahead of the CPU, so the next sequential code
    /// fetch from ROM would find its word already there. Set by such a fetch, whether or not that
    /// fetch itself hit the buffer; cleared by anything else that touches the bus.
    prefetch_primed: bool,
}

impl WaitControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (WAITCNT..WAITCNT + 4).contains(&addr)
    }

    pub fn read16(&self) -> u16 {
        self.value
    }

    pub fn write16(&mut self, value: u16) {
        self.value = value;
    }

    fn sram_setting(&self) -> usize {
        (self.value & 3) as usize
    }

    fn rom_first(&self, window: u8) -> usize {
        match window {
            0 => ((self.value >> 2) & 3) as usize,
            1 => ((self.value >> 5) & 3) as usize,
            _ => ((self.value >> 8) & 3) as usize,
        }
    }

    fn rom_sequential(&self, window: u8) -> usize {
        match window {
            0 => ((self.value >> 4) & 1) as usize,
            1 => ((self.value >> 7) & 1) as usize,
            _ => ((self.value >> 10) & 1) as usize,
        }
    }

    /// Cycles an access to `addr` costs, including the one cycle every access takes.
    ///
    /// `width` is 1, 2, or 4 bytes. `is_fetch` is whether this access is the CPU fetching its next
    /// instruction rather than a load or store — only a fetch can prime or hit the prefetch
    /// buffer; see the module docs.
    pub fn cost(&mut self, addr: u32, width: u32, access: Access, is_fetch: bool) -> u32 {
        let region = Region::of(addr);
        let wide = width == 4;

        let Region::Rom { wait_state } = region else {
            // Anything that is not a ROM access breaks whatever run of sequential code fetches
            // the buffer was following, exactly as a non-sequential ROM access or a ROM data
            // access does — the cartridge bus was just used for something the buffer was not
            // walking ahead through.
            self.prefetch_primed = false;
            return match region {
                // The only memory that answers at full width in one cycle.
                Region::IWram | Region::Io | Region::Bios => 1,
                // 16-bit bus, two wait states, so a word costs twice.
                Region::EWram => {
                    if wide {
                        6
                    } else {
                        3
                    }
                }
                // 16-bit bus but no wait state, so only a word costs extra.
                Region::Palette | Region::Vram => {
                    if wide {
                        2
                    } else {
                        1
                    }
                }
                // OAM is 32 bits wide, unlike the palette and VRAM beside it.
                Region::Oam => 1,
                // The save chip is 8 bits wide: every access is one byte, whatever was asked for.
                Region::Sram => 1 + SRAM_WAIT[self.sram_setting()],
                Region::Unmapped => 1,
                Region::Rom { .. } => unreachable!("matched above"),
            };
        };

        let prefetch_enabled = self.value & PREFETCH_ENABLE != 0;
        // The buffer answers only a sequential *code* fetch, and only when it was already primed
        // by the fetch before this one — the first fetch of a run, right after a branch, still
        // pays full price no matter how the register is set.
        let hits_buffer =
            is_fetch && access == Access::Sequential && prefetch_enabled && self.prefetch_primed;

        let first = 1 + FIRST_ACCESS[self.rom_first(wait_state)];
        let sequential = 1 + match wait_state {
            0 => SEQUENTIAL_0[self.rom_sequential(0)],
            1 => SEQUENTIAL_1[self.rom_sequential(1)],
            _ => SEQUENTIAL_2[self.rom_sequential(2)],
        };
        let one = match access {
            Access::Sequential => sequential,
            Access::NonSequential => first,
        };
        // The second half of a word access is always sequential, whatever the first was.
        let full_cost = if wide { one + sequential } else { one };

        // Whether *this* access leaves the buffer primed for the next one: any code fetch does,
        // whether or not this one hit the buffer itself, because either way the buffer is now
        // free to walk ahead from here. Anything else — a data access, reaching ROM this time
        // rather than another region — is exactly the case the module docs call out: it still
        // clears the buffer even though it is a ROM access, which is why this is not folded into
        // the early return above.
        self.prefetch_primed = is_fetch && prefetch_enabled;

        if hits_buffer {
            1
        } else {
            full_cost
        }
    }
}

impl Savable for WaitControl {
    // `prefetch_primed` is not persisted, on the same reasoning as `GbaSystemBus`'s
    // `next_sequential`: both describe a relationship to whatever access came immediately before,
    // a save state is only ever taken between complete instructions, and the cost of losing either
    // one is the same — the single next access pays as if it were the first in a fresh run, which
    // is at most a few cycles once, not a correctness or determinism difference.
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.value);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.value = r.read_u16()?;
        self.prefetch_primed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IWRAM: u32 = 0x0300_0000;
    const EWRAM: u32 = 0x0200_0000;
    const VRAM: u32 = 0x0600_0000;
    const ROM0: u32 = 0x0800_0000;
    const ROM1: u32 = 0x0A00_0000;
    const ROM2: u32 = 0x0C00_0000;
    const SRAM: u32 = 0x0E00_0000;

    #[test]
    fn internal_work_ram_is_the_only_memory_that_answers_at_full_width_in_one_cycle() {
        let mut wait = WaitControl::new();
        for width in [1, 2, 4] {
            assert_eq!(wait.cost(IWRAM, width, Access::NonSequential, false), 1);
        }
    }

    #[test]
    fn external_work_ram_costs_three_cycles_and_twice_that_for_a_word() {
        // It is on a 16-bit bus with two wait states, so a word is two accesses. This is why a
        // game puts its hot loop in IWRAM.
        let mut wait = WaitControl::new();
        assert_eq!(wait.cost(EWRAM, 2, Access::NonSequential, false), 3);
        assert_eq!(wait.cost(EWRAM, 4, Access::NonSequential, false), 6);
    }

    #[test]
    fn video_memory_costs_one_cycle_but_a_word_costs_two() {
        let mut wait = WaitControl::new();
        assert_eq!(wait.cost(VRAM, 2, Access::NonSequential, false), 1);
        assert_eq!(wait.cost(VRAM, 4, Access::NonSequential, false), 2);
    }

    #[test]
    fn oam_is_thirty_two_bits_wide_unlike_the_memory_beside_it() {
        // Palette RAM and VRAM are 16-bit; OAM is not, so a word costs the same as a halfword.
        let mut wait = WaitControl::new();
        assert_eq!(wait.cost(0x0700_0000, 4, Access::NonSequential, false), 1);
    }

    #[test]
    fn a_sequential_rom_access_is_cheaper_than_one_that_jumps() {
        // Why a loop walking forward through ROM is much faster than one chasing pointers.
        let mut wait = WaitControl::new();
        let jump = wait.cost(ROM0, 2, Access::NonSequential, false);
        let walk = wait.cost(ROM0, 2, Access::Sequential, false);
        assert!(walk < jump, "{walk} should be cheaper than {jump}");
    }

    #[test]
    fn the_three_rom_windows_are_timed_independently() {
        // The same ROM at three speeds: a game links its code into whichever window suits, and
        // a flat cost would erase the choice.
        let mut wait = WaitControl::new();
        // Window 0 at the fastest setting, window 2 at the slowest.
        wait.write16((2 << 2) | (3 << 8));
        let fast = wait.cost(ROM0, 2, Access::NonSequential, false);
        let slow = wait.cost(ROM2, 2, Access::NonSequential, false);
        assert!(fast < slow, "{fast} against {slow}");
    }

    #[test]
    fn every_first_access_setting_changes_the_cost() {
        let mut wait = WaitControl::new();
        let mut seen = Vec::new();
        for setting in 0..4u16 {
            wait.write16(setting << 2);
            seen.push(wait.cost(ROM0, 2, Access::NonSequential, false));
        }
        assert_eq!(seen, vec![5, 4, 3, 9], "1 cycle plus the wait states");
    }

    #[test]
    fn the_second_half_of_a_word_access_is_always_sequential() {
        // Even when the first half was a jump: the cartridge bus has latched by then.
        let mut wait = WaitControl::new();
        let halfword_jump = wait.cost(ROM0, 2, Access::NonSequential, false);
        let halfword_walk = wait.cost(ROM0, 2, Access::Sequential, false);
        assert_eq!(
            wait.cost(ROM0, 4, Access::NonSequential, false),
            halfword_jump + halfword_walk
        );
    }

    #[test]
    fn each_rom_window_has_its_own_sequential_timing() {
        let mut wait = WaitControl::new();
        wait.write16(0);
        let w0 = wait.cost(ROM0, 2, Access::Sequential, false);
        let w1 = wait.cost(ROM1, 2, Access::Sequential, false);
        let w2 = wait.cost(ROM2, 2, Access::Sequential, false);
        assert_eq!((w0, w1, w2), (3, 5, 9), "2, 4, and 8 wait states plus one");
    }

    #[test]
    fn the_fast_sequential_bit_makes_every_window_the_same_speed() {
        let mut wait = WaitControl::new();
        wait.write16((1 << 4) | (1 << 7) | (1 << 10));
        for window in [ROM0, ROM1, ROM2] {
            assert_eq!(wait.cost(window, 2, Access::Sequential, false), 2);
        }
    }

    #[test]
    fn the_save_chip_is_eight_bits_wide_so_width_does_not_change_its_cost() {
        let mut wait = WaitControl::new();
        let byte = wait.cost(SRAM, 1, Access::NonSequential, false);
        assert_eq!(wait.cost(SRAM, 4, Access::NonSequential, false), byte);
    }

    #[test]
    fn a_sequential_code_fetch_costs_one_cycle_once_the_buffer_is_primed() {
        // The first fetch of a run pays full price: nothing has been prefetched yet. Every
        // sequential code fetch after it finds its word already there.
        let mut wait = WaitControl::new();
        wait.write16(1 << 14); // prefetch enabled, default wait states otherwise
        let first = wait.cost(ROM0, 2, Access::NonSequential, true);
        let second = wait.cost(ROM0, 2, Access::Sequential, true);
        let third = wait.cost(ROM0, 2, Access::Sequential, true);
        assert!(second < first, "{second} should be cheaper than {first}");
        assert_eq!(second, 1, "the minimum bus-independent cost");
        assert_eq!(third, 1, "and it stays cheap as the run continues");
    }

    #[test]
    fn a_data_access_to_rom_invalidates_the_prefetch_buffer() {
        // Reading ROM as data — this machine allows it, and some games do — uses the one
        // cartridge bus the buffer also needs, so the run of code fetches it was following breaks
        // exactly as a jump would break it, even though the address itself is still sequential.
        let mut wait = WaitControl::new();
        wait.write16(1 << 14);
        wait.cost(ROM0, 2, Access::NonSequential, true); // primes the buffer
        wait.cost(ROM0, 2, Access::Sequential, true); // and this one hits it
        let data = wait.cost(ROM0, 2, Access::Sequential, false);
        let after = wait.cost(ROM0, 2, Access::Sequential, true);
        assert!(data > 1, "a data access never hits the buffer: {data}");
        assert!(
            after > 1,
            "and it invalidated the buffer for the fetch after it too: {after}"
        );
    }

    #[test]
    fn the_prefetch_buffer_does_nothing_when_the_bit_is_clear() {
        let mut fetch = WaitControl::new();
        let mut data = WaitControl::new();
        fetch.cost(ROM0, 2, Access::NonSequential, true);
        data.cost(ROM0, 2, Access::NonSequential, false);
        assert_eq!(
            fetch.cost(ROM0, 2, Access::Sequential, true),
            data.cost(ROM0, 2, Access::Sequential, false),
            "is_fetch changes nothing when WAITCNT's bit is not set"
        );
    }

    #[test]
    fn wait_control_reads_back_what_was_written() {
        let mut wait = WaitControl::new();
        wait.write16(0x4317);
        assert_eq!(wait.read16(), 0x4317);
    }

    #[test]
    fn the_register_claims_its_word_and_no_more() {
        assert!(WaitControl::owns(WAITCNT));
        assert!(WaitControl::owns(WAITCNT + 3));
        assert!(!WaitControl::owns(WAITCNT - 1), "that is IME's neighbour");
        assert!(!WaitControl::owns(WAITCNT + 4));
    }

    #[test]
    fn wait_state_configuration_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut wait = WaitControl::new();
        wait.write16(0x4014);
        let bytes = encode_state("gba-wait", 1, &wait);
        let mut restored = WaitControl::new();
        decode_state("gba-wait", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored, wait);
    }
}
