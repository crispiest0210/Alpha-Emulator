//! Turning a live machine into a displayable snapshot.
//!
//! # Why a snapshot rather than a live handle
//!
//! The panel that displays this runs on the drawing thread; the machine lives on the emulation
//! thread. Handing the panel a `&dyn DebugTarget` would mean either a lock held across a frame of
//! `egui` layout, or a machine that is not running while the debugger is open — and the first of
//! those stalls emulation on the renderer, which is exactly the coupling `frontend-core`'s frame
//! pipe exists to avoid.
//!
//! So the emulation thread [`capture`]s a snapshot when asked, a few times a second, and sends it
//! across. The cost is that the display can be a frame or two stale, which for a *paused* machine is
//! no cost at all and for a running one is what a debugger view of a running machine inherently is.
//!
//! # `Option<u8>`, all the way to the screen
//!
//! [`DebugTarget::peek8`] answers `None` where a side-effect-free read is impossible, and that
//! `None` survives into [`MemoryRow`] and out to a hex viewer showing `--`. Substituting a zero
//! anywhere along the way would be the debugger inventing data, which is worse than a gap: a gap is
//! obviously missing, and a zero looks like a fact.

use crate::Breakpoints;
use crate::Watchpoint;
use core_common::{DebugRegion, DebugTarget, RegisterValue};

/// One line of a disassembly view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisasmLine {
    pub addr: u32,
    /// The rendered instruction, or a marker where memory could not be read.
    pub text: String,
    /// Encoded length, or 0 when the instruction could not be decoded.
    pub length: u8,
    /// Whether this is the instruction about to execute.
    pub is_program_counter: bool,
    /// Whether an execution breakpoint is set here.
    pub has_breakpoint: bool,
    /// The raw bytes, for the reader who wants the encoding rather than the mnemonic.
    pub bytes: Vec<u8>,
}

/// Bytes per row in the hex viewer. Sixteen is what every hex editor uses and what makes an
/// address's low nibble readable as a column.
pub const BYTES_PER_ROW: usize = 16;

/// One row of a hex viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRow {
    pub addr: u32,
    /// `None` where the address could not be read without side effects. Shown as `--`.
    pub bytes: Vec<Option<u8>>,
}

impl MemoryRow {
    /// The row rendered as printable ASCII, with `.` for anything else and a space for a byte that
    /// could not be read.
    ///
    /// A space rather than `.` for an unreadable byte, deliberately: `.` already means "a byte that
    /// is not printable", and using it for both would make an I/O register indistinguishable from a
    /// byte of value 0x01.
    pub fn ascii(&self) -> String {
        self.bytes
            .iter()
            .map(|byte| match byte {
                Some(byte) if byte.is_ascii_graphic() || *byte == b' ' => *byte as char,
                Some(_) => '.',
                None => ' ',
            })
            .collect()
    }
}

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// Where to start disassembling. `None` means "at the program counter", which is what a view
    /// that has not been scrolled wants.
    pub disassembly_at: Option<u32>,
    pub disassembly_lines: usize,
    pub memory_at: u32,
    pub memory_rows: usize,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            disassembly_at: None,
            // Enough to fill a panel without making the snapshot expensive: each line is a peek per
            // byte, so a hundred lines would be a few hundred peeks several times a second.
            disassembly_lines: 24,
            memory_at: 0,
            memory_rows: 16,
        }
    }
}

impl Request {
    /// Clamp the sizes so a caller cannot ask for a snapshot that takes a visible amount of time.
    ///
    /// The emulation thread serves these between frames, and it has 16.7 ms for everything. A
    /// request for a million rows would not be malicious, it would be an off-by-one in a scroll
    /// calculation — and the failure would look like the emulator stuttering.
    pub fn clamped(mut self) -> Self {
        self.disassembly_lines = self.disassembly_lines.min(512);
        self.memory_rows = self.memory_rows.min(512);
        self
    }
}

/// Everything a debugger panel displays, captured at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub registers: Vec<RegisterValue>,
    pub program_counter: u32,
    pub flags: String,
    pub halted: bool,
    /// Hex digits an address takes on this machine: 4 for a Game Boy, 8 for a GBA.
    pub address_digits: u8,
    pub regions: &'static [DebugRegion],
    pub disassembly: Vec<DisasmLine>,
    pub memory: Vec<MemoryRow>,
    /// Execution breakpoints currently set, for the breakpoint list.
    pub execution_breakpoints: Vec<u32>,
    /// Watchpoints currently set. Carried in the snapshot for the same reason the breakpoints are:
    /// the panel that lists them has no access to the registry.
    pub watchpoints: Vec<Watchpoint>,
}

impl Snapshot {
    /// Format an address at this machine's natural width.
    pub fn format_address(&self, addr: u32) -> String {
        format!("{addr:0>width$X}", width = self.address_digits as usize)
    }

    /// The name of the region an address falls in, if any.
    pub fn region_of(&self, addr: u32) -> Option<&'static str> {
        self.regions
            .iter()
            .find(|region| region.contains(addr))
            .map(|region| region.name)
    }
}

/// Read a machine into a snapshot.
///
/// Never mutates the target: every call here is `&self` on [`DebugTarget`], which is what makes it
/// safe to do while the machine is running.
pub fn capture(target: &dyn DebugTarget, breakpoints: &Breakpoints, request: &Request) -> Snapshot {
    let request = request.clamped();
    let program_counter = target.program_counter();
    let start = request.disassembly_at.unwrap_or(program_counter);

    let mut disassembly = Vec::with_capacity(request.disassembly_lines);
    let mut addr = start;
    for _ in 0..request.disassembly_lines {
        let decoded = target.disassemble(addr);
        let length = decoded.as_ref().map(|d| d.length).unwrap_or(0);
        let bytes: Vec<u8> = (0..length.max(1) as u32)
            .filter_map(|offset| target.peek8(addr.wrapping_add(offset)))
            .collect();
        disassembly.push(DisasmLine {
            addr,
            text: match &decoded {
                Some(decoded) => decoded.text.clone(),
                // Not "???" — say *why*. An unreadable address and an undefined encoding are
                // different problems, and the disassemblers already render the second as undefined
                // text, so reaching here means the first.
                None => "(unreadable)".to_string(),
            },
            length,
            is_program_counter: addr == program_counter,
            has_breakpoint: breakpoints.execution_breakpoints().contains(&addr),
            bytes,
        });
        // A zero length would loop forever on the same address. Advancing by one instead walks
        // through the unreadable region a byte at a time, which is what a reader scrolling into
        // unmapped space should see.
        addr = addr.wrapping_add(if length == 0 { 1 } else { length as u32 });
    }

    let mut memory = Vec::with_capacity(request.memory_rows);
    for row in 0..request.memory_rows {
        let base = request.memory_at.wrapping_add((row * BYTES_PER_ROW) as u32);
        memory.push(MemoryRow {
            addr: base,
            bytes: (0..BYTES_PER_ROW as u32)
                .map(|offset| target.peek8(base.wrapping_add(offset)))
                .collect(),
        });
    }

    Snapshot {
        registers: target.registers(),
        program_counter,
        flags: target.flags_summary(),
        halted: target.is_halted(),
        address_digits: target.address_digits(),
        regions: target.regions(),
        disassembly,
        memory,
        execution_breakpoints: breakpoints.execution_breakpoints().to_vec(),
        watchpoints: breakpoints.watchpoints().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_common::DisasmInstruction;

    const REGIONS: &[DebugRegion] = &[
        DebugRegion::new("ROM", 0x0000, 0x7FFF),
        DebugRegion::new("RAM", 0xC000, 0xDFFF),
    ];

    /// A machine made of a byte array, with two-byte instructions and a hole that cannot be peeked.
    ///
    /// A fake rather than a real system, because what is under test is the *capture* — how a refusal
    /// propagates, how a zero length is handled, where the highlight lands. Driving a real Game Boy
    /// would make those cases hard to arrange and would not exercise them any better.
    struct Fake {
        bytes: Vec<u8>,
        pc: u32,
        /// Addresses that refuse to be peeked, standing in for MMIO.
        unreadable: Vec<u32>,
        /// When set, `disassemble` refuses everywhere — an undecodable machine.
        undecodable: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                // Sixteen two-byte "instructions".
                bytes: (0..32u8).collect(),
                pc: 4,
                unreadable: Vec::new(),
                undecodable: false,
            }
        }
    }

    impl DebugTarget for Fake {
        fn registers(&self) -> Vec<RegisterValue> {
            vec![RegisterValue::new("A", 0x42, 8)]
        }

        fn program_counter(&self) -> u32 {
            self.pc
        }

        fn set_program_counter(&mut self, pc: u32) {
            self.pc = pc;
        }

        fn flags_summary(&self) -> String {
            "Z-H-".into()
        }

        fn is_halted(&self) -> bool {
            false
        }

        fn peek8(&self, addr: u32) -> Option<u8> {
            if self.unreadable.contains(&addr) {
                return None;
            }
            self.bytes.get(addr as usize).copied()
        }

        fn disassemble(&self, addr: u32) -> Option<DisasmInstruction> {
            if self.undecodable {
                return None;
            }
            let a = self.peek8(addr)?;
            let b = self.peek8(addr + 1)?;
            Some(DisasmInstruction {
                text: format!("op {a:02X},{b:02X}"),
                length: 2,
            })
        }

        fn regions(&self) -> &'static [DebugRegion] {
            REGIONS
        }

        fn address_digits(&self) -> u8 {
            4
        }
    }

    fn capture_default(target: &Fake, breakpoints: &Breakpoints) -> Snapshot {
        capture(
            target,
            breakpoints,
            &Request {
                disassembly_lines: 4,
                memory_rows: 1,
                ..Request::default()
            },
        )
    }

    #[test]
    fn disassembly_starts_at_the_program_counter_by_default() {
        let snapshot = capture_default(&Fake::new(), &Breakpoints::new());
        assert_eq!(snapshot.disassembly[0].addr, 4);
        assert!(snapshot.disassembly[0].is_program_counter);
        assert!(
            !snapshot.disassembly[1].is_program_counter,
            "exactly one line is the PC"
        );
    }

    #[test]
    fn disassembly_advances_by_each_instructions_own_length() {
        let snapshot = capture_default(&Fake::new(), &Breakpoints::new());
        let addresses: Vec<_> = snapshot.disassembly.iter().map(|line| line.addr).collect();
        assert_eq!(addresses, vec![4, 6, 8, 10]);
    }

    #[test]
    fn an_explicit_start_overrides_the_program_counter() {
        let snapshot = capture(
            &Fake::new(),
            &Breakpoints::new(),
            &Request {
                disassembly_at: Some(0),
                disassembly_lines: 2,
                memory_rows: 0,
                ..Request::default()
            },
        );
        assert_eq!(snapshot.disassembly[0].addr, 0);
        assert!(
            !snapshot.disassembly[0].is_program_counter,
            "the highlight follows the PC, not the scroll position"
        );
    }

    #[test]
    fn breakpoints_are_marked_on_the_lines_they_are_set_at() {
        let mut breakpoints = Breakpoints::new();
        breakpoints.add_execution(8);
        let snapshot = capture_default(&Fake::new(), &breakpoints);

        let marked: Vec<_> = snapshot
            .disassembly
            .iter()
            .filter(|line| line.has_breakpoint)
            .map(|line| line.addr)
            .collect();
        assert_eq!(marked, vec![8]);
        assert_eq!(snapshot.execution_breakpoints, vec![8]);
    }

    #[test]
    fn an_undecodable_address_says_so_and_the_walk_still_advances() {
        // The bug this guards against is an infinite loop: a zero-length instruction at the same
        // address forever, which would hang the emulation thread inside a debugger request.
        let mut fake = Fake::new();
        fake.undecodable = true;
        let snapshot = capture_default(&fake, &Breakpoints::new());

        assert_eq!(snapshot.disassembly[0].text, "(unreadable)");
        assert_eq!(snapshot.disassembly[0].length, 0);
        let addresses: Vec<_> = snapshot.disassembly.iter().map(|line| line.addr).collect();
        assert_eq!(addresses, vec![4, 5, 6, 7], "advanced one byte at a time");
    }

    #[test]
    fn an_unreadable_byte_stays_unreadable_all_the_way_out() {
        let mut fake = Fake::new();
        fake.unreadable = vec![0xC002, 0xC003];
        let snapshot = capture(
            &fake,
            &Breakpoints::new(),
            &Request {
                memory_at: 0xC000,
                memory_rows: 1,
                disassembly_lines: 0,
                ..Request::default()
            },
        );
        let row = &snapshot.memory[0];
        assert_eq!(row.addr, 0xC000);
        assert_eq!(row.bytes.len(), BYTES_PER_ROW);
        assert_eq!(row.bytes[2], None, "a refusal must not become a zero");
        assert_eq!(row.bytes[3], None);
    }

    #[test]
    fn memory_rows_are_contiguous_and_sixteen_wide() {
        let snapshot = capture(
            &Fake::new(),
            &Breakpoints::new(),
            &Request {
                memory_at: 0,
                memory_rows: 2,
                disassembly_lines: 0,
                ..Request::default()
            },
        );
        assert_eq!(snapshot.memory[0].addr, 0x00);
        assert_eq!(snapshot.memory[1].addr, 0x10);
        assert_eq!(snapshot.memory[0].bytes[0], Some(0));
        assert_eq!(snapshot.memory[1].bytes[0], Some(16));
    }

    #[test]
    fn the_ascii_column_distinguishes_unprintable_from_unreadable() {
        let row = MemoryRow {
            addr: 0,
            bytes: vec![Some(b'A'), Some(0x01), None, Some(b' ')],
        };
        assert_eq!(
            row.ascii(),
            "A. \u{20}",
            "printable, unprintable, unreadable, space"
        );
    }

    #[test]
    fn addresses_format_at_the_machines_width() {
        let snapshot = capture_default(&Fake::new(), &Breakpoints::new());
        assert_eq!(snapshot.format_address(0xC0), "00C0");
        assert_eq!(snapshot.address_digits, 4);
    }

    #[test]
    fn regions_are_reported_by_name() {
        let snapshot = capture_default(&Fake::new(), &Breakpoints::new());
        assert_eq!(snapshot.region_of(0x0100), Some("ROM"));
        assert_eq!(snapshot.region_of(0xC000), Some("RAM"));
        assert_eq!(
            snapshot.region_of(0x9000),
            None,
            "an unmapped gap is unnamed"
        );
    }

    #[test]
    fn registers_and_flags_come_through() {
        let snapshot = capture_default(&Fake::new(), &Breakpoints::new());
        assert_eq!(snapshot.registers.len(), 1);
        assert_eq!(snapshot.registers[0].name, "A");
        assert_eq!(snapshot.flags, "Z-H-");
        assert!(!snapshot.halted);
    }

    #[test]
    fn an_absurd_request_is_clamped_rather_than_served() {
        let request = Request {
            disassembly_lines: usize::MAX,
            memory_rows: usize::MAX,
            ..Request::default()
        }
        .clamped();
        assert_eq!(request.disassembly_lines, 512);
        assert_eq!(request.memory_rows, 512);

        // And `capture` applies it, so the emulation thread cannot be asked to spend a second here.
        let snapshot = capture(
            &Fake::new(),
            &Breakpoints::new(),
            &Request {
                disassembly_lines: usize::MAX,
                memory_rows: usize::MAX,
                ..Request::default()
            },
        );
        assert_eq!(snapshot.disassembly.len(), 512);
        assert_eq!(snapshot.memory.len(), 512);
    }

    #[test]
    fn a_walk_off_the_end_of_the_address_space_wraps_rather_than_panicking() {
        let snapshot = capture(
            &Fake::new(),
            &Breakpoints::new(),
            &Request {
                disassembly_at: Some(u32::MAX),
                disassembly_lines: 3,
                memory_at: u32::MAX - 4,
                memory_rows: 2,
            },
        );
        assert_eq!(snapshot.disassembly.len(), 3);
        assert_eq!(snapshot.memory.len(), 2);
    }
}
