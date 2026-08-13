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
        bus.write16(crate::video::reg::DISPCNT, 3); // bitmap mode
        for x in 0..240u32 {
            bus.write16(0x0600_0000 + x * 2, 0x001F); // red
        }
    }
    gba.step_frame(InputState::default());
    assert_eq!(gba.framebuffer().pixel(0, 0).r, 0xFF);
    assert_eq!(gba.framebuffer().pixel(239, 0).r, 0xFF);
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
fn a_layer_override_never_changes_the_save_state_bytes() {
    // `LayerOverrides` is a debugger lens over the picture, not part of the machine — see
    // `core_common::LayerOverrides` and `GbaSystemBus::layer_overrides`'s docs for the
    // contract. This is the direct check on it: toggling one must not perturb a single byte of
    // what gets written to disk, or a save file made with the debugger open would stop matching
    // one made without it.
    let mut plain = system();
    plain.step_frame(InputState::default());
    let plain_state = plain.save_state();

    let mut with_overrides = system();
    with_overrides.step_frame(InputState::default());
    with_overrides.bus.layer_overrides = core_common::LayerOverrides {
        bg_hidden: [true, false, true, false],
        obj_hidden: true,
        win_hidden: [true, true],
        solo: Some(core_common::DebugLayer::Bg2),
    };
    let overridden_state = with_overrides.save_state();

    assert_eq!(plain_state, overridden_state);
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
