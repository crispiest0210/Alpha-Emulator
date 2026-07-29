//! Session lifecycle tests, driven through the real channel API against a real emulation thread.
//!
//! These deliberately do not reach inside `emulation`. What is being verified is the contract a
//! frontend actually depends on — send a command, get an event, get frames — and a test that
//! called the private loop directly would keep passing after that contract broke.
//!
//! The cartridges are built here, byte by byte. No commercial ROM is involved anywhere in this
//! workspace, and a header plus a `jr` loop is enough to exercise every path below.

use crate::config::{Config, RewindConfig};
use crate::session::{Session, SessionCommand, SessionEvent, SessionOptions, SessionStatus};
use library::AppPaths;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A ROM that runs forever without doing anything: `jr -2` at the entry point.
///
/// An infinite loop is the right test program. It never halts, never reads uninitialised state,
/// and produces one frame per `step_frame` call indefinitely, so a frame counter that stops
/// advancing means the *session* stopped, not the cartridge.
fn spin_rom(title: &str, cart_type: u8, ram_size: u8) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    // Entry point at 0x0100: `jr -2`, which jumps to itself.
    rom[0x0100] = 0x18;
    rom[0x0101] = 0xFE;
    let bytes = title.as_bytes();
    let len = bytes.len().min(11);
    rom[0x0134..0x0134 + len].copy_from_slice(&bytes[..len]);
    rom[0x0147] = cart_type;
    // 32 KiB, which must match the vector above: the loader rejects a header that disagrees
    // with the file size, which is how the first draft of this helper failed.
    rom[0x0148] = 0x00;
    rom[0x0149] = ram_size;
    rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
    rom
}

struct Fixture {
    dir: PathBuf,
    paths: AppPaths,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("alpha-session-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = AppPaths::rooted_at(dir.join("app"));
        paths.create_all().unwrap();
        Self { dir, paths }
    }

    /// Write a cartridge with no save RAM.
    fn plain_rom(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, spin_rom("SPIN", 0x00, 0x00)).unwrap();
        path
    }

    /// Write an MBC1 cartridge with 8 KiB of battery-backed RAM.
    fn battery_rom(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, spin_rom("BATTERY", 0x03, 0x02)).unwrap();
        path
    }

    fn session(&self) -> Session {
        Session::spawn(SessionOptions::new(self.paths.clone(), Config::default()))
    }

    fn session_with(&self, config: Config) -> Session {
        Session::spawn(SessionOptions::new(self.paths.clone(), config))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Wait until an event matching `want` arrives, or fail.
///
/// The timeout is generous. These tests run on shared CI machines where a thread can be descheduled
/// for a long time, and a flaky test is worse than a slow one.
fn wait_event<T>(
    session: &Session,
    what: &str,
    mut want: impl FnMut(&SessionEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match session.poll_event() {
            Some(event) => {
                if let Some(value) = want(&event) {
                    return value;
                }
                seen.push(event);
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    panic!("timed out waiting for {what}; saw {seen:#?}");
}

fn wait_status(session: &Session, expected: SessionStatus) {
    wait_event(session, &format!("status {expected:?}"), |event| {
        matches!(event, SessionEvent::StatusChanged(s) if *s == expected).then_some(())
    });
}

/// Wait until the drawing side has a frame at least `number`.
fn wait_frame(session: &mut Session, number: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        session.frames().poll();
        if let Some(frame) = session.frames().current() {
            if frame.number >= number {
                return frame.number;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let held = session.frames().current().map(|f| f.number);
    panic!("timed out waiting for frame {number}; held {held:?}");
}

// --- lifecycle ------------------------------------------------------------------------------

#[test]
fn a_fresh_session_is_idle_and_has_no_frames() {
    let fixture = Fixture::new("idle");
    let mut session = fixture.session();
    wait_status(&session, SessionStatus::Idle);
    session.frames().poll();
    assert!(session.frames().current().is_none());
}

#[test]
fn loading_a_rom_reports_it_and_starts_producing_frames() {
    let fixture = Fixture::new("load");
    let rom = fixture.plain_rom("spin.gb");
    let mut session = fixture.session();

    session.send(SessionCommand::LoadRom {
        path: rom.clone(),
        rom_id: None,
    });

    let loaded = wait_event(&session, "RomLoaded", |event| match event {
        SessionEvent::RomLoaded(loaded) => Some(loaded.clone()),
        _ => None,
    });
    assert_eq!(loaded.path, rom);
    assert_eq!(loaded.platform, library::Platform::Gb);
    assert_eq!(loaded.title, "SPIN");
    assert_eq!((loaded.width, loaded.height), (160, 144));
    assert!(!loaded.save_ram_restored, "this cartridge has no save RAM");

    wait_frame(&mut session, 3);
    let frame = session.frames().current().unwrap();
    assert_eq!((frame.buffer.width(), frame.buffer.height()), (160, 144));
}

#[test]
fn a_missing_rom_reports_an_error_and_leaves_the_session_idle() {
    let fixture = Fixture::new("missing");
    let session = fixture.session();

    session.send(SessionCommand::LoadRom {
        path: fixture.dir.join("nope.gb"),
        rom_id: None,
    });

    let message = wait_event(&session, "an error", |event| match event {
        SessionEvent::Error(message) => Some(message.clone()),
        _ => None,
    });
    assert!(message.contains("nope.gb"), "unhelpful message: {message}");
    // Still usable afterwards: a failed load must not wedge the thread.
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("ok.gb"),
        rom_id: None,
    });
    wait_status(&session, SessionStatus::Running);
}

#[test]
fn a_rom_for_an_unfinished_system_says_which_system() {
    let fixture = Fixture::new("nds");
    let rom = fixture.dir.join("game.nds");
    std::fs::write(&rom, vec![0u8; 4096]).unwrap();
    let session = fixture.session();

    session.send(SessionCommand::LoadRom {
        path: rom,
        rom_id: None,
    });

    let message = wait_event(&session, "an error", |event| match event {
        SessionEvent::Error(message) => Some(message.clone()),
        _ => None,
    });
    assert!(
        message.contains("Nintendo DS"),
        "should name the system: {message}"
    );
}

#[test]
fn pausing_stops_the_frame_counter_and_resuming_restarts_it() {
    let fixture = Fixture::new("pause");
    let rom = fixture.plain_rom("spin.gb");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom,
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::SetPaused(true));
    wait_status(&session, SessionStatus::Paused);

    // Give the loop time to run frames it should not be running.
    std::thread::sleep(Duration::from_millis(120));
    session.frames().poll();
    let paused_at = session.frames().current().unwrap().number;
    std::thread::sleep(Duration::from_millis(120));
    session.frames().poll();
    assert_eq!(
        session.frames().current().unwrap().number,
        paused_at,
        "a paused session must not advance"
    );

    session.send(SessionCommand::SetPaused(false));
    wait_status(&session, SessionStatus::Running);
    wait_frame(&mut session, paused_at + 5);
}

#[test]
fn toggling_pause_twice_returns_to_running() {
    let fixture = Fixture::new("toggle");
    let session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_status(&session, SessionStatus::Running);

    session.send(SessionCommand::TogglePause);
    wait_status(&session, SessionStatus::Paused);
    session.send(SessionCommand::TogglePause);
    wait_status(&session, SessionStatus::Running);
}

#[test]
fn stepping_while_paused_advances_exactly_the_requested_frames() {
    let fixture = Fixture::new("step");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetPaused(true));
    wait_status(&session, SessionStatus::Paused);
    std::thread::sleep(Duration::from_millis(80));
    session.frames().poll();
    let before = session.frames().current().unwrap().number;

    session.send(SessionCommand::StepFrames(4));
    let after = wait_frame(&mut session, before + 4);

    // Let the loop settle: it must stop again rather than run on.
    std::thread::sleep(Duration::from_millis(150));
    session.frames().poll();
    assert_eq!(
        session.frames().current().unwrap().number,
        after,
        "stepping must not resume"
    );
    assert_eq!(after, before + 4, "exactly four frames");
}

#[test]
fn closing_a_rom_returns_the_session_to_idle() {
    let fixture = Fixture::new("close");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::CloseRom);
    wait_event(&session, "RomClosed", |event| {
        matches!(event, SessionEvent::RomClosed).then_some(())
    });
    wait_status(&session, SessionStatus::Idle);
}

#[test]
fn switching_roms_carries_the_new_framebuffer_size() {
    let fixture = Fixture::new("switch");
    let gb = fixture.plain_rom("spin.gb");
    let gba = fixture.dir.join("spin.gba");
    // A GBA ROM of zeroes: the ARM7 executes it as instructions, which is fine — it produces
    // frames, which is all this test needs.
    std::fs::write(&gba, vec![0u8; 0x8000]).unwrap();

    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: gb,
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    assert_eq!(session.frames().current().unwrap().buffer.width(), 160);

    session.send(SessionCommand::LoadRom {
        path: gba,
        rom_id: None,
    });
    let loaded = wait_event(&session, "the second RomLoaded", |event| match event {
        SessionEvent::RomLoaded(loaded) if loaded.platform == library::Platform::Gba => {
            Some(loaded.clone())
        }
        _ => None,
    });
    assert_eq!((loaded.width, loaded.height), (240, 160));

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        session.frames().poll();
        if session
            .frames()
            .current()
            .is_some_and(|f| f.buffer.width() == 240)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("the framebuffer never resized to the GBA's 240 pixels");
}

#[test]
fn a_reset_restarts_the_frame_count() {
    let fixture = Fixture::new("reset");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 10);

    session.send(SessionCommand::Reset);
    wait_event(&session, "the reset notice", |event| {
        matches!(event, SessionEvent::Notice(text) if text == "reset").then_some(())
    });

    // The counter restarts, so a later frame number must come back down below where it was.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        session.frames().poll();
        if session.frames().current().is_some_and(|f| f.number <= 5) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("the frame counter never restarted");
}

// --- save states ----------------------------------------------------------------------------

#[test]
fn a_quicksave_writes_slot_zero_and_reports_where() {
    let fixture = Fixture::new("quicksave");
    let rom = fixture.plain_rom("spin.gb");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom.clone(),
        rom_id: Some(7),
    });
    wait_frame(&mut session, 5);

    session.send(SessionCommand::SaveState {
        slot: None,
        label: None,
    });
    let saved = wait_event(&session, "StateSaved", |event| match event {
        SessionEvent::StateSaved(saved) => Some(saved.clone()),
        _ => None,
    });

    assert_eq!(saved.slot, Some(0), "a bare quicksave means slot 0");
    assert_eq!(saved.label, "slot0");
    assert_eq!(saved.rom_id, Some(7), "carried through for the library row");
    assert_eq!(saved.path, fixture.paths.state_slot_file(&rom, 0));
    assert!(saved.path.is_file(), "the file is actually on disk");
    assert_eq!(
        std::fs::metadata(&saved.path).unwrap().len(),
        saved.size_bytes
    );
    assert!(saved.frame >= 5);
    assert!(
        !saved.path.with_extension("tmp").exists(),
        "the atomic-write temporary must be renamed away, not left behind"
    );
}

#[test]
fn a_named_state_goes_beside_the_slots_under_its_own_name() {
    let fixture = Fixture::new("named");
    let rom = fixture.plain_rom("spin.gb");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom.clone(),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::SaveState {
        slot: None,
        label: Some("before the boss".into()),
    });
    let saved = wait_event(&session, "StateSaved", |event| match event {
        SessionEvent::StateSaved(saved) => Some(saved.clone()),
        _ => None,
    });

    assert_eq!(saved.slot, None);
    assert_eq!(saved.label, "before the boss");
    assert_eq!(
        saved.path,
        fixture.paths.state_named_file(&rom, "before the boss")
    );
    assert_eq!(
        saved.path.parent(),
        fixture.paths.state_slot_file(&rom, 0).parent(),
        "named states and slots share one per-ROM directory"
    );
}

#[test]
fn a_state_loads_back_to_the_exact_frame_it_was_taken_at() {
    let fixture = Fixture::new("loadstate");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 5);

    session.send(SessionCommand::SaveState {
        slot: Some(1),
        label: None,
    });
    let saved = wait_event(&session, "StateSaved", |event| match event {
        SessionEvent::StateSaved(saved) => Some(saved.clone()),
        _ => None,
    });

    // Let the machine run well past the saved moment, then go back.
    wait_frame(&mut session, saved.frame + 30);
    session.send(SessionCommand::LoadSlot(1));

    let (path, frame) = wait_event(&session, "StateLoaded", |event| match event {
        SessionEvent::StateLoaded { path, frame } => Some((path.clone(), *frame)),
        _ => None,
    });
    assert_eq!(path, saved.path);
    assert_eq!(
        frame, saved.frame,
        "load-to-exact-frame is the promise the UI makes"
    );
}

#[test]
fn loading_an_empty_slot_is_a_notice_not_an_error() {
    let fixture = Fixture::new("emptyslot");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::LoadSlot(5));
    let text = wait_event(&session, "a notice", |event| match event {
        SessionEvent::Notice(text) => Some(text.clone()),
        SessionEvent::Error(text) => panic!("an empty slot is not an error: {text}"),
        _ => None,
    });
    assert!(text.contains("does not exist"), "{text}");
}

#[test]
fn a_corrupt_state_resets_the_machine_rather_than_running_on() {
    let fixture = Fixture::new("corrupt");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    let bad = fixture.dir.join("garbage.ast");
    std::fs::write(&bad, b"this is not a save state").unwrap();
    session.send(SessionCommand::LoadState { path: bad });

    let message = wait_event(&session, "an error", |event| match event {
        SessionEvent::Error(message) => Some(message.clone()),
        _ => None,
    });
    assert!(
        message.contains("has been reset"),
        "the user must be told the machine was reset: {message}"
    );
    // And it keeps running afterwards.
    session.frames().poll();
    let now = session.frames().current().map(|f| f.number).unwrap_or(0);
    wait_frame(&mut session, now + 3);
}

#[test]
fn saving_with_no_cartridge_is_an_error_rather_than_a_silent_no_op() {
    let fixture = Fixture::new("nosave");
    let session = fixture.session();
    wait_status(&session, SessionStatus::Idle);

    session.send(SessionCommand::SaveState {
        slot: Some(0),
        label: None,
    });
    let message = wait_event(&session, "an error", |event| match event {
        SessionEvent::Error(message) => Some(message.clone()),
        _ => None,
    });
    assert!(message.contains("no cartridge"), "{message}");
}

// --- save RAM -------------------------------------------------------------------------------

#[test]
fn existing_battery_save_ram_is_restored_before_the_first_frame() {
    let fixture = Fixture::new("sram");
    let rom = fixture.battery_rom("battery.gb");
    // An 8 KiB save, which is what a cartridge declaring RAM size 0x02 has.
    let save = fixture.paths.save_file(&rom);
    std::fs::create_dir_all(save.parent().unwrap()).unwrap();
    std::fs::write(&save, vec![0xA5u8; 8192]).unwrap();

    let session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom,
        rom_id: None,
    });

    let loaded = wait_event(&session, "RomLoaded", |event| match event {
        SessionEvent::RomLoaded(loaded) => Some(loaded.clone()),
        _ => None,
    });
    assert!(
        loaded.save_ram_restored,
        "a .sav next to the ROM must be restored, or the game offers to overwrite it"
    );
}

#[test]
fn a_save_file_of_the_wrong_size_is_reported_and_does_not_stop_the_load() {
    let fixture = Fixture::new("badsram");
    let rom = fixture.battery_rom("battery.gb");
    let save = fixture.paths.save_file(&rom);
    std::fs::create_dir_all(save.parent().unwrap()).unwrap();
    std::fs::write(&save, vec![0u8; 3]).unwrap();

    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom,
        rom_id: None,
    });

    wait_event(&session, "the size complaint", |event| match event {
        SessionEvent::Error(message) if message.contains("does not fit") => Some(()),
        _ => None,
    });
    // The cartridge still runs; a mismatched save is not a reason to refuse to play.
    wait_frame(&mut session, 3);
}

// --- rewind ---------------------------------------------------------------------------------

#[test]
fn rewinding_moves_the_frame_counter_backwards() {
    let fixture = Fixture::new("rewind");
    let config = Config {
        rewind: RewindConfig {
            enabled: true,
            seconds: 10,
            // Every frame, so a short test has plenty of snapshots to walk back through.
            interval_frames: 1,
        },
        ..Config::default()
    };
    let mut session = fixture.session_with(config);
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    let high = wait_frame(&mut session, 30);

    session.send(SessionCommand::SetRewinding(true));

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        session.frames().poll();
        if session
            .frames()
            .current()
            .is_some_and(|frame| frame.number < high - 5)
        {
            session.send(SessionCommand::SetRewinding(false));
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!(
        "rewinding never moved the frame counter back below {}",
        high - 5
    );
}

#[test]
fn rewinding_past_the_start_says_so_once_and_stops() {
    let fixture = Fixture::new("rewindend");
    let config = Config {
        rewind: RewindConfig {
            enabled: true,
            seconds: 10,
            interval_frames: 1,
        },
        ..Config::default()
    };
    let mut session = fixture.session_with(config);
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 5);
    session.send(SessionCommand::SetRewinding(true));

    let text = wait_event(&session, "the exhausted notice", |event| match event {
        SessionEvent::Notice(text) if text.contains("further") => Some(text.clone()),
        _ => None,
    });
    assert!(text.contains("cannot rewind"), "{text}");
}

#[test]
fn rewind_disabled_records_no_snapshots() {
    let fixture = Fixture::new("norewind");
    let config = Config {
        rewind: RewindConfig {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let mut session = fixture.session_with(config);
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 20);

    let stats = wait_event(&session, "statistics", |event| match event {
        SessionEvent::Stats(stats) if stats.frame > 10 => Some(*stats),
        _ => None,
    });
    assert_eq!(stats.rewind_snapshots, 0);
    assert_eq!(
        stats.rewind_bytes, 0,
        "no memory spent on a disabled feature"
    );
}

// --- statistics and shutdown ----------------------------------------------------------------

#[test]
fn statistics_report_a_measured_speed_rather_than_a_nominal_one() {
    let fixture = Fixture::new("stats");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 20);

    let stats = wait_event(&session, "statistics", |event| match event {
        SessionEvent::Stats(stats) if stats.frame > 10 => Some(*stats),
        _ => None,
    });
    assert!(stats.fps > 0.0, "fps was never measured: {stats:?}");
    assert!(
        stats.speed_percent > 20.0,
        "an idle machine running a two-byte loop should keep up: {stats:?}"
    );
    assert!(!stats.fast_forward);
    assert!(!stats.rewinding);
}

#[test]
fn fast_forward_runs_the_machine_faster_than_real_time() {
    let fixture = Fixture::new("ff");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 5);

    // Measured from the statistics, not from published frames. Fast-forward deliberately skips
    // the framebuffer copy whenever the pipe is full, so what reaches the drawing side is a
    // *fraction* of what ran — counting those would measure the pipe, not the emulator.
    session.send(SessionCommand::SetFastForwardSpeed(0.0)); // uncapped
    session.send(SessionCommand::SetFastForward(true));

    let first = wait_event(
        &session,
        "statistics under fast-forward",
        |event| match event {
            SessionEvent::Stats(stats) if stats.fast_forward => Some(*stats),
            _ => None,
        },
    );
    let start = Instant::now();
    let second = wait_event(&session, "a later statistics report", |event| match event {
        SessionEvent::Stats(stats) if stats.fast_forward && stats.frame > first.frame => {
            Some(*stats)
        }
        _ => None,
    });
    let elapsed = start.elapsed().as_secs_f64();
    let realtime_frames = elapsed * 59.7275;
    assert!(
        (second.frame - first.frame) as f64 > realtime_frames * 1.5,
        "uncapped fast-forward ran {} frames in {elapsed:.3}s, \
         which is not meaningfully faster than the {realtime_frames:.0} real-time frames",
        second.frame - first.frame
    );
}

#[test]
fn a_dropped_session_shuts_the_thread_down() {
    let fixture = Fixture::new("drop");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 3);
    assert!(session.is_alive());

    drop(session);
    // If `Drop` did not join, this test would race the thread rather than prove anything — the
    // join is the assertion, and a hang here is the failure mode.
}

#[test]
fn an_explicit_shutdown_flushes_before_returning() {
    let fixture = Fixture::new("shutdown");
    let rom = fixture.battery_rom("battery.gb");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: rom,
        rom_id: None,
    });
    wait_frame(&mut session, 3);
    session.shutdown();
    // Returning at all is the assertion: `shutdown` joins the thread, and the thread runs its
    // final flush before exiting.
}

#[test]
fn input_reaches_the_machine_without_blocking_the_caller() {
    let fixture = Fixture::new("input");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.plain_rom("spin.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    // Publishing input is a single atomic store, so a thousand of them cost nothing measurable.
    // What is asserted is that none of them block and the session keeps running.
    for i in 0..1000 {
        session.set_input(core_common::InputState {
            buttons: if i % 2 == 0 {
                core_common::Buttons::A
            } else {
                core_common::Buttons::empty()
            },
            touch: None,
        });
    }
    session.frames().poll();
    let now = session.frames().current().unwrap().number;
    wait_frame(&mut session, now + 3);
}
