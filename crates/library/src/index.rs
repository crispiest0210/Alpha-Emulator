//! The durable index: ROM metadata, save-state metadata, and reconciliation against disk.
//!
//! # Why this is a database and not a directory scan
//!
//! Predecessor lesson §5: the old project rebuilt its notion of "your games" from a directory
//! listing on every launch. Everything that was not in a file name therefore did not exist —
//! last-played time, a corrected title, a save state's label — and a ROM moved to another folder
//! came back as a brand-new entry with all of that lost.
//!
//! Here the index **is** the source of truth and the filesystem scan is a *reconciliation pass
//! against it*, which is a different operation with a different result:
//!
//! - a file that vanished is marked missing, not deleted, so its play count and states survive a
//!   ROM sitting on an unmounted drive;
//! - a file that moved is recognised by content hash and its row is *updated*, so it keeps
//!   everything it had;
//! - a file that is genuinely new is added.
//!
//! Session state — which ROM is running, paused or not — is deliberately **not** here. That the
//! predecessor got right: it is ephemeral, and persisting it only creates the problem of a stale
//! row describing a session that no longer exists.

use crate::{paths::AppPaths, LibraryError, Platform};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Row identifier for a ROM.
pub type RomId = i64;
/// Row identifier for a save state.
pub type SaveId = i64;

type Result<T> = std::result::Result<T, LibraryError>;

/// Current schema version. Bump when the schema changes and add a migration step.
const SCHEMA_VERSION: i32 = 1;

/// One indexed ROM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomEntry {
    pub id: RomId,
    pub path: PathBuf,
    /// Display title. Seeded from the cartridge header when the importer can read one and from
    /// the file stem otherwise, and editable afterwards — which is the whole reason it is a
    /// column instead of being recomputed from the path every time it is shown.
    pub title: String,
    pub platform: Platform,
    pub size_bytes: u64,
    pub content_hash: u64,
    pub added_at: i64,
    pub last_played_at: Option<i64>,
    pub play_count: u32,
    /// Whether the file was found on disk at the last reconciliation.
    pub present: bool,
}

/// What the caller knows about a ROM it is adding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRom {
    pub path: PathBuf,
    pub title: String,
    pub platform: Platform,
    pub size_bytes: u64,
    pub content_hash: u64,
}

/// One indexed save state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveStateEntry {
    pub id: SaveId,
    pub rom_id: RomId,
    pub path: PathBuf,
    pub label: String,
    /// `Some(n)` for a numbered quick-save slot, `None` for a user-labelled state. Slots are
    /// overwritten in place; named states accumulate.
    pub slot: Option<u8>,
    /// Emulated frame the state was taken at. Shown in the UI because "load to exact frame" is
    /// only a meaningful promise if the frame is visible.
    pub frame: u64,
    pub created_at: i64,
    pub size_bytes: u64,
}

/// What one reconciliation pass changed.
///
/// Returned rather than logged so the UI can say "3 added, 1 moved, 2 missing" instead of the
/// user having to guess whether anything happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Files found in a watched folder that were not in the index.
    pub added: usize,
    /// Rows whose file was found at a new path, matched by content hash.
    pub moved: usize,
    /// Rows whose file is not where the index says and was not found elsewhere.
    pub missing: usize,
    /// Rows previously missing whose file is back at the recorded path.
    pub restored: usize,
    /// Save-state rows whose file no longer exists, removed from the index.
    pub states_dropped: usize,
    /// Save-state files found on disk that the index did not know about.
    pub states_adopted: usize,
}

impl Reconciliation {
    pub fn changed_anything(&self) -> bool {
        *self != Self::default()
    }
}

/// The library index.
pub struct Library {
    conn: Connection,
    paths: AppPaths,
}

impl Library {
    /// Open (creating if needed) the index for a given layout.
    pub fn open(paths: AppPaths) -> Result<Self> {
        paths.create_all()?;
        let conn = Connection::open(paths.database())?;
        Self::from_connection(conn, paths)
    }

    /// An index that exists only for the duration of the process.
    ///
    /// Not a testing shortcut bolted on: it is how the reconciliation tests get a real SQLite
    /// engine with real constraint enforcement without a temporary file to clean up.
    pub fn open_in_memory(paths: AppPaths) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, paths)
    }

    fn from_connection(conn: Connection, paths: AppPaths) -> Result<Self> {
        // Foreign keys are off by default in SQLite, which would silently turn the
        // `ON DELETE CASCADE` below into a no-op and orphan every save-state row when a ROM is
        // removed. Enabling it is per-connection, so it happens here and nowhere else.
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL survives a hard kill without corrupting the index, which for a program that will
        // be force-quit mid-frame is the relevant durability property.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let mut library = Self { conn, paths };
        library.migrate()?;
        Ok(library)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        if version == 0 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE roms (
                    id             INTEGER PRIMARY KEY,
                    path           TEXT    NOT NULL UNIQUE,
                    title          TEXT    NOT NULL,
                    platform       TEXT    NOT NULL,
                    size_bytes     INTEGER NOT NULL,
                    content_hash   INTEGER NOT NULL,
                    added_at       INTEGER NOT NULL,
                    last_played_at INTEGER,
                    play_count     INTEGER NOT NULL DEFAULT 0,
                    present        INTEGER NOT NULL DEFAULT 1
                );
                CREATE INDEX roms_by_hash ON roms(content_hash);

                CREATE TABLE save_states (
                    id         INTEGER PRIMARY KEY,
                    rom_id     INTEGER NOT NULL REFERENCES roms(id) ON DELETE CASCADE,
                    path       TEXT    NOT NULL UNIQUE,
                    label      TEXT    NOT NULL,
                    slot       INTEGER,
                    frame      INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    size_bytes INTEGER NOT NULL
                );
                CREATE INDEX states_by_rom ON save_states(rom_id);

                CREATE TABLE folders (
                    path      TEXT    NOT NULL PRIMARY KEY,
                    recursive INTEGER NOT NULL DEFAULT 1
                );
                "#,
            )?;
        }
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    // --- ROMs ------------------------------------------------------------------------------

    /// Insert a ROM, or update the existing row for the same path.
    ///
    /// Re-importing a file already in the library refreshes its size and hash — a ROM can
    /// legitimately be replaced by a patched build at the same path — but deliberately leaves
    /// `play_count`, `last_played_at`, and the title alone. Those are the user's data, not the
    /// file's.
    pub fn add_rom(&mut self, rom: &NewRom) -> Result<RomId> {
        let now = unix_now();
        let path = path_key(&rom.path);
        if let Some(id) = self.rom_id_for_path(&rom.path)? {
            self.conn.execute(
                "UPDATE roms SET size_bytes = ?1, content_hash = ?2, present = 1 WHERE id = ?3",
                params![rom.size_bytes as i64, rom.content_hash as i64, id],
            )?;
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO roms (path, title, platform, size_bytes, content_hash, added_at, present)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                path,
                rom.title,
                rom.platform.id(),
                rom.size_bytes as i64,
                rom.content_hash as i64,
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read a file, work out what it is, and index it.
    ///
    /// The title comes from the file stem. A cartridge header carries a better one, but reading
    /// it needs `cart-common` and this crate deliberately does not depend on it — the importer
    /// in `frontend-core` calls [`set_title`](Self::set_title) once it has parsed the header.
    /// The stem is what shows in the meantime, which is never blank and never wrong-looking.
    pub fn import_file(&mut self, path: &Path) -> Result<RomId> {
        let platform = Platform::from_extension(path)
            .ok_or_else(|| LibraryError::UnknownPlatform(path.to_path_buf()))?;
        let bytes = std::fs::read(path)?;
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string();
        let rom = NewRom {
            path: canonical_or_given(path),
            title,
            platform,
            size_bytes: bytes.len() as u64,
            content_hash: content_hash(&bytes),
        };
        self.add_rom(&rom)
    }

    /// Index every recognised ROM in a directory, and remember the directory for future
    /// reconciliation passes.
    ///
    /// Remembering it is the point: a folder the user imported once should pick up files added
    /// to it later without a second import, and that is what makes reconciliation an ongoing
    /// service rather than a one-shot scan.
    pub fn import_dir(&mut self, dir: &Path, recursive: bool) -> Result<Vec<RomId>> {
        self.add_folder(dir, recursive)?;
        let mut ids = Vec::new();
        for path in scan_dir(dir, recursive) {
            match self.import_file(&path) {
                Ok(id) => ids.push(id),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        Ok(ids)
    }

    pub fn rom_id_for_path(&self, path: &Path) -> Result<Option<RomId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM roms WHERE path = ?1",
                params![path_key(path)],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn rom(&self, id: RomId) -> Result<Option<RomEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, title, platform, size_bytes, content_hash, added_at, \
                 last_played_at, play_count, present FROM roms WHERE id = ?1",
                params![id],
                rom_from_row,
            )
            .optional()?)
    }

    /// Every ROM, most recently played first and never-played entries after them by title.
    ///
    /// That ordering is the one a library browser wants by default: continue what you were
    /// doing, then browse what you have not started.
    pub fn roms(&self) -> Result<Vec<RomEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, platform, size_bytes, content_hash, added_at, \
             last_played_at, play_count, present FROM roms \
             ORDER BY last_played_at DESC NULLS LAST, title COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], rom_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_title(&mut self, id: RomId, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE roms SET title = ?1 WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    /// Record that a ROM was launched.
    pub fn mark_played(&mut self, id: RomId) -> Result<()> {
        self.conn.execute(
            "UPDATE roms SET last_played_at = ?1, play_count = play_count + 1 WHERE id = ?2",
            params![unix_now(), id],
        )?;
        Ok(())
    }

    /// Forget a ROM.
    ///
    /// `delete_state_files` extends the removal to the save states on disk, matching the
    /// predecessor's delete-from-both-UI-and-disk behaviour. The **ROM file itself is never
    /// deleted** — it is the user's copy of a game, not application data, and an emulator that
    /// can silently erase a cartridge dump has one bug away from being a catastrophe.
    pub fn remove_rom(&mut self, id: RomId, delete_state_files: bool) -> Result<()> {
        if delete_state_files {
            for state in self.states_for(id)? {
                let _ = std::fs::remove_file(&state.path);
            }
            if let Some(rom) = self.rom(id)? {
                let _ = std::fs::remove_dir(self.paths.states_dir_for(&rom.path));
            }
        }
        // The `ON DELETE CASCADE` removes the save-state rows, which is why `foreign_keys` is
        // switched on when the connection opens.
        self.conn
            .execute("DELETE FROM roms WHERE id = ?1", params![id])?;
        Ok(())
    }

    // --- watched folders -------------------------------------------------------------------

    pub fn add_folder(&mut self, dir: &Path, recursive: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO folders (path, recursive) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET recursive = ?2",
            params![path_key(dir), recursive as i32],
        )?;
        Ok(())
    }

    pub fn remove_folder(&mut self, dir: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM folders WHERE path = ?1",
            params![path_key(dir)],
        )?;
        Ok(())
    }

    pub fn folders(&self) -> Result<Vec<(PathBuf, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, recursive FROM folders ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, i32>(1)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --- save states ----------------------------------------------------------------------

    /// Index a save state that has just been written.
    ///
    /// Upserts on path, so a quick-save slot written a second time updates its row rather than
    /// accumulating duplicates for one file.
    pub fn record_state(
        &mut self,
        rom_id: RomId,
        path: &Path,
        label: &str,
        slot: Option<u8>,
        frame: u64,
        size_bytes: u64,
    ) -> Result<SaveId> {
        self.conn.execute(
            "INSERT INTO save_states (rom_id, path, label, slot, frame, created_at, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                 label = ?3, slot = ?4, frame = ?5, created_at = ?6, size_bytes = ?7",
            params![
                rom_id,
                path_key(path),
                label,
                slot.map(|s| s as i64),
                frame as i64,
                unix_now(),
                size_bytes as i64,
            ],
        )?;
        self.state_id_for_path(path)?
            .ok_or_else(|| LibraryError::Missing(path.to_path_buf()))
    }

    pub fn state_id_for_path(&self, path: &Path) -> Result<Option<SaveId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM save_states WHERE path = ?1",
                params![path_key(path)],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn state(&self, id: SaveId) -> Result<Option<SaveStateEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, rom_id, path, label, slot, frame, created_at, size_bytes \
                 FROM save_states WHERE id = ?1",
                params![id],
                state_from_row,
            )
            .optional()?)
    }

    /// One ROM's save states, newest first, with numbered slots ahead of named states.
    pub fn states_for(&self, rom_id: RomId) -> Result<Vec<SaveStateEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rom_id, path, label, slot, frame, created_at, size_bytes \
             FROM save_states WHERE rom_id = ?1 \
             ORDER BY slot IS NULL ASC, slot ASC, created_at DESC",
        )?;
        let rows = stmt.query_map(params![rom_id], state_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn state_in_slot(&self, rom_id: RomId, slot: u8) -> Result<Option<SaveStateEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, rom_id, path, label, slot, frame, created_at, size_bytes \
                 FROM save_states WHERE rom_id = ?1 AND slot = ?2",
                params![rom_id, slot as i64],
                state_from_row,
            )
            .optional()?)
    }

    /// Delete a save state from the index **and** from disk.
    ///
    /// Both halves, always. The predecessor did this correctly and it is worth naming: a delete
    /// that only removes the list entry leaves the file behind forever, and one that only
    /// removes the file leaves a row that fails on click.
    pub fn delete_state(&mut self, id: SaveId) -> Result<()> {
        if let Some(state) = self.state(id)? {
            match std::fs::remove_file(&state.path) {
                Ok(()) => {}
                // Already gone is the desired end state, not an error worth refusing over.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.conn
            .execute("DELETE FROM save_states WHERE id = ?1", params![id])?;
        Ok(())
    }

    // --- reconciliation --------------------------------------------------------------------

    /// Bring the index into agreement with the filesystem, without discarding what only the
    /// index knows.
    ///
    /// The order matters. Missing rows are collected *before* the folder scan, so a file that
    /// moved can be recognised as the same ROM by content hash and have its row updated —
    /// rather than being added as a stranger while the original row rots as missing.
    pub fn reconcile(&mut self) -> Result<Reconciliation> {
        let mut report = Reconciliation::default();
        let all = self.roms()?;

        // Step 1: which recorded paths still resolve?
        let mut missing: Vec<RomEntry> = Vec::new();
        for rom in &all {
            let exists = rom.path.is_file();
            if exists && !rom.present {
                self.set_present(rom.id, true)?;
                report.restored += 1;
            } else if !exists {
                if rom.present {
                    self.set_present(rom.id, false)?;
                }
                missing.push(rom.clone());
            }
        }

        // Step 2: candidate files in watched folders that no row claims.
        let indexed: HashMap<String, RomId> = all
            .iter()
            .map(|rom| (path_key(&rom.path), rom.id))
            .collect();
        let mut candidates = Vec::new();
        for (dir, recursive) in self.folders()? {
            for path in scan_dir(&dir, recursive) {
                if !indexed.contains_key(&path_key(&path)) {
                    candidates.push(path);
                }
            }
        }

        // Step 3: match candidates against missing rows before treating them as new.
        //
        // Size is checked first because it is a stat and the hash is a full read: for a
        // directory of large GBA ROMs, hashing every candidate against every missing row would
        // turn startup into a disk benchmark.
        for path in candidates {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let size = meta.len();
            let mut hash: Option<u64> = None;
            let mut matched = None;
            for (i, rom) in missing.iter().enumerate() {
                if rom.size_bytes != size {
                    continue;
                }
                let h = match hash {
                    Some(h) => h,
                    None => match std::fs::read(&path) {
                        Ok(bytes) => *hash.insert(content_hash(&bytes)),
                        Err(e) => {
                            tracing::warn!("cannot hash {}: {e}", path.display());
                            break;
                        }
                    },
                };
                if rom.content_hash == h {
                    matched = Some(i);
                    break;
                }
            }
            match matched {
                Some(i) => {
                    let rom = missing.remove(i);
                    self.conn.execute(
                        "UPDATE roms SET path = ?1, present = 1 WHERE id = ?2",
                        params![path_key(&path), rom.id],
                    )?;
                    tracing::info!("library: {} moved to {}", rom.title, path.display());
                    report.moved += 1;
                }
                None => {
                    if self.import_file(&path).is_ok() {
                        report.added += 1;
                    }
                }
            }
        }
        report.missing = missing.len();

        // Step 4: save states. A state whose file is gone is dropped from the index, because
        // unlike a ROM there is nothing to recover — the row's only content was the file.
        report.states_dropped = self.drop_vanished_states()?;
        report.states_adopted = self.adopt_orphan_states()?;
        Ok(report)
    }

    fn set_present(&mut self, id: RomId, present: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE roms SET present = ?1 WHERE id = ?2",
            params![present as i32, id],
        )?;
        Ok(())
    }

    fn drop_vanished_states(&mut self) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT id, path FROM save_states")?;
        let rows: Vec<(SaveId, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut dropped = 0;
        for (id, path) in rows {
            if !Path::new(&path).is_file() {
                self.conn
                    .execute("DELETE FROM save_states WHERE id = ?1", params![id])?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// Pick up state files sitting in a ROM's state directory that the index has no row for.
    ///
    /// This is how a state copied in from a backup, or written by a build whose index was later
    /// deleted, becomes visible again. The frame number is unknown without opening the state, so
    /// it is recorded as 0 and the label comes from the file name — honest placeholders rather
    /// than a guess presented as fact.
    fn adopt_orphan_states(&mut self) -> Result<usize> {
        let roms = self.roms()?;
        let mut adopted = 0;
        for rom in roms {
            let dir = self.paths.states_dir_for(&rom.path);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some(crate::paths::STATE_EXTENSION)
                {
                    continue;
                }
                if self.state_id_for_path(&path)?.is_some() {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("recovered")
                    .to_string();
                let slot = stem.strip_prefix("slot").and_then(|n| n.parse::<u8>().ok());
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                self.record_state(rom.id, &path, &stem, slot, 0, size)?;
                adopted += 1;
            }
        }
        Ok(adopted)
    }
}

fn rom_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RomEntry> {
    let platform: String = row.get(3)?;
    Ok(RomEntry {
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        title: row.get(2)?,
        // An unrecognised platform string can only come from a newer build's database. Reading
        // it as a Game Boy would be a lie, so it is reported as the DS: not runnable, visible,
        // and impossible to mistake for something that works.
        platform: Platform::from_id(&platform).unwrap_or(Platform::Nds),
        size_bytes: row.get::<_, i64>(4)? as u64,
        content_hash: row.get::<_, i64>(5)? as u64,
        added_at: row.get(6)?,
        last_played_at: row.get(7)?,
        play_count: row.get::<_, i64>(8)? as u32,
        present: row.get::<_, i32>(9)? != 0,
    })
}

fn state_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SaveStateEntry> {
    Ok(SaveStateEntry {
        id: row.get(0)?,
        rom_id: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        label: row.get(3)?,
        slot: row.get::<_, Option<i64>>(4)?.map(|s| s as u8),
        frame: row.get::<_, i64>(5)? as u64,
        created_at: row.get(6)?,
        size_bytes: row.get::<_, i64>(7)? as u64,
    })
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The string form a path takes in the database.
///
/// Lossy conversion is acceptable here and the alternative is not: refusing to index a file
/// whose name is not UTF-8 would make the library silently incomplete on exactly the systems
/// where such names occur. A lossily-keyed row still shows up and still plays, because the
/// `PathBuf` it produces round-trips on every platform this targets.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Prefer an absolute path so the same file imported through two different relative paths is
/// one row, not two. Falls back to the given path when the file cannot be canonicalised.
fn canonical_or_given(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// FNV-1a over the whole file, used as content identity for move detection.
///
/// Deliberately the same function the accuracy harness hashes framebuffers with, for the same
/// reason: this compares two files on the user's own disk, it does not defend against a forged
/// collision. A full read is the cost — it happens on import and on a candidate file the index
/// has never seen, never on every launch for every ROM, which is what would make it matter.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Every file with a recognised ROM extension under `dir`, in canonical form.
///
/// Canonicalising here is not tidiness, it is correctness: reconciliation decides whether a
/// found file is new by looking its path up among the indexed ones, and [`import_file`] stores
/// canonical paths. Without it, a watched folder reached through a symlink — `/tmp` on macOS,
/// `/home` on many Linux installs — makes every already-indexed ROM look brand new, and every
/// reconciliation pass duplicates the whole library.
///
/// Symlinked directories are not followed. A library folder that contains a link back to its own
/// parent is not exotic — one careless `ln -s` produces it — and the scan would not terminate.
///
/// [`import_file`]: Library::import_file
fn scan_dir(dir: &Path, recursive: bool) -> Vec<PathBuf> {
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
                found.push(canonical_or_given(&path));
            }
        }
    }
    found.sort();
    found
}
