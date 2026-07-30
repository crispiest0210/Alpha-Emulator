use super::*;

const ARM9: Core = Core::Arm9;
const ARM7: Core = Core::Arm7;

/// `IPCFIFOCNT` with the FIFOs enabled and no interrupts.
const ENABLE: u16 = 1 << 15;

fn enabled() -> Ipc {
    let mut ipc = Ipc::new();
    ipc.write_control(ARM9, ENABLE);
    ipc.write_control(ARM7, ENABLE);
    ipc
}

#[test]
fn each_core_reads_the_others_sync_nibble_and_its_own() {
    let mut ipc = Ipc::new();
    ipc.write_sync(ARM9, 0x0500);
    ipc.write_sync(ARM7, 0x0A00);

    assert_eq!(ipc.read_sync(ARM9) & 0x0F, 0x0A, "ARM7's output, low");
    assert_eq!((ipc.read_sync(ARM9) >> 8) & 0x0F, 0x05, "its own, high");
    assert_eq!(ipc.read_sync(ARM7) & 0x0F, 0x05);
    assert_eq!((ipc.read_sync(ARM7) >> 8) & 0x0F, 0x0A);
}

#[test]
fn the_sync_strobe_is_not_a_stored_bit() {
    let mut ipc = Ipc::new();
    ipc.write_sync(ARM9, 1 << 13);
    assert_eq!(
        ipc.read_sync(ARM9) & (1 << 13),
        0,
        "bit 13 never reads back"
    );
}

#[test]
fn a_sync_strobe_interrupts_the_other_core_only_when_it_asked() {
    let mut ipc = Ipc::new();
    // ARM7 has not enabled the interrupt.
    ipc.write_sync(ARM9, 1 << 13);
    assert!(!ipc.take_pending(ARM7).any());

    ipc.write_sync(ARM7, 1 << 14);
    ipc.write_sync(ARM9, 1 << 13);
    let irqs = ipc.take_pending(ARM7);
    assert!(irqs.sync);
    assert!(!ipc.take_pending(ARM9).any(), "and never the sender");
    // Taking it clears it.
    assert!(!ipc.take_pending(ARM7).any());
}

#[test]
fn a_word_sent_by_one_core_is_received_by_the_other() {
    let mut ipc = enabled();
    ipc.send(ARM9, 0xDEAD_BEEF);
    assert_eq!(ipc.receive_len(ARM7), 1);
    assert_eq!(ipc.receive_len(ARM9), 0, "the other direction is separate");
    assert_eq!(ipc.receive(ARM7), 0xDEAD_BEEF);
    assert_eq!(ipc.receive_len(ARM7), 0);

    ipc.send(ARM7, 0x1234_5678);
    assert_eq!(ipc.receive(ARM9), 0x1234_5678);
}

#[test]
fn the_fifo_is_first_in_first_out_and_holds_sixteen_words() {
    let mut ipc = enabled();
    for i in 0..FIFO_DEPTH as u32 {
        ipc.send(ARM9, i);
    }
    assert_eq!(ipc.receive_len(ARM7), FIFO_DEPTH);
    assert_eq!(ipc.read_control(ARM9) & 0b10, 0b10, "send FIFO reads full");
    assert_eq!(ipc.read_control(ARM7) & (1 << 9), 1 << 9, "recv reads full");

    for i in 0..FIFO_DEPTH as u32 {
        assert_eq!(ipc.receive(ARM7), i);
    }
    assert_eq!(ipc.read_control(ARM9) & 1, 1, "send FIFO reads empty");
}

#[test]
fn the_ring_wraps_rather_than_filling_up_permanently() {
    let mut ipc = enabled();
    // Three laps through a sixteen-word ring, never more than half full.
    for round in 0..3u32 {
        for i in 0..10u32 {
            ipc.send(ARM9, round * 100 + i);
        }
        for i in 0..10u32 {
            assert_eq!(ipc.receive(ARM7), round * 100 + i);
        }
    }
    assert_eq!(ipc.receive_len(ARM7), 0);
}

#[test]
fn overflowing_the_send_fifo_sets_the_error_flag_and_drops_the_word() {
    let mut ipc = enabled();
    for i in 0..FIFO_DEPTH as u32 {
        ipc.send(ARM9, i);
    }
    assert_eq!(ipc.read_control(ARM9) & (1 << 14), 0, "no error yet");
    ipc.send(ARM9, 0xFFFF);
    assert_eq!(ipc.read_control(ARM9) & (1 << 14), 1 << 14);
    assert_eq!(ipc.receive_len(ARM7), FIFO_DEPTH, "nothing was overwritten");
    // The dropped word is the new one, not the oldest.
    for i in 0..FIFO_DEPTH as u32 {
        assert_eq!(ipc.receive(ARM7), i);
    }
}

#[test]
fn reading_an_empty_fifo_repeats_the_last_word_and_flags_the_error() {
    let mut ipc = enabled();
    ipc.send(ARM9, 0xAAAA_5555);
    assert_eq!(ipc.receive(ARM7), 0xAAAA_5555);
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 0);

    assert_eq!(ipc.receive(ARM7), 0xAAAA_5555, "repeats, not zero");
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 1 << 14);
}

#[test]
fn the_error_flag_is_sticky_until_acknowledged_by_writing_it() {
    let mut ipc = enabled();
    ipc.receive(ARM7);
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 1 << 14);
    // An unrelated write leaves it set.
    ipc.write_control(ARM7, ENABLE);
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 1 << 14);
    // Writing a 1 to bit 14 acknowledges.
    ipc.write_control(ARM7, ENABLE | (1 << 14));
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 0);
}

#[test]
fn the_receive_interrupt_fires_on_the_empty_to_nonempty_edge_only() {
    let mut ipc = enabled();
    ipc.write_control(ARM7, ENABLE | (1 << 10));

    ipc.send(ARM9, 1);
    assert!(ipc.take_pending(ARM7).recv_not_empty, "the edge");
    ipc.send(ARM9, 2);
    assert!(
        !ipc.take_pending(ARM7).any(),
        "a second word is not a second edge"
    );

    // Draining and refilling produces the edge again.
    ipc.receive(ARM7);
    ipc.receive(ARM7);
    ipc.send(ARM9, 3);
    assert!(ipc.take_pending(ARM7).recv_not_empty);
}

#[test]
fn the_send_empty_interrupt_fires_when_the_other_core_drains_the_last_word() {
    let mut ipc = enabled();
    ipc.write_control(ARM9, ENABLE | (1 << 2));
    // Arming it while already empty raises it immediately, which is how software gets the
    // first one at all.
    assert!(ipc.take_pending(ARM9).send_empty);

    ipc.send(ARM9, 1);
    ipc.send(ARM9, 2);
    assert!(!ipc.take_pending(ARM9).any());

    ipc.receive(ARM7);
    assert!(!ipc.take_pending(ARM9).any(), "still one word left");
    ipc.receive(ARM7);
    assert!(ipc.take_pending(ARM9).send_empty, "now it is empty");
}

#[test]
fn arming_an_interrupt_that_is_already_armed_does_not_re_raise_it() {
    let mut ipc = enabled();
    ipc.write_control(ARM9, ENABLE | (1 << 2));
    assert!(ipc.take_pending(ARM9).send_empty);
    ipc.write_control(ARM9, ENABLE | (1 << 2));
    assert!(
        !ipc.take_pending(ARM9).any(),
        "level, not edge, would flood the core"
    );
}

#[test]
fn clearing_the_send_fifo_discards_only_that_direction() {
    let mut ipc = enabled();
    ipc.send(ARM9, 1);
    ipc.send(ARM7, 2);
    ipc.write_control(ARM9, ENABLE | (1 << 3));
    assert_eq!(ipc.receive_len(ARM7), 0, "the ARM9's send FIFO");
    assert_eq!(ipc.receive_len(ARM9), 1, "not the ARM7's");
}

#[test]
fn a_disabled_fifo_swallows_sends_and_repeats_on_receive() {
    let mut ipc = Ipc::new();
    ipc.send(ARM9, 0x1234);
    assert_eq!(ipc.receive_len(ARM7), 0, "not stored");
    assert_eq!(ipc.read_control(ARM9) & (1 << 14), 0, "and not an error");

    // A queued word is not delivered while the receiving side has its FIFO off.
    ipc.write_control(ARM9, ENABLE);
    ipc.send(ARM9, 0x5678);
    assert_eq!(ipc.receive(ARM7), 0, "reads the last-received value, zero");
    assert_eq!(ipc.receive_len(ARM7), 1, "and does not pop");
    ipc.write_control(ARM7, ENABLE);
    assert_eq!(ipc.receive(ARM7), 0x5678);
}

#[test]
fn the_control_register_reports_both_directions_from_each_side() {
    let mut ipc = enabled();
    ipc.send(ARM9, 1);
    // The ARM9 sees its send FIFO non-empty; the ARM7 sees its receive FIFO non-empty. Those
    // are the same sixteen words seen from two sides.
    assert_eq!(ipc.read_control(ARM9) & 1, 0, "ARM9 send not empty");
    assert_eq!(ipc.read_control(ARM7) & (1 << 8), 0, "ARM7 recv not empty");
    assert_eq!(ipc.read_control(ARM7) & 1, 1, "ARM7 send is empty");
    assert_eq!(ipc.read_control(ARM9) & (1 << 8), 1 << 8, "ARM9 recv empty");
}

#[test]
fn a_full_round_trip_handshake_behaves_the_way_a_game_drives_it() {
    // The shape of a real IPC exchange: ARM9 arms the receive interrupt, ARM7 sends a command
    // word and syncs, ARM9 takes both interrupts, replies, ARM7 collects.
    let mut ipc = enabled();
    ipc.write_control(ARM9, ENABLE | (1 << 10));
    ipc.write_sync(ARM9, 1 << 14);
    ipc.write_sync(ARM7, 1 << 14);

    ipc.send(ARM7, 0xC0DE_0001);
    ipc.write_sync(ARM7, 0x0100 | (1 << 13) | (1 << 14));

    let arm9 = ipc.take_pending(ARM9);
    assert!(arm9.recv_not_empty && arm9.sync);
    assert_eq!(ipc.receive(ARM9), 0xC0DE_0001);
    assert_eq!(
        ipc.read_sync(ARM9) & 0x0F,
        1,
        "the ARM7's nibble came with it"
    );

    ipc.send(ARM9, 0xC0DE_0002);
    assert_eq!(ipc.receive(ARM7), 0xC0DE_0002);
    assert_eq!(ipc.read_control(ARM9) & (1 << 14), 0, "no errors anywhere");
    assert_eq!(ipc.read_control(ARM7) & (1 << 14), 0);
}

#[test]
fn ipc_round_trips_through_a_save_state_mid_queue() {
    use savestate::{decode_state, encode_state};

    let mut ipc = enabled();
    // Leave the ring wrapped, so a state that stored only a slice would restore wrongly.
    for i in 0..12u32 {
        ipc.send(ARM9, i);
    }
    for _ in 0..8 {
        ipc.receive(ARM7);
    }
    for i in 100..106u32 {
        ipc.send(ARM9, i);
    }
    ipc.write_sync(ARM9, 0x0700);
    ipc.receive(ARM9); // sets the ARM9's error flag

    let blob = encode_state("nds", 1, &ipc);
    let mut restored = Ipc::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.receive_len(ARM7), 10);
    for i in 8..12u32 {
        assert_eq!(restored.receive(ARM7), i);
    }
    for i in 100..106u32 {
        assert_eq!(restored.receive(ARM7), i);
    }
    assert_eq!((restored.read_sync(ARM7)) & 0x0F, 0x07);
    assert_eq!(restored.read_control(ARM9) & (1 << 14), 1 << 14);
}

#[test]
fn a_state_with_impossible_ring_indices_is_rejected() {
    use savestate::{decode_state, encode_state, StateWriter};

    let ipc = enabled();
    let mut w = StateWriter::new();
    ipc.save(&mut w);
    let mut bytes = w.into_inner();
    // The last two bytes are the second queue's head and length.
    *bytes.last_mut().unwrap() = 99;
    let blob = encode_state("nds", 1, &RawBlob(bytes));

    let mut restored = Ipc::new();
    assert!(decode_state("nds", 1, &blob, &mut restored).is_err());
}

/// Writes pre-encoded bytes verbatim, so a test can build a deliberately malformed body.
struct RawBlob(Vec<u8>);

impl Savable for RawBlob {
    fn save(&self, w: &mut StateWriter) {
        w.write_bytes(&self.0);
    }
    fn load(&mut self, _r: &mut StateReader) -> Result<(), StateError> {
        Ok(())
    }
}
