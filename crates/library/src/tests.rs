//! Reconciliation is the behaviour worth testing, so these tests use real files.
//!
//! A mocked filesystem would pass while the real one failed, because the thing being verified
//! *is* the interaction between recorded rows and what `std::fs` reports. The index itself is
//! in memory — a real SQLite engine, so constraints and cascades are genuinely enforced — while
//! the ROM and state files are real files in a scratch directory.

use crate::{AppPaths, Library, NewRom, Platform};
use std::path::{Path, PathBuf};

/// A scratch directory that removes itself.
///
/// Hand-rolled rather than pulling in `tempfile`: this needs one directory with a unique name,
/// and a dependency added to the workspace is a dependency every consumer of the workspace
/// resolves forever.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // The counter distinguishes directories within one test binary; the process id
        // distinguishes concurrent `cargo test` invocations, which do share a temp directory.
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("alpha-library-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write a file that looks enough like a ROM for the index: a recognised extension and some
/// bytes to hash. The index never parses one, which is what makes this legitimate rather than a
/// shortcut — and it is also why no commercial ROM is needed to test the library.
fn write_rom(dir: &Path, name: &str, filler: u8, len: usize) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&path, vec![filler; len]).unwrap();
    path
}

fn library(scratch: &Scratch) -> Library {
    Library::open_in_memory(AppPaths::rooted_at(scratch.path().join("app")))
        .expect("open in-memory library")
}

#[test]
fn importing_a_file_records_platform_size_and_hash() {
    let scratch = Scratch::new("import");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Kirby.gbc", 0x42, 1024);

    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let entry = lib.rom(id).unwrap().unwrap();

    assert_eq!(entry.platform, Platform::Gbc);
    assert_eq!(entry.size_bytes, 1024);
    assert_eq!(entry.content_hash, crate::content_hash(&[0x42; 1024]));
    assert_eq!(entry.title, "Kirby");
    assert!(entry.present);
    assert_eq!(entry.play_count, 0);
    assert_eq!(entry.last_played_at, None);
}

#[test]
fn importing_the_same_file_twice_is_one_row_and_keeps_user_data() {
    let scratch = Scratch::new("reimport");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Tetris.gb", 1, 512);

    let mut lib = library(&scratch);
    let first = lib.import_file(&rom).unwrap();
    lib.set_title(first, "Tetris (World)").unwrap();
    lib.mark_played(first).unwrap();

    let second = lib.import_file(&rom).unwrap();
    assert_eq!(first, second, "re-import must not create a second row");
    assert_eq!(lib.roms().unwrap().len(), 1);

    let entry = lib.rom(first).unwrap().unwrap();
    assert_eq!(entry.title, "Tetris (World)", "a re-import kept the title");
    assert_eq!(entry.play_count, 1, "a re-import kept the play count");
}

#[test]
fn a_non_rom_extension_is_refused_by_name() {
    let scratch = Scratch::new("notarom");
    let path = write_rom(scratch.path(), "notes.txt", 0, 8);
    let mut lib = library(&scratch);
    let err = lib.import_file(&path).unwrap_err();
    assert!(
        matches!(err, crate::LibraryError::UnknownPlatform(_)),
        "got {err:?}"
    );
}

#[test]
fn a_directory_import_finds_roms_recursively_and_ignores_other_files() {
    let scratch = Scratch::new("dirimport");
    let games = scratch.path().join("games");
    write_rom(&games, "a.gb", 1, 64);
    write_rom(&games.join("gba"), "b.gba", 2, 64);
    write_rom(&games, "readme.txt", 3, 64);

    let mut lib = library(&scratch);
    let ids = lib.import_dir(&games, true).unwrap();
    assert_eq!(ids.len(), 2, "two ROMs and one text file");

    let platforms: Vec<_> = lib.roms().unwrap().iter().map(|r| r.platform).collect();
    assert!(platforms.contains(&Platform::Gb));
    assert!(platforms.contains(&Platform::Gba));
}

#[test]
fn a_non_recursive_import_stops_at_the_top_level() {
    let scratch = Scratch::new("nonrecursive");
    let games = scratch.path().join("games");
    write_rom(&games, "top.gb", 1, 64);
    write_rom(&games.join("sub"), "deep.gb", 2, 64);

    let mut lib = library(&scratch);
    let ids = lib.import_dir(&games, false).unwrap();
    assert_eq!(ids.len(), 1);
}

// --- reconciliation -----------------------------------------------------------------------

#[test]
fn a_file_added_to_a_watched_folder_is_picked_up_without_a_second_import() {
    let scratch = Scratch::new("added");
    let games = scratch.path().join("games");
    write_rom(&games, "first.gb", 1, 64);

    let mut lib = library(&scratch);
    lib.import_dir(&games, true).unwrap();

    write_rom(&games, "second.gba", 2, 64);
    let report = lib.reconcile().unwrap();

    assert_eq!(report.added, 1);
    assert_eq!(report.moved, 0);
    assert_eq!(report.missing, 0);
    assert_eq!(lib.roms().unwrap().len(), 2);
}

#[test]
fn a_deleted_file_is_marked_missing_and_keeps_its_row() {
    let scratch = Scratch::new("missing");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "gone.gb", 1, 64);

    let mut lib = library(&scratch);
    let id = lib.import_dir(&games, true).unwrap()[0];
    lib.mark_played(id).unwrap();

    std::fs::remove_file(&rom).unwrap();
    let report = lib.reconcile().unwrap();

    assert_eq!(report.missing, 1);
    let entry = lib.rom(id).unwrap().expect("the row survives the file");
    assert!(!entry.present);
    assert_eq!(
        entry.play_count, 1,
        "an unmounted drive must not cost the user their history"
    );
}

#[test]
fn a_restored_file_becomes_present_again() {
    let scratch = Scratch::new("restored");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "back.gb", 7, 64);

    let mut lib = library(&scratch);
    let id = lib.import_dir(&games, true).unwrap()[0];

    std::fs::remove_file(&rom).unwrap();
    assert_eq!(lib.reconcile().unwrap().missing, 1);

    write_rom(&games, "back.gb", 7, 64);
    let report = lib.reconcile().unwrap();
    assert_eq!(report.restored, 1);
    assert_eq!(report.added, 0, "a restored file is not a new one");
    assert!(lib.rom(id).unwrap().unwrap().present);
}

/// The predecessor's §5 failure, reproduced as a test: a ROM moved outside the application.
///
/// A rebuild-from-scan frontend answers this with a new entry and a lost history. The index must
/// answer it with the *same* row at a new path.
#[test]
fn a_moved_file_updates_its_row_rather_than_becoming_a_new_one() {
    let scratch = Scratch::new("moved");
    let games = scratch.path().join("games");
    let archive = scratch.path().join("games").join("archive");
    let rom = write_rom(&games, "Metroid.gba", 0x5A, 4096);

    let mut lib = library(&scratch);
    let id = lib.import_dir(&games, true).unwrap()[0];
    lib.set_title(id, "Metroid Fusion").unwrap();
    lib.mark_played(id).unwrap();

    std::fs::create_dir_all(&archive).unwrap();
    let moved_to = archive.join("Metroid.gba");
    std::fs::rename(&rom, &moved_to).unwrap();

    let report = lib.reconcile().unwrap();

    assert_eq!(report.moved, 1, "recognised by content hash");
    assert_eq!(report.added, 0, "not indexed a second time");
    assert_eq!(report.missing, 0, "the original row is not left dangling");
    assert_eq!(lib.roms().unwrap().len(), 1);

    let entry = lib.rom(id).unwrap().unwrap();
    // Indexed paths are canonical, which on macOS means `/private/var/...` rather than the
    // `/var/...` symlink the test constructed.
    assert_eq!(entry.path, std::fs::canonicalize(&moved_to).unwrap());
    assert_eq!(entry.title, "Metroid Fusion");
    assert_eq!(entry.play_count, 1);
    assert!(entry.present);
}

#[test]
fn two_files_of_equal_size_but_different_contents_are_not_confused() {
    let scratch = Scratch::new("samesize");
    let games = scratch.path().join("games");
    write_rom(&games, "one.gb", 1, 256);

    let mut lib = library(&scratch);
    lib.import_dir(&games, true).unwrap();
    std::fs::remove_file(games.join("one.gb")).unwrap();

    // Same length, different bytes: the size pre-filter matches and the hash must not.
    write_rom(&games, "two.gb", 2, 256);
    let report = lib.reconcile().unwrap();

    assert_eq!(report.moved, 0);
    assert_eq!(report.added, 1);
    assert_eq!(report.missing, 1);
}

#[test]
fn reconciliation_reports_no_change_when_nothing_changed() {
    let scratch = Scratch::new("stable");
    let games = scratch.path().join("games");
    write_rom(&games, "a.gb", 1, 64);

    let mut lib = library(&scratch);
    lib.import_dir(&games, true).unwrap();
    lib.reconcile().unwrap();

    let report = lib.reconcile().unwrap();
    assert!(
        !report.changed_anything(),
        "a second pass over an unchanged tree must be a no-op, got {report:?}"
    );
}

// --- save states --------------------------------------------------------------------------

#[test]
fn a_slot_written_twice_updates_one_row() {
    let scratch = Scratch::new("slot");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Zelda.gbc", 3, 64);

    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("app/data/states/Zelda/slot1.ast");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, [0u8; 16]).unwrap();

    let first = lib
        .record_state(id, &state_path, "slot1", Some(1), 100, 16)
        .unwrap();
    let second = lib
        .record_state(id, &state_path, "slot1", Some(1), 900, 16)
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(lib.states_for(id).unwrap().len(), 1);
    assert_eq!(lib.state(first).unwrap().unwrap().frame, 900);
}

#[test]
fn deleting_a_state_removes_the_row_and_the_file() {
    let scratch = Scratch::new("delstate");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Pokemon.gb", 4, 64);

    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("state.ast");
    std::fs::write(&state_path, [1u8; 8]).unwrap();
    let save = lib
        .record_state(id, &state_path, "before gym", None, 42, 8)
        .unwrap();

    lib.delete_state(save).unwrap();

    assert!(lib.state(save).unwrap().is_none(), "row removed");
    assert!(!state_path.exists(), "file removed");
}

#[test]
fn deleting_a_state_whose_file_is_already_gone_still_clears_the_row() {
    let scratch = Scratch::new("delgone");
    let rom = write_rom(&scratch.path().join("games"), "x.gb", 5, 64);
    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("vanished.ast");
    std::fs::write(&state_path, [1u8; 8]).unwrap();
    let save = lib.record_state(id, &state_path, "s", None, 1, 8).unwrap();

    std::fs::remove_file(&state_path).unwrap();
    lib.delete_state(save)
        .expect("already-gone is not an error");
    assert!(lib.state(save).unwrap().is_none());
}

#[test]
fn removing_a_rom_cascades_to_its_state_rows() {
    let scratch = Scratch::new("cascade");
    let rom = write_rom(&scratch.path().join("games"), "y.gba", 6, 64);
    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("s.ast");
    std::fs::write(&state_path, [1u8; 8]).unwrap();
    let save = lib.record_state(id, &state_path, "s", None, 1, 8).unwrap();

    lib.remove_rom(id, false).unwrap();

    assert!(lib.rom(id).unwrap().is_none());
    assert!(
        lib.state(save).unwrap().is_none(),
        "the cascade needs PRAGMA foreign_keys, which is easy to forget"
    );
    assert!(
        state_path.exists(),
        "removing a ROM without asking for file deletion leaves the files alone"
    );
}

#[test]
fn removing_a_rom_with_file_deletion_erases_the_states_but_never_the_rom_file() {
    let scratch = Scratch::new("cascadefiles");
    let rom = write_rom(&scratch.path().join("games"), "z.gba", 6, 64);
    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("s2.ast");
    std::fs::write(&state_path, [1u8; 8]).unwrap();
    lib.record_state(id, &state_path, "s", None, 1, 8).unwrap();

    lib.remove_rom(id, true).unwrap();

    assert!(!state_path.exists(), "state file deleted");
    assert!(
        rom.exists(),
        "the user's cartridge dump is never application data"
    );
}

#[test]
fn a_state_file_removed_outside_the_app_is_dropped_from_the_index() {
    let scratch = Scratch::new("statesync");
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "w.gb", 8, 64);
    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();
    let state_path = scratch.path().join("outside.ast");
    std::fs::write(&state_path, [1u8; 8]).unwrap();
    lib.record_state(id, &state_path, "s", None, 1, 8).unwrap();

    std::fs::remove_file(&state_path).unwrap();
    let report = lib.reconcile().unwrap();

    assert_eq!(report.states_dropped, 1);
    assert!(lib.states_for(id).unwrap().is_empty());
}

#[test]
fn a_state_file_copied_in_is_adopted_with_its_slot_recognised() {
    let scratch = Scratch::new("adopt");
    let app = AppPaths::rooted_at(scratch.path().join("app"));
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Adopted.gb", 9, 64);

    let mut lib = Library::open_in_memory(app.clone()).unwrap();
    let id = lib.import_file(&rom).unwrap();
    let indexed_path = lib.rom(id).unwrap().unwrap().path;

    let dir = app.states_dir_for(&indexed_path);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("slot4.ast"), [2u8; 32]).unwrap();
    std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();

    let report = lib.reconcile().unwrap();

    assert_eq!(report.states_adopted, 1, "the .txt is not a save state");
    let states = lib.states_for(id).unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].slot, Some(4));
    assert_eq!(states[0].size_bytes, 32);
}

#[test]
fn states_list_slots_first_then_named_states() {
    let scratch = Scratch::new("order");
    let rom = write_rom(&scratch.path().join("games"), "o.gb", 1, 64);
    let mut lib = library(&scratch);
    let id = lib.import_file(&rom).unwrap();

    for (name, slot) in [("named", None), ("slot2", Some(2)), ("slot0", Some(0))] {
        let path = scratch.path().join(format!("{name}.ast"));
        std::fs::write(&path, [0u8; 4]).unwrap();
        lib.record_state(id, &path, name, slot, 0, 4).unwrap();
    }

    let labels: Vec<_> = lib
        .states_for(id)
        .unwrap()
        .into_iter()
        .map(|s| s.label)
        .collect();
    assert_eq!(labels, vec!["slot0", "slot2", "named"]);
}

#[test]
fn the_rom_list_puts_recently_played_first() {
    let scratch = Scratch::new("ordering");
    let games = scratch.path().join("games");
    write_rom(&games, "Aardvark.gb", 1, 64);
    write_rom(&games, "Zebra.gb", 2, 64);

    let mut lib = library(&scratch);
    lib.import_dir(&games, true).unwrap();
    let zebra = lib
        .roms()
        .unwrap()
        .into_iter()
        .find(|r| r.title == "Zebra")
        .unwrap()
        .id;
    lib.mark_played(zebra).unwrap();

    let titles: Vec<_> = lib.roms().unwrap().into_iter().map(|r| r.title).collect();
    assert_eq!(titles, vec!["Zebra", "Aardvark"]);
}

/// The acceptance criterion for the library half: a restart must not need a re-import.
///
/// Uses a file-backed database, because that is the thing under test — an in-memory one would
/// pass this trivially by never closing.
#[test]
fn a_file_backed_index_survives_being_closed_and_reopened() {
    let scratch = Scratch::new("persist");
    let app = AppPaths::rooted_at(scratch.path().join("app"));
    let games = scratch.path().join("games");
    let rom = write_rom(&games, "Persist.gba", 0x11, 2048);

    let id = {
        let mut lib = Library::open(app.clone()).unwrap();
        let id = lib.import_file(&rom).unwrap();
        lib.set_title(id, "Persisted Title").unwrap();
        lib.mark_played(id).unwrap();
        let state_path = app.state_slot_file(&lib.rom(id).unwrap().unwrap().path, 0);
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&state_path, [7u8; 64]).unwrap();
        lib.record_state(id, &state_path, "slot0", Some(0), 1234, 64)
            .unwrap();
        id
    };

    let lib = Library::open(app).unwrap();
    let entry = lib.rom(id).unwrap().expect("the ROM survived the restart");
    assert_eq!(entry.title, "Persisted Title");
    assert_eq!(entry.play_count, 1);
    assert!(entry.last_played_at.is_some());

    let states = lib.states_for(id).unwrap();
    assert_eq!(states.len(), 1, "the save-state list survived too");
    assert_eq!(states[0].frame, 1234);
}

#[test]
fn add_rom_accepts_a_caller_supplied_title_and_hash() {
    let scratch = Scratch::new("addrom");
    let mut lib = library(&scratch);
    let id = lib
        .add_rom(&NewRom {
            path: PathBuf::from("/nowhere/Header Title.gba"),
            title: "Header Title".into(),
            platform: Platform::Gba,
            size_bytes: 1,
            content_hash: 0xDEAD_BEEF,
        })
        .unwrap();
    let entry = lib.rom(id).unwrap().unwrap();
    assert_eq!(entry.title, "Header Title");
    assert_eq!(entry.content_hash, 0xDEAD_BEEF);
}
