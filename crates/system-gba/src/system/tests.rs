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
