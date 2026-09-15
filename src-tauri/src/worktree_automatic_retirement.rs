//! Durable archive cleanup. The queue carries intent; every attempt goes through
//! the same native retirement validation and recovery journal as manual review.
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, State};

use super::*;

const INTERVAL: Duration = Duration::from_secs(5 * 60);
static WORKER: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();
static PASS: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    #[default]
    Manual,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub schema_version: u32,
    pub version: i64,
    pub mode: Mode,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema_version: 1,
            version: 0,
            mode: Mode::Manual,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub id: String,
    pub path: String,
    pub plan_id: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub updated_at: i64,
}

pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    // Deliberately do not import the obsolete auto_cleanup preference. Enabling
    // removal requires a new, explicit, version-checked save for this project.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_retirement_policy (
        common_dir TEXT NOT NULL, project_path TEXT NOT NULL,
        version INTEGER NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('manual', 'automatic')),
        PRIMARY KEY(common_dir, project_path)
    );
    CREATE TABLE IF NOT EXISTS worktree_automatic_retirement (
        worktree_id TEXT PRIMARY KEY, plan_id TEXT, status TEXT NOT NULL,
        reason TEXT, updated_at INTEGER NOT NULL
    );",
    )
}

pub(super) fn policy(
    conn: &Connection,
    scope: &environment::ProjectScope,
) -> Result<Policy, String> {
    conn.query_row(
        "SELECT version, mode FROM worktree_retirement_policy WHERE common_dir = ?1 AND project_path = ?2",
        params![scope.common, scope.relative],
        |row| {
            let mode: String = row.get(1)?;
            Ok(Policy { schema_version: 1, version: row.get(0)?, mode: if mode == "automatic" { Mode::Automatic } else { Mode::Manual } })
        },
    ).optional().map(|p| p.unwrap_or_default()).map_err(|e| e.to_string())
}

fn save_policy(
    conn: &Connection,
    scope: &environment::ProjectScope,
    requested: Policy,
) -> Result<Policy, String> {
    if requested.schema_version != 1 || requested.version < 0 {
        return Err("Unsupported automatic retirement preference version".into());
    }
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    if policy(&tx, scope)?.version != requested.version {
        return Err(
            "WORKTREE_RETIREMENT_CONFLICT: Retirement preference changed in another window. Reload and save again.".into(),
        );
    }
    let saved = Policy {
        version: requested.version + 1,
        ..requested
    };
    tx.execute("INSERT INTO worktree_retirement_policy(common_dir, project_path, version, mode)
        VALUES (?1, ?2, ?3, ?4) ON CONFLICT(common_dir, project_path) DO UPDATE SET version = excluded.version, mode = excluded.mode",
        params![scope.common, scope.relative, saved.version, if saved.mode == Mode::Automatic { "automatic" } else { "manual" }]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(saved)
}

pub(super) fn enabled(conn: &Connection, entry: &Owned) -> Result<bool, String> {
    Ok(policy(conn, &environment::scope_for_entry(conn, entry)?)?.mode == Mode::Automatic)
}

fn is_automatic_plan(conn: &Connection, snapshot: &RetirementSnapshot) -> Result<bool, String> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM worktree_automatic_retirement WHERE worktree_id = ?1 AND plan_id = ?2)",
        params![snapshot.id, snapshot.plan_id], |r| r.get(0)).map_err(|e| e.to_string())
}

pub(super) fn recheck_policy(
    conn: &Connection,
    entry: &Owned,
    snapshot: &RetirementSnapshot,
) -> Result<(), String> {
    if is_automatic_plan(conn, snapshot)? && !enabled(conn, entry)? {
        return Err("Automatic retirement is disabled; manual review is required".into());
    }
    Ok(())
}

pub(super) fn validate_selection(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    selection: &WorktreeRetirementSelection,
) -> Result<(), String> {
    if (selection.delete_local_branch || selection.delete_remote_branch)
        && is_automatic_plan(conn, snapshot)?
    {
        return Err(
            "Automatic retirement never deletes branches. Create a separate manual review.".into(),
        );
    }
    Ok(())
}

/// A fresh manual review can take over an interrupted automatic attempt even
/// after automatic mode is disabled. This reconciles only the journal: it never
/// removes a folder or branch. The new review carries fresh, explicit choices.
pub(super) fn reconcile_for_manual_review(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: Owned,
) -> Result<Owned, String> {
    let Some(plan) = &entry.pending_retirement_plan_id else {
        return Ok(entry);
    };
    let snapshot = load_retirement_snapshot(conn, plan, &entry.id)?
        .ok_or("Pending retirement recovery record is missing")?;
    if !is_automatic_plan(conn, &snapshot)? {
        return Ok(entry);
    }
    current_owned_for_snapshot(conn, &snapshot)?;
    if let Some(reason) = active_use_reason(conn, windows, &entry)? {
        return Err(reason);
    }
    check_git_locks(&entry)?;
    if repository(&entry.repo)?.1 != entry.common {
        return Err("Repository identity changed".into());
    }
    if actual_worktree_removed(&snapshot) {
        // Verifies exact recovery ref, configuration archive and pending owner.
        complete_worktree_removal(conn, &snapshot)?;
        record_status(conn, &entry.id, "complete", None)?;
    } else {
        setup::validate_checkout(&entry, &entry.path)?;
        cancel_pending_removal_for_present_checkout(conn, &entry)?;
    }
    owned(conn)?
        .into_iter()
        .find(|e| e.id == entry.id)
        .ok_or("Managed worktree ownership is missing".into())
}

fn has_archived_session(conn: &Connection, entry: &Owned) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT id, COALESCE(worktree_cwd, cwd) FROM sessions WHERE archived = 1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, path) = row.map_err(|e| e.to_string())?;
        if id == entry.id || path_inside(&expand_home(&path), Path::new(&entry.path)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn record_status(
    conn: &Connection,
    id: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE worktree_automatic_retirement SET status = ?1, reason = ?2, updated_at = ?3
        WHERE worktree_id = ?4 AND (status != ?1 OR reason IS NOT ?2)",
        params![status, reason, now(), id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn pending(conn: &Connection) -> Result<Vec<Pending>, String> {
    let mut stmt = conn.prepare("SELECT queue.worktree_id, managed.path, queue.plan_id, queue.status, queue.reason, queue.updated_at
        FROM worktree_automatic_retirement queue JOIN managed_worktrees managed ON managed.id = queue.worktree_id
        ORDER BY queue.updated_at DESC, queue.worktree_id").map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Pending {
                id: r.get(0)?,
                path: r.get(1)?,
                plan_id: r.get(2)?,
                status: r.get(3)?,
                reason: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string());
    rows
}

pub(super) fn project_pending(
    conn: &Connection,
    scope: &environment::ProjectScope,
) -> Result<Vec<Pending>, String> {
    let ids = owned(conn)?
        .into_iter()
        .filter(|entry| {
            entry.common == scope.common
                || path_inside(Path::new(&scope.main_path), Path::new(&entry.repo))
        })
        .filter_map(|entry| {
            match environment::scope_for_entry(conn, &entry) {
                Ok(origin)
                    if origin.common == scope.common && origin.relative == scope.relative =>
                {
                    Some(entry.id)
                }
                Ok(_) => None,
                // Keep damaged origins discoverable in their repository settings.
                Err(_) => Some(entry.id),
            }
        })
        .collect::<HashSet<_>>();
    Ok(pending(conn)?
        .into_iter()
        .filter(|p| ids.contains(&p.id))
        .collect())
}

/// Archive state itself is durable. Discovering it on every pass closes a crash
/// between the successful archive and the queue notification, including bulk
/// archives and old archived conversations when the user enables this policy.
fn discover(conn: &Connection, host: &WorktreeHost) -> Result<(), String> {
    for candidate in owned(conn)? {
        if let Err(error) = discover_entry(conn, host, &candidate) {
            conn.execute("INSERT INTO worktree_automatic_retirement(worktree_id, status, reason, updated_at) VALUES (?1, 'blocked', ?2, ?3)
                ON CONFLICT(worktree_id) DO UPDATE SET status = 'blocked', reason = excluded.reason, updated_at = excluded.updated_at",
                params![candidate.id, error, now()]).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn discover_entry(conn: &Connection, host: &WorktreeHost, candidate: &Owned) -> Result<(), String> {
    let _repository = host.repository_guard(&candidate.common)?;
    let _windows = host.operation_guard()?;
    let Some(entry) = owned(conn)?.into_iter().find(|e| e.id == candidate.id) else {
        return Ok(());
    };
    if entry.removed
        || has_unarchived_session(conn, &entry)?
        || !has_archived_session(conn, &entry)?
    {
        return Ok(());
    }
    conn.execute("INSERT INTO worktree_automatic_retirement(worktree_id, status, updated_at) VALUES (?1, 'pending', ?2)
            ON CONFLICT(worktree_id) DO UPDATE SET plan_id = NULL, status = 'pending', reason = NULL, updated_at = excluded.updated_at
            WHERE worktree_automatic_retirement.status = 'complete'", params![entry.id, now()]).map_err(|e| e.to_string())?;
    Ok(())
}

fn prepare_attempt(
    conn: &Connection,
    host: &WorktreeHost,
    id: &str,
    protect: &impl Fn(&HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>>,
) -> Result<Option<String>, String> {
    let candidate = owned(conn)?
        .into_iter()
        .find(|e| e.id == id)
        .ok_or("Managed worktree ownership is missing")?;
    let _repository = host.repository_guard(&candidate.common)?;
    let windows = host.operation_guard()?;
    let entry = owned(conn)?
        .into_iter()
        .find(|e| e.id == id)
        .ok_or("Managed worktree ownership is missing")?;
    if entry.removed {
        record_status(conn, id, "complete", None)?;
        return Ok(None);
    }
    if !enabled(conn, &entry)? {
        let previous: Option<String> = conn
            .query_row(
                "SELECT reason FROM worktree_automatic_retirement WHERE worktree_id = ?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        record_status(
            conn,
            id,
            "paused",
            previous.as_deref().or(Some(
                "Manual review is selected. Automatic retirement is paused.",
            )),
        )?;
        return Ok(None);
    }
    let protected = protect(&windows);
    if protected.contains_key("$unregistered-windows") {
        return Err("Waiting for open windows to register their workspace activity".into());
    }
    if let Some(reason) = active_use_reason(conn, &protected, &entry)? {
        return Err(reason);
    }
    let mut existing: Option<String> = conn
        .query_row(
            "SELECT plan_id FROM worktree_automatic_retirement WHERE worktree_id = ?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if let Some(plan) = &existing {
        if let Some(snapshot) = load_retirement_snapshot(conn, plan, id)? {
            current_owned_for_snapshot(conn, &snapshot)?;
            if entry.pending_retirement_plan_id.is_some() || !Path::new(&entry.path).exists() {
                return Ok(existing);
            }
            if let Some(reason) = environment::check_cleanup(conn, &entry)? {
                return Err(reason);
            }
            if resolve_commit(Path::new(&entry.path), "HEAD")? == snapshot.commit_oid
                && environment::review_is_current(conn, &entry, plan)?
            {
                return Ok(existing);
            }
            // This is newly committed work or explicitly changed preservation
            // policy/configuration, not a retry of the same recovery. Keep the
            // queue record and all immutable older recoveries; review once for
            // the new state. Unchanged failures always reuse their saved plan.
            existing = None;
        }
    }
    if entry.pending_retirement_plan_id.is_some() {
        return Err(
            "A manual retirement is still pending. Review it in Worktrees settings.".into(),
        );
    }
    let plan = existing.unwrap_or_else(plan_id);
    // Persist the identity first. A restart between review and persistence
    // resumes this identity instead of manufacturing a second recovery plan.
    conn.execute(
        "UPDATE worktree_automatic_retirement SET plan_id = ?1 WHERE worktree_id = ?2",
        params![plan, id],
    )
    .map_err(|e| e.to_string())?;
    // No merge evidence, remote discovery, or network operations in this path.
    let snapshot = review_retirement_inner(conn, &protected, &entry, &plan, false)?;
    persist_retirement_plan(conn, &plan, &[snapshot])?;
    conn.execute("UPDATE worktree_automatic_retirement SET plan_id = ?1, status = 'pending', reason = NULL, updated_at = ?2 WHERE worktree_id = ?3", params![plan, now(), id]).map_err(|e| e.to_string())?;
    Ok(Some(plan))
}

fn maintain_with(
    conn: &Connection,
    host: &WorktreeHost,
    protect: impl Fn(&HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>>,
) -> Result<(), String> {
    // All windows, archive IPCs and the timer share a single pass. Manual
    // retirement/preparation share the per-repository and lifecycle guards.
    let _pass = PASS.lock().map_err(|e| e.to_string())?;
    discover(conn, host)?;
    for item in pending(conn)?
        .into_iter()
        .filter(|p| p.status != "complete")
    {
        let plan = match prepare_attempt(conn, host, &item.id, &protect) {
            Ok(Some(plan)) => plan,
            Ok(None) => continue,
            Err(error) => {
                record_status(conn, &item.id, "blocked", Some(&error))?;
                continue;
            }
        };
        let result = execute_retirement_coordinated_with(
            conn,
            host,
            &plan,
            &[WorktreeRetirementSelection {
                id: item.id.clone(),
                delete_local_branch: false,
                delete_remote_branch: false,
            }],
            &protect,
        );
        // Even an error may follow partial Git removal. Invalidate before the
        // next admission; the asynchronous disk worker scans after locks drop.
        host.disk.invalidate();
        match result {
            Ok(report) => {
                let result = report
                    .results
                    .first()
                    .ok_or("Automatic retirement returned no result")?;
                let status = if result.error.is_some() {
                    "failed"
                } else if result.worktree_removed {
                    "complete"
                } else {
                    "pending"
                };
                record_status(conn, &item.id, status, result.error.as_deref())?;
            }
            Err(error) => record_status(conn, &item.id, "failed", Some(&error))?,
        }
    }
    Ok(())
}

fn protection(
    app: &AppHandle,
    windows: &HashMap<String, Vec<PathBuf>>,
) -> HashMap<String, Vec<PathBuf>> {
    let mut protected = protected_windows(app, windows);
    if app
        .webview_windows()
        .keys()
        .any(|label| !windows.contains_key(label))
    {
        protected.insert("$unregistered-windows".into(), vec![]);
    }
    protected
}

pub(super) fn maintain(app: &AppHandle) -> Result<(), String> {
    let result = maintain_with(
        &app.state::<SessionStore>().open_auxiliary_conn()?,
        &app.state::<WorktreeHost>(),
        |windows| protection(app, windows),
    );
    disk::refresh(app);
    let _ = app.emit("worktree-retirement-changed", ());
    let _ = app.emit("worktree-storage-changed", ());
    result
}

pub(crate) fn schedule(app: &AppHandle) {
    if let Some(worker) = WORKER.get() {
        let _ = worker.try_send(());
    }
    let _ = app.emit("worktree-retirement-changed", ());
}

/// Called once after startup lifecycle registration. Window registration also
/// gates removal, so restored editor/terminal leases arrive before cleanup.
pub(crate) fn start(app: &AppHandle) {
    let (sender, receiver) = mpsc::sync_channel(1);
    if WORKER.set(sender).is_err() {
        return;
    }
    for event in ["harness-exit", "pty-exit", "worktree-setup-progress"] {
        let app = app.clone();
        let listener = app.clone();
        listener.listen(event, move |_| schedule(&app));
    }
    let worker_app = app.clone();
    std::thread::spawn(move || loop {
        if matches!(
            receiver.recv_timeout(INTERVAL),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ) {
            break;
        }
        while receiver.try_recv().is_ok() {}
        if let Err(error) = maintain(&worker_app) {
            eprintln!("Automatic worktree retirement deferred: {error}");
            let _ = worker_app.emit("worktree-retirement-error", error);
        }
        storage_maintenance::schedule(&worker_app);
    });
    schedule(app);
}

#[tauri::command(async)]
pub fn worktree_retirement_policy_set(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
    policy: Policy,
) -> Result<Policy, String> {
    let scope = environment::scope_for_cwd(&cwd)?;
    let saved = {
        let _repository = host.repository_guard(&scope.common)?;
        let _windows = host.operation_guard()?;
        save_policy(&store.open_auxiliary_conn()?, &scope, policy)?
    };
    schedule(&app);
    Ok(saved)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveRetirement {
    review: WorktreeRetirementPlan,
    automatic: Vec<Pending>,
}

#[tauri::command(async)]
pub fn worktree_retirement_maintain(app: AppHandle) -> Result<(), String> {
    maintain(&app)
}

fn archive_result(
    conn: &Connection,
    host: &WorktreeHost,
    session_ids: &[String],
    windows: &HashMap<String, Vec<PathBuf>>,
) -> Result<ArchiveRetirement, String> {
    let records = owned(conn)?;
    let mut ids = HashSet::new();
    for id in session_ids {
        let path: Option<String> = conn
            .query_row(
                "SELECT COALESCE(worktree_cwd, cwd) FROM sessions WHERE id = ?1 AND archived = 1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(path) = path {
            for entry in &records {
                if (entry.id == *id || path_inside(&expand_home(&path), Path::new(&entry.path)))
                    && enabled(conn, entry).unwrap_or(true)
                {
                    ids.insert(entry.id.clone());
                }
            }
        }
    }
    let automatic: Vec<_> = pending(conn)?
        .into_iter()
        .filter(|p| ids.contains(&p.id))
        .collect();
    let mut review =
        build_retirement_plan_coordinated(conn, host, windows, session_ids, None, &[])?;
    review.kept.retain(|kept| {
        !automatic.iter().any(|item| {
            kept.id == item.id
                || (!kept.path.is_empty()
                    && path_inside(Path::new(&kept.path), Path::new(&item.path)))
        })
    });
    Ok(ArchiveRetirement { review, automatic })
}

#[tauri::command(async)]
pub fn worktree_archive_retirement(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    session_ids: Vec<String>,
) -> Result<ArchiveRetirement, String> {
    // Archive has already committed. Its durable sessions allow restart retry
    // even if this independent cleanup command fails before queue discovery.
    maintain(&app)?;
    let conn = store.open_auxiliary_conn()?;
    let windows = protection(&app, &*host.operation_guard()?);
    archive_result(&conn, &host, &session_ids, &windows)
}

#[cfg(test)]
#[path = "worktree_automatic_retirement_tests.rs"]
mod tests;
