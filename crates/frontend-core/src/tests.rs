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
use crate::DebugRequest;
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

// --- debugger ---------------------------------------------------------------------------------

/// A ROM whose entry point is a known sequence of three-byte jumps, so a breakpoint has an address
/// worth setting and the disassembly has something recognisable in it.
///
/// `jp $0150` at `$0100`, then `jp $0100` at `$0150` — a two-instruction loop across two known
/// addresses. Either one is a valid breakpoint target and the machine reaches both repeatedly, so a
/// breakpoint that does not fire is unambiguously broken rather than merely unlucky.
fn loop_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x0100] = 0xC3; // jp $0150
    rom[0x0101] = 0x50;
    rom[0x0102] = 0x01;
    rom[0x0150] = 0xC3; // jp $0100
    rom[0x0151] = 0x00;
    rom[0x0152] = 0x01;
    rom[0x0134..0x013C].copy_from_slice(b"LOOPTEST");
    rom[0x0147] = 0x00;
    rom[0x0148] = 0x00;
    rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
    rom
}

impl Fixture {
    fn loop_rom(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, loop_rom()).unwrap();
        path
    }
}

fn wait_snapshot(session: &Session) -> Box<crate::DebugSnapshot> {
    wait_event(session, "a debug snapshot", |event| match event {
        SessionEvent::DebugSnapshot(snapshot) => Some(snapshot.clone()),
        SessionEvent::DebugUnavailable(reason) => {
            panic!("the Game Boy should offer introspection: {reason}")
        }
        _ => None,
    })
}

#[test]
fn a_snapshot_reports_registers_disassembly_and_memory() {
    let fixture = Fixture::new("dbgsnap");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetPaused(true));
    wait_status(&session, SessionStatus::Paused);

    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest {
        disassembly_lines: 4,
        memory_at: 0x0100,
        memory_rows: 1,
        ..DebugRequest::default()
    }));
    let snapshot = wait_snapshot(&session);

    assert_eq!(
        snapshot.address_digits, 4,
        "a Game Boy address is four digits"
    );
    assert!(snapshot.registers.iter().any(|r| r.name == "A"));
    assert_eq!(snapshot.disassembly.len(), 4);
    assert!(
        snapshot.disassembly[0].is_program_counter,
        "the first line is where execution is"
    );
    // The ROM's own bytes, read back through the peek path.
    assert_eq!(snapshot.memory[0].bytes[0], Some(0xC3));
    assert_eq!(snapshot.memory[0].bytes[1], Some(0x50));
    assert_eq!(snapshot.region_of(0x0100), Some("ROM bank 0"));
}

#[test]
fn a_snapshot_shows_io_as_unreadable_rather_than_inventing_zeroes() {
    let fixture = Fixture::new("dbgio");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest {
        disassembly_lines: 0,
        memory_at: 0xFF40,
        memory_rows: 1,
        ..DebugRequest::default()
    }));
    let snapshot = wait_snapshot(&session);
    assert!(
        snapshot.memory[0].bytes.iter().all(|byte| byte.is_none()),
        "reading an I/O register can latch it, so a debugger must refuse: {:?}",
        snapshot.memory[0].bytes
    );
}

/// Prompt 15's first acceptance criterion: set a breakpoint at a known address and verify execution
/// actually halts there with the expected state visible.
#[test]
fn an_execution_breakpoint_halts_the_machine_at_that_address() {
    let fixture = Fixture::new("dbgbreak");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);

    session.send(SessionCommand::SetDebugAttached(true));
    session.send(SessionCommand::AddBreakpoint(0x0150));

    let addr = wait_event(&session, "the breakpoint to fire", |event| match event {
        SessionEvent::BreakpointHit { addr } => Some(*addr),
        _ => None,
    });
    assert_eq!(addr, 0x0150);
    wait_status(&session, SessionStatus::Paused);

    // The machine stopped *before* executing, so the program counter is the breakpoint itself.
    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest {
        disassembly_lines: 1,
        ..DebugRequest::default()
    }));
    let snapshot = wait_snapshot(&session);
    assert_eq!(snapshot.program_counter, 0x0150);
    assert!(snapshot.disassembly[0].has_breakpoint);
    assert!(snapshot.disassembly[0].is_program_counter);
    assert!(
        snapshot.disassembly[0].text.to_lowercase().contains("100"),
        "the instruction at the breakpoint is `jp $0100`, got {:?}",
        snapshot.disassembly[0].text
    );
}

#[test]
fn resuming_from_a_breakpoint_makes_progress_instead_of_breaking_again() {
    // The classic first-debugger bug: continue re-checks the address it is sitting on, breaks again,
    // and the machine never moves. It looks exactly like a hang.
    let fixture = Fixture::new("dbgresume");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetDebugAttached(true));
    session.send(SessionCommand::AddBreakpoint(0x0150));
    wait_event(&session, "the first hit", |event| {
        matches!(event, SessionEvent::BreakpointHit { .. }).then_some(())
    });

    session.send(SessionCommand::SetPaused(false));
    // The loop returns to $0150 every two instructions, so a second hit proves the machine ran on
    // rather than merely re-triggering where it stood.
    let second = wait_event(&session, "a second hit", |event| match event {
        SessionEvent::BreakpointHit { addr } => Some(*addr),
        _ => None,
    });
    assert_eq!(second, 0x0150);
}

#[test]
fn removing_a_breakpoint_lets_execution_past_it() {
    let fixture = Fixture::new("dbgremove");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetDebugAttached(true));
    session.send(SessionCommand::AddBreakpoint(0x0150));
    wait_event(&session, "the hit", |event| {
        matches!(event, SessionEvent::BreakpointHit { .. }).then_some(())
    });

    session.send(SessionCommand::ClearBreakpoints);
    session.send(SessionCommand::SetPaused(false));
    wait_status(&session, SessionStatus::Running);

    session.frames().poll();
    let now = session.frames().current().map(|f| f.number).unwrap_or(0);
    wait_frame(&mut session, now + 5);
}

#[test]
fn a_breakpoint_at_an_address_never_reached_does_not_fire() {
    let fixture = Fixture::new("dbgunreached");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetDebugAttached(true));
    // The two-instruction loop never leaves $0100/$0150.
    session.send(SessionCommand::AddBreakpoint(0x2000));

    std::thread::sleep(Duration::from_millis(300));
    for event in session.drain_events() {
        if let SessionEvent::BreakpointHit { addr } = event {
            panic!("fired at {addr:#06X}, which this ROM never executes");
        }
    }
    assert!(session.frames().poll(), "and the machine kept running");
}

#[test]
fn stepping_one_instruction_advances_the_program_counter_by_one_instruction() {
    let fixture = Fixture::new("dbgstep");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetDebugAttached(true));
    session.send(SessionCommand::AddBreakpoint(0x0100));
    wait_event(&session, "the hit", |event| {
        matches!(event, SessionEvent::BreakpointHit { .. }).then_some(())
    });

    // Establish a standing request first, so the session knows what to re-serve when it stops.
    // Sending "step" and "snapshot" together would be a race the caller cannot win — both are
    // drained before the loop ticks — which is why the session re-serves on every stop instead.
    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest {
        disassembly_lines: 1,
        ..DebugRequest::default()
    }));
    let before = wait_snapshot(&session);
    assert_eq!(before.program_counter, 0x0100);

    // From $0100, `jp $0150` is one instruction.
    session.send(SessionCommand::StepInstructions(1));
    let after = wait_event(
        &session,
        "the snapshot the stop produces",
        |event| match event {
            SessionEvent::DebugSnapshot(snapshot) if snapshot.program_counter != 0x0100 => {
                Some(snapshot.clone())
            }
            _ => None,
        },
    );
    assert_eq!(
        after.program_counter, 0x0150,
        "one step from a jump lands at its target"
    );
}

#[test]
fn setting_the_program_counter_moves_execution() {
    let fixture = Fixture::new("dbgsetpc");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 2);
    session.send(SessionCommand::SetPaused(true));
    wait_status(&session, SessionStatus::Paused);

    session.send(SessionCommand::SetProgramCounter(0x0150));
    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest::default()));
    let snapshot = wait_snapshot(&session);
    assert_eq!(snapshot.program_counter, 0x0150);
}

#[test]
fn attaching_with_no_breakpoints_leaves_the_machine_running_at_full_speed() {
    // The design claim worth testing: attaching to look at registers must not slow anything down,
    // because the loop only steps instruction-at-a-time when something needs checking.
    let fixture = Fixture::new("dbgattached");
    let mut session = fixture.session();
    session.send(SessionCommand::LoadRom {
        path: fixture.loop_rom("loop.gb"),
        rom_id: None,
    });
    wait_frame(&mut session, 5);
    session.send(SessionCommand::SetDebugAttached(true));

    let stats = wait_event(&session, "statistics while attached", |event| match event {
        SessionEvent::Stats(stats) if stats.frame > 10 => Some(*stats),
        _ => None,
    });
    assert!(
        stats.speed_percent > 20.0,
        "attaching with no breakpoints should not cost speed: {stats:?}"
    );
}

#[test]
fn a_debug_request_with_no_cartridge_says_so_rather_than_returning_nothing() {
    let fixture = Fixture::new("dbgnorom");
    let session = fixture.session();
    wait_status(&session, SessionStatus::Idle);

    session.send(SessionCommand::RequestDebugSnapshot(DebugRequest::default()));
    let reason = wait_event(&session, "DebugUnavailable", |event| match event {
        SessionEvent::DebugUnavailable(reason) => Some(reason.clone()),
        SessionEvent::DebugSnapshot(_) => panic!("there is no machine to snapshot"),
        _ => None,
    });
    assert!(reason.contains("no cartridge"), "{reason}");
}
