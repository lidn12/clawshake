//! File-change watcher for the permissions database.
//!
//! When a user runs `clawshake permissions allow ...` in a separate CLI
//! process, the SQLite file is modified.  This watcher detects that change
//! and sends a signal so the bridge can re-publish its DHT announcement
//! with updated tool exposure.
//!
//! SQLite in WAL mode writes to `permissions.db-wal` rather than the main
//! file for most transactions.  The main file is only updated during a
//! checkpoint (auto-checkpoint after 1000 WAL frames).  A single
//! `INSERT`/`UPDATE` from the CLI usually produces just one frame, so the
//! watcher must react to `-wal` changes as well or permission edits go
//! unnoticed until the periodic 5-min DHT re-announce.

use notify::{RecursiveMode, Watcher};
use std::ffi::OsString;
use std::path::Path;
use tracing::warn;

/// Start watching the permissions database file for external modifications.
///
/// Spawns a background thread that monitors the parent directory of `db_path`
/// and sends `()` through `tx` whenever the DB file **or its WAL companion**
/// is created or modified.
pub fn watch_permissions_db(db_path: &Path, tx: tokio::sync::mpsc::Sender<()>) {
    // Watch the parent directory — the file itself may be recreated by SQLite.
    let watch_dir = db_path
        .parent()
        .expect("permissions.db parent dir")
        .to_path_buf();
    let db_name: OsString = db_path
        .file_name()
        .expect("permissions.db file name")
        .into();
    // Also match the WAL companion (e.g. "permissions.db-wal").
    let wal_name: OsString = {
        let mut s = db_name.clone();
        s.push("-wal");
        s
    };

    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = match notify::recommended_watcher(fs_tx) {
        Ok(w) => w,
        Err(e) => {
            warn!("Could not start permissions DB watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
        warn!("Could not watch permissions DB directory: {e}");
        return;
    }

    std::thread::spawn(move || {
        let _watcher = watcher; // prevent drop
        for event in fs_rx.into_iter().flatten() {
            let dominated = event.paths.iter().any(|p| {
                p.file_name()
                    .map(|n| n == db_name || n == wal_name)
                    .unwrap_or(false)
            });
            if dominated
                && matches!(
                    event.kind,
                    notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                )
            {
                let _ = tx.try_send(());
            }
        }
    });
}
