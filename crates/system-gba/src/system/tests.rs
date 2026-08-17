use super::*;

/// A ROM whose entry point is an infinite branch to itself.
fn spin_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    // `EAFFFFFE` is `b .` — a branch to the instruction's own address.
    rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    rom
}

fn system() -> GbaSystem {
    GbaSystem::new(spin_rom(), None).expect("the synthetic ROM is valid")
}

/// A ROM holding `count` copies of `mov r0, r0` in a row from the entry point.
///
/// Cartridge ROM is read-only on real hardware — a bus write to it is a no-op — so a test that
/// wants specific ROM-resident code has to bake it into the image at construction rather than
/// writing it in afterward the way RAM tests do.
fn nop_rom(count: usize) -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    for i in 0..count {
        rom[i * 4..i * 4 + 4].copy_from_slice(&0xE1A0_0000u32.to_le_bytes()); // mov r0, r0
    }
    rom
}

#[test]
fn a_machine_with_no_bios_starts_at_the_cartridge_entry_point() {
    // The documented post-boot state, so the emulator is usable without a BIOS to source.
    let gba = system();
    assert_eq!(gba.cpu().regs.pc(), CARTRIDGE_ENTRY);
    assert!(
        !gba.cpu().cpsr.irq_disabled(),
        "the BIOS has finished with them"
    );
}

#[test]
fn a_machine_with_a_bios_starts_at_the_reset_vector_instead() {
    let gba = GbaSystem::new(spin_rom(), Some(vec![0u8; 0x4000])).unwrap();
    assert_eq!(gba.cpu().regs.pc(), 0);
}

#[test]
fn the_boot_stacks_are_installed_for_every_mode_that_needs_one() {
    let gba = system();
    assert_eq!(gba.cpu().regs.read(Mode::System, 13), SP_SYSTEM);
    assert_eq!(gba.cpu().regs.read(Mode::Irq, 13), SP_IRQ);
    assert_eq!(gba.cpu().regs.read(Mode::Supervisor, 13), SP_SUPERVISOR);
}

#[test]
fn a_frame_runs_until_vertical_blanking() {
    let mut gba = system();
    assert_eq!(gba.bus().video.vcount(), 0);
    gba.step_frame(InputState::default());
    assert_eq!(
        gba.bus().video.vcount() as u32,
        SCREEN_HEIGHT,
        "it stopped at the top of vertical blanking"
    );
}

#[test]
fn a_frame_reports_roughly_a_frames_worth_of_cycles() {
    let mut gba = system();
    let output = gba.step_frame(InputState::default());
    let elapsed = output.cycles_elapsed.get();
    assert!(
        elapsed > FRAME_CYCLES / 2 && elapsed < FRAME_CYCLES * 2,
        "{elapsed} cycles is not a plausible frame"
    );
}

#[test]
fn successive_frames_keep_the_display_running() {
    let mut gba = system();
    for _ in 0..3 {
        gba.step_frame(InputState::default());
    }
    assert!(gba.bus().video.in_vblank());
}

#[test]
fn a_frame_produces_audio_even_with_nothing_playing() {
    // Silence is still samples: a frontend starved of them underruns just as audibly as one
    // given the wrong ones.
    let mut gba = system();
    gba.step_frame(InputState::default());
    let count = gba.take_audio_samples().len();
    assert!(count > 500, "only {count} samples for a frame");
}

#[test]
fn audio_is_drained_exactly_once_per_frame() {
    let mut gba = system();
    gba.step_frame(InputState::default());
    assert!(!gba.take_audio_samples().is_empty());
    assert!(gba.take_audio_samples().is_empty(), "already taken");
}

#[test]
fn the_cpu_reaches_every_region_through_one_bus() {
    let mut gba = system();
    let bus = gba.bus_mut();

    bus.write32(0x0200_0000, 0x1234_5678);
    assert_eq!(bus.read32(0x0200_0000), 0x1234_5678, "EWRAM");

    bus.write32(0x0300_0000, 0xDEAD_BEEF);
    assert_eq!(bus.read32(0x0300_0000), 0xDEAD_BEEF, "IWRAM");

    bus.write16(0x0500_0000, 0x7FFF);
    assert_eq!(bus.read16(0x0500_0000), 0x7FFF, "palette");

    bus.write16(0x0600_0000, 0xABCD);
    assert_eq!(bus.read16(0x0600_0000), 0xABCD, "VRAM");

    assert_eq!(bus.read8(0x0800_0000), 0xFE, "the cartridge's first byte");
}

#[test]
fn an_io_write_reaches_the_module_that_owns_the_address() {
    let mut gba = system();
    let bus = gba.bus_mut();

    bus.write16(crate::video::reg::DISPCNT, 3);
    assert_eq!(bus.video.mode(), 3);

    bus.write16(crate::irq::reg::IE, crate::irq::source::VBLANK);
    assert_eq!(bus.read16(crate::irq::reg::IE), crate::irq::source::VBLANK);

    bus.write16(crate::background::CONTROL_BASE, 2);
    assert_eq!(bus.backgrounds.layers[0].priority(), 2);

    bus.write16(crate::waitstates::WAITCNT, 0x4014);
    assert_eq!(bus.read16(crate::waitstates::WAITCNT), 0x4014);
}

#[test]
fn a_byte_write_to_a_halfword_register_leaves_the_other_half_alone() {
    // Read-modify-write, which matters for registers whose other half is live state rather
    // than a copy of what was written.
    let mut gba = system();
    let bus = gba.bus_mut();
    bus.write16(crate::video::reg::DISPCNT, 0x1234);
    bus.write8(crate::video::reg::DISPCNT, 0xFF);
    assert_eq!(bus.read16(crate::video::reg::DISPCNT), 0x12FF);
}

#[test]
fn a_vblank_interrupt_reaches_the_flag_register() {
    let mut gba = system();
    gba.bus_mut().write16(
        crate::video::reg::DISPSTAT,
        crate::video::dispstat::VBLANK_IRQ,
    );
    gba.step_frame(InputState::default());
    assert_ne!(
        gba.bus_mut().read16(crate::irq::reg::IF) & crate::irq::source::VBLANK,
        0
    );
}

#[test]
fn the_display_renders_through_the_compositor() {
    let mut gba = system();
    {
        let bus = gba.bus_mut();
        bus.write16(crate::video::reg::DISPCNT, 3 | (1 << 10)); // bitmap mode, background 2 enabled
                                                                // A bitmap mode is background 2 sampled through its affine transform, so a real game
                                                                // sets it to the identity before relying on the picture landing where it was drawn — the
                                                                // same registers an affine tile background uses.
        bus.write16(crate::affine::BG2_BASE, 1 << crate::affine::FRACTIONAL_BITS); // pa
        bus.write16(
            crate::affine::BG2_BASE + 6,
            1 << crate::affine::FRACTIONAL_BITS,
        ); // pd
        for x in 0..240u32 {
            bus.write16(0x0600_0000 + x * 2, 0x001F); // red
        }
    }
    gba.step_frame(InputState::default());
    assert_eq!(gba.framebuffer().pixel(0, 0).r, 0xFF);
    assert_eq!(gba.framebuffer().pixel(239, 0).r, 0xFF);
}

#[test]
fn a_multi_line_advance_renders_every_line_it_crosses_not_only_the_last() {
    // `video::tests` checks this at the timing layer in isolation; this drives it through the
    // real system, where a step spanning several lines used to render only the last of them —
    // the same collapsed edge, but visible as stale or missing rows rather than a wrong count.
    let mut gba = system();
    {
        let bus = gba.bus_mut();
        bus.write16(crate::video::reg::DISPCNT, 3 | (1 << 10)); // bitmap mode 3, background 2 on
        bus.write16(crate::affine::BG2_BASE, 1 << crate::affine::FRACTIONAL_BITS); // pa
        bus.write16(
            crate::affine::BG2_BASE + 6,
            1 << crate::affine::FRACTIONAL_BITS,
        ); // pd
        for y in 0..3u32 {
            for x in 0..240u32 {
                bus.write16(0x0600_0000 + (y * 240 + x) * 2, 0x001F); // red
            }
        }
        // Exactly three lines' worth of cycles, in one call.
        bus.advance(crate::video::CYCLES_PER_LINE * 3);
    }
    for y in 0..3u32 {
        assert_eq!(
            gba.framebuffer().pixel(0, y).r,
            0xFF,
            "row {y} was rendered"
        );
    }
    assert_eq!(
        gba.framebuffer().pixel(0, 3).a,
        0,
        "row 3 was not reached by this step, so still untouched"
    );
}

#[test]
fn a_general_purpose_dma_moves_the_bytes_at_once() {
    let mut gba = system();
    let bus = gba.bus_mut();
    for offset in 0..8u32 {
        bus.write32(0x0200_0000 + offset * 4, 0x1111_1111 * (offset + 1));
    }
    bus.write32(crate::dma::BASE, 0x0200_0000);
    bus.write32(crate::dma::BASE + 4, 0x0300_0000);
    bus.write16(crate::dma::BASE + 8, 8);
    bus.write16(crate::dma::BASE + 10, 0x8400); // enable, 32-bit units

    for offset in 0..8u32 {
        assert_eq!(
            bus.read32(0x0300_0000 + offset * 4),
            0x1111_1111 * (offset + 1),
            "word {offset}"
        );
    }
}

#[test]
fn without_a_bios_an_interrupt_enters_the_handler_the_game_left_in_iwram() {
    // The one thing the BIOS does that a game can observe. Skipping it leaves every game's
    // interrupt code unreachable, which looks like a hang rather than a missing BIOS.
    const HANDLER: u32 = 0x0300_0100;
    let mut gba = system();
    {
        let bus = gba.bus_mut();
        bus.write32(crate::irq::HLE_HANDLER_POINTER, HANDLER);
        bus.write16(
            crate::video::reg::DISPSTAT,
            crate::video::dispstat::VBLANK_IRQ,
        );
        bus.write16(crate::irq::reg::IE, crate::irq::source::VBLANK);
        bus.write16(crate::irq::reg::IME, 1);
        // A handler that spins, so the test can see it was entered.
        bus.write32(HANDLER, 0xEAFF_FFFE);
    }
    // The vertical-blank interrupt is raised at the moment the frame ends, so it is taken at
    // the top of the next one rather than within the frame that raised it.
    gba.step_frame(InputState::default());
    assert_ne!(
        gba.cpu().mode(),
        Mode::Irq,
        "not yet — the frame just ended"
    );
    gba.step_frame(InputState::default());
    assert_eq!(
        gba.cpu().mode(),
        Mode::Irq,
        "it took the interrupt rather than ignoring it"
    );
}

#[test]
fn an_interrupt_is_not_taken_while_the_cpu_has_them_masked() {
    let mut gba = system();
    {
        let bus = gba.bus_mut();
        bus.write32(crate::irq::HLE_HANDLER_POINTER, 0x0300_0100);
        bus.write16(crate::irq::reg::IE, crate::irq::source::VBLANK);
        bus.write16(crate::irq::reg::IME, 1);
        bus.irq.raise(crate::irq::source::VBLANK);
    }
    gba.cpu.cpsr.set_irq_disabled(true);
    gba.step_frame(InputState::default());
    assert_ne!(gba.cpu().mode(), Mode::Irq);
}

#[test]
fn a_save_state_round_trips_and_resumes_identically() {
    let mut gba = system();
    gba.step_frame(InputState::default());
    let state = gba.save_state();

    let mut restored = system();
    restored.load_state(&state).expect("the state is valid");

    gba.step_frame(InputState::default());
    restored.step_frame(InputState::default());
    assert_eq!(
        restored.bus().video.vcount(),
        gba.bus().video.vcount(),
        "the display resumed where it left off"
    );
    assert_eq!(
        restored.framebuffer().as_bytes(),
        gba.framebuffer().as_bytes()
    );
}

#[test]
fn two_identical_runs_produce_identical_output() {
    // Determinism is what save states, rewind, and replay all rest on.
    let mut first = system();
    let mut second = system();
    for _ in 0..3 {
        first.step_frame(InputState::default());
        second.step_frame(InputState::default());
    }
    assert_eq!(
        first.framebuffer().as_bytes(),
        second.framebuffer().as_bytes()
    );
}

#[test]
fn the_system_reports_its_own_identity() {
    let gba = system();
    assert_eq!(gba.id(), "gba");
    assert_eq!(gba.display_name(), "Game Boy Advance");
}

#[test]
fn a_malformed_cartridge_is_rejected_rather_than_panicking() {
    assert!(GbaSystem::new(vec![0u8; 8], None).is_err());
}

#[test]
#[ignore = "diagnostic; needs the fetched corpus"]
fn trace_gba_suite_entry() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("testing/test-roms")
        .join(std::env::var("TRACE_ROM").unwrap_or_else(|_| "gba/gba-suite/arm.gba".into()));
    let Ok(rom) = std::fs::read(&path) else {
        eprintln!("no corpus at {}", path.display());
        return;
    };
    let mut gba = GbaSystem::new(rom, None).unwrap();
    let mut history: Vec<(u32, u32)> = Vec::new();
    for step in 0..4_000_000u32 {
        let pc = gba.cpu().regs.pc();
        if step > 60 && history.len() == 12 && history[..4].iter().all(|h| h.0 == pc) {
            eprintln!(
                "settled at {pc:#010X} after {step} steps, r12={}",
                gba.cpu().reg(12)
            );
            return;
        }
        if !(0x0800_0000..0x0A00_0000).contains(&pc) {
            eprintln!("left ROM at step {step}: pc={pc:#010X}");
            for (p, op) in history.iter().rev().take(12).rev() {
                eprintln!("   {p:#010X}  {op:#010X}");
            }
            eprintln!(
                "   cpsr thumb={} mode={:?}",
                gba.cpu().is_thumb(),
                gba.cpu().mode()
            );
            return;
        }
        let opcode = gba.bus_mut().read32(pc);
        history.push((pc, opcode));
        if history.len() > 12 {
            history.remove(0);
        }
        gba.step_instruction();
    }
    eprintln!("stayed in ROM for 4M instructions");
}

#[test]
fn a_word_store_to_palette_ram_is_two_halfwords_and_not_four_bytes() {
    // The default `Bus::write32` decomposes into bytes, and `write8` implements the 16-bit bus
    // quirk where a byte written to palette RAM or VRAM is doubled across its halfword — so a
    // word store wrote each byte and then immediately overwrote it with the next. Storing 1
    // landed as 0. Every 32-bit palette and VRAM write in every game would have been wrong, and
    // `gba-suite`'s memory test caught it on the third check.
    let mut gba = system();
    let bus = gba.bus_mut();

    bus.write32(0x0500_0010, 1);
    assert_eq!(bus.read32(0x0500_0010), 1);

    bus.write32(0x0600_0000, 0xDEAD_BEEF);
    assert_eq!(bus.read32(0x0600_0000), 0xDEAD_BEEF);

    // And the byte quirk itself is still there for an actual byte write.
    bus.write8(0x0500_0020, 0x5A);
    assert_eq!(bus.read16(0x0500_0020), 0x5A5A);
}

#[test]
fn palette_ram_mirrors_every_kilobyte() {
    // What gba-suite's third memory check actually tests: write through one address, read back
    // through its mirror.
    let mut gba = system();
    let bus = gba.bus_mut();
    bus.write32(0x0500_0010, 1);
    assert_eq!(bus.read32(0x0500_0410), 1);
}

#[test]
fn an_instruction_is_charged_for_the_memory_it_touched() {
    // A flat cost per access would erase the difference between a game linked into the fast ROM
    // window and one linked into the slow one, which is a choice games make deliberately.
    let mut gba = system();
    let bus = gba.bus_mut();
    bus.take_pending_waits();

    bus.read32(0x0300_0000); // IWRAM: one cycle at full width
    let fast = bus.take_pending_waits();

    bus.read32(0x0200_0000); // EWRAM: 16-bit bus with wait states
    let slow = bus.take_pending_waits();

    assert!(
        slow > fast,
        "EWRAM ({slow}) should cost more than IWRAM ({fast})"
    );
}

#[test]
fn a_rom_access_that_continues_from_the_last_one_is_cheaper() {
    // The cartridge bus keeps its address latched, which is why a loop walking forward through
    // ROM is much faster than one chasing pointers.
    let mut gba = system();
    let bus = gba.bus_mut();

    bus.read16(0x0800_0000);
    bus.take_pending_waits();
    bus.read16(0x0800_0002); // continues
    let sequential = bus.take_pending_waits();

    bus.read16(0x0801_0000); // jumps
    let jump = bus.take_pending_waits();

    assert!(sequential < jump, "{sequential} should be below {jump}");
}

#[test]
fn an_instruction_costs_what_the_hardware_charges_for_it() {
    // The check that would have caught the worst bug this system has had. Every access was being
    // charged between three and six times — once by the wait-state table, again by each halfword
    // the access decomposed into, again by each byte under that, and a whole second time because
    // the `SWI` interception fetched the opcode through the bus before the CPU fetched it itself.
    // An ARM data-processing instruction in internal WRAM cost 13 cycles instead of 1.
    //
    // Nothing failed. The emulator still ran at 100% of a *frame*, because a frame is a fixed
    // number of cycles however few instructions fit in it — so what a commercial game lost was
    // nine tenths of its processor, and what that looked like was a game that boots, draws its
    // intro, and then appears to hang.
    fn cost_at(pc: u32, instruction: u32) -> u64 {
        let mut gba = system();
        gba.bus_mut().write32(pc, instruction);
        gba.cpu_mut().regs.set_pc(pc);
        gba.bus_mut().take_pending_waits();
        gba.step_instruction().get()
    }

    // `add r0, r0, r0`, fetched from internal WRAM: a 32-bit bus with no wait states, so the
    // whole instruction is the one cycle its single sequential fetch takes.
    assert_eq!(cost_at(0x0300_1000, 0xE080_0000), 1, "internal WRAM");

    // The same instruction from external WRAM, which is a 16-bit bus with two wait states: two
    // accesses at three cycles each.
    assert_eq!(cost_at(0x0200_1000, 0xE080_0000), 6, "external WRAM");

    // The prefetch case: a run of sequential fetches from ROM, with `WAITCNT`'s buffer bit set,
    // costs full price once and then the minimum ever after — the same shape of bug as the rest
    // of this test guards against, just the opposite direction. Overcharging every sequential ROM
    // fetch is invisible the same way undercharging IWRAM was: no test fails and the speed reading
    // stays at 100%, because a frame is still a fixed number of cycles. What a game loses is
    // however much of its processor the buffer was supposed to give back.
    let mut gba = GbaSystem::new(nop_rom(3), None).expect("the synthetic ROM is valid");
    let base = 0x0800_0000u32;
    gba.bus_mut().write16(crate::waitstates::WAITCNT, 1 << 14);
    gba.cpu_mut().regs.set_pc(base);
    gba.bus_mut().take_pending_waits();

    let first = gba.step_instruction().get();
    let second = gba.step_instruction().get();
    let third = gba.step_instruction().get();
    assert!(first > 1, "the first fetch of a run is never free: {first}");
    // Exactly 1: the same free rate as the IWRAM case above, not merely "cheaper than the first" —
    // a sequential ROM access is already cheaper than a jump even with no prefetch at all, so a
    // weaker comparison here would not actually tell the buffer's discount from that baseline
    // difference.
    assert_eq!(second, 1, "primed by the first fetch, the second is free");
    assert_eq!(third, 1, "and stays free as the run continues");
}

#[test]
fn the_bios_call_check_does_not_fetch_the_opcode_a_second_time() {
    // It runs before every instruction and is only asking a question. Reading through the bus
    // charged the access, moved the cartridge's sequential-access latch, and recorded a
    // watchpoint entry — all for an instruction that is not a `SWI` and never was.
    let mut gba = system();
    gba.bus_mut().write32(0x0300_1000, 0xE080_0000);
    gba.cpu_mut().regs.set_pc(0x0300_1000);
    gba.bus_mut().take_pending_waits();

    gba.step_instruction();
    assert_eq!(
        gba.bus_mut().next_sequential_address(),
        0x0300_1004,
        "the latch should sit just past the one fetch the CPU made"
    );
}

#[test]
fn a_slower_cartridge_gets_through_less_code_in_a_frame() {
    // The whole point of the wait-state table: the same code at two speeds.
    //
    // Measured as instructions per frame rather than cycles per frame. A frame is 197120 cycles
    // whatever the cartridge is set to — the video timing decides that, not the CPU — so the
    // cycle count only ever differed by however far the last instruction overshot the boundary.
    // That is a rounding artefact, and it went to zero the moment instructions stopped costing
    // ten times what they should.
    fn instructions_per_frame(waitcnt: u16) -> u32 {
        let mut gba = system();
        gba.bus_mut().write16(crate::waitstates::WAITCNT, waitcnt);
        let mut count = 0;
        while !gba.bus().video.in_vblank() && count < 1_000_000 {
            gba.step_instruction();
            count += 1;
        }
        count
    }

    // Setting 2 is the fastest first access, setting 3 the slowest.
    let fast = instructions_per_frame(2 << 2);
    let slow = instructions_per_frame(3 << 2);
    assert!(
        slow < fast,
        "a slower cartridge should get through fewer instructions before vertical blanking: \
         {slow} against {fast}"
    );
}

/// Print the machine's state after running a ROM for a while.
///
/// `TRACE_ROM=/path/to.gba cargo test -p system-gba --release -- --ignored --nocapture dump_state`
///
/// The companion to `trace_gba_suite_entry`: that one answers "where did it go wrong", this one
/// answers "what is it doing now", which is the question a game that runs and draws nothing poses.
#[test]
#[ignore]
fn dump_state() {
    let Ok(path) = std::env::var("TRACE_ROM") else {
        eprintln!("set TRACE_ROM to a ROM path");
        return;
    };
    let rom = std::fs::read(&path).expect("the ROM");
    let mut system = GbaSystem::new(rom, None).expect("a cartridge");
    let mut run = 0u32;
    for target in [1u32, 60, 400, 1500, 2400, 4400] {
        while run < target {
            system.step_frame(InputState::default());
            run += 1;
        }
        println!(
            "--- after {target} frames ---\n{}{}",
            system.state_dump(),
            system.graphics_dump()
        );
    }
}

/// Find the loop a stalled ROM is spinning in, and disassemble it.
///
/// `TRACE_ROM=/path/to.gba TRACE_FRAMES=400 cargo test -p system-gba --release --
/// --ignored --nocapture trace_stall`
///
/// `dump_state` answers "what is it doing now" one frame at a time, which is enough to see that a
/// program counter is not moving on. This answers the question after it: *which* instructions it is
/// not moving on from. It runs to `TRACE_FRAMES`, then single-steps a frame's worth of instructions
/// recording every program counter, and prints the hottest addresses disassembled.
///
/// A stalled GBA game is almost always spinning on a flag some piece of hardware was supposed to
/// set, and the load in the loop body names the register that did not.
#[test]
#[ignore]
fn trace_stall() {
    let Ok(path) = std::env::var("TRACE_ROM") else {
        eprintln!("set TRACE_ROM to a ROM path");
        return;
    };
    let frames: u32 = std::env::var("TRACE_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let rom = std::fs::read(&path).expect("the ROM");
    let mut system = GbaSystem::new(rom, None).expect("a cartridge");
    for _ in 0..frames {
        system.step_frame(InputState::default());
    }
    println!("--- after {frames} frames ---\n{}", system.state_dump());

    // One frame's worth of instructions is enough for a loop to show up hundreds of times while a
    // program still making progress spreads over thousands of distinct addresses.
    // The instruction set has to be recorded with the address: which decoder applies is machine
    // state at the moment of the fetch, and a Thumb address disassembled as ARM is plausible
    // nonsense rather than an error.
    let mut seen: std::collections::BTreeMap<(u32, bool), (u32, u32)> =
        std::collections::BTreeMap::new();
    let mut halted = 0u32;
    for _ in 0..200_000 {
        if core_common::DebugTarget::is_halted(&system) {
            halted += 1;
            system.step_instruction();
            continue;
        }
        let pc = core_common::DebugTarget::program_counter(&system);
        let thumb = system.cpu().is_thumb();
        let cost = system.step_instruction().get() as u32;
        let slot = seen.entry((pc, thumb)).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += cost;
    }
    println!("{halted} of 200000 steps were spent halted");
    {
        let mut cycles = 0u64;
        for _ in 0..200_000 {
            cycles += system.step_instruction().get();
        }
        // 280896 cycles is one Game Boy Advance frame.
        println!(
            "200000 instructions cost {cycles} cycles = {:.2} frames",
            cycles as f64 / 280_896.0
        );
    }

    let mut hot: Vec<_> = seen.iter().map(|(k, (n, c))| (*n, *k, *c)).collect();
    hot.sort_unstable_by(|a, b| b.cmp(a));
    println!(
        "{} distinct program counters over 200000 instructions",
        hot.len()
    );
    // Split by region, because a game with a working sound driver spends most of its instructions
    // in the mixer it copied into internal WRAM, and that buries the ROM loop that is the question.
    for (label, base) in [
        ("ROM", 0x0800_0000u32),
        ("EWRAM", 0x0200_0000),
        ("IWRAM", 0x0300_0000),
    ] {
        println!("--- hottest in {label} ---");
        for (count, (addr, thumb), cost) in hot
            .iter()
            .filter(|(_, (a, _), _)| a & 0xFF00_0000 == base)
            .take(12)
        {
            let each = *cost as f64 / *count as f64;
            println!(
                "  {count:6}x {each:5.1}cy  {}",
                disassemble_at(&system, *addr, *thumb)
            );
        }
    }

    // Where the instructions went by 4 KiB page, which is what shows a main loop that has stopped
    // covering ground: a running game touches dozens of pages a frame.
    let mut pages: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
    for (count, (addr, _), _) in &hot {
        *pages.entry(addr & !0xFFF).or_insert(0) += count;
    }
    let mut pages: Vec<_> = pages.into_iter().map(|(a, n)| (n, a)).collect();
    pages.sort_unstable_by(|a, b| b.cmp(a));
    println!("--- instructions by 4 KiB page ({} pages) ---", pages.len());
    for (count, page) in pages.iter().take(20) {
        println!("  {count:6}x {page:08X}");
        for (n, (addr, thumb), c) in hot
            .iter()
            .filter(|(_, (a, _), _)| a & !0xFFF == *page)
            .take(3)
        {
            let each = *c as f64 / *n as f64;
            println!(
                "        {n:6}x {each:5.1}cy  {}",
                disassemble_at(&system, *addr, *thumb)
            );
        }
    }

    // Consecutive addresses are the loop body; printing it in order is what makes the flag it is
    // spinning on readable.
    if let Some((_, (hottest, thumb), _)) = hot.first() {
        let width = if *thumb { 2 } else { 4 };
        println!("--- around {hottest:08X} ---");
        for i in -8i32..12 {
            let addr = hottest.wrapping_add((i * width) as u32);
            println!("  {}", disassemble_at(&system, addr, *thumb));
        }
    }
}

#[cfg(test)]
fn disassemble_at(system: &GbaSystem, addr: u32, thumb: bool) -> String {
    use core_common::{Bus, Disassemble};
    use cpu_arm7tdmi::{ArmDisassembler, ThumbDisassembler};

    let width = if thumb { 2 } else { 4 };
    let mut bytes = [0u8; 4];
    for (offset, slot) in bytes.iter_mut().take(width).enumerate() {
        let Some(byte) = system.bus().peek8(addr.wrapping_add(offset as u32)) else {
            return format!("{addr:08X}  <unreadable>");
        };
        *slot = byte;
    }
    let bytes = &bytes[..width];
    let decoded = if thumb {
        ThumbDisassembler.disassemble(bytes, addr)
    } else {
        ArmDisassembler.disassemble(bytes, addr)
    };
    let word = match width {
        2 => format!("    {:04X}", u16::from_le_bytes([bytes[0], bytes[1]])),
        _ => format!("{:08X}", u32::from_le_bytes(bytes.try_into().unwrap())),
    };
    match decoded {
        Some(i) => format!("{addr:08X}  {word}  {}", i.text),
        None => format!("{addr:08X}  {word}  ??"),
    }
}

/// The `IntrWait` tests share one small machine: a main loop that waits and counts, and an
/// interrupt handler that acknowledges `IF` the way a real one does.
///
/// Both halves matter. A handler that does not acknowledge leaves the source pending, so the
/// machine re-enters it forever and no test below can distinguish a correct wait from a wrong one.
mod intr_wait {
    use super::*;

    /// Where the game leaves its handler, and where these tests assemble one.
    const HANDLER: u32 = 0x0300_0100;
    /// The counter the main loop keeps. `r4` survives the interrupt wrapper, which pushes only the
    /// registers the procedure standard lets a callee clobber.
    const COUNTER: usize = 4;

    /// A main loop of `SWI <call>; add r4, r4, #1; b .-16`.
    ///
    /// `r4` therefore counts *completed waits*, which is the one number every test here is about:
    /// a wait that returns on the wrong interrupt shows up as a count in the hundreds.
    fn rom_waiting_on(call: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        for (index, word) in [
            0xEF00_0000 | (call as u32) << 16, // swi <call>
            0xE284_4001,                       // add r4, r4, #1
            0xEAFF_FFFC,                       // b back to the swi
        ]
        .into_iter()
        .enumerate()
        {
            rom[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        rom
    }

    /// A machine running that loop, with an interrupt handler that acknowledges `IF`.
    ///
    /// `enabled` goes into `IE` and picks the matching `DISPSTAT` enables; `r0` and `r1` are the
    /// call's arguments, which for `SWI 0x04` are the discard flag and the mask.
    fn machine(call: u8, enabled: u16, r0: u32, r1: u32) -> GbaSystem {
        let mut gba = GbaSystem::new(rom_waiting_on(call), None).expect("the ROM is valid");
        {
            let bus = gba.bus_mut();
            bus.write32(crate::irq::HLE_HANDLER_POINTER, HANDLER);

            let mut status = 0;
            if enabled & crate::irq::source::VBLANK != 0 {
                status |= crate::video::dispstat::VBLANK_IRQ;
            }
            if enabled & crate::irq::source::HBLANK != 0 {
                status |= crate::video::dispstat::HBLANK_IRQ;
            }
            bus.write16(crate::video::reg::DISPSTAT, status);
            bus.write16(crate::irq::reg::IE, enabled);
            bus.write16(crate::irq::reg::IME, 1);

            // ldr r0, [pc, #12] / ldrh r1, [r0] / strh r1, [r0] / bx lr, then the address of `IF`.
            // Writing `IF` back over itself is how this hardware acknowledges: a one clears.
            for (offset, word) in [
                (0x00, 0xE59F_000C),
                (0x04, 0xE1D0_10B0),
                (0x08, 0xE1C0_10B0),
                (0x0C, 0xE12F_FF1E),
                (0x14, crate::irq::reg::IF),
            ] {
                bus.write32(HANDLER + offset, word);
            }
        }
        gba.cpu.set_reg(0, r0);
        gba.cpu.set_reg(1, r1);
        gba
    }

    fn run_frames(gba: &mut GbaSystem, frames: usize) -> u32 {
        for _ in 0..frames {
            gba.step_frame(InputState::default());
        }
        gba.cpu().reg(COUNTER)
    }

    #[test]
    fn vblank_intr_wait_returns_on_vertical_blank_and_not_on_the_horizontal_one() {
        // The bug this whole change exists for. With `HBlank` also enabled — which is most
        // commercial software, using it for raster effects — a `VBlankIntrWait` implemented as a
        // plain halt returns on whichever interrupt happens to be next. That is up to 160 returns
        // a frame instead of one, and every symptom downstream of frame pacing follows from it.
        let mut gba = machine(
            0x05,
            crate::irq::source::VBLANK | crate::irq::source::HBLANK,
            0,
            0,
        );
        let completed = run_frames(&mut gba, 3);
        assert!(
            (1..=3).contains(&completed),
            "three frames should complete about three waits, not {completed} — \
             a count in the hundreds means it returned on every HBlank"
        );
    }

    #[test]
    fn intr_wait_returns_on_a_source_it_named_and_not_on_one_it_did_not() {
        // A multi-bit mask naming vertical blank and a timer, while horizontal blank is *enabled*
        // but unnamed. Only the named one may end the wait.
        let mask = crate::irq::source::VBLANK | crate::irq::source::TIMER0;
        let mut gba = machine(
            0x04,
            crate::irq::source::VBLANK | crate::irq::source::HBLANK,
            1,
            mask as u32,
        );
        let completed = run_frames(&mut gba, 3);
        assert!(
            (1..=3).contains(&completed),
            "{completed} completions means the unnamed HBlank was ending the wait"
        );
    }

    #[test]
    fn the_interrupt_path_records_what_it_serviced_and_the_wait_consumes_it() {
        // The two halves of the flag word's contract, which only work as a pair: the interrupt
        // path sets a bit, and the wait that was asking for it clears that bit and no other.
        let mut gba = machine(
            0x05,
            crate::irq::source::VBLANK | crate::irq::source::HBLANK,
            0,
            0,
        );
        assert_eq!(
            gba.bus_mut().read16(crate::bios::INTRWAIT_FLAGS),
            0,
            "nothing has been serviced yet"
        );

        run_frames(&mut gba, 2);
        let flags = gba.bus_mut().read16(crate::bios::INTRWAIT_FLAGS);
        assert_eq!(
            flags & crate::irq::source::HBLANK,
            crate::irq::source::HBLANK,
            "HBlank was serviced and nothing asked for it, so it stays set"
        );
        assert_eq!(
            flags & crate::irq::source::VBLANK,
            0,
            "but VBlank was consumed by the wait that was asking for it"
        );
    }

    #[test]
    fn a_wait_for_an_interrupt_that_is_never_enabled_does_not_livelock() {
        // Hardware hangs here, and so does this — deliberately. What must not happen is the
        // frontend hanging with it: `step_frame` is bounded, so the frame comes back regardless
        // and the machine is left visibly sitting on its `SWI` rather than wedging the process.
        let mut gba = machine(
            0x04,
            crate::irq::source::VBLANK,
            1,
            crate::irq::source::SERIAL as u32,
        );
        let completed = run_frames(&mut gba, 2);
        assert_eq!(completed, 0, "nothing can satisfy it, so it never returns");
        assert!(
            core_common::DebugTarget::is_halted(&gba),
            "and it is sitting in the wait rather than having run off somewhere"
        );
    }

    #[test]
    fn a_state_saved_mid_wait_resumes_into_the_wait_rather_than_restarting_it() {
        // Where a game spends most of its time is inside this call, so most quicksaves land here.
        // Restoring into a *fresh* call would discard the flags the wait had already begun on,
        // which costs a frame; restoring with the mask intact resumes the same wait.
        let mut gba = machine(
            0x05,
            crate::irq::source::VBLANK | crate::irq::source::HBLANK,
            0,
            0,
        );
        gba.step_frame(InputState::default());
        assert_eq!(
            gba.intr_wait,
            Some(crate::irq::source::VBLANK),
            "the frame ended with the machine waiting"
        );

        let state = gba.save_state();
        let mut restored = machine(
            0x05,
            crate::irq::source::VBLANK | crate::irq::source::HBLANK,
            0,
            0,
        );
        restored.load_state(&state).expect("the state is valid");
        assert_eq!(restored.intr_wait, gba.intr_wait);

        gba.step_frame(InputState::default());
        restored.step_frame(InputState::default());
        assert_eq!(
            restored.cpu().reg(COUNTER),
            gba.cpu().reg(COUNTER),
            "and it completed the same number of waits from there"
        );
    }

    #[test]
    fn halt_still_wakes_on_any_interrupt_at_all() {
        // Pinned because `Halt` used to share an arm with the two waits above, and splitting that
        // arm is exactly the kind of change that quietly takes the other case with it. `Halt` has
        // no mask: whatever the controller raises wakes it.
        let mut gba = machine(0x02, crate::irq::source::HBLANK, 0, 0);
        let completed = run_frames(&mut gba, 1);
        assert!(
            completed > 1,
            "HBlank alone should wake a plain Halt many times a frame, not {completed}"
        );
    }

    #[test]
    fn the_fast_forward_halt_lands_at_the_same_state_as_the_one_cycle_slow_path() {
        // Not just faster: this asserts the two paths agree, cycle for cycle and instruction
        // count for instruction count, on where a halted machine ends up. A predictor that landed
        // even one cycle short or long of the real wake edge would still pass every other test
        // here, since they only check that a wait eventually completes, not exactly when.
        let mut fast = machine(0x02, crate::irq::source::VBLANK, 0, 0);
        let mut slow = machine(0x02, crate::irq::source::VBLANK, 0, 0);
        slow.disable_halt_fast_forward = true;

        let mut fast_calls = 0u32;
        let mut fast_cycles = 0u64;
        while fast.cpu().reg(COUNTER) < 3 {
            fast_cycles += fast.step_instruction().get();
            fast_calls += 1;
            assert!(
                fast_calls < 10_000,
                "the fast path should reach three waits in far fewer calls than this"
            );
        }

        let mut slow_calls = 0u32;
        let mut slow_cycles = 0u64;
        while slow.cpu().reg(COUNTER) < 3 {
            slow_cycles += slow.step_instruction().get();
            slow_calls += 1;
        }

        assert_eq!(
            fast_cycles, slow_cycles,
            "the two paths must land on the same total cycle count"
        );
        assert_eq!(
            fast.cpu().regs,
            slow.cpu().regs,
            "and the same register state"
        );
        assert!(
            fast_calls < slow_calls / 100,
            "the whole point is skipping the one-cycle-at-a-time calls: {fast_calls} fast \
             vs {slow_calls} slow"
        );
    }

    #[test]
    fn the_fast_forward_reaches_a_vblank_intr_wait_the_same_way_it_reaches_a_plain_halt() {
        // `VBlankIntrWait` under an also-enabled `HBlank` is the case the fast path has to get
        // right and a plain `Halt` cannot exercise: `intercept_bios_call` re-enters `bios::dispatch`
        // on every step while the wait is in progress, so the fast path has to run *before* that
        // interception or it never reaches this call shape at all — the bug this test would have
        // caught, since `Halt` alone cannot see it (a `Halt` never calls `intercept_bios_call` a
        // second time). And the machine still has to take the same repeated detours through the
        // handler that HBlank forces on real hardware, landing on the same state either way.
        let enabled = crate::irq::source::VBLANK | crate::irq::source::HBLANK;
        let mut fast = machine(0x05, enabled, 0, 0);
        let mut slow = machine(0x05, enabled, 0, 0);
        slow.disable_halt_fast_forward = true;

        let mut fast_calls = 0u32;
        let mut fast_cycles = 0u64;
        while fast.cpu().reg(COUNTER) < 2 {
            fast_cycles += fast.step_instruction().get();
            fast_calls += 1;
            assert!(
                fast_calls < 100_000,
                "the fast path should reach two waits in far fewer calls than this"
            );
        }

        let mut slow_calls = 0u32;
        let mut slow_cycles = 0u64;
        while slow.cpu().reg(COUNTER) < 2 {
            slow_cycles += slow.step_instruction().get();
            slow_calls += 1;
        }

        assert_eq!(
            fast_cycles, slow_cycles,
            "the two paths must land on the same total cycle count"
        );
        assert_eq!(
            fast.cpu().regs,
            slow.cpu().regs,
            "and the same register state"
        );
        assert!(
            fast_calls < slow_calls / 50,
            "the whole point is skipping the one-cycle-at-a-time calls: {fast_calls} fast \
             vs {slow_calls} slow"
        );
    }
}

/// HBlank DMA does not run during vertical blanking, only the interrupt does.
///
/// Driven straight at `GbaSystemBus::advance` rather than through the CPU: one call per scanline
/// gives exactly one hblank edge per call, which is what a real instruction stream also produces
/// — no instruction costs anywhere near the 272 cycles between hblank start and a line's end, so
/// `video::VideoTiming::tick` never has to fold more than one edge into a single report outside a
/// test built to call it a whole line at a time, as this one deliberately does.
mod hblank_dma {
    use super::*;

    /// Channel 0's registers, and the control bits this test needs. `dma::control` is private to
    /// that module, so the bits actually in use are named here instead of imported.
    const SOURCE: u32 = crate::dma::BASE;
    const DESTINATION: u32 = crate::dma::BASE + 4;
    const WORD_COUNT: u32 = crate::dma::BASE + 8;
    const CONTROL: u32 = crate::dma::BASE + 10;
    /// Source fixed (bits 7-8 = 2), repeat (bit 9), HBlank timing (bits 12-13 = 2), enable (bit
    /// 15). Destination step is left at its default, `Increment`.
    const ARM_HBLANK_REPEATING_FIXED_SOURCE: u16 = 0x0100 | 0x0200 | 0x2000 | 0x8000;

    /// A single halfword this test recognises, so counting how many destination slots hold it
    /// counts how many transfers actually ran — the closest a test outside `dma.rs` can get to
    /// counting `DmaController::take_transfer` calls directly.
    const MARK: u16 = 0xAAAA;
    const SOURCE_ADDR: u32 = 0x0200_0000;
    const DEST_BASE: u32 = 0x0200_1000;

    /// Arm channel 0 to copy [`MARK`] into successive halfwords on every HBlank, repeating.
    fn arm_marking_channel(gba: &mut GbaSystem) {
        let bus = gba.bus_mut();
        bus.memory.write16(SOURCE_ADDR, MARK);
        bus.write32(SOURCE, SOURCE_ADDR);
        bus.write32(DESTINATION, DEST_BASE);
        bus.write16(WORD_COUNT, 1);
        bus.write16(CONTROL, ARM_HBLANK_REPEATING_FIXED_SOURCE);
    }

    /// How many consecutive marked halfwords sit at `DEST_BASE`, i.e. how many transfers ran.
    fn marks_written(gba: &mut GbaSystem) -> u32 {
        let mut count = 0u32;
        while gba.bus_mut().memory.read16(DEST_BASE + count * 2) == Some(MARK) {
            count += 1;
        }
        count
    }

    #[test]
    fn hblank_dma_runs_on_the_hundred_and_sixty_visible_lines_and_no_more() {
        let mut gba = system();
        arm_marking_channel(&mut gba);

        // Driven a dot at a time and counted by `VCOUNT` rather than as a fixed number of
        // line-sized advances. Each transfer now costs cycles of its own, so 228 calls of one
        // line each land eight cycles per visible line past the frame boundary — well into line 0
        // of the next frame, whose HBlank then writes a hundred and sixty-first mark.
        let mut lines = 0;
        while lines < crate::video::LINES_PER_FRAME {
            let before = gba.bus().video.vcount();
            gba.bus_mut().advance(crate::video::CYCLES_PER_DOT);
            if gba.bus().video.vcount() != before {
                lines += 1;
            }
        }

        assert_eq!(
            marks_written(&mut gba),
            crate::video::SCREEN_HEIGHT,
            "one transfer per visible line, none during the 68 lines of vertical blanking"
        );
    }

    #[test]
    fn the_hblank_interrupt_still_fires_during_vertical_blanking() {
        // DMA arming is gated on the visible lines; the interrupt line is a separate signal and
        // must not be, or a game using HBlank purely for an interrupt-driven effect during
        // VBlank — rare, but real — would stop being told about it.
        let mut gba = system();
        {
            let bus = gba.bus_mut();
            bus.write16(
                crate::video::reg::DISPSTAT,
                crate::video::dispstat::HBLANK_IRQ,
            );
            bus.write16(crate::irq::reg::IE, crate::irq::source::HBLANK);
            bus.write16(crate::irq::reg::IME, 1);
        }

        // Every visible line's own HBlank interrupt fires along the way; run past all of them and
        // acknowledge before isolating a line that is entirely inside vertical blanking.
        for _ in 0..crate::video::SCREEN_HEIGHT {
            gba.bus_mut().advance(crate::video::CYCLES_PER_LINE);
        }
        gba.bus_mut()
            .irq
            .write16(crate::irq::reg::IF, crate::irq::source::ALL);
        assert_eq!(gba.bus_mut().irq.read16(crate::irq::reg::IF), Some(0));

        // One more full line: entirely inside VBlank, so this is the interrupt under test.
        gba.bus_mut().advance(crate::video::CYCLES_PER_LINE);
        assert_eq!(
            gba.bus_mut().irq.read16(crate::irq::reg::IF).unwrap() & crate::irq::source::HBLANK,
            crate::irq::source::HBLANK,
            "HBlank still interrupts in VBlank even though its DMA no longer arms there"
        );
    }
}

/// A transfer takes time, and the machine runs *through* it rather than around it.
///
/// DMA used to copy its whole block inside one `while` loop in zero emulated cycles: the display
/// stood still, no timer ticked, and the CPU paid nothing for a burst that on hardware holds the
/// bus for most of a scanline. The tests here pin the cost and the three things that were wrong
/// downstream of it.
mod dma_timing {
    use super::*;

    /// Channel 0's registers. `dma::control` is private to that module, so the bits in use are
    /// named here rather than imported.
    const SOURCE: u32 = crate::dma::BASE;
    const DESTINATION: u32 = crate::dma::BASE + 4;
    const WORD_COUNT: u32 = crate::dma::BASE + 8;
    const CONTROL: u32 = crate::dma::BASE + 10;
    /// Enable (bit 15) and 32-bit units (bit 10). Timing bits clear, so it starts immediately.
    const START_NOW_IN_WORDS: u16 = 0x8000 | 0x0400;

    /// Timer 0's two registers, and the two control bits these tests need.
    const TIMER_RELOAD: u32 = crate::timers::BASE;
    const TIMER_CONTROL: u32 = crate::timers::BASE + 2;
    const TIMER_ENABLE: u16 = 1 << 7;
    const TIMER_IRQ: u16 = 1 << 6;

    const IWRAM: u32 = 0x0300_0000;
    const VRAM: u32 = 0x0600_0000;

    /// The transfer under test: 240 32-bit units from internal WRAM to video RAM, which is one
    /// scanline of a mode 3 bitmap and the shape a game's per-frame blit actually has.
    ///
    /// Two cycles of startup, then a 1-cycle IWRAM read and a 2-cycle VRAM write for each unit —
    /// VRAM being on a 16-bit bus, so a word is two accesses there and one here.
    const WORDS: u16 = 240;
    const EXPECTED_CYCLES: u32 = 2 + WORDS as u32 * (1 + 2);

    /// Arm channel 0 for that transfer. The final store is what runs it, immediately.
    fn run_the_transfer(bus: &mut GbaSystemBus) {
        bus.write32(SOURCE, IWRAM);
        bus.write32(DESTINATION, VRAM);
        bus.write16(WORD_COUNT, WORDS);
        bus.write16(CONTROL, START_NOW_IN_WORDS);
    }

    #[test]
    fn a_transfer_costs_startup_plus_a_read_and_a_write_for_every_unit() {
        // Measured with timer 0 at prescaler 1, which is a cycle counter with hardware's name on
        // it: if the machine did not advance, neither did the count.
        let mut gba = system();
        let bus = gba.bus_mut();
        bus.write16(TIMER_RELOAD, 0);
        bus.write16(TIMER_CONTROL, TIMER_ENABLE);

        run_the_transfer(bus);
        assert_eq!(
            bus.timers.counter(0) as u32,
            EXPECTED_CYCLES,
            "722 cycles: two of startup and three for each of the 240 units"
        );
    }

    #[test]
    fn a_transfer_advances_the_display_through_itself() {
        // The acceptance question in its plainest form: does `VCOUNT` move? Parked 600 cycles
        // into line 0 first, so the 722 the transfer takes have to carry the beam over the
        // 1232-cycle line boundary rather than merely along it.
        let mut gba = system();
        gba.bus_mut().advance(600);
        assert_eq!(gba.bus().video.vcount(), 0, "still on the first line");

        run_the_transfer(gba.bus_mut());
        assert_eq!(
            gba.bus().video.vcount(),
            1,
            "600 + 722 cycles is past the end of line 0"
        );
    }

    #[test]
    fn a_timer_overflow_inside_a_transfer_lands_where_it_falls_rather_than_after_it() {
        // Sixteen cycles from overflowing, reloading to zero, asking for an interrupt. The
        // interrupt is the visible half; the counter afterwards is what says *when*.
        let mut gba = system();
        {
            let bus = gba.bus_mut();
            bus.write16(crate::irq::reg::IE, crate::irq::source::timer(0));
            bus.write16(crate::irq::reg::IME, 1);
            bus.write16(TIMER_RELOAD, 0xFFF0);
            bus.write16(TIMER_CONTROL, TIMER_ENABLE | TIMER_IRQ);
            bus.write16(TIMER_RELOAD, 0);
        }

        run_the_transfer(gba.bus_mut());
        let bus = gba.bus_mut();
        assert_ne!(
            bus.irq.read16(crate::irq::reg::IF).unwrap() & crate::irq::source::timer(0),
            0,
            "the overflow raised its interrupt"
        );
        assert_eq!(
            bus.timers.counter(0) as u32,
            EXPECTED_CYCLES - 16,
            "sixteen cycles into the transfer, not at the end of it"
        );
    }

    #[test]
    fn n_timer_overflows_in_one_advance_pop_n_fifo_samples() {
        // A reload of 0xFFFF at prescaler 1 overflows on every single cycle, which is the
        // cheapest way to ask for more than one overflow in one call. It is also what a bitmask
        // return cannot express: four overflows arrive as one bit, and channel A advances by one
        // sample where hardware advanced by four.
        let mut gba = system();
        let bus = gba.bus_mut();
        bus.write16(crate::fifo::reg::SOUNDCNT_X, 1 << 7);
        for _ in 0..2 {
            bus.write32(crate::fifo::reg::FIFO_A, 0x0403_0201);
        }
        assert_eq!(bus.sound.a.len(), 8, "eight samples queued");

        bus.write16(TIMER_RELOAD, 0xFFFF);
        bus.write16(TIMER_CONTROL, TIMER_ENABLE);
        bus.advance(4);

        assert_eq!(
            bus.sound.a.len(),
            4,
            "four overflows in one call, four samples popped"
        );
    }

    #[test]
    fn a_transfer_does_not_charge_its_wait_states_to_whatever_runs_next() {
        // External WRAM is the expensive end of the map — six cycles for a word — so a burst of
        // it is unmistakable if it lands on the wrong account. It used to: every wait state a
        // transfer incurred sat in `pending_waits` until the *next instruction* took the lot,
        // and the latch it left behind made that instruction's fetch look like a jump too.
        let mut gba = system();
        let bus = gba.bus_mut();
        bus.write32(SOURCE, 0x0200_0000);
        bus.write32(DESTINATION, 0x0200_2000);
        bus.write16(WORD_COUNT, 64);
        bus.take_pending_waits();

        bus.write16(CONTROL, START_NOW_IN_WORDS);
        assert_eq!(
            bus.take_pending_waits(),
            0,
            "an I/O store waits for nothing, and the transfer's accesses are the transfer's"
        );
        assert_eq!(
            bus.next_sequential_address(),
            CONTROL + 2,
            "the latch sits just past the store the CPU made, not inside the copy"
        );
    }

    #[test]
    fn a_transfer_that_re_arms_itself_terminates_instead_of_recursing() {
        // The pathological input the re-entrancy guard exists for: a channel whose destination is
        // its own control register, copying an enable bit into it. Every completed block arms the
        // same channel again from inside `write16_routed`'s DMA hook, which used to call straight
        // back into `run_pending_dma` — unbounded recursion, and a stack overflow rather than a
        // failed assertion. It now finishes the block, picks the re-armed channel up from the
        // drain loop, and gives the bus back after a frame's worth of cycles so the machine can
        // be traced rather than hung.
        let mut gba = system();
        let bus = gba.bus_mut();
        // Immediate, 16-bit units, enabled: the value the copy will keep writing to CONTROL.
        bus.memory.write16(0x0200_0000, 0x8000);
        bus.write32(SOURCE, 0x0200_0000);
        bus.write32(DESTINATION, CONTROL);
        bus.write16(WORD_COUNT, 1);
        // Source and destination both fixed (bits 7-8 and 5-6 = 2), enabled, immediate.
        bus.write16(CONTROL, 0x0100 | 0x0040 | 0x8000);

        assert!(
            bus.take_dma_cycles() >= FRAME_CYCLES as u32,
            "it ran until the progress bound rather than for ever"
        );
    }
}

/// There is no floating bus on a `None`-typed emulated read: an unmapped address, or the BIOS
/// read from outside it, has to answer *something*, and hardware answers with whatever it last
/// fetched. Driven from `GbaSystem::update_open_bus`, called once per `step_instruction` from the
/// program counter about to run.
mod open_bus {
    use super::*;

    /// A physical address nothing in this crate maps, so a read here can only be open bus.
    const UNMAPPED: u32 = 0x1000_0000;

    #[test]
    fn an_unmapped_read_returns_the_last_arm_word_fetched() {
        // `system()`'s ROM is `b .` (0xEAFF_FFFE) at the cartridge entry, in ARM state.
        let mut gba = system();
        gba.step_instruction();
        assert_eq!(
            gba.bus_mut().memory.read32(UNMAPPED),
            Some(0xEAFF_FFFE),
            "open bus mirrors the last instruction fetched"
        );
    }

    #[test]
    fn an_unmapped_read_returns_the_last_thumb_halfword_duplicated_into_both_halves() {
        let mut gba = system();
        {
            gba.cpu.cpsr.set_thumb(true);
            // Internal WRAM rather than the cartridge entry: ROM writes are no-ops, and this test
            // needs to plant its own instruction.
            gba.cpu.regs.set_pc(0x0300_0000);
            gba.bus_mut().write16(0x0300_0000, 0xE7FE); // Thumb `b .`
        }
        gba.step_instruction();
        assert_eq!(
            gba.bus_mut().memory.read32(UNMAPPED),
            Some(0xE7FE_E7FE),
            "a Thumb fetch is a halfword; open bus duplicates it into both halves of the word"
        );
    }
}

/// The BIOS is visible only to code executing inside it, and `GbaSystem::update_in_bios`
/// recomputes that per step rather than trusting a flag latched once at construction.
mod in_bios {
    use super::*;

    #[test]
    fn a_read_of_bios_space_from_outside_it_is_refused_even_with_a_bios_loaded() {
        let mut bios = vec![0u8; 0x4000];
        // A byte only the real BIOS image has, so a leak is unmistakable.
        bios[0] = 0x5A;
        let mut gba = GbaSystem::new(spin_rom(), Some(bios)).unwrap();

        // Sanity check: the boot state starts at the reset vector, inside the BIOS, so its
        // content is visible from there.
        assert_eq!(
            gba.bus_mut().memory.read8(0),
            Some(0x5A),
            "the BIOS is visible to code running inside it"
        );

        // Move execution to the cartridge — well outside the BIOS's 16 KiB — and take one step,
        // which is what `update_in_bios` keys off.
        gba.cpu.regs.set_pc(CARTRIDGE_ENTRY);
        gba.step_instruction();

        assert_ne!(
            gba.bus_mut().memory.read8(0),
            Some(0x5A),
            "BIOS content leaked to code running outside the BIOS"
        );
    }
}
