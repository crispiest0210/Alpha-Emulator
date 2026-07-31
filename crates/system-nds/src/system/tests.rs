use super::*;
use crate::cartridge::HEADER_SIZE;
use crate::video::CYCLES_PER_FRAME;
use core_common::{Buttons, TouchPoint, AUDIO_SAMPLE_RATE};

/// `b .` — branch to itself, the idle loop every test program ends with.
const SPIN: u32 = 0xEAFF_FFFE;

/// Build a ROM whose two binaries are the given ARM words, each entered at its own load address.
fn rom(arm9: &[u32], arm7: &[u32]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[..12].copy_from_slice(b"NDSTEST\0\0\0\0\0");
    rom[0x0C..0x10].copy_from_slice(b"ANDS");
    rom[0x10..0x12].copy_from_slice(b"01");

    let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
        rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut rom, 0x20, 0x4000);
    put(&mut rom, 0x24, 0x0200_0000);
    put(&mut rom, 0x28, 0x0200_0000);
    put(&mut rom, 0x2C, (arm9.len() * 4) as u32);
    put(&mut rom, 0x30, 0x6000);
    put(&mut rom, 0x34, 0x0380_0000);
    put(&mut rom, 0x38, 0x0380_0000);
    put(&mut rom, 0x3C, (arm7.len() * 4) as u32);

    for (i, word) in arm9.iter().enumerate() {
        put(&mut rom, 0x4000 + i * 4, *word);
    }
    for (i, word) in arm7.iter().enumerate() {
        put(&mut rom, 0x6000 + i * 4, *word);
    }
    rom
}

fn booted(arm9: &[u32], arm7: &[u32]) -> NdsSystem {
    let mut nds = NdsSystem::default();
    nds.load_cartridge(&rom(arm9, arm7)).unwrap();
    nds
}

fn idle() -> NdsSystem {
    booted(&[SPIN], &[SPIN])
}

/// The rotate-and-byte immediate field, if this value fits one.
///
/// ARM data-processing immediates are eight bits rotated right by an even amount, so most
/// addresses do not fit one at all — which is why [`load`] exists.
fn imm_field(value: u32) -> Option<u32> {
    (0..16u32).find_map(|rot| {
        let candidate = value.rotate_left(rot * 2);
        (candidate <= 0xFF).then_some((rot << 8) | candidate)
    })
}

/// `mov rd, #imm`, for a value that fits one immediate.
fn mov_imm(rd: u32, value: u32) -> u32 {
    0xE3A0_0000 | (rd << 12) | imm_field(value).expect("fits one immediate")
}

/// Load any 32-bit constant, in as many instructions as it takes.
fn load(rd: u32, value: u32) -> Vec<u32> {
    if let Some(field) = imm_field(value) {
        return vec![0xE3A0_0000 | (rd << 12) | field];
    }
    let mut out = Vec::new();
    for shift in [0u32, 8, 16, 24] {
        let part = value & (0xFF << shift);
        if part == 0 {
            continue;
        }
        let field = imm_field(part).expect("one byte always fits");
        out.push(if out.is_empty() {
            0xE3A0_0000 | (rd << 12) | field
        } else {
            // orr rd, rd, #part
            0xE380_0000 | (rd << 16) | (rd << 12) | field
        });
    }
    out
}

/// `str rd, [rn]`
fn str_word(rd: u32, rn: u32) -> u32 {
    0xE580_0000 | (rn << 16) | (rd << 12)
}

/// `strh rd, [rn]`
fn strh(rd: u32, rn: u32) -> u32 {
    0xE1C0_00B0 | (rn << 16) | (rd << 12)
}

#[test]
fn the_system_reports_itself_the_way_the_frontend_expects() {
    let nds = NdsSystem::default();
    assert_eq!(nds.id(), "nds");
    assert_eq!(nds.display_name(), "Nintendo DS");
    // The dual-screen framebuffer `frontend-core` already carries the layout for.
    assert_eq!(nds.framebuffer().width(), 256);
    assert_eq!(nds.framebuffer().height(), 384);
}

#[test]
fn a_system_with_no_cartridge_still_runs_a_frame() {
    // The `System` contract: `step_frame` must always return, even with nothing loaded.
    let mut nds = NdsSystem::default();
    let out = nds.step_frame(InputState::default());
    assert_eq!(out.cycles_elapsed.0, CYCLES_PER_FRAME as u64);
    assert!(!out.stopped);
}

#[test]
fn direct_boot_copies_both_binaries_and_starts_both_cores() {
    let nds = booted(&[mov_imm(0, 0x42), SPIN], &[mov_imm(1, 0x77), SPIN]);
    // The ARM9's binary is in main RAM, which the ARM7 can also see.
    assert_eq!(
        nds.bus().memory.read8_arm7(0x0200_0000),
        Some(mov_imm(0, 0x42) as u8)
    );
    // The ARM7's is in its own work RAM, which the ARM9 cannot see at all.
    assert_eq!(
        nds.bus().memory.read8_arm7(0x0380_0000),
        Some(mov_imm(1, 0x77) as u8)
    );
    // And the header is where software looks for it.
    assert_eq!(nds.bus().memory.read8_arm9(HEADER_MIRROR), Some(b'N'));
}

#[test]
fn a_rom_whose_header_is_wrong_is_rejected_without_disturbing_the_machine() {
    let mut nds = NdsSystem::default();
    assert!(nds.load_cartridge(&[0u8; 16]).is_err());
    assert!(!nds.bus().cart.is_present());
    // Still runnable afterwards.
    nds.step_frame(InputState::default());
}

#[test]
fn a_frame_is_exactly_one_frames_worth_of_cycles() {
    let mut nds = idle();
    for _ in 0..3 {
        let out = nds.step_frame(InputState::default());
        assert_eq!(out.cycles_elapsed.0, CYCLES_PER_FRAME as u64);
    }
}

#[test]
fn the_arm9_actually_executes_the_code_direct_boot_loaded() {
    // Write 0x42 into main RAM well past the program, then spin.
    let program = [
        vec![mov_imm(0, 0x42)],
        load(1, 0x0201_0000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&program, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(nds.bus().memory.read8_arm9(0x0201_0000), Some(0x42));
}

#[test]
fn the_arm7_runs_too_and_writes_where_only_it_can_see() {
    let program = [
        vec![mov_imm(0, 0x99)],
        load(1, 0x0380_1000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&[SPIN], &program);
    nds.step_frame(InputState::default());
    assert_eq!(nds.bus().memory.read8_arm7(0x0380_1000), Some(0x99));
    // The ARM9's window at that address is the shared WRAM, a different memory entirely.
    assert_ne!(nds.bus().memory.read8_arm9(0x0380_1000), Some(0x99));
}

#[test]
fn the_two_cores_talk_to_each_other_through_the_fifo() {
    // The ARM9 enables its FIFO and pushes a word; the ARM7 enables its FIFO and pops it into
    // its own work RAM. This is prompt 13's dual-CPU IPC acceptance criterion, driven by two
    // real programs on two real cores rather than by calling the IPC module directly.
    let arm9 = [
        load(1, 0x0400_0184),
        load(0, 0x8000), // enable the FIFO
        vec![strh(0, 1)],
        load(1, 0x0400_0188),
        vec![mov_imm(0, 0xAB), str_word(0, 1), SPIN],
    ]
    .concat();
    let arm7 = [
        load(1, 0x0400_0184),
        load(0, 0x8000),
        vec![strh(0, 1)],
        // Wait for the ARM9's push: spin on the receive-FIFO-empty flag.
        vec![
            0xE1D1_00B0, // ldrh r0, [r1]
            0xE310_0C01, // tst r0, #0x100
            0x1AFF_FFFC, // bne back to the ldrh
        ],
        load(1, 0x0410_0000),
        vec![0xE591_0000], // ldr r0, [r1]
        load(1, 0x0380_2000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&arm9, &arm7);
    nds.step_frame(InputState::default());
    assert_eq!(nds.bus().memory.read8_arm7(0x0380_2000), Some(0xAB));
}

#[test]
fn a_vblank_interrupt_reaches_a_handler_with_no_bios_present() {
    // No BIOS is vendored, so the machine has to do what the BIOS would: read the handler
    // address the game left at the pointer and enter it. Without that every game's interrupt
    // code is unreachable, which presents as a hang.
    let arm9 = [
        // Install a handler pointer at the top of DTCM, where the ARM9's BIOS reads one.
        load(0, 0x0200_0100), // the handler address
        load(1, 0x027C_3FFC),
        vec![str_word(0, 1)],
        // DISPSTAT: enable the vblank interrupt.
        vec![mov_imm(0, 1 << 3)],
        load(1, 0x0400_0004),
        vec![strh(0, 1)],
        // IE = vblank, IME = 1.
        vec![mov_imm(0, 1)],
        load(1, 0x0400_0210),
        vec![str_word(0, 1)],
        load(1, 0x0400_0208),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    // The handler, at 0x02000100: store a marker and spin. It never returns, which is fine —
    // the test only needs to observe that it was entered.
    let mut image = arm9.clone();
    assert!(image.len() < 0x40);
    image.resize(0x40, SPIN);
    image.extend_from_slice(
        &[
            load(2, 0x5A),
            load(3, 0x0202_0000),
            vec![str_word(2, 3), SPIN],
        ]
        .concat(),
    );

    let mut nds = booted(&image, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(nds.bus().memory.read8_arm9(0x0202_0000), Some(0x5A));
}

#[test]
fn a_frame_with_the_display_off_is_white_on_both_screens() {
    // Display mode 0 on both engines, which is where a machine starts.
    let mut nds = idle();
    nds.step_frame(InputState::default());
    let fb = nds.framebuffer();
    assert_eq!(&fb.as_bytes()[0..4], &[255, 255, 255, 255], "top screen");
    let bottom = 256 * 192 * 4;
    assert_eq!(&fb.as_bytes()[bottom..bottom + 4], &[255, 255, 255, 255]);
}

#[test]
fn a_program_that_sets_up_a_bitmap_puts_a_colour_on_the_top_screen() {
    // Engine A in display mode 2 reads a VRAM bank directly, which is the shortest path from a
    // program to a visible pixel and so the best end-to-end check of the whole assembly.
    let arm9 = [
        // VRAMCNT_A = enabled, MST 0 (the LCDC window).
        vec![mov_imm(0, 0x80)],
        load(1, 0x0400_0240),
        vec![0xE5C1_0000], // strb r0, [r1]
        // Write a red pixel at the top left of the bank.
        load(0, 0x001F),
        load(1, 0x0680_0000),
        vec![strh(0, 1)],
        // DISPCNT = display mode 2, VRAM block 0.
        load(0, 2 << 16),
        load(1, 0x0400_0000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&arm9, &[SPIN]);
    nds.step_frame(InputState::default());

    let expected = ppu_tile2d::bgr555_to_rgba(0x001F);
    let fb = nds.framebuffer();
    assert_eq!(
        &fb.as_bytes()[0..4],
        &[expected.r, expected.g, expected.b, expected.a]
    );
}

#[test]
fn powcnt1_decides_which_engine_drives_which_screen() {
    let mut nds = idle();
    // Force engine A to draw something identifiable through display mode 2.
    nds.bus.vram.set_control(0, 0x80);
    nds.bus.vram.write16(crate::VramSpace::Lcdc, 0, 0x001F);
    nds.bus.engine_a.write32(0x0400_0000, 2 << 16);

    // The green channel tells the two apart: engine A draws pure red, engine B is white.
    let green = |nds: &NdsSystem, row: usize| nds.framebuffer().as_bytes()[row * 256 * 4 + 1];

    nds.bus.powcnt1 = 0x820F; // engine A on top, as the firmware leaves it
    nds.step_frame(InputState::default());
    assert_eq!(green(&nds, 0), 0, "engine A is on the top screen");
    assert_eq!(green(&nds, 192), 255, "and engine B on the bottom");

    nds.bus.powcnt1 = 0x020F; // engine A on the bottom, as it is at power-on
    nds.step_frame(InputState::default());
    assert_eq!(green(&nds, 0), 255, "the top is engine B now");
    assert_eq!(green(&nds, 192), 0);
}

#[test]
fn an_arm9_program_can_draw_a_triangle_through_the_geometry_fifo() {
    // The whole 3D path end to end: the ARM9 enables the layer, sets a viewport, feeds a display
    // list through GXFIFO, and the triangle appears on the top screen through engine A's BG0.
    let attr = (1u32 << 6) | (1 << 7); // both faces
    let vtx = |x: i32, y: i32| {
        let f = |v: i32| ((v * 4096 / 10) as u32) & 0xFFFF;
        [f(x) | (f(y) << 16), 0u32]
    };
    let mut fifo: Vec<u32> = Vec::new();
    let mut push = |opcode: u32, params: &[u32]| {
        fifo.push(opcode);
        fifo.extend_from_slice(params);
    };
    push(0x60, &[(255 << 16) | (191 << 24)]); // VIEWPORT
    push(0x20, &[0x001F]); // COLOR: red
    push(0x29, &[attr]); // POLYGON_ATTR
    push(0x40, &[0]); // BEGIN_VTXS triangles
    for (x, y) in [(-5, 5), (5, 5), (0, -5)] {
        push(0x23, &vtx(x, y));
    }
    push(0x41, &[]); // END_VTXS
    push(0x50, &[0]); // SWAP_BUFFERS

    let mut program: Vec<u32> = Vec::new();
    // DISPCNT: graphics display, BG0 on, BG0 is the 3D layer.
    program.extend(load(0, (1 << 16) | (1 << 8) | (1 << 3)));
    program.extend(load(1, 0x0400_0000));
    program.push(str_word(0, 1));
    // DISP3DCNT: enable the 3D layer.
    program.extend(load(0, 1));
    program.extend(load(1, 0x0400_0060));
    program.push(str_word(0, 1));
    // Feed the display list, one word at a time to the FIFO.
    program.extend(load(1, 0x0400_0400));
    for word in &fifo {
        program.extend(load(0, *word));
        program.push(str_word(0, 1));
    }
    program.push(SPIN);

    let mut nds = booted(&program, &[SPIN]);
    // Two frames: the first runs the list and swaps at its vblank, the second draws it.
    nds.step_frame(InputState::default());
    nds.step_frame(InputState::default());

    let fb = nds.framebuffer();
    let pixel = |x: usize, y: usize| {
        let base = (y * 256 + x) * 4;
        [fb.as_bytes()[base], fb.as_bytes()[base + 1]]
    };
    // The middle of the triangle is red, and a corner of the screen is not.
    let [r, g] = pixel(128, 110);
    assert!(r > 200 && g < 40, "the triangle: r={r} g={g}");
    assert_ne!(pixel(4, 4), [r, g], "and the corner is something else");
}

#[test]
fn the_geometry_fifo_and_the_sound_channels_share_an_address() {
    // 0x04000400 is GXFIFO to the ARM9 and SOUND0CNT to the ARM7. Which one an address means is
    // decided by which core is asking, which is why the bus decode takes a core at all.
    let mut nds = idle();
    nds.bus.write32(Core::Arm7, 0x0400_0400, 0x8000_007F);
    assert!(nds.bus.apu.channel_is_busy(0));

    // The same word from the ARM9 is a packed command set, and must not touch the sound channel.
    nds.bus.write32(Core::Arm9, 0x0400_0400, 0x0000_0010); // MTX_MODE
    nds.bus.write32(Core::Arm9, 0x0400_0400, 1);
    assert!(
        nds.bus.apu.channel_is_busy(0),
        "the ARM7's channel is intact"
    );
    assert_eq!(
        nds.bus.gpu3d.geometry.matrices.mode,
        crate::gpu3d::matrix::MatrixMode::Position
    );
}

#[test]
fn input_reaches_both_cores_and_the_touchscreen_reaches_only_one() {
    let mut nds = idle();
    nds.set_input(InputState {
        buttons: Buttons::A | Buttons::X,
        touch: Some(TouchPoint { x: 128, y: 96 }),
    });
    // KEYINPUT is active low and visible to both.
    assert_eq!(nds.bus.read16(Core::Arm9, 0x0400_0130), 0x03FE);
    assert_eq!(nds.bus.read16(Core::Arm7, 0x0400_0130), 0x03FE);
    // EXTKEYIN carries X, and only the ARM7 has it.
    assert_eq!(nds.bus.read16(Core::Arm7, 0x0400_0136) & 1, 0);
    assert_eq!(nds.bus.read16(Core::Arm9, 0x0400_0136), 0);
}

#[test]
fn the_shared_wram_split_is_visible_from_both_views_of_the_bus() {
    let mut nds = idle();
    nds.bus.memory.set_split(WramSplit::Arm9First);
    nds.bus.write8(Core::Arm9, 0x0300_0000, 0x11);
    nds.bus.write8(Core::Arm7, 0x0300_0000, 0x22);
    assert_eq!(nds.bus.read8(Core::Arm9, 0x0300_0000), 0x11);
    assert_eq!(nds.bus.read8(Core::Arm7, 0x0300_0000), 0x22);

    // Swapping the halves swaps what each core sees, without moving a byte.
    nds.bus.memory.set_split(WramSplit::Arm9Second);
    assert_eq!(nds.bus.read8(Core::Arm9, 0x0300_0000), 0x22);
    assert_eq!(nds.bus.read8(Core::Arm7, 0x0300_0000), 0x11);
}

#[test]
fn a_dma_transfer_moves_memory_between_the_cores_views() {
    let mut nds = idle();
    for i in 0..16u32 {
        nds.bus.write32(Core::Arm9, 0x0201_0000 + i * 4, 0x1000 + i);
    }
    // Channel 0: 16 words from 0x02010000 to 0x02020000, immediate.
    nds.bus.write32(Core::Arm9, 0x0400_00B0, 0x0201_0000);
    nds.bus.write32(Core::Arm9, 0x0400_00B4, 0x0202_0000);
    nds.bus
        .write32(Core::Arm9, 0x0400_00B8, 16 | ((1u32 << 15 | 1 << 10) << 16));
    nds.run_dma();

    for i in 0..16u32 {
        assert_eq!(
            nds.bus.read32(Core::Arm9, 0x0202_0000 + i * 4),
            0x1000 + i,
            "word {i}"
        );
    }
}

#[test]
fn the_card_slot_belongs_to_one_core_at_a_time() {
    let mut nds = idle();
    // EXMEMCNT bit 11 clear: the ARM9 owns it, so the ARM7 reads nothing there.
    nds.bus.exmemcnt = 0;
    nds.bus.write32(Core::Arm9, 0x0400_01A8, 0xB800_0000);
    nds.bus
        .write32(Core::Arm9, 0x0400_01A4, (1 << 31) | (7 << 24));
    assert_ne!(nds.bus.read32(Core::Arm9, 0x0410_0010), 0);

    nds.bus.exmemcnt = 1 << 11;
    nds.bus.write32(Core::Arm7, 0x0400_01A8, 0xB800_0000);
    nds.bus
        .write32(Core::Arm7, 0x0400_01A4, (1 << 31) | (7 << 24));
    assert_ne!(nds.bus.read32(Core::Arm7, 0x0410_0010), 0);
    // The ARM9 no longer answers for it.
    assert_eq!(nds.bus.read32(Core::Arm9, 0x0400_01A4), 0);
}

#[test]
fn interleaving_is_deterministic_across_identical_runs() {
    // Prompt 13's constraint: the dual-CPU interleaving must produce the same result every
    // time. Two machines given the same ROM and the same input must agree byte for byte.
    let program = [
        vec![mov_imm(0, 0)],
        load(1, 0x0201_0000),
        vec![
            0xE281_0004, // add r1, r1, #4
            str_word(0, 1),
            0xE280_0001, // add r0, r0, #1
            0xEAFF_FFFB, // b back to the add
        ],
    ]
    .concat();
    let mut a = booted(&program, &program);
    let mut b = booted(&program, &program);
    for _ in 0..4 {
        a.step_frame(InputState::default());
        b.step_frame(InputState::default());
    }
    assert_eq!(a.save_state(), b.save_state());
}

#[test]
fn a_save_state_round_trips_and_the_machine_carries_on_identically() {
    let program = [
        vec![mov_imm(0, 0)],
        load(1, 0x0201_0000),
        vec![0xE281_0004, str_word(0, 1), 0xE280_0001, 0xEAFF_FFFB],
    ]
    .concat();
    let mut nds = booted(&program, &program);
    for _ in 0..2 {
        nds.step_frame(InputState::default());
    }
    let state = nds.save_state();

    let mut restored = booted(&program, &program);
    restored.load_state(&state).unwrap();
    assert_eq!(restored.save_state(), state, "the state is a fixed point");

    // And running on from the restore matches running on from the original.
    for _ in 0..2 {
        nds.step_frame(InputState::default());
        restored.step_frame(InputState::default());
    }
    assert_eq!(restored.save_state(), nds.save_state());
    assert_eq!(
        restored.framebuffer().as_bytes(),
        nds.framebuffer().as_bytes()
    );
}

#[test]
fn a_state_from_another_system_is_rejected() {
    let mut nds = idle();
    let mut blob = nds.save_state();
    // Corrupt the system identifier the container carries.
    blob[8] = b'g';
    assert!(nds.load_state(&blob).is_err());
}

#[test]
fn stepping_one_instruction_advances_the_machine_a_little() {
    let mut nds = booted(&[mov_imm(0, 1), mov_imm(0, 2), SPIN], &[SPIN]);
    let before = nds.bus.video.cycle_in_line();
    let cycles = nds.step_instruction();
    assert!(cycles.0 > 0, "a step must always make progress");
    assert!(nds.bus.video.cycle_in_line() >= before);
}

#[test]
fn a_frame_produces_a_frames_worth_of_audio() {
    let mut nds = idle();
    nds.step_frame(InputState::default());
    let count = nds.take_audio_samples().len();
    // 48 kHz over a 59.8261 Hz frame is about 802 samples. Exactness is not the point; producing
    // roughly the right number every frame is, because the frontend's ring depends on it.
    assert!(
        count.abs_diff(802) < 40,
        "{count} samples for one frame at {AUDIO_SAMPLE_RATE} Hz"
    );
    assert!(nds.take_audio_samples().is_empty(), "and draining works");
}

#[test]
fn a_program_that_starts_a_sound_channel_is_heard() {
    // Only the ARM7 can reach the sound hardware, so this is also an end-to-end check that the
    // ARM7 half of a DS program can do the one job it usually exists for.
    let arm7 = [
        // Sample data: a run of loud bytes in the ARM7's own work RAM.
        load(0, 0x6060_6060),
        load(1, 0x0380_2000),
        vec![str_word(0, 1), 0xE2811004, str_word(0, 1)],
        // SOUNDCNT = master enable, full volume.
        load(0, (1 << 15) | 0x7F),
        load(1, 0x0400_0500),
        vec![str_word(0, 1)],
        // Channel 0: source, timer, length, then control with the busy bit.
        load(0, 0x0380_2000),
        load(1, 0x0400_0404),
        vec![str_word(0, 1)],
        load(0, 0xFC00),
        load(1, 0x0400_0408),
        vec![str_word(0, 1)],
        load(0, 2),
        load(1, 0x0400_040C),
        vec![str_word(0, 1)],
        // Busy, centre panning, full volume, PCM8, looping.
        load(0, 0x8840_007F),
        load(1, 0x0400_0400),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&[SPIN], &arm7);
    nds.step_frame(InputState::default());

    let samples = nds.take_audio_samples();
    assert!(
        samples.iter().any(|s| s.left.abs() > 0.01),
        "the channel produced nothing"
    );
}

#[test]
fn the_arm9_cannot_reach_the_sound_hardware() {
    let mut nds = idle();
    nds.bus.write32(Core::Arm7, 0x0400_0400, 0x8000_007F);
    assert!(nds.bus.apu.channel_is_busy(0));
    // The same address on the ARM9 is not the sound hardware and must not disturb it.
    nds.bus.write32(Core::Arm9, 0x0400_0400, 0);
    assert!(nds.bus.apu.channel_is_busy(0));
    assert_eq!(nds.bus.read32(Core::Arm9, 0x0400_0400), 0);
}

#[test]
fn a_save_file_is_only_offered_once_the_chip_has_identified_itself() {
    // Nothing has touched the save chip, so its type is unknown and there is nothing to write.
    let mut nds = idle();
    assert!(nds.save_ram().is_none());

    // A file of a standard size settles the type outright, with no inference.
    nds.load_save_ram(&[0x5A; 8192]).expect("a standard size");
    assert_eq!(nds.save_ram().map(|s| s.len()), Some(8192));

    // And one of no standard size is refused rather than padded into something wrong.
    assert!(matches!(
        nds.load_save_ram(&[0; 1234]),
        Err(CartridgeError::SaveSizeMismatch { .. })
    ));
}

#[test]
fn a_frame_reports_the_save_as_dirty_only_when_the_game_changed_it() {
    // The frontend uses this to schedule a debounced flush, so reporting it every frame would
    // rewrite the file sixty times a second.
    let mut nds = idle();
    nds.load_save_ram(&[0xFF; 8192]).unwrap();
    assert!(!nds.step_frame(InputState::default()).save_ram_dirty);

    // Drive the save chip directly: enable the bus, hold chip select, write-enable, then a page.
    // Through the ARM9, because `EXMEMCNT` gives it the slot after a direct boot.
    let cnt = 0x0400_01A0;
    let data = 0x0400_01A2;
    nds.bus.write16(Core::Arm9, cnt, (1 << 15) | (1 << 6));
    nds.bus.write16(Core::Arm9, data, 0x06); // WREN
    nds.bus.write16(Core::Arm9, cnt, 1 << 15);
    nds.bus.write16(Core::Arm9, data, 0x00);

    nds.bus.write16(Core::Arm9, cnt, (1 << 15) | (1 << 6));
    for byte in [0x02u16, 0x00, 0x00] {
        nds.bus.write16(Core::Arm9, data, byte);
    }
    for i in 0..31u16 {
        nds.bus.write16(Core::Arm9, data, 0x40 + i);
    }
    nds.bus.write16(Core::Arm9, cnt, 1 << 15);
    nds.bus.write16(Core::Arm9, data, 0x60);

    assert!(nds.step_frame(InputState::default()).save_ram_dirty);
    assert!(
        !nds.step_frame(InputState::default()).save_ram_dirty,
        "and the flag clears once reported"
    );
    assert_eq!(nds.save_ram().unwrap()[0], 0x40);
}

#[test]
fn a_reset_returns_the_machine_to_its_freshly_booted_state() {
    let program = [
        vec![mov_imm(0, 0x42)],
        load(1, 0x0201_0000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();
    let mut nds = booted(&program, &[SPIN]);
    let fresh = nds.save_state();
    nds.step_frame(InputState::default());
    assert_ne!(nds.save_state(), fresh);

    nds.reset();
    assert_eq!(nds.save_state(), fresh, "and the cartridge is still loaded");
    assert!(nds.bus().cart.is_present());
    assert_eq!(nds.bus().cart.header().title, "NDSTEST");
    assert_eq!(nds.bus().cart.rom().len(), 0x8000);
    assert!(HEADER_SIZE <= nds.bus().cart.rom().len());
}
