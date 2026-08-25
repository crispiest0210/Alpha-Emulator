use super::*;

/// Step the controller `cycles` t-cycles, collecting every byte it asked to move.
fn run(dma: &mut OamDma, cycles: u32) -> Vec<Copy> {
    (0..cycles).filter_map(|_| dma.step()).collect()
}

#[test]
fn a_write_moves_nothing_for_two_machine_cycles() {
    let mut dma = OamDma::new();
    dma.request(0xC0);

    // The write itself is cycle zero. The next two machine cycles belong to the delay, and
    // hardware keeps OAM readable through both of them.
    assert_eq!(run(&mut dma, STARTUP_CYCLES - 1), vec![]);
    assert!(!dma.is_running(), "still only scheduled");
    assert_eq!(dma.busy_bus(), None, "so nothing is locked out yet");

    // The cycle the delay expires on starts the transfer without moving a byte.
    assert_eq!(run(&mut dma, 1), vec![]);
    assert!(dma.is_running());
    assert_eq!(dma.busy_bus(), Some(MemoryBus::External));
}

#[test]
fn the_transfer_moves_one_byte_per_machine_cycle_and_ends_after_160() {
    let mut dma = OamDma::new();
    dma.request(0xC0);
    run(&mut dma, STARTUP_CYCLES);

    let moved = run(&mut dma, TRANSFER_CYCLES);
    assert_eq!(moved.len(), OAM_BYTES as usize);
    assert_eq!(
        moved[0],
        Copy {
            source: 0xC000,
            offset: 0
        }
    );
    assert_eq!(
        moved[159],
        Copy {
            source: 0xC09F,
            offset: 0x9F
        }
    );
    assert!(!dma.is_running(), "the last byte is also the last cycle");
    assert!(dma.is_idle());
}

#[test]
fn the_first_byte_lands_a_machine_cycle_after_the_transfer_starts() {
    let mut dma = OamDma::new();
    dma.request(0x00);
    run(&mut dma, STARTUP_CYCLES);

    assert_eq!(run(&mut dma, BYTE_CYCLES - 1), vec![], "not yet");
    assert_eq!(
        run(&mut dma, 1),
        vec![Copy {
            source: 0x0000,
            offset: 0
        }]
    );
}

#[test]
fn a_restart_leaves_the_old_transfer_running_until_the_new_one_begins() {
    let mut dma = OamDma::new();
    dma.request(0xC0);
    run(&mut dma, STARTUP_CYCLES + BYTE_CYCLES * 4);
    assert_eq!(dma.busy_bus(), Some(MemoryBus::External));

    // A second write while the first transfer is a quarter done.
    dma.request(0x80);
    let during_delay = run(&mut dma, STARTUP_CYCLES - 1);
    assert!(
        during_delay.iter().all(|c| c.source >> 8 == 0xC0),
        "the old transfer keeps moving its own bytes through the delay"
    );
    assert_eq!(
        dma.busy_bus(),
        Some(MemoryBus::External),
        "and keeps its bus locked out, so OAM stays unreachable across a restart"
    );

    run(&mut dma, 1);
    assert_eq!(
        dma.busy_bus(),
        Some(MemoryBus::Video),
        "the new transfer replaces the old one when it actually starts"
    );
    let moved = run(&mut dma, TRANSFER_CYCLES);
    assert_eq!(moved.len(), OAM_BYTES as usize, "and starts from byte zero");
    assert_eq!(moved[0].source, 0x8000);
}

#[test]
fn a_transfer_only_occupies_the_bus_its_source_is_on() {
    // The distinction is not academic: mooneye's `oam_dma_start` runs a VRAM-sourced transfer
    // and then executes its interrupt vector out of ROM while that transfer is in flight,
    // which only works because the cartridge bus is still free.
    assert_eq!(MemoryBus::of_dma_source(0x80), MemoryBus::Video);
    assert_eq!(MemoryBus::of_dma_source(0x9F), MemoryBus::Video);
    assert_eq!(MemoryBus::of_dma_source(0x00), MemoryBus::External);
    assert_eq!(MemoryBus::of_dma_source(0xC0), MemoryBus::External);
    // Above 0xDF the source aliases work RAM the way the echo region does.
    assert_eq!(MemoryBus::of_dma_source(0xFF), MemoryBus::External);

    assert_eq!(MemoryBus::of(0x0000), MemoryBus::External);
    assert_eq!(MemoryBus::of(0xDFFF), MemoryBus::External);
    assert_eq!(MemoryBus::of(0x8000), MemoryBus::Video);
    assert_eq!(MemoryBus::of(0xFE00), MemoryBus::Video);
    assert_eq!(MemoryBus::of(0xFF80), MemoryBus::Internal, "HRAM");
    assert_eq!(
        MemoryBus::of(0xFF46),
        MemoryBus::Internal,
        "and the I/O page"
    );
}

#[test]
fn a_transfer_in_flight_round_trips() {
    use savestate::{decode_state, encode_state};

    let mut dma = OamDma::new();
    dma.request(0xC0);
    run(&mut dma, STARTUP_CYCLES + BYTE_CYCLES * 3);
    dma.request(0x80);
    run(&mut dma, 1);

    assert!(dma.is_running() && !dma.is_idle());
    let bytes = encode_state("gb-oam-dma", 1, &dma);
    let mut restored = OamDma::new();
    decode_state("gb-oam-dma", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, dma);

    // And the restored controller carries on identically rather than merely comparing equal.
    assert_eq!(run(&mut restored, 64), run(&mut dma, 64));
}

#[test]
fn an_idle_controller_round_trips_as_idle() {
    use savestate::{decode_state, encode_state};

    let dma = OamDma::new();
    let bytes = encode_state("gb-oam-dma", 1, &dma);
    let mut restored = OamDma::new();
    restored.request(0x40);
    decode_state("gb-oam-dma", 1, &bytes, &mut restored).unwrap();
    assert!(restored.is_idle(), "the pending request did not survive");
}
