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

use core_common::{Savable, StateError, StateReader, StateWriter};

use crate::memory::Region;

pub const WAITCNT: u32 = 0x0400_0204;

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
    /// `width` is 1, 2, or 4 bytes.
    pub fn cost(&self, addr: u32, width: u32, access: Access) -> u32 {
        let region = Region::of(addr);
        let wide = width == 4;

        match region {
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
            Region::Rom { wait_state } => {
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
                if wide {
                    one + sequential
                } else {
                    one
                }
            }
            // The save chip is 8 bits wide: every access is one byte, whatever was asked for.
            Region::Sram => 1 + SRAM_WAIT[self.sram_setting()],
            Region::Unmapped => 1,
        }
    }
}

impl Savable for WaitControl {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.value);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.value = r.read_u16()?;
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
        let wait = WaitControl::new();
        for width in [1, 2, 4] {
            assert_eq!(wait.cost(IWRAM, width, Access::NonSequential), 1);
        }
    }

    #[test]
    fn external_work_ram_costs_three_cycles_and_twice_that_for_a_word() {
        // It is on a 16-bit bus with two wait states, so a word is two accesses. This is why a
        // game puts its hot loop in IWRAM.
        let wait = WaitControl::new();
        assert_eq!(wait.cost(EWRAM, 2, Access::NonSequential), 3);
        assert_eq!(wait.cost(EWRAM, 4, Access::NonSequential), 6);
    }

    #[test]
    fn video_memory_costs_one_cycle_but_a_word_costs_two() {
        let wait = WaitControl::new();
        assert_eq!(wait.cost(VRAM, 2, Access::NonSequential), 1);
        assert_eq!(wait.cost(VRAM, 4, Access::NonSequential), 2);
    }

    #[test]
    fn oam_is_thirty_two_bits_wide_unlike_the_memory_beside_it() {
        // Palette RAM and VRAM are 16-bit; OAM is not, so a word costs the same as a halfword.
        let wait = WaitControl::new();
        assert_eq!(wait.cost(0x0700_0000, 4, Access::NonSequential), 1);
    }

    #[test]
    fn a_sequential_rom_access_is_cheaper_than_one_that_jumps() {
        // Why a loop walking forward through ROM is much faster than one chasing pointers.
        let wait = WaitControl::new();
        let jump = wait.cost(ROM0, 2, Access::NonSequential);
        let walk = wait.cost(ROM0, 2, Access::Sequential);
        assert!(walk < jump, "{walk} should be cheaper than {jump}");
    }

    #[test]
    fn the_three_rom_windows_are_timed_independently() {
        // The same ROM at three speeds: a game links its code into whichever window suits, and
        // a flat cost would erase the choice.
        let mut wait = WaitControl::new();
        // Window 0 at the fastest setting, window 2 at the slowest.
        wait.write16((2 << 2) | (3 << 8));
        let fast = wait.cost(ROM0, 2, Access::NonSequential);
        let slow = wait.cost(ROM2, 2, Access::NonSequential);
        assert!(fast < slow, "{fast} against {slow}");
    }

    #[test]
    fn every_first_access_setting_changes_the_cost() {
        let mut wait = WaitControl::new();
        let mut seen = Vec::new();
        for setting in 0..4u16 {
            wait.write16(setting << 2);
            seen.push(wait.cost(ROM0, 2, Access::NonSequential));
        }
        assert_eq!(seen, vec![5, 4, 3, 9], "1 cycle plus the wait states");
    }

    #[test]
    fn the_second_half_of_a_word_access_is_always_sequential() {
        // Even when the first half was a jump: the cartridge bus has latched by then.
        let wait = WaitControl::new();
        let halfword_jump = wait.cost(ROM0, 2, Access::NonSequential);
        let halfword_walk = wait.cost(ROM0, 2, Access::Sequential);
        assert_eq!(
            wait.cost(ROM0, 4, Access::NonSequential),
            halfword_jump + halfword_walk
        );
    }

    #[test]
    fn each_rom_window_has_its_own_sequential_timing() {
        let mut wait = WaitControl::new();
        wait.write16(0);
        let w0 = wait.cost(ROM0, 2, Access::Sequential);
        let w1 = wait.cost(ROM1, 2, Access::Sequential);
        let w2 = wait.cost(ROM2, 2, Access::Sequential);
        assert_eq!((w0, w1, w2), (3, 5, 9), "2, 4, and 8 wait states plus one");
    }

    #[test]
    fn the_fast_sequential_bit_makes_every_window_the_same_speed() {
        let mut wait = WaitControl::new();
        wait.write16((1 << 4) | (1 << 7) | (1 << 10));
        for window in [ROM0, ROM1, ROM2] {
            assert_eq!(wait.cost(window, 2, Access::Sequential), 2);
        }
    }

    #[test]
    fn the_save_chip_is_eight_bits_wide_so_width_does_not_change_its_cost() {
        let wait = WaitControl::new();
        let byte = wait.cost(SRAM, 1, Access::NonSequential);
        assert_eq!(wait.cost(SRAM, 4, Access::NonSequential), byte);
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
