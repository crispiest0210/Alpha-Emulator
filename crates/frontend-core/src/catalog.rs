//! Library operations that need to know what a ROM *is*.
//!
//! The `library` crate deliberately does not parse cartridges — it stores a title, it does not
//! discover one. This module is the other half: it reads the header through `cart-common`, then
//! hands the result to the index. That is why importing lives here and not there.
//!
//! # Threading
//!
//! Everything here takes `&mut Library` and therefore runs on whichever thread owns the
//! connection — the frontend's, never the emulation thread's. A `rusqlite::Connection` behind a
//! mutex shared with a thread that has a 16.7 ms deadline is a stall waiting to happen, so the
//! emulation thread reports *facts* (a state file was written, at this path, at this frame) and
//! this module turns them into rows.
//!
//! Importing a folder reads every file in it, which for a large collection is slow enough to
//! block a UI frame. Callers are expected to run [`import_folder`] on a worker thread; it takes
//! `&mut Library` rather than a handle precisely so that ownership question cannot be dodged.

use crate::platform;
use crate::session::SavedState;
use library::{Library, LibraryError, NewRom, Platform, Reconciliation, RomEntry, RomId, SaveId};
use std::path::Path;

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Library(#[from] LibraryError),

    #[error(transparent)]
    Load(#[from] platform::LoadError),
}

/// Index one ROM file, reading its header for a title.
///
/// Re-importing a file already in the library returns its existing row and refreshes the title
/// **only if** it is still the placeholder derived from the file name. A title the user has edited
/// survives, which is the whole reason the index is authoritative.
pub fn import_rom(library: &mut Library, path: &Path) -> Result<RomId, CatalogError> {
    let info = platform::probe(path)?;
    let existing = library.rom_id_for_path(path)?;
    let id = library.add_rom(&NewRom {
        path: path.to_path_buf(),
        title: info.title.clone(),
        platform: info.platform,
        size_bytes: info.size_bytes,
        content_hash: info.content_hash,
    })?;
    if existing.is_none() {
        library.set_title(id, &info.title)?;
    }
    Ok(id)
}

/// Index every recognised ROM in a folder, and watch the folder from then on.
///
/// Returns the rows added and one message per file that could not be read. A single unreadable
/// file must not abandon the rest of an import — a collection with one bad dump in it is
/// completely ordinary.
pub fn import_folder(
    library: &mut Library,
    dir: &Path,
    recursive: bool,
) -> Result<(Vec<RomId>, Vec<String>), CatalogError> {
    library.add_folder(dir, recursive)?;
    let mut ids = Vec::new();
    let mut problems = Vec::new();
    for path in recognised_files(dir, recursive) {
        match import_rom(library, &path) {
            Ok(id) => ids.push(id),
            Err(e) => problems.push(format!("{}: {e}", path.display())),
        }
    }
    Ok((ids, problems))
}

/// Import whatever was dropped on the window: files, folders, or a mix.
///
/// A drag-and-drop of a folder is the common way to import a collection, and treating it as an
/// unreadable file would be the wrong answer to the most obvious gesture the UI offers.
pub fn import_dropped(
    library: &mut Library,
    paths: &[std::path::PathBuf],
) -> (Vec<RomId>, Vec<String>) {
    let mut ids = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        let result = if path.is_dir() {
            import_folder(library, path, true).map(|(mut new, mut errs)| {
                ids.append(&mut new);
                problems.append(&mut errs);
            })
        } else {
            import_rom(library, path).map(|id| ids.push(id))
        };
        if let Err(e) = result {
            problems.push(format!("{}: {e}", path.display()));
        }
    }
    (ids, problems)
}

/// Record a save state the emulation thread has just written.
///
/// Does nothing and reports `None` when the state belongs to a ROM that is not in the library —
/// which happens when a file was dragged in and played without being imported. The state file is
/// still on disk and still loadable by path; it simply has no row to appear in a list, and
/// inventing a ROM row for it would put a phantom entry in the browser.
pub fn record_saved_state(
    library: &mut Library,
    saved: &SavedState,
) -> Result<Option<SaveId>, CatalogError> {
    let Some(rom_id) = saved.rom_id else {
        return Ok(None);
    };
    let id = library.record_state(
        rom_id,
        &saved.path,
        &saved.label,
        saved.slot,
        saved.frame,
        saved.size_bytes,
    )?;
    Ok(Some(id))
}

/// The startup pass: bring the index into agreement with the filesystem.
///
/// Called once before the library browser is first shown, so what the user sees reflects what is
/// actually on disk. This is the reconciliation-not-rescan behaviour prompt 14 asks for; see
/// [`library::index`] for why the distinction matters.
pub fn reconcile(library: &mut Library) -> Result<Reconciliation, CatalogError> {
    let report = library.reconcile()?;
    if report.changed_anything() {
        tracing::info!(
            "library reconciled: {} added, {} moved, {} missing, {} states dropped, {} adopted",
            report.added,
            report.moved,
            report.missing,
            report.states_dropped,
            report.states_adopted
        );
    }
    Ok(report)
}

/// The ROMs to show, optionally filtered by a search string and a platform.
///
/// Filtering here rather than in SQL keeps the query one statement and the matching rule one
/// obvious function; a library of a few thousand entries is nothing to scan in memory, and the
/// moment it is, this becomes a `WHERE` clause without changing the caller.
pub fn browse(
    library: &Library,
    query: &str,
    platform: Option<Platform>,
) -> Result<Vec<RomEntry>, CatalogError> {
    let needle = query.trim().to_lowercase();
    let mut roms = library.roms()?;
    roms.retain(|rom| {
        let platform_matches = platform.is_none_or(|p| rom.platform == p);
        let text_matches = needle.is_empty()
            || rom.title.to_lowercase().contains(&needle)
            || rom.path.to_string_lossy().to_lowercase().contains(&needle);
        platform_matches && text_matches
    });
    Ok(roms)
}

/// Files under `dir` with an extension one of the platforms claims.
fn recognised_files(dir: &Path, recursive: bool) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if recursive {
                    stack.push(path);
                }
            } else if kind.is_file() && Platform::from_extension(&path).is_some() {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use library::AppPaths;

    /// A GB cartridge with a real header, built here. No commercial ROM is used anywhere in this
    /// workspace, and a header is all the importer reads.
    fn gb_rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        let bytes = title.as_bytes();
        let len = bytes.len().min(11);
        rom[0x0134..0x0134 + len].copy_from_slice(&bytes[..len]);
        rom[0x0147] = 0x00;
        rom[0x0148] = 0x00;
        rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
        rom
    }

    /// A GBA cartridge with a parseable header. All that is read is the first 0xC0 bytes.
    fn gba_rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        let bytes = title.as_bytes();
        let len = bytes.len().min(12);
        rom[0xA0..0xA0 + len].copy_from_slice(&bytes[..len]);
        rom
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("alpha-catalog-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_import_prefers_the_header_title_over_the_file_name() {
        let scratch = Scratch::new("title");
        let path = scratch.0.join("cryptic_filename_01.gb");
        std::fs::write(&path, gb_rom("REALTITLE")).unwrap();

        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let id = import_rom(&mut library, &path).unwrap();

        assert_eq!(library.rom(id).unwrap().unwrap().title, "REALTITLE");
    }

    #[test]
    fn a_header_with_no_usable_title_falls_back_to_the_file_name() {
        let scratch = Scratch::new("notitle");
        let path = scratch.0.join("Fallback Name.gb");
        std::fs::write(&path, gb_rom("")).unwrap();

        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let id = import_rom(&mut library, &path).unwrap();

        assert_eq!(library.rom(id).unwrap().unwrap().title, "Fallback Name");
    }

    #[test]
    fn a_user_edited_title_survives_a_reimport() {
        let scratch = Scratch::new("edited");
        let path = scratch.0.join("game.gb");
        std::fs::write(&path, gb_rom("HEADER")).unwrap();

        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let id = import_rom(&mut library, &path).unwrap();
        library.set_title(id, "My Preferred Name").unwrap();

        let again = import_rom(&mut library, &path).unwrap();
        assert_eq!(again, id);
        assert_eq!(
            library.rom(id).unwrap().unwrap().title,
            "My Preferred Name",
            "an import must not overwrite what the user typed"
        );
    }

    #[test]
    fn a_folder_import_reports_the_files_it_could_not_read_and_keeps_the_rest() {
        let scratch = Scratch::new("folder");
        std::fs::write(scratch.0.join("good.gb"), gb_rom("GOOD")).unwrap();
        // Too small for any header: a truncated dump, which is a real thing to find in a folder.
        std::fs::write(scratch.0.join("truncated.gb"), [0u8; 16]).unwrap();
        std::fs::write(scratch.0.join("notes.txt"), b"ignored").unwrap();

        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let (ids, problems) = import_folder(&mut library, &scratch.0, false).unwrap();

        assert_eq!(ids.len(), 1, "the good ROM was still imported");
        assert_eq!(problems.len(), 1, "and the bad one was reported");
        assert!(problems[0].contains("truncated.gb"), "{:?}", problems);
    }

    #[test]
    fn browse_filters_by_text_and_platform() {
        let scratch = Scratch::new("browse");
        std::fs::write(scratch.0.join("Aardvark.gb"), gb_rom("AARDVARK")).unwrap();
        std::fs::write(scratch.0.join("Zebra.gba"), gba_rom("ZEBRA")).unwrap();

        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        import_folder(&mut library, &scratch.0, false).unwrap();

        assert_eq!(browse(&library, "", None).unwrap().len(), 2);
        assert_eq!(browse(&library, "aardv", None).unwrap().len(), 1);
        assert_eq!(browse(&library, "", Some(Platform::Gba)).unwrap().len(), 1);
        assert_eq!(
            browse(&library, "aardv", Some(Platform::Gba))
                .unwrap()
                .len(),
            0,
            "the two filters combine rather than either one winning"
        );
    }

    #[test]
    fn a_state_for_an_unimported_rom_is_not_forced_into_the_index() {
        let scratch = Scratch::new("orphanstate");
        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let saved = SavedState {
            rom_id: None,
            path: scratch.0.join("s.ast"),
            label: "slot0".into(),
            slot: Some(0),
            frame: 10,
            size_bytes: 4,
        };
        assert_eq!(record_saved_state(&mut library, &saved).unwrap(), None);
    }

    #[test]
    fn a_state_for_an_imported_rom_becomes_a_row() {
        let scratch = Scratch::new("state");
        let rom_path = scratch.0.join("g.gb");
        std::fs::write(&rom_path, gb_rom("G")).unwrap();
        let mut library = Library::open_in_memory(AppPaths::rooted_at(&scratch.0)).unwrap();
        let rom_id = import_rom(&mut library, &rom_path).unwrap();

        let state_path = scratch.0.join("slot0.ast");
        std::fs::write(&state_path, [0u8; 32]).unwrap();
        let saved = SavedState {
            rom_id: Some(rom_id),
            path: state_path,
            label: "slot0".into(),
            slot: Some(0),
            frame: 900,
            size_bytes: 32,
        };

        let id = record_saved_state(&mut library, &saved).unwrap().unwrap();
        assert_eq!(library.state(id).unwrap().unwrap().frame, 900);
        assert_eq!(library.states_for(rom_id).unwrap().len(), 1);
    }
}
