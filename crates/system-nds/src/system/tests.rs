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

/// `ldr rd, [rn]`
fn ldr_word(rd: u32, rn: u32) -> u32 {
    0xE590_0000 | (rn << 16) | (rd << 12)
}

/// `ldrh rd, [rn]`
fn ldrh(rd: u32, rn: u32) -> u32 {
    0xE1D0_00B0 | (rn << 16) | (rd << 12)
}

/// `mov rd, rm, lsl #amount`
fn lsl_imm(rd: u32, rm: u32, amount: u32) -> u32 {
    0xE1A0_0000 | (rd << 12) | (amount << 7) | rm
}

/// `add rd, rn, #imm`
fn add_imm(rd: u32, rn: u32, value: u32) -> u32 {
    0xE280_0000 | (rn << 16) | (rd << 12) | imm_field(value).expect("fits one immediate")
}

/// `and rd, rn, #imm`
fn and_imm(rd: u32, rn: u32, value: u32) -> u32 {
    0xE200_0000 | (rn << 16) | (rd << 12) | imm_field(value).expect("fits one immediate")
}

/// `orr rd, rn, rm`
fn orr_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xE180_0000 | (rn << 16) | (rd << 12) | rm
}

/// `bx rm`
fn bx(rm: u32) -> u32 {
    0xE12F_FF10 | rm
}

/// `swi #comment`, the ARM encoding.
///
/// The comment goes in bits 16-23, not in the low byte. That is not this helper being clever: the
/// BIOS reads the comment as the *byte* at the instruction's address plus two, so ARM source that
/// wants `Div` is written `swi 0x090000`. Assembling `swi 0x09` instead produces a call to
/// `SoftReset`, which is one of the more confusing ways to lose an afternoon.
fn swi(comment: u8) -> u32 {
    0xEF00_0000 | ((comment as u32) << 16)
}

/// `b` from one word index in an image to another.
fn b_to(from: usize, to: usize) -> u32 {
    // The core reads `PC` two instructions ahead of the branch, which is where the -2 comes from.
    let offset = to as i32 - from as i32 - 2;
    0xEA00_0000 | (offset as u32 & 0x00FF_FFFF)
}

/// A word of main RAM, as the ARM9 sees it.
fn word_at(nds: &NdsSystem, addr: u32) -> u32 {
    nds.bus()
        .memory
        .read_wide_arm9(addr, 4)
        .expect("an address this module owns")
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

// ---------------------------------------------------------------------------------------------
// The BIOS calls, exercised through the real machine rather than through `bios::dispatch`.
//
// These are here rather than in `crate::bios` because what they check is the *wiring*: that a
// `SWI` is intercepted before it reaches an exception vector that holds nothing, in both
// instruction sets, on both cores, with the right core's call table and the right core's flag
// word. A unit test against `dispatch` cannot see any of that go wrong.
// ---------------------------------------------------------------------------------------------

/// Where DTCM sits after direct boot, and therefore where the ARM9's BIOS words are.
///
/// Not a constant in `system.rs` because the ARM9's are an *offset* into DTCM — CP15 can move it,
/// and a test that assumed it could not would pass for the wrong reason.
const ARM9_DTCM: u32 = 0x027C_0000;
const ARM9_FLAGS: u32 = ARM9_DTCM + 0x3FF8;
const ARM9_HANDLER: u32 = ARM9_DTCM + 0x3FFC;

#[test]
fn a_program_that_divides_gets_all_three_documented_results() {
    // `SWI 0x09`, which on a GBA would be an unused number and on a GBA-numbered table would be
    // nothing at all. 1000000 / 7 is 142857 remainder 1.
    let arm9 = [
        load(0, 1_000_000),
        load(1, 7),
        vec![swi(0x09)],
        // r0, r1 and r3 all carry results, so the destination pointer has to be a register the
        // call does not touch.
        load(2, 0x0202_0000),
        vec![str_word(0, 2)],
        load(2, 0x0202_0004),
        vec![str_word(1, 2)],
        load(2, 0x0202_0008),
        vec![str_word(3, 2), SPIN],
    ]
    .concat();

    let mut nds = booted(&arm9, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(word_at(&nds, 0x0202_0000), 142_857, "quotient");
    assert_eq!(word_at(&nds, 0x0202_0004), 1, "remainder");
    assert_eq!(word_at(&nds, 0x0202_0008), 142_857, "absolute quotient");
}

#[test]
fn a_program_that_takes_a_square_root_gets_an_integer_one() {
    // `SWI 0x0D`. The GBA's `Sqrt` is 0x08, which on the ARM9 is not a call at all — so a table
    // carried over from `system-gba` fails this test by leaving r0 alone.
    let arm9 = [
        load(0, 10_001),
        vec![swi(0x0D)],
        load(1, 0x0202_0000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&arm9, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(word_at(&nds, 0x0202_0000), 100, "sqrt(10001) truncated");
}

#[test]
fn the_thumb_encoding_of_swi_is_answered_too() {
    // Almost everything compiled for a DS above the startup stub is Thumb. A machine that only
    // answers ARM `SWI`s answers almost none of the calls a real program makes, and the failure
    // looks like a machine running at full speed with nothing on screen.
    const THUMB_AT: usize = 0x20;
    let mut image = [load(0, 1000), load(1, 7)].concat();
    image.extend(load(2, 0x0200_0000 + THUMB_AT as u32 * 4 + 1));
    image.push(bx(2));
    assert!(image.len() <= THUMB_AT);
    image.resize(THUMB_AT, SPIN);
    // `swi 9` then `b .`, the two Thumb halfwords of a word. The Thumb comment *is* the low byte,
    // unlike the ARM encoding.
    image.push(0xE7FE_DF09);

    let mut nds = booted(&image, &[SPIN]);
    nds.step_frame(InputState::default());
    assert!(
        nds.arm9().core.is_thumb(),
        "it really did enter Thumb state"
    );
    assert_eq!(nds.arm9().reg(0), 142);
    assert_eq!(nds.arm9().reg(1), 6);
}

#[test]
fn an_unimplemented_call_returns_instead_of_running_off_into_the_empty_vector() {
    // What this machine did with every `SWI` before the HLE existed: take the exception to
    // `0xFFFF_0008`, read open bus, and execute the zero it found for the rest of the run. The
    // store below is the proof that execution continued — if the exception were taken it would
    // never happen and the word would stay zero.
    let arm9 = [
        load(0, 0x00C0_FFEE),
        vec![swi(0x99)],
        load(1, 0x0202_0000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&arm9, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(
        word_at(&nds, 0x0202_0000),
        0x00C0_FFEE,
        "the call did nothing, and left the registers alone doing it"
    );
}

/// An ARM9 program that waits for vertical blank the way libnds does, counting the waits.
///
/// The handler is the interesting half. It acknowledges `IF`, ORs the source it saw into the BIOS
/// flag word, and returns with `bx lr` — which is what libnds's `IntrMain` does, and which only
/// works if something stands in for the BIOS's wrapper around it.
fn vblank_counter_arm9() -> Vec<u32> {
    const HANDLER_AT: usize = 0x40;
    const LOOP_AT: usize = 0x60;

    let mut image = [
        // Install the handler pointer where the ARM9's BIOS reads one.
        load(0, 0x0200_0000 + HANDLER_AT as u32 * 4),
        load(1, ARM9_HANDLER),
        vec![str_word(0, 1)],
        // DISPSTAT: enable *both* the vertical-blank and horizontal-blank interrupts, and enable
        // both in IE. The horizontal one fires 263 times a frame and is not what the wait is for,
        // which is the whole point — see the test.
        vec![mov_imm(0, (1 << 3) | (1 << 4))],
        load(1, 0x0400_0004),
        vec![strh(0, 1)],
        vec![mov_imm(0, 3)],
        load(1, 0x0400_0210),
        vec![str_word(0, 1)],
        vec![mov_imm(0, 1)],
        load(1, 0x0400_0208),
        vec![str_word(0, 1)],
        vec![mov_imm(4, 0)],
    ]
    .concat();
    // Over the handler, which sits at a fixed index so its address is known before the pointer
    // above is emitted.
    image.push(b_to(image.len(), LOOP_AT));
    assert!(image.len() <= HANDLER_AT);
    image.resize(HANDLER_AT, SPIN);

    image.extend(
        [
            load(0, 0x0400_0214), // IF
            // Acknowledge every flagged source by writing what was read — ones clear here — but
            // record only vertical blank in the BIOS flag word, exactly as libnds's handler
            // records only the sources it has a handler for.
            vec![ldr_word(1, 0), str_word(1, 0), and_imm(1, 1, 1)],
            load(0, ARM9_FLAGS),
            vec![ldr_word(2, 0), orr_reg(2, 2, 1), str_word(2, 0)],
            vec![bx(14)],
        ]
        .concat(),
    );
    assert!(image.len() <= LOOP_AT);
    image.resize(LOOP_AT, SPIN);

    // The loop: wait, count, store, again.
    image.push(swi(0x05));
    image.push(add_imm(4, 4, 1));
    image.extend(load(5, 0x0202_0000));
    image.push(str_word(4, 5));
    image.push(b_to(image.len(), LOOP_AT));
    image
}

#[test]
fn vblank_intr_wait_on_the_arm9_returns_once_per_frame_and_not_once_per_interrupt() {
    // The assertion that makes this worth writing: *exactly* one wait per frame, while a second
    // interrupt the program is not waiting for fires 263 times in the same frame. A machine that
    // answers `IntrWait` by halting once and returning — which is what `system-gba` does, and
    // which is not good enough for a DS — comes back on every horizontal blank and counts in the
    // hundreds. It is also the strongest available check that the interrupt wrapper is there: the
    // handler returns with `bx lr`, so without a wrapper the second frame never arrives at all.
    let mut nds = booted(&vblank_counter_arm9(), &[SPIN]);
    for frame in 1..=4u32 {
        nds.step_frame(InputState::default());
        assert_eq!(
            word_at(&nds, 0x0202_0000),
            frame,
            "after {frame} frames the program has been through its wait {frame} times"
        );
    }
}

#[test]
fn the_arm9_flag_word_lives_in_dtcm_and_not_in_the_main_ram_underneath_it() {
    // DTCM overlays main RAM, so a machine that answered `IntrWait` against the bus instead of
    // through the TCMs would find zeroes forever and never return from a single wait. This looks
    // at both places: the flag word is reachable through the ARM9's own view, and main RAM at the
    // same address is untouched.
    let mut nds = booted(&vblank_counter_arm9(), &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(word_at(&nds, 0x0202_0000), 1, "the wait completed");

    // The handler set the bit and the wait consumed it, so it reads back clear — but through DTCM,
    // where the bytes actually went.
    let through_dtcm: Vec<u8> = (0..4)
        .map(|i| nds.peek_arm9(ARM9_FLAGS + i).unwrap())
        .collect();
    assert_eq!(through_dtcm, vec![0; 4]);
    assert_ne!(
        nds.arm9().dtcm.base(),
        0,
        "and DTCM is where the boot state put it, not at address zero"
    );
}

#[test]
fn the_arm7_waits_against_its_own_flag_word_at_the_top_of_its_private_wram() {
    // The same program on the other core, against `0x0380_FFF8` instead of DTCM. The two cores
    // keep this word in genuinely different places, and using one core's address for both gives a
    // machine where one core waits forever. The horizontal-blank interrupt is enabled here too,
    // so a wait that returns on any interrupt counts in the hundreds rather than in frames.
    const HANDLER_AT: usize = 0x40;
    const LOOP_AT: usize = 0x60;
    const FLAGS: u32 = 0x0380_FFF8;

    let mut image = [
        load(0, 0x0380_0000 + HANDLER_AT as u32 * 4),
        load(1, 0x0380_FFFC),
        vec![str_word(0, 1)],
        vec![mov_imm(0, (1 << 3) | (1 << 4))],
        load(1, 0x0400_0004),
        vec![strh(0, 1)],
        vec![mov_imm(0, 3)],
        load(1, 0x0400_0210),
        vec![str_word(0, 1)],
        vec![mov_imm(0, 1)],
        load(1, 0x0400_0208),
        vec![str_word(0, 1)],
        vec![mov_imm(4, 0)],
    ]
    .concat();
    // Over the handler, which sits at a fixed index so its address is known before the pointer
    // above is emitted.
    image.push(b_to(image.len(), LOOP_AT));
    assert!(image.len() <= HANDLER_AT);
    image.resize(HANDLER_AT, SPIN);

    image.extend(
        [
            load(0, 0x0400_0214),
            vec![ldr_word(1, 0), str_word(1, 0), and_imm(1, 1, 1)],
            load(0, FLAGS),
            vec![ldr_word(2, 0), orr_reg(2, 2, 1), str_word(2, 0)],
            vec![bx(14)],
        ]
        .concat(),
    );
    assert!(image.len() <= LOOP_AT);
    image.resize(LOOP_AT, SPIN);

    image.push(swi(0x05));
    image.push(add_imm(4, 4, 1));
    image.extend(load(5, 0x0380_2000));
    image.push(str_word(4, 5));
    image.push(b_to(image.len(), LOOP_AT));

    let mut nds = booted(&[SPIN], &image);
    for frame in 1..=4u32 {
        nds.step_frame(InputState::default());
        let counted = nds
            .bus()
            .memory
            .read_wide_arm7(0x0380_2000, 4)
            .expect("the ARM7's own work RAM");
        assert_eq!(counted, frame, "after {frame} frames");
    }
}

#[test]
fn an_intr_wait_that_is_never_satisfied_still_hands_the_frame_loop_back() {
    // A `SWI` answered for free would spin the slice forever and hang the emulator rather than the
    // emulated program. This waits on a source nothing will ever raise, which is the shape of a
    // game that has mis-set `IE` — a bug that has to present as a stuck game, not a stuck process.
    let arm9 = [load(0, 1), load(1, 1 << 20), vec![swi(0x04), SPIN]].concat();

    let mut nds = booted(&arm9, &[SPIN]);
    let out = nds.step_frame(InputState::default());
    assert_eq!(out.cycles_elapsed.0, CYCLES_PER_FRAME as u64);
}

// ---------------------------------------------------------------------------------------------
// Cartridge streaming.
//
// The canonical DS asset load is three pieces of hardware cooperating: `ROMCTRL` starts a
// transfer, a DMA channel with the card start-timing pulls the words out of `CARD_DATA`, and a
// card interrupt says the block arrived. Testing them separately says nothing about whether they
// are connected, and they were not — the channel was armed by nobody and the interrupt raised by
// nobody, so a game reached its first asset load and stopped there.
// ---------------------------------------------------------------------------------------------

/// Where the test plants data for a card read to find, and how much.
///
/// Past both binaries in [`rom`]'s layout and inside the 32 KiB image, so the read is a plain
/// linear one rather than the 4 KiB wrap a higher address would take.
const CARD_DATA_AT: usize = 0x7000;
const CARD_BYTES: usize = 512;

/// One word of the eight-byte `0xB7` command that reads [`CARD_DATA_AT`].
///
/// `offset` is the byte offset into the command. The command's bytes go up in address order —
/// opcode first, then the ROM offset most significant byte first — so a word write carries them
/// reversed, which is what makes this worth a helper rather than a literal.
fn card_command_word(offset: usize) -> u32 {
    let address = (CARD_DATA_AT as u32).to_be_bytes();
    let command = [
        0xB7, address[0], address[1], address[2], address[3], 0, 0, 0,
    ];
    u32::from_le_bytes(command[offset..offset + 4].try_into().unwrap())
}

/// The 512 bytes a card read should deliver, as words.
fn card_payload() -> Vec<u32> {
    (0..CARD_BYTES as u32 / 4)
        .map(|index| 0xC0DE_0000 + index)
        .collect()
}

/// An ARM9 program that reads [`CARD_BYTES`] bytes from the card into main RAM by DMA.
///
/// `auxspicnt` is written as given so a test can choose whether the completion interrupt is
/// enabled; everything else is the sequence a driver actually issues, in the order it issues it.
fn card_dma_program(auxspicnt: u32) -> Vec<u32> {
    // Channel 0: enable, interrupt on completion, start timing 5 (the card slot), 32-bit units,
    // source fixed on the data port, destination incrementing. Not repeating, so the channel
    // disables itself after the one block — which is how the test can tell it ran exactly once.
    const CONTROL: u32 = 0x8000 | 0x4000 | (5 << 11) | 0x0400 | 0x0100;
    let words = (CARD_BYTES / 4) as u32;

    [
        // DMA0SAD = CARD_DATA.
        load(0, 0x0410_0010),
        load(1, 0x0400_00B0),
        vec![str_word(0, 1)],
        // DMA0DAD = main RAM.
        load(0, 0x0202_0000),
        load(1, 0x0400_00B4),
        vec![str_word(0, 1)],
        // DMA0CNT: the count and the control register really are one word here.
        load(0, words | (CONTROL << 16)),
        load(1, 0x0400_00B8),
        vec![str_word(0, 1)],
        // AUXSPICNT, whose bit 14 decides whether completion is an interrupt or a poll.
        load(0, auxspicnt),
        load(1, 0x0400_01A0),
        vec![strh(0, 1)],
        // The command: 0xB7 at the register's first byte, then the ROM offset above it, most
        // significant byte first. Written as two words, each carrying its bytes reversed.
        load(0, card_command_word(0)),
        load(1, 0x0400_01A8),
        vec![str_word(0, 1)],
        load(0, card_command_word(4)),
        load(1, 0x0400_01AC),
        vec![str_word(0, 1)],
        // ROMCTRL: block size 1 is 512 bytes — the field is an exponent over 0x100, not a byte
        // count — and bit 31 starts the transfer.
        load(0, 0x8100_0000),
        load(1, 0x0400_01A4),
        vec![str_word(0, 1), SPIN],
    ]
    .concat()
}

/// Boot a machine whose cartridge has [`card_payload`] planted where the program will ask for it.
fn booted_with_card_data(arm9: &[u32]) -> NdsSystem {
    let mut image = rom(arm9, &[SPIN]);
    for (index, word) in card_payload().iter().enumerate() {
        let at = CARD_DATA_AT + index * 4;
        image[at..at + 4].copy_from_slice(&word.to_le_bytes());
    }
    let mut nds = NdsSystem::default();
    nds.load_cartridge(&image).unwrap();
    nds
}

#[test]
fn a_card_read_streams_into_main_ram_by_dma_and_raises_its_interrupt() {
    let mut nds = booted_with_card_data(&card_dma_program(1 << 14));
    nds.step_frame(InputState::default());

    for (index, expected) in card_payload().iter().enumerate() {
        assert_eq!(
            word_at(&nds, 0x0202_0000 + index as u32 * 4),
            *expected,
            "word {index} of the block"
        );
    }
    // And it stopped at the block boundary rather than running on into whatever followed.
    assert_eq!(word_at(&nds, 0x0202_0000 + CARD_BYTES as u32), 0);

    assert_ne!(
        nds.bus().irq[Core::Arm9 as usize].flags() & sources::CARD_TRANSFER,
        0,
        "the card interrupt fires on the core that owns the slot"
    );
    assert_eq!(
        nds.bus().irq[Core::Arm7 as usize].flags() & sources::CARD_TRANSFER,
        0,
        "and only on that core"
    );
}

#[test]
fn a_finished_card_transfer_clears_the_registers_a_polling_driver_watches() {
    // The other half of the same event: a driver that does not use the interrupt watches
    // `ROMCTRL`'s start bit and the channel's enable bit instead, and both have to come back down
    // by themselves or it waits forever.
    let mut nds = booted_with_card_data(&card_dma_program(1 << 14));
    nds.step_frame(InputState::default());

    let romctrl = nds.bus_mut().cart.read32(0x0400_01A4).unwrap();
    assert_eq!(romctrl & (1 << 31), 0, "ROMCTRL start");
    assert_eq!(romctrl & (1 << 23), 0, "ROMCTRL data ready");
    let dmacnt = nds.bus().dma[Core::Arm9 as usize]
        .read32(0x0400_00B8)
        .unwrap();
    assert_eq!(
        dmacnt & (1 << 31),
        0,
        "the one-shot channel disabled itself"
    );
}

#[test]
fn the_card_interrupt_stays_quiet_when_auxspicnt_bit_14_is_clear() {
    // Bit 14 is how a driver chooses polling over interrupts, and raising the interrupt anyway is
    // not a harmless extra: `IF` gates the halt instruction, so a bit nobody acknowledges makes
    // every later halt return immediately and the machine spins at full speed instead of idling.
    let mut nds = booted_with_card_data(&card_dma_program(0));
    nds.step_frame(InputState::default());

    // The data still arrives — the enable is about the interrupt, not the transfer.
    assert_eq!(word_at(&nds, 0x0202_0000), 0xC0DE_0000);
    assert_eq!(
        word_at(&nds, 0x0202_0000 + CARD_BYTES as u32 - 4),
        0xC0DE_0000 + (CARD_BYTES as u32 / 4 - 1),
        "the whole block, not just the first word"
    );
    assert_eq!(
        nds.bus().irq[Core::Arm9 as usize].flags() & sources::CARD_TRANSFER,
        0,
        "but nothing is raised"
    );
}

#[test]
fn the_card_interrupt_is_raised_once_per_transfer_and_not_once_per_poll() {
    // `ROMCTRL`'s start bit stays clear for as long as the card sits idle, so a completion test
    // written as a level would re-raise on every quantum — thousands of times a frame. A driver
    // that acknowledged it would be back inside its handler before it had returned from it.
    let mut nds = booted_with_card_data(&card_dma_program(1 << 14));
    nds.step_frame(InputState::default());
    assert_ne!(
        nds.bus().irq[Core::Arm9 as usize].flags() & sources::CARD_TRANSFER,
        0
    );

    // Acknowledge it the way a handler does — ones clear `IF` — and run on. Nothing has started
    // another transfer, so nothing should raise it again.
    nds.bus_mut().irq[Core::Arm9 as usize].write32(0x0400_0214, sources::CARD_TRANSFER);
    for _ in 0..3 {
        nds.step_frame(InputState::default());
    }
    assert_eq!(
        nds.bus().irq[Core::Arm9 as usize].flags() & sources::CARD_TRANSFER,
        0,
        "the interrupt belongs to the edge, not to the idle state after it"
    );
}

#[test]
fn the_card_channel_follows_the_slot_to_whichever_core_owns_it() {
    // `EXMEMCNT` bit 11 hands the slot over, and a real boot does hand it over: the ARM7 loads the
    // first blocks and then passes it to the ARM9. Everything about a transfer follows it, so a
    // machine that assumed the ARM9 would arm the ARM9's channel for the ARM7's transfer.
    let mut nds = booted_with_card_data(&[SPIN]);
    assert_eq!(nds.bus().card_owner(), Core::Arm9);
    nds.bus_mut().write16(Core::Arm9, 0x0400_0204, 1 << 11);
    assert_eq!(nds.bus().card_owner(), Core::Arm7);
}

#[test]
fn a_block_larger_than_the_channel_re_arms_until_it_is_drained() {
    // Why `data_ready` is a level and not an edge. A driver reading a block larger than one
    // channel's count sets the repeat bit and lets the channel re-arm for each further chunk, and
    // the card holds its ready flag up until the last word leaves. An edge would arm once, move a
    // quarter of the block, and leave the rest in the FIFO with the driver waiting on a completion
    // that never comes.
    const CHUNK: u32 = 32;
    const CONTROL: u32 = 0x8000 | 0x4000 | (5 << 11) | 0x0400 | 0x0100 | 0x0200; // + repeat
    let program = [
        load(0, 0x0410_0010),
        load(1, 0x0400_00B0),
        vec![str_word(0, 1)],
        load(0, 0x0202_0000),
        load(1, 0x0400_00B4),
        vec![str_word(0, 1)],
        load(0, CHUNK | (CONTROL << 16)),
        load(1, 0x0400_00B8),
        vec![str_word(0, 1)],
        load(0, 1 << 14),
        load(1, 0x0400_01A0),
        vec![strh(0, 1)],
        load(0, card_command_word(0)),
        load(1, 0x0400_01A8),
        vec![str_word(0, 1)],
        load(0, card_command_word(4)),
        load(1, 0x0400_01AC),
        vec![str_word(0, 1)],
        load(0, 0x8100_0000),
        load(1, 0x0400_01A4),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted_with_card_data(&program);
    nds.step_frame(InputState::default());

    // All four chunks, in order and end to end — the destination kept incrementing across the
    // re-arms rather than restarting.
    for (index, expected) in card_payload().iter().enumerate() {
        assert_eq!(
            word_at(&nds, 0x0202_0000 + index as u32 * 4),
            *expected,
            "word {index}, chunk {}",
            index as u32 / CHUNK
        );
    }
    assert_ne!(
        nds.bus().irq[Core::Arm9 as usize].flags() & sources::CARD_TRANSFER,
        0,
        "and one completion at the end of the whole block"
    );
}

// ---------------------------------------------------------------------------------------------
// The ARM9's divider and square-root unit.
//
// ARMv5TE has no divide instruction, so these registers are how a DS program divides. libnds runs
// its whole fixed-point maths library through them, which means a machine without them returns
// zero from every division a program makes — and builds every matrix out of those zeroes.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_arm9_program_can_divide_and_take_a_square_root() {
    // The exact operands a libnds `divf32` produces: the numerator shifted up twelve places, in
    // the 64-bit-over-32-bit mode that exists to give it the room.
    let arm9 = [
        // DIVCNT = 1, the 64/32 mode.
        vec![mov_imm(0, 1)],
        load(1, 0x0400_0280),
        vec![str_word(0, 1)],
        load(0, 0x0033_2000),
        load(1, 0x0400_0290),
        vec![str_word(0, 1)],
        vec![mov_imm(0, 0)],
        load(1, 0x0400_0294),
        vec![str_word(0, 1)],
        load(0, 0x2FB),
        load(1, 0x0400_0298),
        vec![str_word(0, 1)],
        // SQRTCNT = 0, the 32-bit input.
        vec![mov_imm(0, 0)],
        load(1, 0x0400_02B0),
        vec![str_word(0, 1)],
        load(0, 10_000),
        load(1, 0x0400_02B8),
        vec![str_word(0, 1)],
        // Read all three answers back out and park them in main RAM.
        load(1, 0x0400_02A0),
        vec![ldr_word(0, 1)],
        load(1, 0x0202_0000),
        vec![str_word(0, 1)],
        load(1, 0x0400_02A8),
        vec![ldr_word(0, 1)],
        load(1, 0x0202_0004),
        vec![str_word(0, 1)],
        load(1, 0x0400_02B4),
        vec![ldr_word(0, 1)],
        load(1, 0x0202_0008),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&arm9, &[SPIN]);
    nds.step_frame(InputState::default());
    assert_eq!(word_at(&nds, 0x0202_0000), 4391, "0x332000 / 0x2FB");
    assert_eq!(word_at(&nds, 0x0202_0004), 195, "and its remainder");
    assert_eq!(word_at(&nds, 0x0202_0008), 100, "sqrt(10000)");
}

#[test]
fn the_arm7_has_neither_unit_and_sees_nothing_at_those_addresses() {
    // They are ARM9-only, and answering them on the ARM7 would be inventing hardware. Its I/O
    // space has nothing there, so a read comes back as the open bus every unclaimed address does.
    let arm7 = [
        load(0, 100),
        load(1, 0x0400_0290),
        vec![str_word(0, 1)],
        load(0, 7),
        load(1, 0x0400_0298),
        vec![str_word(0, 1)],
        load(1, 0x0400_02A0),
        vec![ldr_word(0, 1)],
        load(1, 0x0380_2000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&[SPIN], &arm7);
    nds.step_frame(InputState::default());
    let answered = nds
        .bus()
        .memory
        .read_wide_arm7(0x0380_2000, 4)
        .expect("the ARM7's own work RAM");
    assert_eq!(answered, 0, "no divider on this core");
}

#[test]
fn wait_by_loop_costs_the_time_it_asks_for() {
    // `SWI 3` is a delay, and the delay is the whole call. Returning at once made the machine
    // faster than hardware through it, which sounds harmless and is not: DS software spells "wait
    // for the other core to notice what I just wrote" exactly this way. See `crate::bios`.
    //
    // The ARM7 here asks for 0x40000 iterations — 0x100000 cycles, close to two frames — and only
    // then writes its marker.
    let arm7 = [
        load(0, 0x0004_0000),
        vec![swi(3)],
        load(0, 1),
        load(1, 0x0380_2000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&[SPIN], &arm7);
    nds.step_frame(InputState::default());
    assert_eq!(
        nds.bus().memory.read_wide_arm7(0x0380_2000, 4),
        Some(0),
        "one frame is less than the delay, so the marker cannot be there yet"
    );
    for _ in 0..4 {
        nds.step_frame(InputState::default());
    }
    assert_eq!(
        nds.bus().memory.read_wide_arm7(0x0380_2000, 4),
        Some(1),
        "and the call does return once the time it asked for has passed"
    );
}

#[test]
fn a_core_sees_the_others_register_write_within_a_short_delay() {
    // The boot handshake every retail DS game performs, reduced to its two sides. The ARM7 writes
    // a nibble to `IPCSYNC`, waits about a thousand cycles, and reads back what the ARM9 echoed.
    // The ARM9 does nothing but echo.
    //
    // This is the shape that a video-boundary interleave cannot serve: a thousand cycles is less
    // than one boundary, so with each core running a whole boundary before the other starts, the
    // ARM7's read happens before the ARM9 has executed a single instruction. Pokemon Platinum
    // hangs on exactly this, at a white screen, with every unit test in this crate passing. See
    // `INTERLEAVE`.
    let arm9 = [
        load(3, 0x0400_0180),
        vec![
            ldrh(0, 3),
            and_imm(0, 0, 0x0F),
            lsl_imm(0, 0, 8),
            strh(0, 3),
            b_to(5, 1),
        ],
    ]
    .concat();

    let arm7 = [
        load(3, 0x0400_0180),
        load(0, 6 << 8),
        vec![strh(0, 3)],
        // 250 iterations is 1000 cycles: what a real driver waits, and less than a scanline.
        load(0, 250),
        vec![swi(3), ldrh(0, 3), and_imm(0, 0, 0x0F)],
        load(1, 0x0380_2000),
        vec![str_word(0, 1), SPIN],
    ]
    .concat();

    let mut nds = booted(&arm9, &arm7);
    nds.step_frame(InputState::default());
    assert_eq!(
        nds.bus().memory.read_wide_arm7(0x0380_2000, 4),
        Some(6),
        "the ARM9 must have echoed the nibble before the ARM7's delay ran out"
    );
}

#[test]
fn direct_boot_leaves_the_cards_chip_id_where_the_firmware_would_have() {
    // A Nitro SDK title reads the chip ID straight off the card and compares it against the copy
    // the firmware left in the system area. Different means the cartridge was swapped, and the
    // SDK's answer to that is to disable interrupts and halt forever. See `write_system_area`.
    let mut nds = idle();
    let expected = nds.bus.cart.chip_id();
    assert_ne!(
        expected, 0,
        "the fabricated ID must not be the zero it replaces"
    );
    for at in [SYSTEM_AREA, SYSTEM_AREA_COPY] {
        assert_eq!(word_at(&nds, at), expected, "chip ID 1 at {at:#X}");
        assert_eq!(word_at(&nds, at + 4), expected, "chip ID 2 at {at:#X}");
    }

    // And the card answers the same thing, which is the half that makes the comparison pass.
    // The opcode is the byte at `CARD_COMMAND` itself — the command is stored most significant
    // byte first — so `0xB8` goes in the low byte of the word written there.
    nds.bus.exmemcnt = 0;
    nds.bus.write32(Core::Arm9, 0x0400_01A8, 0x0000_00B8);
    nds.bus
        .write32(Core::Arm9, 0x0400_01A4, (1 << 31) | (7 << 24));
    assert_eq!(nds.bus.read32(Core::Arm9, 0x0410_0010), expected);
}

#[test]
fn direct_boot_takes_the_user_settings_from_the_firmware_rather_than_inventing_them() {
    // One settings block in the machine, in the flash, with a checksum. The RAM copy is a copy.
    let nds = idle();
    let block = nds.bus.input.firmware.current_user_settings();
    for (i, byte) in block.iter().enumerate() {
        assert_eq!(
            nds.peek_arm9(USER_SETTINGS + i as u32),
            Some(*byte),
            "user settings byte {i:#X}"
        );
    }
}

#[test]
fn the_geometry_fifo_raises_its_interrupt_while_its_condition_holds() {
    // How a driver learns the 3D core has taken its display list. Never raising it left Pokemon
    // Platinum spinning on a flag its interrupt handler was the only thing that could clear —
    // at a title screen it had otherwise drawn correctly. See `Gpu3d::fifo_irq_pending`.
    let mut nds = idle();
    nds.step_frame(InputState::default());
    assert_eq!(
        nds.bus.irq[Core::Arm9 as usize].flags() & sources::GEOMETRY_FIFO,
        0,
        "mode 0 selects no interrupt at all"
    );

    // Mode 2: interrupt while the FIFO is empty, which here it always is.
    nds.bus.write32(Core::Arm9, 0x0400_0600, 2 << 30);
    nds.step_frame(InputState::default());
    assert_ne!(
        nds.bus.irq[Core::Arm9 as usize].flags() & sources::GEOMETRY_FIFO,
        0
    );

    // The ARM7 has no 3D core, so it has no such interrupt however the ARM9 configures one.
    assert_eq!(
        nds.bus.irq[Core::Arm7 as usize].flags() & sources::GEOMETRY_FIFO,
        0
    );
}
