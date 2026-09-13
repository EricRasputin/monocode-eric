//! Coalesced database housekeeping; never removes Git checkouts or references.
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode};
use tauri::{AppHandle, Emitter, Manager};

use crate::session_store::SessionStore;

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(1);
const MIN_RECLAIM_BYTES: u64 = 1024 * 1024;
const MAX_COMPACT_DATABASE_BYTES: u64 = 128 * 1024 * 1024;

static WORKER: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

pub(super) fn schedule(app: &AppHandle) {
    let sender = WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(1);
        let app = app.clone();
        let result = std::thread::Builder::new()
            .name("worktree-storage-maintenance".into())
            .spawn(move || loop {
                if matches!(
                    receiver.recv_timeout(MAINTENANCE_INTERVAL),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ) {
                    break;
                }
                while receiver.try_recv().is_ok() {}
                if let Err(error) = maintain(&app) {
                    // Housekeeping can retry later; a completed retirement is
                    // not turned into a failure by optional space reclamation.
                    eprintln!("Recovery storage maintenance deferred: {error}");
                }
            });
        if let Err(error) = result {
            eprintln!("Could not start recovery storage maintenance: {error}");
        }
        sender
    });
    let _ = sender.try_send(());
}

fn maintain(app: &AppHandle) -> Result<(), String> {
    let conn = app.state::<SessionStore>().open_auxiliary_conn()?;
    conn.busy_timeout(Duration::from_millis(150))
        .map_err(|error| error.to_string())?;
    let report = super::storage::maintenance(&conn)?;
    if report.reclaimed_bytes > 0 {
        let _ = app.emit("worktree-storage-changed", ());
    }
    // The shared database also stores conversations. Avoid rewriting it while
    // Monocode-owned agents, terminals or setup commands are active.
    if !app
        .state::<crate::harness::HarnessHost>()
        .active_workdirs()
        .is_empty()
        || !app
            .state::<crate::pty::PtyHost>()
            .active_workdirs()
            .is_empty()
        || !super::setup::active_paths().is_empty()
    {
        return Ok(());
    }
    compact_if_worthwhile(&conn)?;
    Ok(())
}

fn compact_if_worthwhile(conn: &Connection) -> Result<bool, String> {
    if conn.path().is_none_or(str::is_empty) || !conn.is_autocommit() {
        return Ok(false);
    }
    let pragma = |name: &str| -> Result<u64, String> {
        let value: i64 = conn
            .pragma_query_value(None, name, |row| row.get(0))
            .map_err(|error| error.to_string())?;
        u64::try_from(value).map_err(|error| error.to_string())
    };
    let pages = pragma("page_count")?;
    let free_pages = pragma("freelist_count")?;
    let page_size = pragma("page_size")?;
    // Large conversation databases reuse free pages instead of undertaking an
    // expensive full rewrite. Tiny savings don't justify rewriting either.
    if free_pages.saturating_mul(page_size) < MIN_RECLAIM_BYTES
        || pages.saturating_mul(page_size) > MAX_COMPACT_DATABASE_BYTES
        || free_pages.saturating_mul(8) < pages
    {
        return Ok(false);
    }

    let interrupt = conn.get_interrupt_handle();
    let (cancel, timeout) = mpsc::channel();
    let watchdog = std::thread::Builder::new()
        .name("worktree-storage-compaction-timeout".into())
        .spawn(move || {
            if timeout.recv_timeout(COMPACTION_TIMEOUT).is_err() {
                interrupt.interrupt();
            }
        })
        .map_err(|error| error.to_string())?;
    let result = conn.execute_batch("VACUUM;");
    let _ = cancel.send(());
    let _ = watchdog.join();
    match result {
        Ok(()) => {
            // In WAL mode the new compact database reaches the main file at a
            // checkpoint. Busy readers are allowed to defer truncation.
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            Ok(true)
        }
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                ErrorCode::DatabaseBusy
                    | ErrorCode::DatabaseLocked
                    | ErrorCode::OperationInterrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn maintenance_reclaims_database_pages_without_changing_retained_data() {
        let path = std::env::temp_dir().join(format!(
            "monocode-storage-compaction-{}-{}.sqlite3",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE retained (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO retained VALUES (1, 'configuration still recoverable');
             CREATE TABLE discarded (value BLOB);
             INSERT INTO discarded VALUES (zeroblob(4 * 1024 * 1024));
             DELETE FROM discarded;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(compact_if_worthwhile(&conn).unwrap());
        drop(conn);
        let reopened = Connection::open(&path).unwrap();
        assert_eq!(
            reopened
                .query_row("SELECT value FROM retained", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "configuration still recoverable"
        );
        assert_eq!(
            reopened
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(std::fs::metadata(&path).unwrap().len() < before);
        assert!(!compact_if_worthwhile(&reopened).unwrap());
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
}
