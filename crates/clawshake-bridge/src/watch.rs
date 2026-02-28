//! File-change watcher for the permissions database.
//!
//! When a user runs `clawshake permissions allow ...` in a separate CLI
//! process, the SQLite file is modified.  This watcher detects that change
//! and sends a signal so the bridge can re-publish its DHT announcement
//! with updated tool exposure.

use notify::{RecursiveMode, Watcher};
use std::path::Path;
use tracing::warn;

/// Start watching the permissions database file for external modifications.
///
/// Spawns a background thread that monitors the parent directory of `db_path`
/// and sends `()` through `tx` whenever the DB file is created or modified.
/// The watcher deliberately targets only the exact file name (ignoring
/// SQLite's `-wal` and `-shm` companions) to avoid spurious signals.
pub fn watch_permissions_db(db_path: &Path, tx: tokio::sync::mpsc::Sender<()>) {
    // Watch the parent directory — the file itself may be recreated by SQLite.
    let watch_dir = db_path
        .parent()
        .expect("permissions.db parent dir")
        .to_path_buf();
    let db_name = db_path
        .file_name()
        .expect("permissions.db file name")
        .to_os_string();

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
        for res in fs_rx {
            if let Ok(event) = res {
                let dominated = event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|n| n == db_name)
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
        }
    });
}
