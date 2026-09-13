//! One-time semantic branch naming. The checkout path never changes. A durable
//! intent bridges Git's rename and SQLite's ownership update after interruption.
use super::*;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeNamed {
    pub id: String,
    pub session_ids: Vec<String>,
    pub path: String,
    pub branch: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeNameStatus {
    Waiting,
    Pending,
    Named,
    Skipped,
}

fn status(conn: &Connection, id: &str, token: &str) -> Result<WorktreeNameStatus, String> {
    let state: Option<String> = conn
        .query_row(
            "SELECT n.state FROM worktree_naming n
             JOIN managed_worktrees m ON m.id = n.worktree_id
             WHERE n.worktree_id = ?1 AND n.token = ?2 AND m.removed = 0",
            params![id, token],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(match state.as_deref() {
        Some("waiting") => WorktreeNameStatus::Waiting,
        Some("ready" | "renaming") => WorktreeNameStatus::Pending,
        Some("done") => WorktreeNameStatus::Named,
        _ => WorktreeNameStatus::Skipped,
    })
}

/// A failed generation leaves the original request waiting. Explicit retries
/// can inspect it, but never reopen a request frozen by a user Git operation.
#[tauri::command(async)]
pub fn worktree_name_status(
    store: State<'_, SessionStore>,
    session_id: String,
    token: String,
) -> Result<WorktreeNameStatus, String> {
    status(&store.open_auxiliary_conn()?, &session_id, &token)
}

pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_naming (
            worktree_id TEXT PRIMARY KEY REFERENCES managed_worktrees(id) ON DELETE CASCADE,
            token TEXT NOT NULL, source_branch TEXT NOT NULL,
            state TEXT NOT NULL, target_branch TEXT, rename_oid TEXT
         );",
    )
}

pub(super) fn register(conn: &Connection, entry: &Owned, token: &str) -> Result<(), String> {
    validate_id(token)?;
    conn.execute(
        "INSERT INTO worktree_naming (worktree_id, token, source_branch, state)
         VALUES (?1, ?2, ?3, 'waiting')",
        params![entry.id, token, entry.branch],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn available_branch(repo: &Path, desired: &str) -> Result<String, String> {
    git(repo, &["check-ref-format", "--branch", desired])?;
    for suffix in 0..=100 {
        let name = if suffix == 0 {
            desired.to_string()
        } else {
            format!("{desired}-{suffix}")
        };
        if ref_oid(repo, &format!("refs/heads/{name}"))?.is_none() {
            return Ok(name);
        }
    }
    Err("Could not allocate an unused worktree branch name".into())
}

fn normalize(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let raw = lower.strip_prefix("refs/heads/").unwrap_or(&lower);
    let raw = raw.strip_prefix("monocode/").unwrap_or(raw);
    let fragment = raw
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let fragment = fragment.chars().take(64).collect::<String>();
    let fragment = fragment.trim_end_matches('-');
    (!fragment.is_empty()).then(|| format!("monocode/{fragment}"))
}

#[derive(Debug)]
struct Naming {
    source: String,
    target: Option<String>,
    oid: Option<String>,
    state: String,
}

fn record(conn: &Connection, id: &str) -> Result<Option<Naming>, String> {
    conn.query_row(
        "SELECT source_branch, target_branch, rename_oid, state FROM worktree_naming
         WHERE worktree_id = ?1",
        [id],
        |row| {
            Ok(Naming {
                source: row.get(0)?,
                target: row.get(1)?,
                oid: row.get(2)?,
                state: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn skip(conn: &Connection, id: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE worktree_naming SET state = 'skipped' WHERE worktree_id = ?1",
        [id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn complete(conn: &Connection, entry: &Owned, naming: &Naming) -> Result<WorktreeNamed, String> {
    let target = naming
        .target
        .as_deref()
        .ok_or("Missing worktree rename target")?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE managed_worktrees SET branch = ?1
         WHERE id = ?2 AND branch = ?3 AND removed = 0",
            params![target, entry.id, naming.source],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Worktree ownership changed during naming".into());
    }
    let session_ids = {
        let mut statement = tx
            .prepare(
                "SELECT id FROM sessions WHERE id = ?1 OR COALESCE(worktree_cwd, cwd) = ?2 OR
             substr(COALESCE(worktree_cwd, cwd), 1, length(?2) + 1) = ?2 || '/'",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![entry.id, entry.path], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    tx.execute(
        "UPDATE sessions SET branch = ?1 WHERE id = ?2 OR
         COALESCE(worktree_cwd, cwd) = ?3 OR
         substr(COALESCE(worktree_cwd, cwd), 1, length(?3) + 1) = ?3 || '/'",
        params![target, entry.id, entry.path],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE worktree_naming SET state = 'done' WHERE worktree_id = ?1",
        [&entry.id],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(WorktreeNamed {
        id: entry.id.clone(),
        session_ids,
        path: entry.path.clone(),
        branch: target.into(),
    })
}

/// Only recognize the exact journaled transition. Never adopt arbitrary branch
/// drift as an owned rename. The branch may have gained commits after Git ran.
fn reconcile(
    conn: &Connection,
    entry: &Owned,
    naming: &Naming,
) -> Result<Option<WorktreeNamed>, String> {
    let target = naming
        .target
        .as_deref()
        .ok_or("Missing worktree rename target")?;
    let oid = naming
        .oid
        .as_deref()
        .ok_or("Missing worktree rename commit")?;
    if entry.removed || entry.branch != naming.source {
        return Err("Worktree ownership changed during interrupted naming".into());
    }
    let mut renamed = entry.clone();
    renamed.branch = target.into();
    let source_oid = ref_oid(
        Path::new(&entry.repo),
        &format!("refs/heads/{}", naming.source),
    )?;
    if source_oid.is_none() && setup::validate_checkout(&renamed, &entry.path).is_ok() {
        let head = resolve_commit(Path::new(&entry.path), "HEAD")?;
        if is_ancestor(Path::new(&entry.repo), oid, &head)? {
            return complete(conn, entry, naming).map(Some);
        }
    }
    if source_oid.is_some() && setup::validate_checkout(entry, &entry.path).is_ok() {
        // Git did not complete the rename. Keep the original branch; never
        // replay a delayed mutation after a restart or a user operation.
        skip(conn, &entry.id)?;
        return Ok(None);
    }
    Err("Interrupted worktree naming could not be verified; checkout was preserved".into())
}

/// Caller holds this repository's reservation and the short lifecycle lock.
pub(super) fn apply_pending(conn: &Connection, id: &str) -> Result<Option<WorktreeNamed>, String> {
    let Some(naming) = record(conn, id)? else {
        return Ok(None);
    };
    if !matches!(naming.state.as_str(), "ready" | "renaming") {
        return Ok(None);
    }
    let Some(entry) = owned(conn)?.into_iter().find(|entry| entry.id == id) else {
        return Ok(None);
    };
    if naming.state == "renaming" {
        return reconcile(conn, &entry, &naming);
    }
    let has_review: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM worktree_retirement_items WHERE worktree_id = ?1)",
            [id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let unavailable_session: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE (id = ?1 AND archived = 1) OR
           (id != ?1 AND (COALESCE(worktree_cwd, cwd) = ?2 OR
            substr(COALESCE(worktree_cwd, cwd), 1, length(?2) + 1) = ?2 || '/')))",
            params![id, entry.path],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if entry.removed
        || entry.creation_oid.is_some()
        || entry.branch != naming.source
        || entry.pending_retirement_plan_id.is_some()
        || entry.active_retirement_plan_id.is_some()
        || has_review
        || unavailable_session
        || setup::validate_checkout(&entry, &entry.path).is_err()
    {
        skip(conn, id)?;
        return Ok(None);
    }
    let ready: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM worktree_environment_setup
         WHERE worktree_id = ?1 AND status = 'ready')",
            [id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !ready {
        return Ok(None);
    }
    if checkouts(Path::new(&entry.repo))?
        .iter()
        .any(|checkout| checkout.path == entry.path && checkout.locked)
    {
        skip(conn, id)?;
        return Ok(None);
    }
    let repo = Path::new(&entry.repo);
    let upstream = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(upstream)",
            &format!("refs/heads/{}", entry.branch),
        ],
    )?;
    let remotes = git(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/remotes/"],
    )?;
    if !upstream.trim().is_empty()
        || remotes
            .lines()
            .any(|line| line.ends_with(&format!("/{}", entry.branch)))
    {
        skip(conn, id)?;
        return Ok(None);
    }
    let desired = naming
        .target
        .as_deref()
        .ok_or("Missing worktree name suggestion")?;
    let target = available_branch(repo, desired)?;
    let oid = resolve_commit(Path::new(&entry.path), "HEAD")?;
    conn.execute(
        "UPDATE worktree_naming SET state = 'renaming', target_branch = ?1, rename_oid = ?2
         WHERE worktree_id = ?3 AND state = 'ready'",
        params![target, oid, id],
    )
    .map_err(|e| e.to_string())?;
    // Never force a rename: another process may have claimed the target since
    // allocation. The explicit source avoids renaming a newly selected branch.
    if let Err(error) = git(
        Path::new(&entry.path),
        &["branch", "-m", "--", &naming.source, &target],
    ) {
        // Reconcile even when Git reports failure: it may have partially run.
        let journal = record(conn, id)?.ok_or("Missing worktree naming journal")?;
        return match reconcile(conn, &entry, &journal) {
            Ok(Some(changed)) => Ok(Some(changed)),
            _ => Err(error),
        };
    }
    let journal = record(conn, id)?.ok_or("Missing worktree naming journal")?;
    reconcile(conn, &entry, &journal)
}

fn suggest(
    conn: &Connection,
    id: &str,
    token: &str,
    branch: Option<&str>,
) -> Result<Option<WorktreeNamed>, String> {
    let target = branch.and_then(normalize);
    let changed = conn
        .execute(
            "UPDATE worktree_naming SET target_branch = ?1, state = ?2
         WHERE worktree_id = ?3 AND token = ?4 AND state = 'waiting'",
            params![
                target,
                if target.is_some() { "ready" } else { "skipped" },
                id,
                token
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        if status(conn, id, token)? == WorktreeNameStatus::Pending {
            return apply_pending(conn, id);
        }
        return Ok(None);
    }
    apply_pending(conn, id)
}

pub(super) fn emit(app: &AppHandle, result: Result<Option<WorktreeNamed>, String>) {
    match result {
        Ok(Some(event)) => {
            let _ = app.emit("worktree-named", event);
        }
        Ok(None) => {}
        Err(error) => eprintln!("[monocode] worktree naming: {error}"),
    }
}

#[tauri::command(async)]
pub fn worktree_name(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    session_id: String,
    token: String,
    branch: Option<String>,
) -> Result<WorktreeNameStatus, String> {
    let conn = store.open_auxiliary_conn()?;
    let Some(entry) = owned(&conn)?
        .into_iter()
        .find(|entry| entry.id == session_id)
    else {
        return Ok(WorktreeNameStatus::Skipped);
    };
    let _repository = host.repository_guard(&entry.common)?;
    let _windows = host.operation_guard()?;
    let result = suggest(&conn, &session_id, &token, branch.as_deref())?;
    emit(&app, Ok(result));
    status(&conn, &session_id, &token)
}

pub(super) fn reconcile_repository(
    conn: &Connection,
    common: &str,
) -> Result<Vec<WorktreeNamed>, String> {
    let mut changes = Vec::new();
    for entry in owned(conn)?
        .into_iter()
        .filter(|entry| entry.common == common)
    {
        if let Some(naming) = record(conn, &entry.id)? {
            if naming.state == "renaming" {
                if let Some(changed) = reconcile(conn, &entry, &naming)? {
                    changes.push(changed);
                }
            }
        }
    }
    Ok(changes)
}

pub(super) fn recover(app: &AppHandle, conn: &Connection) -> Result<(), String> {
    for entry in owned(conn)? {
        // At startup no other app operation is running. Only finish journaled
        // Git mutations, never resume model requests or start fresh renames.
        if let Some(naming) = record(conn, &entry.id)? {
            if naming.state == "renaming" {
                emit(app, reconcile(conn, &entry, &naming));
            }
        }
    }
    Ok(())
}

/// Resolve a possible interrupted rename, then freeze naming before publishing
/// or an explicit checkout change. Release locks before any network operation.
pub(crate) fn stabilize_branch(app: &AppHandle, cwd: &str) -> Result<(), String> {
    let conn = app.state::<SessionStore>().open_auxiliary_conn()?;
    let Some(entry) = owned(&conn)?
        .into_iter()
        .find(|entry| path_inside(&expand_home(cwd), Path::new(&entry.path)))
    else {
        return Ok(());
    };
    let host = app.state::<WorktreeHost>();
    let _repository = host.repository_guard(&entry.common)?;
    let _windows = host.operation_guard()?;
    emit(app, Ok(freeze_pending(&conn, &entry)?));
    Ok(())
}

fn freeze_pending(conn: &Connection, entry: &Owned) -> Result<Option<WorktreeNamed>, String> {
    let mut changed = None;
    if let Some(naming) = record(conn, &entry.id)? {
        if naming.state == "renaming" {
            changed = reconcile(conn, entry, &naming)?;
        }
        conn.execute("UPDATE worktree_naming SET state = 'skipped' WHERE worktree_id = ?1 AND state IN ('waiting', 'ready')", [&entry.id])
            .map_err(|e| e.to_string())?;
    }
    Ok(changed)
}

#[cfg(test)]
#[path = "worktree_naming_tests.rs"]
mod tests;
