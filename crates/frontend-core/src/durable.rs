//! Writes that survive a crash or a power cut, not only a clean exit.
//!
//! Battery save RAM, save states, and the settings file all go through [`write_atomically`]. It
//! used to be two copies of `write` then `rename`, which is atomic against a crash of *this
//! process* but not against the machine losing power: without a sync, the rename can reach the
//! disk before the data it points at, and the player is left with a correctly named file of zeros
//! — the same truncated save the rename existed to prevent. One helper, so the config writer and
//! the save writer cannot drift apart on this again.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` through a synced temporary file, then rename it into place and sync the
/// directory that holds it.
///
/// The temporary is the full file name plus `.tmp` — `Zelda.sav.tmp`, `config.toml.tmp` — rather
/// than the extension replaced, so two files differing only in extension never share a temporary.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = temp_path(path);
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Rename `path` to the first free `<name>.<suffix>`, `<name>.<suffix>.1`, … and return where it
/// went.
///
/// Never onto an existing backup: a player who hits this twice with two different bad files has
/// two files worth keeping. The existence check and the rename are not one atomic step, which is
/// acceptable because each caller is the only writer of the directory it moves within — save RAM
/// is written only by the emulation thread, the settings file only by the UI thread at shutdown.
pub(crate) fn move_aside(path: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = (0u32..)
        .map(|n| match n {
            0 => path.with_file_name(format!("{name}.{suffix}")),
            n => path.with_file_name(format!("{name}.{suffix}.{n}")),
        })
        .find(|candidate| !candidate.exists())
        .expect("an unbounded range always yields a free name");
    std::fs::rename(path, &backup)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    Ok(backup)
}

/// Make a rename inside `dir` durable, where the platform lets a directory be synced.
///
/// Best effort, deliberately. By the time this runs the data is synced and the rename has happened;
/// a failure here means only that the rename's durability is unconfirmed, and reporting the whole
/// write as failed would tell the player their save was lost when it almost certainly was not.
/// Some filesystems refuse to sync a directory at all (`EINVAL`), and Windows cannot open one as a
/// file without `FILE_FLAG_BACKUP_SEMANTICS` — NTFS journals the rename itself, so it is skipped
/// there rather than reached through `unsafe` flags for no gain.
pub(crate) fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temporary_keeps_the_whole_file_name() {
        assert_eq!(
            temp_path(Path::new("/saves/Zelda.sav")),
            Path::new("/saves/Zelda.sav.tmp")
        );
    }

    #[test]
    fn a_write_replaces_the_file_and_leaves_no_temporary_behind() {
        let dir = std::env::temp_dir().join(format!("alpha-durable-{}", std::process::id()));
        let path = dir.join("nested").join("game.sav");
        write_atomically(&path, b"first").unwrap();
        write_atomically(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(!temp_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
