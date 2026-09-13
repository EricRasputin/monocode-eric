//! Owned worktree lifecycle. Git is the source of truth; SQLite records ownership,
//! retirement and recovery information. Reviewed removals preserve recovery refs.
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use crate::fs::{expand_home, path_to_js as fs_path_to_js};
use crate::session_store::SessionStore;

#[path = "worktree_remote_git.rs"]
mod remote_git;

#[path = "worktree_environment.rs"]
mod environment;
#[cfg(all(test, unix))]
#[path = "worktree_environment_integration_tests.rs"]
mod environment_integration_tests;
#[path = "worktree_naming.rs"]
pub(crate) mod naming;
#[path = "worktree_setup.rs"]
pub(crate) mod setup;
#[path = "worktree_storage.rs"]
pub(crate) mod storage;
#[cfg(all(test, unix))]
#[path = "worktree_storage_integration_tests.rs"]
mod storage_integration_tests;
#[path = "worktree_storage_maintenance.rs"]
mod storage_maintenance;

#[path = "worktree_merge.rs"]
mod worktree_merge;

pub struct WorktreeHost {
    root: PathBuf,
    // Local lifecycle operations and window leases share one short-held lock.
    // A checkout cannot be opened between its last safety check and removal.
    windows: Mutex<HashMap<String, Vec<PathBuf>>>,
    repositories: RepositoryReservations,
}

#[derive(Default)]
struct RepositoryReservations {
    active: Mutex<HashSet<String>>,
    changed: Condvar,
}

pub(crate) struct RepositoryGuard<'a> {
    reservations: &'a RepositoryReservations,
    key: String,
}

impl Drop for RepositoryGuard<'_> {
    fn drop(&mut self) {
        let mut active = self
            .reservations
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.remove(&self.key);
        self.reservations.changed.notify_all();
    }
}

static RETIREMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl WorktreeHost {
    pub(crate) fn operation_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<PathBuf>>>, String> {
        self.windows.lock().map_err(|e| e.to_string())
    }

    /// Serialize lifecycle work for one repository without blocking unrelated
    /// projects. Callers must acquire this before `operation_guard`.
    pub(crate) fn repository_guard(&self, common: &str) -> Result<RepositoryGuard<'_>, String> {
        if common.is_empty() {
            return Err("Repository identity is required".into());
        }
        let mut active = self
            .repositories
            .active
            .lock()
            .map_err(|error| error.to_string())?;
        while active.contains(common) {
            active = self
                .repositories
                .changed
                .wait(active)
                .map_err(|error| error.to_string())?;
        }
        active.insert(common.to_string());
        Ok(RepositoryGuard {
            reservations: &self.repositories,
            key: common.to_string(),
        })
    }

    /// Acquire multiple repositories in stable order for multi-path window
    /// lease updates. No other path should hold more than one reservation.
    fn repository_guards(&self, keys: Vec<String>) -> Result<Vec<RepositoryGuard<'_>>, String> {
        let mut keys = keys;
        keys.sort();
        keys.dedup();
        keys.into_iter()
            .map(|key| self.repository_guard(&key))
            .collect()
    }
}

fn protected_windows(
    app: &AppHandle,
    windows: &HashMap<String, Vec<PathBuf>>,
) -> HashMap<String, Vec<PathBuf>> {
    let mut result: HashMap<String, Vec<PathBuf>> = windows
        .iter()
        .filter(|(label, _)| app.get_webview_window(label).is_some())
        .map(|(label, paths)| (label.clone(), paths.clone()))
        .collect();
    result.insert(
        "$running-processes".into(),
        [
            app.state::<crate::harness::HarnessHost>().active_workdirs(),
            app.state::<crate::pty::PtyHost>().active_workdirs(),
            setup::active_paths(),
        ]
        .concat(),
    );
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSettings {
    pub isolate_by_default: bool,
    #[serde(flatten)]
    pub environment: environment::EnvironmentSettings,
}

impl Default for WorktreeSettings {
    fn default() -> Self {
        Self {
            isolate_by_default: true,
            environment: environment::EnvironmentSettings::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct Owned {
    id: String,
    repo: String,
    common: String,
    path: String,
    branch: String,
    base_ref: String,
    pinned: bool,
    last_used: i64,
    removed: bool,
    creation_oid: Option<String>,
    active_retirement_plan_id: Option<String>,
    pending_retirement_plan_id: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalRecovery {
    plan_id: String,
    commit_oid: String,
    recovery_ref: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEntry {
    pub id: Option<String>,
    pub path: String,
    pub project_cwd: Option<String>,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub main: bool,
    pub pinned: bool,
    pub missing: bool,
    pub last_used: Option<i64>,
    pub blocked_reason: Option<String>,
    /// A removed checkout or interrupted retirement can still have branch
    /// steps to retry. Settings uses this to keep the record discoverable.
    pub retirement_pending: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeOverview {
    pub repo: String,
    pub project_cwd: String,
    pub settings: WorktreeSettings,
    pub entries: Vec<WorktreeEntry>,
}

#[cfg(test)]
#[derive(Default)]
struct CleanupReport {
    removed: Vec<String>,
    skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementPlan {
    pub plan_id: String,
    pub entries: Vec<WorktreeRetirementEntry>,
    pub kept: Vec<WorktreeRetirementKept>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementEntry {
    pub id: String,
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub worktree_removed: bool,
    pub blocked_reason: Option<String>,
    pub local_branch: RetirementLocalBranch,
    pub remote_branch: Option<RetirementRemoteBranch>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetirementLocalBranch {
    pub name: String,
    pub allowed: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetirementRemoteBranch {
    /// Short branch name, without refs/heads/.
    pub name: String,
    /// Exact configured Git remote alias.
    pub remote: String,
    /// Configured remote URL with credentials and query data removed.
    pub destination: String,
    pub allowed: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementKept {
    pub id: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementSelection {
    pub id: String,
    pub delete_local_branch: bool,
    pub delete_remote_branch: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementReport {
    pub results: Vec<WorktreeRetirementResult>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRetirementResult {
    pub id: String,
    pub path: String,
    pub worktree_removed: bool,
    pub local_branch_deleted: bool,
    pub remote_branch_deleted: bool,
    pub recovery_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct RetirementSnapshot {
    plan_id: String,
    id: String,
    repo: String,
    common: String,
    path: String,
    branch: String,
    base_ref: String,
    commit_oid: String,
    base_oid: String,
    recovery_ref: String,
    checkout_present: bool,
    local_allowed: bool,
    local_reason: Option<String>,
    remote: Option<String>,
    remote_branch: Option<String>,
    remote_destination: Option<String>,
    remote_fingerprint: Option<String>,
    remote_allowed: bool,
    remote_reason: Option<String>,
    remote_expected_oid: Option<String>,
    worktree_removed: bool,
    local_deleted: bool,
    remote_deleted: bool,
    local_requested: bool,
    remote_requested: bool,
}

#[derive(Debug)]
struct Checkout {
    path: String,
    branch: Option<String>,
    locked: bool,
    prunable: bool,
}

fn path_to_js(path: &Path) -> String {
    let path = fs_path_to_js(path);
    if cfg!(windows) {
        if let Some(unc) = path.strip_prefix("//?/UNC/") {
            return format!("//{unc}");
        }
        if let Some(path) = path.strip_prefix("//?/") {
            return path.to_string();
        }
    }
    path
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_worktrees (
            id TEXT PRIMARY KEY, repo TEXT NOT NULL, common_dir TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE, branch TEXT NOT NULL, base_ref TEXT NOT NULL,
            pinned INTEGER NOT NULL DEFAULT 0, last_used INTEGER NOT NULL,
            removed INTEGER NOT NULL DEFAULT 0, creation_oid TEXT,
            active_retirement_plan_id TEXT, pending_retirement_plan_id TEXT
         );
         CREATE TABLE IF NOT EXISTS worktree_settings (
            common_dir TEXT PRIMARY KEY, isolate_by_default INTEGER NOT NULL,
            auto_cleanup INTEGER NOT NULL, retention_days INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS worktree_project_settings (
            common_dir TEXT NOT NULL, project_path TEXT NOT NULL,
            isolate_by_default INTEGER NOT NULL, retention_days INTEGER NOT NULL,
            PRIMARY KEY (common_dir, project_path)
         );
         CREATE TABLE IF NOT EXISTS worktree_retirement_plans (
            plan_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS worktree_retirement_items (
            plan_id TEXT NOT NULL, worktree_id TEXT NOT NULL,
            repo TEXT NOT NULL, common_dir TEXT NOT NULL, path TEXT NOT NULL,
            branch TEXT NOT NULL, base_ref TEXT NOT NULL,
            commit_oid TEXT NOT NULL, base_oid TEXT NOT NULL,
            recovery_ref TEXT NOT NULL, checkout_present INTEGER NOT NULL,
            local_allowed INTEGER NOT NULL, local_reason TEXT,
            remote_name TEXT, remote_branch TEXT, remote_destination TEXT,
            remote_fingerprint TEXT,
            remote_allowed INTEGER NOT NULL DEFAULT 0, remote_reason TEXT,
            remote_expected_oid TEXT,
            worktree_removed INTEGER NOT NULL DEFAULT 0,
            local_deleted INTEGER NOT NULL DEFAULT 0,
            remote_deleted INTEGER NOT NULL DEFAULT 0,
            local_requested INTEGER NOT NULL DEFAULT 0,
            remote_requested INTEGER NOT NULL DEFAULT 0,
            last_error TEXT, updated_at INTEGER NOT NULL,
            PRIMARY KEY (plan_id, worktree_id),
            FOREIGN KEY (plan_id) REFERENCES worktree_retirement_plans(plan_id)
         );
         CREATE TABLE IF NOT EXISTS worktree_retirement_journal (
            plan_id TEXT NOT NULL, worktree_id TEXT NOT NULL,
            step TEXT NOT NULL, status TEXT NOT NULL, error TEXT,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (plan_id, worktree_id, step)
         );
         CREATE TABLE IF NOT EXISTS worktree_recoveries (
            plan_id TEXT NOT NULL, worktree_id TEXT NOT NULL,
            kind TEXT NOT NULL, repo TEXT NOT NULL, path TEXT NOT NULL,
            branch TEXT NOT NULL, commit_oid TEXT NOT NULL,
            recovery_ref TEXT NOT NULL, created_at INTEGER NOT NULL,
            PRIMARY KEY (plan_id, worktree_id, kind)
         );
         CREATE INDEX IF NOT EXISTS worktree_recoveries_latest
           ON worktree_recoveries(worktree_id, created_at DESC);",
    )?;
    let has_fingerprint: bool = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM pragma_table_info('worktree_retirement_items')
            WHERE name = 'remote_fingerprint')",
        [],
        |row| row.get(0),
    )?;
    if !has_fingerprint {
        conn.execute(
            "ALTER TABLE worktree_retirement_items ADD COLUMN remote_fingerprint TEXT",
            [],
        )?;
    }
    let has_legacy_url: bool = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM pragma_table_info('worktree_retirement_items')
            WHERE name = 'remote_url')",
        [],
        |row| row.get(0),
    )?;
    if has_legacy_url {
        // A short-lived development schema stored raw URLs. Scrub it on open;
        // old plans are intentionally no longer executable without re-review.
        conn.execute("UPDATE worktree_retirement_items SET remote_url = NULL", [])?;
    }
    ensure_managed_column(conn, "creation_oid", "TEXT")?;
    ensure_managed_column(conn, "active_retirement_plan_id", "TEXT")?;
    ensure_managed_column(conn, "pending_retirement_plan_id", "TEXT")?;
    environment::schema(conn)?;
    // Older databases selected recovery rows by mutable insertion/update time.
    // Freeze the newest reviewed removal with its recovery and, for
    // environment-aware reviews, its matching configuration archive. A later
    // retry of an older plan must not outrank the plan reviewed after it. Plans
    // predating environment reviews stay valid.
    conn.execute(
        "UPDATE managed_worktrees
            SET active_retirement_plan_id = (
                SELECT item.plan_id
                  FROM worktree_retirement_items item
                  JOIN worktree_retirement_plans plan ON plan.plan_id = item.plan_id
                  JOIN worktree_recoveries recovery
                    ON recovery.plan_id = item.plan_id
                   AND recovery.worktree_id = item.worktree_id
                   AND recovery.kind = 'local'
                  LEFT JOIN worktree_environment_archives archive
                    ON archive.plan_id = item.plan_id
                   AND archive.worktree_id = item.worktree_id
                  LEFT JOIN worktree_environment_reviews review
                    ON review.plan_id = item.plan_id
                   AND review.worktree_id = item.worktree_id
                 WHERE item.worktree_id = managed_worktrees.id
                   AND item.worktree_removed = 1
                   AND (archive.archive_id IS NOT NULL OR review.plan_id IS NULL)
                 ORDER BY plan.created_at DESC, item.plan_id DESC
                 LIMIT 1
            )
          WHERE removed = 1 AND active_retirement_plan_id IS NULL",
        [],
    )?;
    naming::schema(conn)?;
    Ok(())
}

fn ensure_managed_column(
    conn: &Connection,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM pragma_table_info('managed_worktrees') WHERE name = ?1
         )",
        [column],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute(
            &format!("ALTER TABLE managed_worktrees ADD COLUMN {column} {declaration}"),
            [],
        )?;
    }
    Ok(())
}

pub fn init(app: &AppHandle) -> Result<(), String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("worktrees");
    app.manage(WorktreeHost {
        root,
        windows: Mutex::new(HashMap::new()),
        repositories: RepositoryReservations::default(),
    });
    environment::reset_interrupted(&app.state::<SessionStore>().open_auxiliary_conn()?)?;
    naming::recover(app, &app.state::<SessionStore>().open_auxiliary_conn()?)?;
    storage_maintenance::schedule(app);
    // Cleanup is only invoked with the IDs the user reviewed and confirmed.
    Ok(())
}

fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    crate::hide_window_console(&mut cmd);
    cmd.arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    git_command(root, args)
        .output()
        .map_err(|e| format!("Could not run Git: {e}"))
}

fn git_remote_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    remote_git::output(git_command(root, args), Duration::from_secs(10))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
        .map_err(|_| "Git returned a path that is not valid UTF-8".into())
}

fn checkouts(root: &Path) -> Result<Vec<Checkout>, String> {
    let raw = git(root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut result = Vec::new();
    let mut current: Option<Checkout> = None;
    for field in raw.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                result.push(entry);
            }
            current = Some(Checkout {
                path: path_to_js(Path::new(path)),
                branch: None,
                locked: false,
                prunable: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.into());
            }
            if field == "locked" || field.starts_with("locked ") {
                entry.locked = true;
            }
            if field == "prunable" || field.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }
    if let Some(entry) = current {
        result.push(entry);
    }
    Ok(result)
}

fn repository(cwd: &str) -> Result<(String, String), String> {
    let root = expand_home(cwd);
    let common = git(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = std::fs::canonicalize(common).map_err(|e| e.to_string())?;
    let all = checkouts(&root)?;
    let main = all.first().ok_or("No Git checkout found")?;
    Ok((main.path.clone(), path_to_js(&common)))
}

fn resolve_commit(root: &Path, reference: &str) -> Result<String, String> {
    if reference.is_empty() || reference.starts_with('-') || reference.contains(['\0', '\n', '\r'])
    {
        return Err("Choose a valid base branch, tag or commit".into());
    }
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
}

fn settings(conn: &Connection, cwd: &str) -> Result<WorktreeSettings, String> {
    let scope = environment::scope_for_cwd(cwd)?;
    let decode = |row: &rusqlite::Row<'_>| {
        Ok(WorktreeSettings {
            isolate_by_default: row.get(0)?,
            environment: environment::EnvironmentSettings::default(),
        })
    };
    let current = conn
        .query_row(
            "SELECT isolate_by_default FROM worktree_project_settings
          WHERE common_dir = ?1 AND project_path = ?2",
            params![scope.common, scope.relative],
            decode,
        )
        .optional()
        .map_err(|error| error.to_string())?;
    // Legacy preferences belonged to the repository root. A nested project
    // starts independently and never inherits another project's policy.
    let mut settings = match current {
        Some(settings) => settings,
        None if scope.relative.is_empty() => conn
            .query_row(
                "SELECT isolate_by_default FROM worktree_settings WHERE common_dir = ?1",
                [&scope.common],
                decode,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or_default(),
        None => WorktreeSettings::default(),
    };
    settings.environment = environment::load_settings(conn, &scope)?;
    Ok(settings)
}

fn owned(conn: &Connection) -> Result<Vec<Owned>, String> {
    let mut stmt = conn.prepare("SELECT id, repo, common_dir, path, branch, base_ref, pinned, last_used, removed, creation_oid, active_retirement_plan_id, pending_retirement_plan_id FROM managed_worktrees ORDER BY last_used DESC").map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Owned {
                id: row.get(0)?,
                repo: row.get(1)?,
                common: row.get(2)?,
                path: row.get(3)?,
                branch: row.get(4)?,
                base_ref: row.get(5)?,
                pinned: row.get(6)?,
                last_used: row.get(7)?,
                removed: row.get(8)?,
                creation_oid: row.get(9)?,
                active_retirement_plan_id: row.get(10)?,
                pending_retirement_plan_id: row.get(11)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

pub(crate) fn managed_repository_for_path(
    conn: &Connection,
    path: &str,
) -> Result<Option<String>, String> {
    let path = expand_home(path);
    Ok(owned(conn)?
        .into_iter()
        .filter(|entry| path_inside(&path, Path::new(&entry.path)))
        .max_by_key(|entry| entry.path.len())
        .map(|entry| entry.common))
}

fn repository_common(cwd: &str) -> Result<String, String> {
    repository(cwd).map(|(_, common)| common)
}

fn path_inside(path: &Path, parent: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    // Windows canonicalization adds a verbatim prefix only to existing paths.
    // Missing descendants must still match their ordinary stored checkout path.
    #[cfg(windows)]
    let path = PathBuf::from(path_to_js(&path));
    #[cfg(windows)]
    let parent = PathBuf::from(path_to_js(&parent));
    path.starts_with(parent)
}

fn blocked(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
) -> Result<Option<String>, String> {
    if entry.pinned {
        return Ok(Some("Pinned".into()));
    }
    let root = Path::new(&entry.path);
    if windows
        .values()
        .flatten()
        .any(|path| path_inside(path, root))
    {
        return Ok(Some(
            "Open in a window (session, editor or terminal)".into(),
        ));
    }
    let mut statement = conn
        .prepare(
            "SELECT id, COALESCE(worktree_cwd, cwd) FROM sessions WHERE archived = 0 OR pinned = 1",
        )
        .map_err(|e| e.to_string())?;
    let references = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    for reference in references {
        let (id, path) = reference.map_err(|e| e.to_string())?;
        if id == entry.id || path_inside(&expand_home(&path), root) {
            return Ok(Some(
                "Archive its conversations and unpin them first".into(),
            ));
        }
    }
    if !root.exists() {
        return Ok(Some(
            "Checkout missing; open to restore its preserved branch".into(),
        ));
    }
    let (_, common) = repository(&entry.path)?;
    if common != entry.common {
        return Ok(Some("Repository identity changed".into()));
    }
    let all = checkouts(root)?;
    if all.first().is_some_and(|v| v.path == entry.path) {
        return Ok(Some("Primary checkout".into()));
    }
    let Some(checkout) = all.iter().find(|v| v.path == entry.path) else {
        return Ok(Some("Not registered with Git".into()));
    };
    if checkout.locked || checkout.prunable {
        return Ok(Some("Git worktree is locked or prunable".into()));
    }
    if checkout.branch.as_deref() != Some(&entry.branch) {
        return Ok(Some("Branch changed or HEAD is detached".into()));
    }
    environment::check_cleanup(conn, entry)
}

fn optional_git(root: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let output = git_output(root, args)?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map(|value| Some(value.trim_end_matches(['\r', '\n']).to_string()))
            .map_err(|_| "Git returned text that is not valid UTF-8".into());
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

fn ref_oid(root: &Path, reference: &str) -> Result<Option<String>, String> {
    optional_git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            reference,
        ],
    )
}

fn direct_ref_oid(root: &Path, reference: &str) -> Result<Option<String>, String> {
    if optional_git(root, &["symbolic-ref", "-q", reference])?.is_some() {
        return Err("Recovery ref is symbolic".into());
    }
    ref_oid(root, reference)
}

fn is_ancestor(root: &Path, commit: &str, base: &str) -> Result<bool, String> {
    let output = git_output(root, &["merge-base", "--is-ancestor", commit, base])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
    }
}

fn is_owned_branch(entry: &Owned) -> bool {
    entry.branch.starts_with("monocode/")
        && entry.branch.len() <= 240
        && !entry.branch.contains(['\0', '\n', '\r'])
}

fn protected_branch_name(branch: &str) -> bool {
    matches!(
        branch.to_ascii_lowercase().as_str(),
        "main"
            | "master"
            | "trunk"
            | "develop"
            | "development"
            | "production"
            | "stable"
            | "release"
    )
}

fn active_use_reason(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
) -> Result<Option<String>, String> {
    if entry.pinned {
        return Ok(Some("Pinned".into()));
    }
    let root = Path::new(&entry.path);
    if windows
        .values()
        .flatten()
        .any(|path| path_inside(path, root))
    {
        return Ok(Some(
            "Open in a window (session, editor or terminal)".into(),
        ));
    }
    let mut statement = conn
        .prepare(
            "SELECT id, COALESCE(worktree_cwd, cwd), archived, pinned
               FROM sessions WHERE archived = 0 OR pinned = 1",
        )
        .map_err(|e| e.to_string())?;
    let references = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for reference in references {
        let (id, path, _archived, pinned) = reference.map_err(|e| e.to_string())?;
        if id == entry.id || path_inside(&expand_home(&path), root) {
            return Ok(Some(if pinned {
                format!("Conversation {id} is pinned")
            } else {
                format!("Conversation {id} is still active")
            }));
        }
    }
    Ok(None)
}

fn has_unarchived_session(conn: &Connection, entry: &Owned) -> Result<bool, String> {
    let mut statement = conn
        .prepare("SELECT id, COALESCE(worktree_cwd, cwd) FROM sessions WHERE archived = 0")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, path) = row.map_err(|e| e.to_string())?;
        if id == entry.id || path_inside(&expand_home(&path), Path::new(&entry.path)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn latest_local_recovery(
    conn: &Connection,
    worktree_id: &str,
) -> Result<Option<LocalRecovery>, String> {
    let active_plan = conn
        .query_row(
            "SELECT active_retirement_plan_id FROM managed_worktrees WHERE id = ?1",
            [worktree_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    if let Some(plan_id) = active_plan {
        return local_recovery_for_plan(conn, worktree_id, &plan_id)?
            .ok_or_else(|| "The active retirement recovery record is missing".into())
            .map(Some);
    }
    // Legacy rows have no active identity. Only a retirement whose removal was
    // durably completed may supply recovery; newer failed plans are excluded.
    conn.query_row(
        "SELECT recovery.plan_id, recovery.commit_oid, recovery.recovery_ref
           FROM worktree_recoveries recovery
           JOIN worktree_retirement_items item
             ON item.plan_id = recovery.plan_id
            AND item.worktree_id = recovery.worktree_id
           JOIN worktree_retirement_plans plan ON plan.plan_id = item.plan_id
           LEFT JOIN worktree_environment_archives archive
             ON archive.plan_id = item.plan_id
            AND archive.worktree_id = item.worktree_id
           LEFT JOIN worktree_environment_reviews review
             ON review.plan_id = item.plan_id
            AND review.worktree_id = item.worktree_id
          WHERE recovery.worktree_id = ?1 AND recovery.kind = 'local'
            AND item.worktree_removed = 1
            AND (archive.archive_id IS NOT NULL OR review.plan_id IS NULL)
          ORDER BY plan.created_at DESC, item.plan_id DESC LIMIT 1",
        [worktree_id],
        |row| {
            Ok(LocalRecovery {
                plan_id: row.get(0)?,
                commit_oid: row.get(1)?,
                recovery_ref: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn local_recovery_for_plan(
    conn: &Connection,
    worktree_id: &str,
    plan_id: &str,
) -> Result<Option<LocalRecovery>, String> {
    conn.query_row(
        "SELECT plan_id, commit_oid, recovery_ref
           FROM worktree_recoveries
          WHERE worktree_id = ?1 AND plan_id = ?2 AND kind = 'local'",
        params![worktree_id, plan_id],
        |row| {
            Ok(LocalRecovery {
                plan_id: row.get(0)?,
                commit_oid: row.get(1)?,
                recovery_ref: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn retirement_ref(plan_id: &str, worktree_id: &str, kind: &str) -> String {
    format!("refs/monocode/recovery/{worktree_id}/{plan_id}/{kind}")
}

fn creation_ref(worktree_id: &str, commit_oid: &str) -> String {
    format!("refs/monocode/creation/{worktree_id}/{commit_oid}")
}

fn ensure_creation_ref(root: &Path, worktree_id: &str, commit_oid: &str) -> Result<String, String> {
    let reference = creation_ref(worktree_id, commit_oid);
    git(root, &["check-ref-format", &reference])
        .map_err(|error| format!("Invalid creation recovery ref: {error}"))?;
    if optional_git(root, &["symbolic-ref", "-q", &reference])?.is_some() {
        return Err("Creation recovery ref is symbolic".into());
    }
    match direct_ref_oid(root, &reference)? {
        Some(existing) if existing == commit_oid => {}
        Some(_) => return Err("Creation recovery ref points to different work".into()),
        None => {
            git(
                root,
                &["update-ref", "--no-deref", &reference, commit_oid, ""],
            )
            .map_err(|error| format!("Could not create creation recovery ref: {error}"))?;
        }
    }
    if direct_ref_oid(root, &reference)?.as_deref() != Some(commit_oid) {
        return Err("Could not verify the creation recovery ref".into());
    }
    Ok(reference)
}

fn plan_id() -> String {
    format!(
        "retire-{}-{}-{}",
        now(),
        std::process::id(),
        RETIREMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn safe_remote_destination(value: &str) -> String {
    let without_query = value.split(['?', '#']).next().unwrap_or(value).trim();
    if let Some((scheme, rest)) = without_query.split_once("://") {
        let rest = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
        return format!("{scheme}://{rest}");
    }
    if let Some((identity, host_path)) = without_query.split_once('@') {
        if !identity.contains('/') && host_path.contains(':') {
            return host_path.to_string();
        }
    }
    without_query.to_string()
}

fn remote_fingerprint(root: &Path, value: &str) -> Result<String, String> {
    let mut command = git_command(root, &["hash-object", "--stdin"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not fingerprint remote destination: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("Could not open Git fingerprint input")?
        .write_all(value.as_bytes())
        .map_err(|e| format!("Could not fingerprint remote destination: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Could not fingerprint remote destination: {e}"))?;
    if !output.status.success() {
        return Err("Could not fingerprint remote destination".into());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "Git returned an invalid remote fingerprint".into())
}

fn remote_push_url(root: &Path, remote: &str) -> Result<String, String> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains(['\0', '\n', '\r']) {
        return Err("Configured remote name is invalid".into());
    }
    let output = git_output(root, &["remote", "get-url", "--push", "--all", remote])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned remote data that is not valid UTF-8")?;
    let urls: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    if urls.len() != 1 {
        return Err("Remote must have exactly one push destination".into());
    }
    Ok(urls[0].to_string())
}

fn remote_oid(root: &Path, target: &str, branch: &str) -> Result<Option<String>, String> {
    if target.is_empty()
        || target.starts_with('-')
        || target.contains(['\0', '\n', '\r'])
        || branch.is_empty()
        || branch.starts_with('-')
        || branch.contains(['\0', '\n', '\r'])
    {
        return Err("Configured remote tracking is invalid".into());
    }
    let full_ref = format!("refs/heads/{branch}");
    let output = git_remote_output(root, &["ls-remote", "--heads", target, &full_ref])?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr)
            .trim()
            .replace(target, &safe_remote_destination(target));
        return Err(if error.is_empty() {
            format!(
                "Could not contact remote {}",
                safe_remote_destination(target)
            )
        } else {
            error
        });
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned remote data that is not valid UTF-8")?;
    let mut matches = stdout.lines().filter_map(|line| {
        let (oid, reference) = line.split_once('\t')?;
        (reference == full_ref).then(|| oid.to_string())
    });
    let result = matches.next();
    if matches.next().is_some() {
        return Err("Remote returned an ambiguous branch".into());
    }
    Ok(result)
}

fn remote_default_branch(root: &Path, target: &str) -> Result<Option<String>, String> {
    let output = git_remote_output(root, &["ls-remote", "--symref", target, "HEAD"])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .replace(target, &safe_remote_destination(target)));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned remote data that is not valid UTF-8")?;
    Ok(stdout.lines().find_map(|line| {
        let value = line.strip_prefix("ref: refs/heads/")?;
        value.strip_suffix("\tHEAD").map(str::to_string)
    }))
}

struct TrackingRemote {
    name: String,
    branch: String,
    destination: String,
    url: String,
    allowed: bool,
    reason: Option<String>,
    expected_oid: Option<String>,
}

fn tracking_remote(
    root: &Path,
    branch: &str,
    target_oid: Option<&str>,
    target_reason: Option<&str>,
) -> Option<TrackingRemote> {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let remote = match optional_git(root, &["config", "--get", &remote_key]) {
        Ok(Some(remote)) if !remote.is_empty() && remote != "." => remote,
        _ => return None,
    };
    let configured_merge = match optional_git(root, &["config", "--get", &merge_key]) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Some(TrackingRemote {
                name: remote,
                branch: branch.into(),
                destination: String::new(),
                url: String::new(),
                allowed: false,
                reason: Some("Tracking branch is not configured".into()),
                expected_oid: None,
            })
        }
        Err(error) => {
            return Some(TrackingRemote {
                name: remote,
                branch: branch.into(),
                destination: String::new(),
                url: String::new(),
                allowed: false,
                reason: Some(error),
                expected_oid: None,
            })
        }
    };
    let Some(remote_branch) = configured_merge.strip_prefix("refs/heads/") else {
        return Some(TrackingRemote {
            name: remote,
            branch: branch.into(),
            destination: String::new(),
            url: String::new(),
            allowed: false,
            reason: Some("Tracking destination is not a branch".into()),
            expected_oid: None,
        });
    };
    let remote_url = match remote_push_url(root, &remote) {
        Ok(value) => value,
        Err(error) => {
            return Some(TrackingRemote {
                name: remote,
                branch: remote_branch.into(),
                destination: String::new(),
                url: String::new(),
                allowed: false,
                reason: Some(error),
                expected_oid: None,
            })
        }
    };
    let destination = safe_remote_destination(&remote_url);
    if remote_branch != branch {
        return Some(TrackingRemote {
            name: remote,
            branch: remote_branch.into(),
            destination,
            url: remote_url,
            allowed: false,
            reason: Some("Tracking destination does not match the owned branch".into()),
            expected_oid: None,
        });
    }
    match remote_oid(root, &remote_url, remote_branch) {
        Ok(Some(oid)) => {
            let (default_known, is_default, default_reason) =
                match remote_default_branch(root, &remote_url) {
                    Ok(Some(default)) => (true, default == remote_branch, None),
                    Ok(None) => (
                        false,
                        false,
                        Some("Remote default branch could not be determined".into()),
                    ),
                    Err(error) => (
                        false,
                        false,
                        Some(format!(
                            "Remote default branch could not be verified: {error}"
                        )),
                    ),
                };
            let fetched = worktree_merge::fetch_remote_ref(
                root,
                &remote_url,
                &format!("refs/heads/{remote_branch}"),
            );
            let (integrated, integration_error) = match (&fetched, target_oid) {
                (Ok(fetched), Some(target)) if fetched == &oid => {
                    match worktree_merge::is_integrated(root, &oid, target) {
                        Ok(integrated) => (integrated, None),
                        Err(error) => (false, Some(error)),
                    }
                }
                (Ok(_), _) if target_oid.is_none() => (false, None),
                (Ok(_), _) => (
                    false,
                    Some("Remote branch moved while integration was reviewed".into()),
                ),
                (Err(_), _) => (false, None),
            };
            let allowed =
                integrated && is_owned_branch_name(remote_branch) && default_known && !is_default;
            let reason = (!allowed).then(|| {
                if is_default {
                    "Remote branch is the remote default branch".into()
                } else if let Some(reason) = default_reason {
                    reason
                } else if let Err(error) = fetched {
                    format!("Remote branch integration could not be verified: {error}")
                } else if let Some(error) = integration_error {
                    format!("Remote branch integration could not be verified: {error}")
                } else if target_oid.is_none() {
                    target_reason
                        .unwrap_or("Recorded base integration could not be verified")
                        .to_string()
                } else {
                    "Remote branch tip is not confirmed integrated into the recorded base".into()
                }
            });
            Some(TrackingRemote {
                name: remote,
                branch: remote_branch.into(),
                destination,
                url: remote_url,
                allowed,
                reason,
                expected_oid: Some(oid),
            })
        }
        Ok(None) => Some(TrackingRemote {
            name: remote,
            branch: remote_branch.into(),
            destination,
            url: remote_url,
            allowed: false,
            reason: Some("Remote branch is already absent".into()),
            expected_oid: None,
        }),
        Err(error) => Some(TrackingRemote {
            name: remote,
            branch: remote_branch.into(),
            destination,
            url: remote_url,
            allowed: false,
            reason: Some(format!("Remote unavailable: {error}")),
            expected_oid: None,
        }),
    }
}

fn is_owned_branch_name(branch: &str) -> bool {
    branch.starts_with("monocode/")
        && !protected_branch_name(branch)
        && !branch.contains(['\0', '\n', '\r'])
}

fn review_retirement(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
    plan_id: &str,
) -> Result<RetirementSnapshot, String> {
    if let Some(reason) = active_use_reason(conn, windows, entry)? {
        return Err(reason);
    }
    if !is_owned_branch(entry) || protected_branch_name(&entry.branch) {
        return Err("Branch is not an app-owned retirement branch".into());
    }
    let (_, common) = repository(&entry.repo)?;
    if common != entry.common {
        return Err("Repository identity changed".into());
    }
    let all = checkouts(Path::new(&entry.repo))?;
    let primary = all.first().ok_or("No primary Git checkout found")?;
    if primary.path == entry.path {
        return Err("Primary checkout".into());
    }
    let registered = all.iter().find(|checkout| checkout.path == entry.path);
    let (commit_oid, checkout_present) = if let Some(checkout) = registered {
        if checkout.locked || checkout.prunable {
            return Err("Git worktree is locked or prunable".into());
        }
        if checkout.branch.as_deref() != Some(&entry.branch) {
            return Err("Branch changed or HEAD is detached".into());
        }
        if !Path::new(&entry.path).is_dir() {
            return Err("Registered worktree directory is missing".into());
        }
        if repository(&entry.path)?.1 != entry.common {
            return Err("Repository identity changed".into());
        }
        if let Some(reason) = environment::check_cleanup(conn, entry)? {
            return Err(reason);
        }
        let head = resolve_commit(Path::new(&entry.path), "HEAD")?;
        let branch_oid = ref_oid(
            Path::new(&entry.repo),
            &format!("refs/heads/{}", entry.branch),
        )?
        .ok_or("Owned local branch is missing")?;
        if branch_oid != head {
            return Err("Owned branch moved away from the checkout HEAD".into());
        }
        (head, true)
    } else {
        if Path::new(&entry.path).exists() {
            return Err("Worktree path is occupied but not registered with Git".into());
        }
        if !entry.removed {
            return Err("Worktree disappeared outside the retirement journal".into());
        }
        if let Some(recovery) = latest_local_recovery(conn, &entry.id)? {
            if direct_ref_oid(Path::new(&entry.repo), &recovery.recovery_ref)?.as_deref()
                != Some(&recovery.commit_oid)
            {
                return Err("Recorded recovery ref is missing or changed".into());
            }
            (recovery.commit_oid, false)
        } else if let Some(commit) = ref_oid(
            Path::new(&entry.repo),
            &format!("refs/heads/{}", entry.branch),
        )? {
            // Legacy cleanup preserved the branch but predated recovery rows.
            (commit, false)
        } else {
            return Err("Checkout is missing and has no durable recovery ref".into());
        }
    };
    environment::record_review(conn, entry, plan_id)?;
    let merge_evidence =
        worktree_merge::review(Path::new(&entry.repo), &entry.base_ref, &commit_oid);
    let (base_oid, integrated, integration_reason) = match merge_evidence {
        Ok(evidence) => (
            evidence.target_oid,
            evidence.integrated,
            (!evidence.integrated)
                .then(|| format!("Exact branch tip is not integrated into {}", entry.base_ref)),
        ),
        Err(error) => (
            String::new(),
            false,
            Some(format!("Merge integration could not be verified: {error}")),
        ),
    };
    let local_oid = ref_oid(
        Path::new(&entry.repo),
        &format!("refs/heads/{}", entry.branch),
    )?;
    let branch_used_elsewhere = all.iter().any(|checkout| {
        checkout.path != entry.path && checkout.branch.as_deref() == Some(&entry.branch)
    });
    let (local_allowed, local_reason) = match local_oid.as_deref() {
        Some(_oid) if branch_used_elsewhere => {
            (false, Some("Local branch is checked out elsewhere".into()))
        }
        Some(oid) if oid == commit_oid && integrated => (true, None),
        Some(oid) if oid == commit_oid => (false, integration_reason.clone()),
        Some(_) => (
            false,
            Some("Branch name now points to different work".into()),
        ),
        None => (false, Some("Local branch is already absent".into())),
    };
    let remote = tracking_remote(
        Path::new(&entry.repo),
        &entry.branch,
        (!base_oid.is_empty()).then_some(base_oid.as_str()),
        integration_reason.as_deref(),
    );
    let (
        remote_name,
        remote_branch,
        remote_destination,
        remote_url,
        mut remote_allowed,
        mut remote_reason,
        remote_expected_oid,
    ) = match remote {
        Some(remote) => (
            Some(remote.name),
            Some(remote.branch),
            Some(remote.destination),
            Some(remote.url),
            remote.allowed,
            remote.reason,
            remote.expected_oid,
        ),
        None => (None, None, None, None, false, None, None),
    };
    let remote_fingerprint = match remote_url.as_deref() {
        Some(url) => match remote_fingerprint(Path::new(&entry.repo), url) {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                remote_allowed = false;
                remote_reason = Some(error);
                None
            }
        },
        None => None,
    };
    Ok(RetirementSnapshot {
        plan_id: plan_id.into(),
        id: entry.id.clone(),
        repo: entry.repo.clone(),
        common: entry.common.clone(),
        path: entry.path.clone(),
        branch: entry.branch.clone(),
        base_ref: entry.base_ref.clone(),
        commit_oid,
        base_oid,
        recovery_ref: retirement_ref(plan_id, &entry.id, "local"),
        checkout_present,
        local_allowed,
        local_reason,
        remote: remote_name,
        remote_branch,
        remote_destination,
        remote_fingerprint,
        remote_allowed,
        remote_reason,
        remote_expected_oid,
        worktree_removed: entry.removed,
        local_deleted: false,
        remote_deleted: false,
        local_requested: false,
        remote_requested: false,
    })
}

fn snapshot_entry(snapshot: &RetirementSnapshot) -> WorktreeRetirementEntry {
    WorktreeRetirementEntry {
        id: snapshot.id.clone(),
        repo: snapshot.repo.clone(),
        path: snapshot.path.clone(),
        branch: snapshot.branch.clone(),
        worktree_removed: !snapshot.checkout_present || snapshot.worktree_removed,
        blocked_reason: None,
        local_branch: RetirementLocalBranch {
            name: snapshot.branch.clone(),
            allowed: snapshot.local_allowed,
            reason: snapshot.local_reason.clone(),
        },
        remote_branch: snapshot
            .remote
            .as_ref()
            .map(|remote| RetirementRemoteBranch {
                name: snapshot.remote_branch.clone().unwrap_or_default(),
                remote: remote.clone(),
                destination: snapshot
                    .remote_destination
                    .clone()
                    .unwrap_or_else(|| remote.clone()),
                allowed: snapshot.remote_allowed,
                reason: snapshot.remote_reason.clone(),
            }),
    }
}

fn persist_retirement_plan(
    conn: &Connection,
    plan_id: &str,
    snapshots: &[RetirementSnapshot],
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO worktree_retirement_plans (plan_id, created_at) VALUES (?1, ?2)",
        params![plan_id, now()],
    )
    .map_err(|e| e.to_string())?;
    for snapshot in snapshots {
        tx.execute(
            "INSERT INTO worktree_retirement_items (
               plan_id, worktree_id, repo, common_dir, path, branch, base_ref,
               commit_oid, base_oid, recovery_ref, checkout_present,
               local_allowed, local_reason, remote_name, remote_branch,
               remote_destination, remote_fingerprint, remote_allowed, remote_reason,
               remote_expected_oid, worktree_removed, local_deleted,
               remote_deleted, updated_at
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
               ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
             )",
            params![
                snapshot.plan_id,
                snapshot.id,
                snapshot.repo,
                snapshot.common,
                snapshot.path,
                snapshot.branch,
                snapshot.base_ref,
                snapshot.commit_oid,
                snapshot.base_oid,
                snapshot.recovery_ref,
                snapshot.checkout_present,
                snapshot.local_allowed,
                snapshot.local_reason,
                snapshot.remote,
                snapshot.remote_branch,
                snapshot.remote_destination,
                snapshot.remote_fingerprint,
                snapshot.remote_allowed,
                snapshot.remote_reason,
                snapshot.remote_expected_oid,
                snapshot.worktree_removed,
                snapshot.local_deleted,
                snapshot.remote_deleted,
                now(),
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn load_retirement_snapshot(
    conn: &Connection,
    plan_id: &str,
    worktree_id: &str,
) -> Result<Option<RetirementSnapshot>, String> {
    conn.query_row(
        "SELECT plan_id, worktree_id, repo, common_dir, path, branch, base_ref,
                commit_oid, base_oid, recovery_ref, checkout_present,
                local_allowed, local_reason, remote_name, remote_branch,
                remote_destination, remote_fingerprint, remote_allowed, remote_reason,
                remote_expected_oid, worktree_removed, local_deleted,
                remote_deleted, local_requested, remote_requested
           FROM worktree_retirement_items
          WHERE plan_id = ?1 AND worktree_id = ?2",
        params![plan_id, worktree_id],
        |row| {
            Ok(RetirementSnapshot {
                plan_id: row.get(0)?,
                id: row.get(1)?,
                repo: row.get(2)?,
                common: row.get(3)?,
                path: row.get(4)?,
                branch: row.get(5)?,
                base_ref: row.get(6)?,
                commit_oid: row.get(7)?,
                base_oid: row.get(8)?,
                recovery_ref: row.get(9)?,
                checkout_present: row.get(10)?,
                local_allowed: row.get(11)?,
                local_reason: row.get(12)?,
                remote: row.get(13)?,
                remote_branch: row.get(14)?,
                remote_destination: row.get(15)?,
                remote_fingerprint: row.get(16)?,
                remote_allowed: row.get(17)?,
                remote_reason: row.get(18)?,
                remote_expected_oid: row.get(19)?,
                worktree_removed: row.get(20)?,
                local_deleted: row.get(21)?,
                remote_deleted: row.get(22)?,
                local_requested: row.get(23)?,
                remote_requested: row.get(24)?,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn journal_step(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    step: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO worktree_retirement_journal
           (plan_id, worktree_id, step, status, error, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(plan_id, worktree_id, step) DO UPDATE SET
           status = excluded.status, error = excluded.error,
           updated_at = excluded.updated_at",
        params![snapshot.plan_id, snapshot.id, step, status, error, now()],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE worktree_retirement_items SET last_error = ?1, updated_at = ?2
          WHERE plan_id = ?3 AND worktree_id = ?4",
        params![error, now(), snapshot.plan_id, snapshot.id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn begin_worktree_removal(conn: &Connection, snapshot: &RetirementSnapshot) -> Result<(), String> {
    let updated_at = now();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE managed_worktrees
                SET pending_retirement_plan_id = ?1
              WHERE id = ?2 AND repo = ?3 AND common_dir = ?4
                AND path = ?5 AND branch = ?6 AND creation_oid IS NULL
                AND (pending_retirement_plan_id IS NULL
                     OR pending_retirement_plan_id = ?1)",
            params![
                snapshot.plan_id,
                snapshot.id,
                snapshot.repo,
                snapshot.common,
                snapshot.path,
                snapshot.branch
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Another retirement owns the pending worktree removal".into());
    }
    tx.execute(
        "INSERT INTO worktree_retirement_journal
           (plan_id, worktree_id, step, status, error, updated_at)
         VALUES (?1, ?2, 'worktree', 'pending', NULL, ?3)
         ON CONFLICT(plan_id, worktree_id, step) DO UPDATE SET
           status = 'pending', error = NULL, updated_at = excluded.updated_at",
        params![snapshot.plan_id, snapshot.id, updated_at],
    )
    .map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE worktree_retirement_items
                SET last_error = NULL, updated_at = ?1
              WHERE plan_id = ?2 AND worktree_id = ?3",
            params![updated_at, snapshot.plan_id, snapshot.id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("The retirement item disappeared before worktree removal".into());
    }
    tx.commit().map_err(|e| e.to_string())
}

fn sole_interrupted_removal(
    conn: &Connection,
    worktree_id: &str,
) -> Result<Option<String>, String> {
    let mut statement = conn
        .prepare(
            "SELECT plan_id
               FROM worktree_retirement_journal
              WHERE worktree_id = ?1 AND step = 'worktree' AND status = 'pending'
              ORDER BY plan_id",
        )
        .map_err(|e| e.to_string())?;
    let plans = statement
        .query_map([worktree_id], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    match plans.as_slice() {
        [] => Ok(None),
        [plan_id] => Ok(Some(plan_id.clone())),
        _ => Err("Multiple interrupted retirements require a new review".into()),
    }
}

fn claim_interrupted_removal(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
) -> Result<(), String> {
    let entry = current_owned_for_snapshot(conn, snapshot)?;
    match entry.pending_retirement_plan_id.as_deref() {
        Some(plan_id) if plan_id == snapshot.plan_id => return Ok(()),
        Some(_) => return Err("Another retirement owns the pending worktree removal".into()),
        None => {}
    }
    if sole_interrupted_removal(conn, &snapshot.id)?.as_deref() != Some(&snapshot.plan_id) {
        return Err("The interrupted worktree removal has no unique pending identity".into());
    }
    let changed = conn
        .execute(
            "UPDATE managed_worktrees
                SET pending_retirement_plan_id = ?1
              WHERE id = ?2 AND repo = ?3 AND common_dir = ?4
                AND path = ?5 AND branch = ?6
                AND pending_retirement_plan_id IS NULL",
            params![
                snapshot.plan_id,
                snapshot.id,
                snapshot.repo,
                snapshot.common,
                snapshot.path,
                snapshot.branch
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Another retirement claimed the pending worktree removal".into());
    }
    Ok(())
}

fn clear_pending_removal(conn: &Connection, snapshot: &RetirementSnapshot) -> Result<(), String> {
    conn.execute(
        "UPDATE managed_worktrees SET pending_retirement_plan_id = NULL
          WHERE id = ?1 AND pending_retirement_plan_id = ?2",
        params![snapshot.id, snapshot.plan_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn cancel_pending_removal_for_present_checkout(
    conn: &Connection,
    entry: &Owned,
) -> Result<(), String> {
    let Some(plan_id) = entry.pending_retirement_plan_id.as_deref() else {
        return Ok(());
    };
    let error = "The checkout is present; its interrupted removal was not completed";
    let updated_at = now();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO worktree_retirement_journal
           (plan_id, worktree_id, step, status, error, updated_at)
         VALUES (?1, ?2, 'worktree', 'failed', ?3, ?4)
         ON CONFLICT(plan_id, worktree_id, step) DO UPDATE SET
           status = 'failed', error = excluded.error, updated_at = excluded.updated_at",
        params![plan_id, entry.id, error, updated_at],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE worktree_retirement_items SET last_error = ?1, updated_at = ?2
          WHERE plan_id = ?3 AND worktree_id = ?4",
        params![error, updated_at, plan_id, entry.id],
    )
    .map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE managed_worktrees SET pending_retirement_plan_id = NULL
              WHERE id = ?1 AND pending_retirement_plan_id = ?2",
            params![entry.id, plan_id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Pending worktree removal changed while reopening".into());
    }
    tx.commit().map_err(|e| e.to_string())
}

fn complete_worktree_removal(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
) -> Result<(), String> {
    let entry = current_owned_for_snapshot(conn, snapshot)?;
    let authorized = match entry.pending_retirement_plan_id.as_deref() {
        Some(plan_id) => plan_id == snapshot.plan_id,
        None => {
            entry.removed && entry.active_retirement_plan_id.as_deref() == Some(&snapshot.plan_id)
        }
    };
    if !authorized {
        return Err("The retirement does not own this worktree removal".into());
    }
    let recovery = local_recovery_for_plan(conn, &snapshot.id, &snapshot.plan_id)?
        .ok_or("The retirement recovery record is missing")?;
    if recovery.commit_oid != snapshot.commit_oid
        || recovery.recovery_ref != snapshot.recovery_ref
        || direct_ref_oid(Path::new(&snapshot.repo), &recovery.recovery_ref)?.as_deref()
            != Some(&recovery.commit_oid)
    {
        return Err("The retirement recovery ref is missing or changed".into());
    }
    let (review_exists, archive_exists): (bool, bool) = conn
        .query_row(
            "SELECT
               EXISTS(SELECT 1 FROM worktree_environment_reviews
                       WHERE plan_id = ?1 AND worktree_id = ?2),
               EXISTS(SELECT 1 FROM worktree_environment_archives
                       WHERE plan_id = ?1 AND worktree_id = ?2)",
            params![snapshot.plan_id, snapshot.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if review_exists && !archive_exists {
        return Err("The retirement configuration archive is missing".into());
    }
    if !actual_worktree_removed(snapshot) {
        return Err("Git did not remove the worktree completely".into());
    }
    let updated_at = now();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE managed_worktrees
                SET removed = 1, active_retirement_plan_id = ?1,
                    pending_retirement_plan_id = NULL
              WHERE id = ?2 AND repo = ?3 AND common_dir = ?4
                AND path = ?5 AND branch = ?6
                AND (pending_retirement_plan_id = ?1
                     OR (pending_retirement_plan_id IS NULL AND removed = 1
                         AND active_retirement_plan_id = ?1))",
            params![
                snapshot.plan_id,
                snapshot.id,
                snapshot.repo,
                snapshot.common,
                snapshot.path,
                snapshot.branch
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Worktree ownership changed while completing retirement".into());
    }
    let changed = tx
        .execute(
            "UPDATE worktree_retirement_items
                SET worktree_removed = 1, last_error = NULL, updated_at = ?1
              WHERE plan_id = ?2 AND worktree_id = ?3",
            params![updated_at, snapshot.plan_id, snapshot.id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("The retirement item disappeared while completing removal".into());
    }
    tx.execute(
        "INSERT INTO worktree_retirement_journal
           (plan_id, worktree_id, step, status, error, updated_at)
         VALUES (?1, ?2, 'worktree', 'complete', NULL, ?3)
         ON CONFLICT(plan_id, worktree_id, step) DO UPDATE SET
           status = 'complete', error = NULL, updated_at = excluded.updated_at",
        params![snapshot.plan_id, snapshot.id, updated_at],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

fn set_retirement_flag(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    column: &str,
) -> Result<(), String> {
    let allowed = matches!(
        column,
        "worktree_removed" | "local_deleted" | "remote_deleted"
    );
    if !allowed {
        return Err("Invalid retirement journal field".into());
    }
    conn.execute(
        &format!(
            "UPDATE worktree_retirement_items SET {column} = 1, last_error = NULL,
             updated_at = ?1 WHERE plan_id = ?2 AND worktree_id = ?3"
        ),
        params![now(), snapshot.plan_id, snapshot.id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn selected_retirement_records(
    conn: &Connection,
    session_ids: &[String],
    cwd: Option<&str>,
    ids: &[String],
) -> Result<(Vec<Owned>, Vec<WorktreeRetirementKept>), String> {
    let records = owned(conn)?;
    let mut selected = Vec::new();
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    if !session_ids.is_empty() {
        for session_id in session_ids.iter().collect::<HashSet<_>>() {
            if session_id.is_empty() || session_id.contains(['\0', '\n', '\r']) {
                kept.push(WorktreeRetirementKept {
                    id: session_id.clone(),
                    path: String::new(),
                    reason: "Invalid conversation identity".into(),
                });
                continue;
            }
            let session: Option<(String, bool, bool)> = conn
                .query_row(
                    "SELECT COALESCE(worktree_cwd, cwd), archived, pinned
                       FROM sessions WHERE id = ?1",
                    [session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let Some((path, archived, pinned)) = session else {
                kept.push(WorktreeRetirementKept {
                    id: session_id.clone(),
                    path: String::new(),
                    reason: "Archived conversation record was not found".into(),
                });
                continue;
            };
            if !archived || pinned {
                kept.push(WorktreeRetirementKept {
                    id: session_id.clone(),
                    path,
                    reason: if pinned {
                        "Conversation is pinned".into()
                    } else {
                        "Conversation is not archived".into()
                    },
                });
                continue;
            }
            let matches: Vec<&Owned> = records
                .iter()
                .filter(|entry| {
                    entry.id == **session_id
                        || path_inside(&expand_home(&path), Path::new(&entry.path))
                })
                .collect();
            if matches.is_empty() {
                // Most conversations use the primary checkout and have no
                // app-owned worktree. Archiving them has no cleanup follow-up.
                continue;
            }
            if matches.len() != 1 {
                kept.push(WorktreeRetirementKept {
                    id: session_id.clone(),
                    path,
                    reason: "Conversation worktree ownership is ambiguous".into(),
                });
                continue;
            }
            let entry = matches[0];
            if seen.insert(entry.id.clone()) {
                selected.push(entry.clone());
            }
        }
        return Ok((selected, kept));
    }

    let cwd = cwd.ok_or("cwd is required when planning selected worktrees")?;
    let (_, common) = repository(cwd)?;
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    for id in &wanted {
        if !records
            .iter()
            .any(|entry| entry.common == common && entry.id == *id)
        {
            kept.push(WorktreeRetirementKept {
                id: (*id).into(),
                path: String::new(),
                reason: "App-owned worktree record was not found".into(),
            });
        }
    }
    for entry in records {
        if entry.common == common
            && wanted.contains(entry.id.as_str())
            && seen.insert(entry.id.clone())
        {
            selected.push(entry);
        }
    }
    Ok((selected, kept))
}

#[cfg(test)]
fn build_retirement_plan(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    session_ids: &[String],
    cwd: Option<&str>,
    ids: &[String],
) -> Result<WorktreeRetirementPlan, String> {
    build_retirement_plan_inner(conn, None, windows, session_ids, cwd, ids)
}

fn build_retirement_plan_coordinated(
    conn: &Connection,
    host: &WorktreeHost,
    windows: &HashMap<String, Vec<PathBuf>>,
    session_ids: &[String],
    cwd: Option<&str>,
    ids: &[String],
) -> Result<WorktreeRetirementPlan, String> {
    build_retirement_plan_inner(conn, Some(host), windows, session_ids, cwd, ids)
}

fn build_retirement_plan_inner(
    conn: &Connection,
    host: Option<&WorktreeHost>,
    windows: &HashMap<String, Vec<PathBuf>>,
    session_ids: &[String],
    cwd: Option<&str>,
    ids: &[String],
) -> Result<WorktreeRetirementPlan, String> {
    if session_ids.is_empty() && ids.is_empty() {
        return Err("Choose at least one conversation or worktree".into());
    }
    let plan_id = plan_id();
    let (records, mut kept) = selected_retirement_records(conn, session_ids, cwd, ids)?;
    let mut snapshots = Vec::new();
    for entry in records {
        let _repository = host
            .map(|host| host.repository_guard(&entry.common))
            .transpose()?;
        if !session_ids.is_empty() && has_unarchived_session(conn, &entry)? {
            // Archiving one of several conversations sharing a checkout is
            // routine. Leave the checkout alone and do not surface cleanup.
            continue;
        }
        match review_retirement(conn, windows, &entry, &plan_id) {
            Ok(snapshot) => {
                let no_checkout = !snapshot.checkout_present;
                let no_local = !snapshot.local_allowed;
                let no_remote = snapshot.remote.is_none() || !snapshot.remote_allowed;
                if no_checkout && no_local && no_remote {
                    kept.push(WorktreeRetirementKept {
                        id: entry.id,
                        path: entry.path,
                        reason: "Checkout and tracked branches are already retired".into(),
                    });
                } else {
                    snapshots.push(snapshot);
                }
            }
            Err(reason) => kept.push(WorktreeRetirementKept {
                id: entry.id,
                path: entry.path,
                reason,
            }),
        }
    }
    persist_retirement_plan(conn, &plan_id, &snapshots)?;
    let entries = snapshots.iter().map(snapshot_entry).collect();
    Ok(WorktreeRetirementPlan {
        plan_id,
        entries,
        kept,
    })
}

fn retirement_step_status(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    step: &str,
) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT status FROM worktree_retirement_journal
          WHERE plan_id = ?1 AND worktree_id = ?2 AND step = ?3",
        params![snapshot.plan_id, snapshot.id, step],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn ensure_recovery_record(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    kind: &str,
    branch: &str,
    oid: &str,
    reference: &str,
) -> Result<(), String> {
    let root = Path::new(&snapshot.repo);
    git(root, &["check-ref-format", reference])
        .map_err(|error| format!("Invalid recovery ref: {error}"))?;
    if optional_git(root, &["symbolic-ref", "-q", reference])
        .map_err(|error| format!("Could not inspect recovery ref: {error}"))?
        .is_some()
    {
        return Err("Recovery ref is symbolic and cannot protect this work".into());
    }
    match ref_oid(root, reference)
        .map_err(|error| format!("Could not read recovery ref: {error}"))?
    {
        Some(existing) if existing == oid => {}
        Some(_) => return Err("Recovery ref already points to different work".into()),
        None => {
            // An empty old value makes creation atomic: a concurrent writer can
            // never replace an existing recovery ref.
            git(root, &["update-ref", "--no-deref", reference, oid, ""])
                .map_err(|error| format!("Could not create recovery ref: {error}"))?;
        }
    }
    if optional_git(root, &["symbolic-ref", "-q", reference])?.is_some()
        || ref_oid(root, reference)?.as_deref() != Some(oid)
    {
        return Err("Could not verify the durable recovery ref".into());
    }
    conn.execute(
        "INSERT INTO worktree_recoveries
           (plan_id, worktree_id, kind, repo, path, branch, commit_oid,
            recovery_ref, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(plan_id, worktree_id, kind) DO UPDATE SET
           repo = excluded.repo, path = excluded.path, branch = excluded.branch,
           commit_oid = excluded.commit_oid, recovery_ref = excluded.recovery_ref",
        params![
            snapshot.plan_id,
            snapshot.id,
            kind,
            snapshot.repo,
            snapshot.path,
            branch,
            oid,
            reference,
            now(),
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn ensure_local_recovery(conn: &Connection, snapshot: &RetirementSnapshot) -> Result<(), String> {
    journal_step(conn, snapshot, "recovery_ref", "pending", None)?;
    match ensure_recovery_record(
        conn,
        snapshot,
        "local",
        &snapshot.branch,
        &snapshot.commit_oid,
        &snapshot.recovery_ref,
    ) {
        Ok(()) => journal_step(conn, snapshot, "recovery_ref", "complete", None),
        Err(error) => {
            let _ = journal_step(conn, snapshot, "recovery_ref", "failed", Some(&error));
            Err(error)
        }
    }
}

fn current_owned_for_snapshot(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
) -> Result<Owned, String> {
    let entry = owned(conn)?
        .into_iter()
        .find(|entry| entry.id == snapshot.id)
        .ok_or("App-owned worktree record was removed")?;
    if entry.repo != snapshot.repo
        || entry.common != snapshot.common
        || entry.path != snapshot.path
        || entry.branch != snapshot.branch
        || entry.base_ref != snapshot.base_ref
    {
        return Err("Worktree ownership changed after review; review it again".into());
    }
    Ok(entry)
}

fn recheck_stable_safety(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    snapshot: &RetirementSnapshot,
) -> Result<Owned, String> {
    let entry = current_owned_for_snapshot(conn, snapshot)?;
    if let Some(reason) = active_use_reason(conn, windows, &entry)? {
        return Err(reason);
    }
    if !is_owned_branch(&entry) || protected_branch_name(&entry.branch) {
        return Err("Branch is no longer an app-owned retirement branch".into());
    }
    let (_, common) = repository(&snapshot.repo)?;
    if common != snapshot.common {
        return Err("Repository identity changed after review".into());
    }
    Ok(entry)
}

fn recheck_branch_integration(
    snapshot: &RetirementSnapshot,
    source_oid: &str,
) -> Result<(), String> {
    worktree_merge::recheck(
        Path::new(&snapshot.repo),
        &snapshot.base_ref,
        source_oid,
        &snapshot.base_oid,
    )
    .map_err(|error| format!("Branch integration could not be verified: {error}"))
}

fn recheck_checkout(conn: &Connection, snapshot: &RetirementSnapshot) -> Result<bool, String> {
    let all = checkouts(Path::new(&snapshot.repo))?;
    if all
        .first()
        .is_some_and(|checkout| checkout.path == snapshot.path)
    {
        return Err("Refusing to retire the primary checkout".into());
    }
    let registered = all.iter().find(|checkout| checkout.path == snapshot.path);
    match registered {
        Some(checkout) => {
            if snapshot.worktree_removed {
                return Err("Worktree was restored after this retirement; review it again".into());
            }
            if checkout.locked || checkout.prunable {
                return Err("Git worktree is locked or prunable".into());
            }
            if checkout.branch.as_deref() != Some(&snapshot.branch) {
                return Err("Branch changed or HEAD is detached".into());
            }
            if repository(&snapshot.path)?.1 != snapshot.common {
                return Err("Repository identity changed".into());
            }
            if resolve_commit(Path::new(&snapshot.path), "HEAD")? != snapshot.commit_oid {
                return Err("Worktree HEAD moved after review".into());
            }
            let entry = current_owned_for_snapshot(conn, snapshot)?;
            if entry
                .pending_retirement_plan_id
                .as_deref()
                .is_some_and(|plan_id| plan_id != snapshot.plan_id)
            {
                return Err("Another retirement owns the pending worktree removal".into());
            }
            if let Some(reason) = environment::check_cleanup(conn, &entry)? {
                return Err(reason);
            }
            Ok(false)
        }
        None if Path::new(&snapshot.path).exists() => {
            Err("Worktree path is occupied but not registered with Git".into())
        }
        None if snapshot.worktree_removed => {
            let entry = current_owned_for_snapshot(conn, snapshot)?;
            if !entry.removed
                || entry.pending_retirement_plan_id.is_some()
                || entry.active_retirement_plan_id.as_deref() != Some(&snapshot.plan_id)
            {
                return Err("A different retirement owns the removed worktree".into());
            }
            Ok(true)
        }
        None => {
            // A retry may observe Git's removal after a crash but before the DB
            // completion write. Prefer the durable identity; old databases may
            // claim their one unambiguous pending journal during completion.
            let entry = current_owned_for_snapshot(conn, snapshot)?;
            match entry.pending_retirement_plan_id.as_deref() {
                Some(plan_id) if plan_id == snapshot.plan_id => Ok(true),
                Some(_) => Err("Another retirement owns the pending worktree removal".into()),
                None if sole_interrupted_removal(conn, &snapshot.id)?.as_deref()
                    == Some(&snapshot.plan_id) =>
                {
                    Ok(true)
                }
                None => Err("Worktree changed after review; review it again".into()),
            }
        }
    }
}

fn actual_worktree_removed(snapshot: &RetirementSnapshot) -> bool {
    !Path::new(&snapshot.path).exists()
        && checkouts(Path::new(&snapshot.repo))
            .map(|all| all.iter().all(|checkout| checkout.path != snapshot.path))
            .unwrap_or(false)
}

fn retirement_failure(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    step: &str,
    error: String,
    selection: &WorktreeRetirementSelection,
) -> WorktreeRetirementResult {
    let _ = journal_step(conn, snapshot, step, "failed", Some(&error));
    let local_was_requested = selection.delete_local_branch || snapshot.local_requested;
    let remote_was_requested = selection.delete_remote_branch || snapshot.remote_requested;
    let local_absent = matches!(
        ref_oid(
            Path::new(&snapshot.repo),
            &format!("refs/heads/{}", snapshot.branch),
        ),
        Ok(None)
    );
    let remote_absent = matches!(
        (
            remote_was_requested,
            snapshot.remote_branch.as_deref(),
            recheck_remote_configuration(snapshot)
        ),
        (true, Some(branch), Ok(target))
            if matches!(remote_oid(Path::new(&snapshot.repo), &target, branch), Ok(None))
    );
    let worktree_absent = actual_worktree_removed(snapshot);
    if worktree_absent {
        let _ = complete_worktree_removal(conn, snapshot);
    } else if step == "worktree" {
        let _ = clear_pending_removal(conn, snapshot);
    }
    if local_was_requested && local_absent {
        let _ = set_retirement_flag(conn, snapshot, "local_deleted");
        let _ = journal_step(conn, snapshot, "local_branch", "complete", None);
    }
    if remote_was_requested && remote_absent {
        let _ = set_retirement_flag(conn, snapshot, "remote_deleted");
        let _ = journal_step(conn, snapshot, "remote_branch", "complete", None);
    }
    WorktreeRetirementResult {
        id: snapshot.id.clone(),
        path: snapshot.path.clone(),
        worktree_removed: worktree_absent,
        local_branch_deleted: local_was_requested && local_absent,
        remote_branch_deleted: remote_absent,
        recovery_ref: (direct_ref_oid(Path::new(&snapshot.repo), &snapshot.recovery_ref)
            .ok()
            .flatten()
            .as_deref()
            == Some(&snapshot.commit_oid))
        .then(|| snapshot.recovery_ref.clone()),
        error: Some(error),
    }
}

fn git_remote_target(root: &Path, args: &[&str], target: &str) -> Result<String, String> {
    let output = git_remote_output(root, args)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .replace(target, &safe_remote_destination(target)));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim_end_matches(['\r', '\n']).to_string())
        .map_err(|_| "Git returned remote data that is not valid UTF-8".into())
}

fn recheck_remote_configuration(snapshot: &RetirementSnapshot) -> Result<String, String> {
    let remote = snapshot
        .remote
        .as_deref()
        .ok_or("No reviewed remote branch")?;
    let branch = snapshot
        .remote_branch
        .as_deref()
        .ok_or("No reviewed remote branch")?;
    let fingerprint = snapshot
        .remote_fingerprint
        .as_deref()
        .ok_or("No reviewed remote destination")?;
    let remote_key = format!("branch.{}.remote", snapshot.branch);
    let merge_key = format!("branch.{}.merge", snapshot.branch);
    if optional_git(Path::new(&snapshot.repo), &["config", "--get", &remote_key])?.as_deref()
        != Some(remote)
        || optional_git(Path::new(&snapshot.repo), &["config", "--get", &merge_key])?.as_deref()
            != Some(&format!("refs/heads/{branch}"))
    {
        return Err("Remote tracking destination changed after review".into());
    }
    let target = remote_push_url(Path::new(&snapshot.repo), remote)?;
    if remote_fingerprint(Path::new(&snapshot.repo), &target)? != fingerprint {
        return Err("Remote push destination changed after review".into());
    }
    match remote_default_branch(Path::new(&snapshot.repo), &target)? {
        Some(default) if default == branch => {
            return Err("Refusing to delete the remote default branch".into())
        }
        Some(_) => {}
        None => return Err("Remote default branch could not be determined".into()),
    }
    if !is_owned_branch_name(branch) {
        return Err("Remote branch is no longer an app-owned branch".into());
    }
    Ok(target)
}

fn configured_tracking_ref(
    root: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    let key = format!("remote.{remote}.fetch");
    let Some(raw) = optional_git(root, &["config", "--get-all", &key])? else {
        return Ok(None);
    };
    let source = format!("refs/heads/{branch}");
    let mut matches = HashSet::new();
    for spec in raw.lines().filter(|line| !line.is_empty()) {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        if spec.starts_with('^') {
            // Negative refspec glob semantics are subtle. Leaving a stale
            // cache ref is safer than deleting outside a proven mapping.
            return Ok(None);
        }
        let Some((from, to)) = spec.split_once(':') else {
            continue;
        };
        let destination = if from == source {
            to.to_string()
        } else if let (Some(from_prefix), Some(to_prefix)) =
            (from.strip_suffix('*'), to.strip_suffix('*'))
        {
            let Some(suffix) = source.strip_prefix(from_prefix) else {
                continue;
            };
            format!("{to_prefix}{suffix}")
        } else {
            continue;
        };
        if destination.starts_with("refs/remotes/")
            && git(root, &["check-ref-format", &destination]).is_ok()
        {
            matches.insert(destination);
        }
    }
    if matches.len() == 1 {
        Ok(matches.into_iter().next())
    } else {
        Ok(None)
    }
}

fn remove_unchanged_tracking_ref(snapshot: &RetirementSnapshot) -> Result<(), String> {
    let remote = snapshot
        .remote
        .as_deref()
        .ok_or("No reviewed remote branch")?;
    let branch = snapshot
        .remote_branch
        .as_deref()
        .ok_or("No reviewed remote branch")?;
    let Some(expected) = snapshot.remote_expected_oid.as_deref() else {
        return Ok(());
    };
    let Some(reference) = configured_tracking_ref(Path::new(&snapshot.repo), remote, branch)?
    else {
        return Ok(());
    };
    if optional_git(
        Path::new(&snapshot.repo),
        &["symbolic-ref", "-q", &reference],
    )?
    .is_some()
    {
        return Ok(());
    }
    if ref_oid(Path::new(&snapshot.repo), &reference)?.as_deref() == Some(expected) {
        git(
            Path::new(&snapshot.repo),
            &["update-ref", "--no-deref", "-d", &reference, expected],
        )?;
    }
    Ok(())
}

fn preflight_selected_branches(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    delete_local_branch: bool,
    delete_remote_branch: bool,
) -> Result<(), String> {
    if delete_local_branch {
        let local_ref = format!("refs/heads/{}", snapshot.branch);
        let local = ref_oid(Path::new(&snapshot.repo), &local_ref)?;
        if snapshot.local_deleted {
            if local.is_some() {
                return Err("Local branch was recreated after retirement; review it again".into());
            }
        } else if local.as_deref() != Some(&snapshot.commit_oid) {
            let pending = retirement_step_status(conn, snapshot, "local_branch")?.as_deref()
                == Some("pending");
            if !(pending && local.is_none()) {
                return Err("Local branch changed after review; its work was preserved".into());
            }
        }
        if optional_git(
            Path::new(&snapshot.repo),
            &["symbolic-ref", "-q", &local_ref],
        )?
        .is_some()
        {
            return Err("Local branch ref is symbolic".into());
        }
        let used_elsewhere = checkouts(Path::new(&snapshot.repo))?
            .iter()
            .any(|checkout| {
                checkout.path != snapshot.path
                    && checkout.branch.as_deref() == Some(&snapshot.branch)
            });
        if used_elsewhere {
            return Err("Local branch is checked out elsewhere".into());
        }
    }
    if delete_remote_branch {
        let target = recheck_remote_configuration(snapshot)?;
        let branch = snapshot
            .remote_branch
            .as_deref()
            .ok_or("No reviewed remote branch")?;
        let current = remote_oid(Path::new(&snapshot.repo), &target, branch)?;
        if snapshot.remote_deleted {
            if current.is_some() {
                return Err("Remote branch was recreated after retirement; review it again".into());
            }
        } else if current.as_deref() != snapshot.remote_expected_oid.as_deref() {
            let pending = retirement_step_status(conn, snapshot, "remote_branch")?.as_deref()
                == Some("pending");
            if !(pending && current.is_none()) {
                return Err("Remote branch changed after review; its work was preserved".into());
            }
        }
    }
    Ok(())
}

fn backup_changed_remote(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    target: &str,
    branch: &str,
    observed_oid: &str,
) -> Result<String, String> {
    let fetched = worktree_merge::fetch_remote_ref(
        Path::new(&snapshot.repo),
        target,
        &format!("refs/heads/{branch}"),
    )?;
    if fetched != observed_oid {
        return Err("Remote branch changed while creating its recovery ref".into());
    }
    if !worktree_merge::is_integrated(Path::new(&snapshot.repo), &fetched, &snapshot.base_oid)? {
        return Err("Changed remote branch is not integrated into the reviewed base".into());
    }
    let short = fetched.chars().take(12).collect::<String>();
    let kind = format!("remote-{short}");
    let reference = retirement_ref(&snapshot.plan_id, &snapshot.id, &kind);
    ensure_recovery_record(conn, snapshot, &kind, branch, &fetched, &reference)?;
    Ok(fetched)
}

fn execute_retirement_item(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    snapshot: RetirementSnapshot,
    selection: &WorktreeRetirementSelection,
) -> WorktreeRetirementResult {
    let worktree_removed = snapshot.worktree_removed;
    let local_deleted = snapshot.local_deleted;
    let remote_deleted = snapshot.remote_deleted;
    let fail =
        |step: &str, error: String| retirement_failure(conn, &snapshot, step, error, selection);
    if let Err(error) = recheck_stable_safety(conn, windows, &snapshot) {
        return fail("validation", error);
    }
    // Validate the exact reviewed checkout before creating a recovery ref or
    // preserving local configuration. A stale plan must be observational only.
    let checkout_already_removed = match recheck_checkout(conn, &snapshot) {
        Ok(value) => value,
        Err(error) => return fail("validation", error),
    };
    if let Err(error) = conn.execute(
        "UPDATE worktree_retirement_items
            SET local_requested = (local_requested OR ?1),
                remote_requested = (remote_requested OR ?2), updated_at = ?3
          WHERE plan_id = ?4 AND worktree_id = ?5",
        params![
            selection.delete_local_branch,
            selection.delete_remote_branch,
            now(),
            snapshot.plan_id,
            snapshot.id
        ],
    ) {
        return fail("journal", error.to_string());
    }
    if let Err(error) = ensure_local_recovery(conn, &snapshot) {
        return fail("recovery_ref", error);
    }
    if !worktree_removed {
        if let Err(error) = recheck_stable_safety(conn, windows, &snapshot) {
            return fail("worktree", error);
        }
        if !checkout_already_removed {
            let entry = match current_owned_for_snapshot(conn, &snapshot) {
                Ok(entry) => entry,
                Err(error) => return fail("worktree", error),
            };
            // Preserve configured local data at the final removal boundary.
            // Unknown files, changed policy and a failed backup keep the folder.
            if let Err(error) = environment::preserve(conn, &entry, &snapshot.plan_id) {
                return fail("local_files", error);
            }
            if let Err(error) = begin_worktree_removal(conn, &snapshot) {
                return fail("worktree", error);
            }
            if let Err(error) = git(
                Path::new(&snapshot.repo),
                &["worktree", "remove", "--", &snapshot.path],
            ) {
                return fail("worktree", error);
            }
        } else if let Err(error) = claim_interrupted_removal(conn, &snapshot) {
            return fail("worktree", error);
        }
        if !actual_worktree_removed(&snapshot) {
            return fail(
                "worktree",
                "Git did not remove the worktree completely".into(),
            );
        }
        if let Err(error) = complete_worktree_removal(conn, &snapshot) {
            return fail("worktree", error);
        }
    }

    if selection.delete_local_branch {
        if !snapshot.local_allowed && !local_deleted {
            return fail(
                "local_branch",
                snapshot
                    .local_reason
                    .clone()
                    .unwrap_or_else(|| "Local branch deletion was not reviewed".into()),
            );
        }
        if let Err(error) = recheck_stable_safety(conn, windows, &snapshot) {
            return fail("local_branch", error);
        }
        if !local_deleted {
            if let Err(error) = recheck_branch_integration(&snapshot, &snapshot.commit_oid) {
                return fail("local_branch", error);
            }
        }
        if let Err(error) = preflight_selected_branches(conn, &snapshot, true, false) {
            return fail("local_branch", error);
        }
        let local_ref = format!("refs/heads/{}", snapshot.branch);
        if local_deleted {
            match ref_oid(Path::new(&snapshot.repo), &local_ref) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    return fail(
                        "local_branch",
                        "Local branch was recreated after retirement; review it again".into(),
                    )
                }
                Err(error) => return fail("local_branch", error),
            }
        }
        if local_deleted {
            // The verified absent branch is an idempotent retry success.
        } else {
            match ref_oid(Path::new(&snapshot.repo), &local_ref) {
                Ok(None) => {}
                Ok(Some(oid)) if oid != snapshot.commit_oid => {
                    return fail(
                        "local_branch",
                        "Local branch moved after review; its new work was preserved".into(),
                    )
                }
                Ok(Some(_)) => {
                    match optional_git(
                        Path::new(&snapshot.repo),
                        &["symbolic-ref", "-q", &local_ref],
                    ) {
                        Ok(Some(_)) => {
                            return fail("local_branch", "Local branch ref is symbolic".into())
                        }
                        Ok(None) => {}
                        Err(error) => return fail("local_branch", error),
                    }
                    let checked_out = match checkouts(Path::new(&snapshot.repo)) {
                        Ok(checkouts) => checkouts
                            .iter()
                            .any(|checkout| checkout.branch.as_deref() == Some(&snapshot.branch)),
                        Err(error) => return fail("local_branch", error),
                    };
                    if checked_out {
                        return fail("local_branch", "Local branch is checked out".into());
                    }
                    if let Err(error) =
                        journal_step(conn, &snapshot, "local_branch", "pending", None)
                    {
                        return fail("local_branch", error);
                    }
                    if let Err(error) = git(
                        Path::new(&snapshot.repo),
                        &[
                            "update-ref",
                            "--no-deref",
                            "-d",
                            &local_ref,
                            &snapshot.commit_oid,
                        ],
                    ) {
                        return fail("local_branch", error);
                    }
                }
                Err(error) => return fail("local_branch", error),
            }
            match ref_oid(Path::new(&snapshot.repo), &local_ref) {
                Ok(None) => {}
                Ok(Some(_)) => return fail("local_branch", "Local branch still exists".into()),
                Err(error) => return fail("local_branch", error),
            }
            if let Err(error) = set_retirement_flag(conn, &snapshot, "local_deleted") {
                return fail("local_branch", error);
            }
            if let Err(error) = journal_step(conn, &snapshot, "local_branch", "complete", None) {
                return fail("local_branch", error);
            }
        }
    }

    if selection.delete_remote_branch {
        if !snapshot.remote_allowed && !remote_deleted {
            return fail(
                "remote_branch",
                snapshot
                    .remote_reason
                    .clone()
                    .unwrap_or_else(|| "Remote branch deletion was not reviewed".into()),
            );
        }
        if let Err(error) = recheck_stable_safety(conn, windows, &snapshot) {
            return fail("remote_branch", error);
        }
        if !remote_deleted {
            let Some(expected_oid) = snapshot.remote_expected_oid.as_deref() else {
                return fail("remote_branch", "No reviewed remote branch commit".into());
            };
            if let Err(error) = recheck_branch_integration(&snapshot, expected_oid) {
                return fail("remote_branch", error);
            }
        }
        if let Err(error) = preflight_selected_branches(conn, &snapshot, false, true) {
            return fail("remote_branch", error);
        }
        let target = match recheck_remote_configuration(&snapshot) {
            Ok(target) => target,
            Err(error) => return fail("remote_branch", error),
        };
        let Some(branch) = snapshot.remote_branch.as_deref() else {
            return fail("remote_branch", "No reviewed remote branch".into());
        };
        if checkouts(Path::new(&snapshot.repo))
            .map(|checkouts| {
                checkouts
                    .iter()
                    .any(|checkout| checkout.branch.as_deref() == Some(branch))
            })
            .unwrap_or(true)
        {
            return fail("remote_branch", "Branch is checked out".into());
        }
        let current = match remote_oid(Path::new(&snapshot.repo), &target, branch) {
            Ok(value) => value,
            Err(error) => return fail("remote_branch", error),
        };
        if remote_deleted {
            if current.is_some() {
                return fail(
                    "remote_branch",
                    "Remote branch was recreated after retirement; review it again".into(),
                );
            }
        } else if current.as_deref() != snapshot.remote_expected_oid.as_deref() {
            let pending = retirement_step_status(conn, &snapshot, "remote_branch")
                .ok()
                .flatten()
                .as_deref()
                == Some("pending");
            if !(pending && current.is_none()) {
                return fail(
                    "remote_branch",
                    "Remote branch changed after review; its work was preserved".into(),
                );
            }
        }
        if !remote_deleted {
            if let Some(mut expected_oid) = current {
                if expected_oid != snapshot.commit_oid {
                    expected_oid = match backup_changed_remote(
                        conn,
                        &snapshot,
                        &target,
                        branch,
                        &expected_oid,
                    ) {
                        Ok(oid) => oid,
                        Err(error) => return fail("remote_branch", error),
                    };
                }
                if let Err(error) = journal_step(conn, &snapshot, "remote_branch", "pending", None)
                {
                    return fail("remote_branch", error);
                }
                let lease = format!("--force-with-lease=refs/heads/{branch}:{expected_oid}");
                let deletion = format!(":refs/heads/{branch}");
                if let Err(error) = git_remote_target(
                    Path::new(&snapshot.repo),
                    &["push", "--porcelain", &lease, &target, &deletion],
                    &target,
                ) {
                    return fail("remote_branch", error);
                }
                match remote_oid(Path::new(&snapshot.repo), &target, branch) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        return fail(
                            "remote_branch",
                            "Remote branch changed during deletion and was preserved".into(),
                        )
                    }
                    Err(error) => return fail("remote_branch", error),
                }
            }
            if let Err(error) = remove_unchanged_tracking_ref(&snapshot) {
                return fail("remote_tracking", error);
            }
            if let Err(error) = journal_step(conn, &snapshot, "remote_tracking", "complete", None) {
                return fail("remote_tracking", error);
            }
            if let Err(error) = set_retirement_flag(conn, &snapshot, "remote_deleted") {
                return fail("remote_branch", error);
            }
            if let Err(error) = journal_step(conn, &snapshot, "remote_branch", "complete", None) {
                return fail("remote_branch", error);
            }
        }
    }

    let actually_removed = actual_worktree_removed(&snapshot);
    let local_was_requested = selection.delete_local_branch || snapshot.local_requested;
    let remote_was_requested = selection.delete_remote_branch || snapshot.remote_requested;
    let local_absent = matches!(
        ref_oid(
            Path::new(&snapshot.repo),
            &format!("refs/heads/{}", snapshot.branch),
        ),
        Ok(None)
    );
    let remote_absent = if remote_was_requested {
        match (
            recheck_remote_configuration(&snapshot),
            snapshot.remote_branch.as_deref(),
        ) {
            (Ok(target), Some(branch)) => matches!(
                remote_oid(Path::new(&snapshot.repo), &target, branch),
                Ok(None)
            ),
            _ => false,
        }
    } else {
        false
    };
    let recovery_verified = matches!(
        direct_ref_oid(Path::new(&snapshot.repo), &snapshot.recovery_ref),
        Ok(Some(ref oid)) if oid == &snapshot.commit_oid
    );
    WorktreeRetirementResult {
        id: snapshot.id,
        path: snapshot.path,
        worktree_removed: actually_removed,
        local_branch_deleted: local_was_requested && local_absent,
        remote_branch_deleted: remote_was_requested && remote_absent,
        recovery_ref: recovery_verified.then_some(snapshot.recovery_ref),
        error: None,
    }
}

#[cfg(test)]
fn execute_retirement(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    plan_id: &str,
    selections: &[WorktreeRetirementSelection],
) -> Result<WorktreeRetirementReport, String> {
    let host = WorktreeHost {
        root: PathBuf::new(),
        windows: Mutex::new(windows.clone()),
        repositories: RepositoryReservations::default(),
    };
    execute_retirement_coordinated_with(conn, &host, plan_id, selections, |windows| windows.clone())
}

fn persist_retirement_selection(
    conn: &Connection,
    snapshot: &RetirementSnapshot,
    selection: &WorktreeRetirementSelection,
) -> Result<(), String> {
    conn.execute(
        "UPDATE worktree_retirement_items
            SET local_requested = (local_requested OR ?1),
                remote_requested = (remote_requested OR ?2), updated_at = ?3
          WHERE plan_id = ?4 AND worktree_id = ?5",
        params![
            selection.delete_local_branch,
            selection.delete_remote_branch,
            now(),
            snapshot.plan_id,
            snapshot.id
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn delete_local_branch_after_review(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    snapshot: &RetirementSnapshot,
) -> Result<(), String> {
    if !snapshot.local_allowed && !snapshot.local_deleted {
        return Err(snapshot
            .local_reason
            .clone()
            .unwrap_or_else(|| "Local branch deletion was not reviewed".into()));
    }
    recheck_stable_safety(conn, windows, snapshot)?;
    preflight_selected_branches(conn, snapshot, true, false)?;
    let local_ref = format!("refs/heads/{}", snapshot.branch);
    if snapshot.local_deleted {
        return match ref_oid(Path::new(&snapshot.repo), &local_ref)? {
            None => Ok(()),
            Some(_) => Err("Local branch was recreated after retirement; review it again".into()),
        };
    }
    match ref_oid(Path::new(&snapshot.repo), &local_ref)? {
        None => {}
        Some(oid) if oid != snapshot.commit_oid => {
            return Err("Local branch moved after review; its new work was preserved".into())
        }
        Some(_) => {
            if optional_git(
                Path::new(&snapshot.repo),
                &["symbolic-ref", "-q", &local_ref],
            )?
            .is_some()
            {
                return Err("Local branch ref is symbolic".into());
            }
            if checkouts(Path::new(&snapshot.repo))?
                .iter()
                .any(|checkout| checkout.branch.as_deref() == Some(&snapshot.branch))
            {
                return Err("Local branch is checked out".into());
            }
            journal_step(conn, snapshot, "local_branch", "pending", None)?;
            git(
                Path::new(&snapshot.repo),
                &[
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &local_ref,
                    &snapshot.commit_oid,
                ],
            )?;
        }
    }
    if ref_oid(Path::new(&snapshot.repo), &local_ref)?.is_some() {
        return Err("Local branch still exists".into());
    }
    set_retirement_flag(conn, snapshot, "local_deleted")?;
    journal_step(conn, snapshot, "local_branch", "complete", None)
}

fn local_retirement_result(snapshot: RetirementSnapshot) -> WorktreeRetirementResult {
    let local_absent = matches!(
        ref_oid(
            Path::new(&snapshot.repo),
            &format!("refs/heads/{}", snapshot.branch),
        ),
        Ok(None)
    );
    let recovery_verified = matches!(
        direct_ref_oid(Path::new(&snapshot.repo), &snapshot.recovery_ref),
        Ok(Some(ref oid)) if oid == &snapshot.commit_oid
    );
    let worktree_removed = actual_worktree_removed(&snapshot);
    WorktreeRetirementResult {
        id: snapshot.id,
        path: snapshot.path,
        worktree_removed,
        local_branch_deleted: snapshot.local_requested && local_absent,
        remote_branch_deleted: false,
        recovery_ref: recovery_verified.then_some(snapshot.recovery_ref),
        error: None,
    }
}

fn execute_retirement_coordinated(
    app: &AppHandle,
    conn: &Connection,
    host: &WorktreeHost,
    plan_id: &str,
    selections: &[WorktreeRetirementSelection],
) -> Result<WorktreeRetirementReport, String> {
    execute_retirement_coordinated_with(conn, host, plan_id, selections, |windows| {
        protected_windows(app, windows)
    })
}

fn execute_retirement_coordinated_with(
    conn: &Connection,
    host: &WorktreeHost,
    plan_id: &str,
    selections: &[WorktreeRetirementSelection],
    protect: impl Fn(&HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>>,
) -> Result<WorktreeRetirementReport, String> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM worktree_retirement_plans WHERE plan_id = ?1)",
            [plan_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !exists {
        return Err("Retirement plan was not found; review the worktrees again".into());
    }

    let mut deduped: Vec<WorktreeRetirementSelection> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for selection in selections {
        if let Some(index) = positions.get(&selection.id).copied() {
            deduped[index].delete_local_branch |= selection.delete_local_branch;
            deduped[index].delete_remote_branch |= selection.delete_remote_branch;
        } else {
            positions.insert(selection.id.clone(), deduped.len());
            deduped.push(selection.clone());
        }
    }

    let mut results = Vec::new();
    for selection in deduped {
        let Some(snapshot) = load_retirement_snapshot(conn, plan_id, &selection.id)? else {
            results.push(WorktreeRetirementResult {
                id: selection.id,
                path: String::new(),
                worktree_removed: false,
                local_branch_deleted: false,
                remote_branch_deleted: false,
                recovery_ref: None,
                error: Some("Worktree was not part of the reviewed plan".into()),
            });
            continue;
        };
        let _repository = host.repository_guard(&snapshot.common)?;

        // Record the complete choice only after a fresh validation. If a later
        // phase fails, Settings can recover every requested step after restart.
        let intent_error = {
            let windows = host.operation_guard()?;
            let protected = protect(&windows);
            recheck_stable_safety(conn, &protected, &snapshot)
                .and_then(|_| recheck_checkout(conn, &snapshot).map(|_| ()))
                .and_then(|_| persist_retirement_selection(conn, &snapshot, &selection))
                .err()
        };
        if let Some(error) = intent_error {
            let no_remote_reconcile = WorktreeRetirementSelection {
                id: selection.id,
                delete_local_branch: false,
                delete_remote_branch: false,
            };
            results.push(retirement_failure(
                conn,
                &snapshot,
                "validation",
                error,
                &no_remote_reconcile,
            ));
            continue;
        }

        // Remove the checkout under the lifecycle lock. In-memory request bits
        // are suppressed so failure/result reconciliation cannot fetch while
        // the global guard is held; the database retains the original intent.
        let completed_remote = snapshot.remote_deleted;
        let mut checkout_snapshot = snapshot;
        checkout_snapshot.local_requested = false;
        checkout_snapshot.remote_requested = false;
        let mut checkout_result = {
            let windows = host.operation_guard()?;
            execute_retirement_item(
                conn,
                &protect(&windows),
                checkout_snapshot,
                &WorktreeRetirementSelection {
                    id: selection.id.clone(),
                    delete_local_branch: false,
                    delete_remote_branch: false,
                },
            )
        };
        checkout_result.remote_branch_deleted = completed_remote;
        if checkout_result.error.is_some() {
            results.push(checkout_result);
            continue;
        }

        if selection.delete_local_branch {
            let Some(local_snapshot) = load_retirement_snapshot(conn, plan_id, &selection.id)?
            else {
                results.push(WorktreeRetirementResult {
                    error: Some("Retirement plan item disappeared after checkout removal".into()),
                    ..checkout_result
                });
                continue;
            };
            // This may refresh an upstream. Keep it outside the global guard.
            let integration_error = if local_snapshot.local_deleted {
                None
            } else {
                recheck_branch_integration(&local_snapshot, &local_snapshot.commit_oid).err()
            };
            if let Some(error) = integration_error {
                results.push(retirement_failure(
                    conn,
                    &local_snapshot,
                    "local_branch",
                    error,
                    &selection,
                ));
                continue;
            }
            let local_error = {
                let windows = host.operation_guard()?;
                delete_local_branch_after_review(conn, &protect(&windows), &local_snapshot).err()
            };
            if let Some(error) = local_error {
                results.push(retirement_failure(
                    conn,
                    &local_snapshot,
                    "local_branch",
                    error,
                    &selection,
                ));
                continue;
            }
        }

        if !selection.delete_remote_branch {
            let snapshot = load_retirement_snapshot(conn, plan_id, &selection.id)?
                .ok_or("Retirement plan item disappeared after local completion")?;
            let mut result = local_retirement_result(snapshot);
            result.remote_branch_deleted = completed_remote;
            results.push(result);
            continue;
        }

        let Some(remote_snapshot) = load_retirement_snapshot(conn, plan_id, &selection.id)? else {
            results.push(WorktreeRetirementResult {
                error: Some("Retirement plan item disappeared after local removal".into()),
                ..checkout_result
            });
            continue;
        };
        let validation_error = {
            let windows = host.operation_guard()?;
            let protected = protect(&windows);
            recheck_stable_safety(conn, &protected, &remote_snapshot)
                .and_then(|_| recheck_checkout(conn, &remote_snapshot).map(|_| ()))
                .err()
        };
        if let Some(error) = validation_error {
            results.push(retirement_failure(
                conn,
                &remote_snapshot,
                "remote_branch",
                error,
                &selection,
            ));
            continue;
        }

        // The repository reservation prevents app-owned starts, setup begin,
        // restore, and window leases in this repository from crossing the
        // destructive remote phase. The force-with-lease protects against
        // external remote writers.
        results.push(execute_retirement_item(
            conn,
            &HashMap::new(),
            remote_snapshot,
            &WorktreeRetirementSelection {
                id: selection.id,
                delete_local_branch: false,
                delete_remote_branch: true,
            },
        ));
    }
    Ok(WorktreeRetirementReport { results })
}

#[cfg(test)]
fn cleanup(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    common: Option<&str>,
    ids: Option<&[String]>,
) -> Result<CleanupReport, String> {
    let mut report = CleanupReport::default();
    for entry in owned(conn)? {
        if entry.removed
            || common.is_some_and(|value| value != entry.common)
            || ids.is_some_and(|ids| !ids.contains(&entry.id))
        {
            continue;
        }
        let reason = match blocked(conn, windows, &entry) {
            Ok(reason) => reason,
            Err(error) => Some(error),
        };
        if let Some(reason) = reason {
            report.skipped.push(format!("{}: {reason}", entry.branch));
            continue;
        }
        match remove_checkout(conn, windows, &entry) {
            Ok(()) => report.removed.push(entry.path),
            Err(error) => report.skipped.push(format!("{}: {error}", entry.branch)),
        }
    }
    Ok(report)
}

#[cfg(test)]
fn remove_checkout(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
) -> Result<(), String> {
    let plan_id = plan_id();
    let snapshot = review_retirement(conn, windows, entry, &plan_id)?;
    persist_retirement_plan(conn, &plan_id, std::slice::from_ref(&snapshot))?;
    let result = execute_retirement_item(
        conn,
        windows,
        snapshot,
        &WorktreeRetirementSelection {
            id: entry.id.clone(),
            delete_local_branch: false,
            delete_remote_branch: false,
        },
    );
    match result.error {
        Some(error) => Err(error),
        None if result.worktree_removed => Ok(()),
        None => Err("Git did not remove the worktree completely".into()),
    }
}

fn retirement_pending(conn: &Connection, entry: &Owned) -> bool {
    if !entry.removed {
        return false;
    }
    if matches!(
        ref_oid(
            Path::new(&entry.repo),
            &format!("refs/heads/{}", entry.branch)
        ),
        Ok(Some(_))
    ) {
        return true;
    }
    conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM worktree_retirement_items
            WHERE worktree_id = ?1
              AND ((local_requested = 1 AND local_deleted = 0)
                OR (remote_expected_oid IS NOT NULL AND remote_deleted = 0))
         )",
        [&entry.id],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn removed_retry_blocked(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
) -> Result<Option<String>, String> {
    if let Some(reason) = active_use_reason(conn, windows, entry)? {
        return Ok(Some(reason));
    }
    if !is_owned_branch(entry) || protected_branch_name(&entry.branch) {
        return Ok(Some("Branch is not an app-owned retirement branch".into()));
    }
    let (_, common) = repository(&entry.repo)?;
    if common != entry.common {
        return Ok(Some("Repository identity changed".into()));
    }
    if Path::new(&entry.path).exists()
        || checkouts(Path::new(&entry.repo))?
            .iter()
            .any(|checkout| checkout.path == entry.path)
    {
        return Ok(Some(
            "Checkout was restored; review its current state".into(),
        ));
    }
    if let Some(recovery) = latest_local_recovery(conn, &entry.id)? {
        if direct_ref_oid(Path::new(&entry.repo), &recovery.recovery_ref)?.as_deref()
            != Some(&recovery.commit_oid)
        {
            return Ok(Some("Recorded recovery ref is missing or changed".into()));
        }
    } else if ref_oid(
        Path::new(&entry.repo),
        &format!("refs/heads/{}", entry.branch),
    )?
    .is_none()
    {
        return Ok(Some("Checkout has no durable recovery ref".into()));
    }
    Ok(None)
}

fn overview(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    cwd: &str,
) -> Result<WorktreeOverview, String> {
    let (repo, common) = repository(cwd)?;
    let all = checkouts(Path::new(&repo))?;
    let records = owned(conn)?;
    let mut entries = Vec::new();
    for (index, checkout) in all.iter().enumerate() {
        let record = records
            .iter()
            .find(|entry| entry.common == common && entry.path == checkout.path);
        let reason = match record {
            Some(entry) => blocked(conn, windows, entry).unwrap_or_else(Some),
            None => Some(
                if index == 0 {
                    "Primary checkout"
                } else {
                    "External worktree; managed by another tool"
                }
                .into(),
            ),
        };
        entries.push(WorktreeEntry {
            id: record.map(|v| v.id.clone()),
            path: checkout.path.clone(),
            project_cwd: record
                .map(|entry| environment::explicit_scope_for_entry(conn, entry))
                .transpose()?
                .flatten()
                .map(|scope| scope.main_path),
            branch: checkout.branch.clone(),
            base_ref: record.map(|v| v.base_ref.clone()),
            main: index == 0,
            pinned: record.is_some_and(|v| v.pinned),
            missing: !Path::new(&checkout.path).is_dir(),
            last_used: record.map(|v| v.last_used),
            blocked_reason: reason,
            retirement_pending: record.is_some_and(|v| retirement_pending(conn, v)),
        });
    }
    for entry in records
        .iter()
        .filter(|v| v.common == common && !all.iter().any(|c| c.path == v.path))
    {
        let pending = retirement_pending(conn, entry);
        let reason = if pending {
            removed_retry_blocked(conn, windows, entry).unwrap_or_else(Some)
        } else {
            Some("Retired; durable recovery ref preserved".into())
        };
        entries.push(WorktreeEntry {
            id: Some(entry.id.clone()),
            path: entry.path.clone(),
            project_cwd: environment::explicit_scope_for_entry(conn, entry)?
                .map(|scope| scope.main_path),
            branch: Some(entry.branch.clone()),
            base_ref: Some(entry.base_ref.clone()),
            main: false,
            pinned: entry.pinned,
            missing: true,
            last_used: Some(entry.last_used),
            blocked_reason: reason,
            retirement_pending: pending,
        });
    }
    Ok(WorktreeOverview {
        repo,
        project_cwd: environment::scope_for_cwd(cwd)?.main_path,
        settings: settings(conn, cwd)?,
        entries,
    })
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.len() < 8
        || id.len() > 80
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("Invalid worktree identity".into());
    }
    Ok(())
}

fn slug(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(42)
        .collect();
    let name = name
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if name.is_empty() {
        "task".into()
    } else {
        name
    }
}

fn create(
    conn: &Connection,
    host: &WorktreeHost,
    cwd: &str,
    id: &str,
    name: &str,
    base_ref: Option<&str>,
) -> Result<Owned, String> {
    create_with_naming(conn, host, cwd, id, name, base_ref, None)
}

fn create_with_naming(
    conn: &Connection,
    host: &WorktreeHost,
    cwd: &str,
    id: &str,
    name: &str,
    base_ref: Option<&str>,
    auto_name_token: Option<&str>,
) -> Result<Owned, String> {
    validate_id(id)?;
    if let Some(token) = auto_name_token {
        validate_id(token)?;
    }
    let (repo, common) = repository(cwd)?;
    if owned(conn)?.iter().any(|v| v.id == id) {
        return Err("This session already owns a worktree".into());
    }
    let default_base = git(&expand_home(cwd), &["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|_| "HEAD".into());
    // Resolve HEAD to a stable SHA in detached checkouts; a symbolic HEAD would
    // otherwise later refer to the primary checkout's unrelated branch.
    let base_ref = base_ref
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(&default_base)
        .trim();
    let commit = resolve_commit(&expand_home(cwd), base_ref)?;
    let base_ref = if base_ref == "HEAD" {
        commit.as_str()
    } else {
        base_ref
    };
    let branch = if auto_name_token.is_some() {
        naming::available_branch(Path::new(&repo), &format!("monocode/task-{}", &id[..8]))?
    } else {
        format!("monocode/{}-{}", slug(name), id)
    };
    git(Path::new(&repo), &["check-ref-format", "--branch", &branch])?;
    let hash = common.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    let repo_name = Path::new(&repo)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("repo");
    let parent = host.root.join(format!("{}-{hash:016x}", slug(repo_name)));
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let path = path_to_js(
        &std::fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .join(id),
    );
    if Path::new(&path).exists()
        || resolve_commit(Path::new(&repo), &format!("refs/heads/{branch}")).is_ok()
    {
        return Err("The worktree path or branch already exists; choose another task".into());
    }
    let creation_ref = ensure_creation_ref(Path::new(&repo), id, &commit)?;
    // Persist ownership and pending setup together before creating files. A
    // crash after Git succeeds must not turn the next open into a setup skip.
    let mut entry = Owned {
        id: id.into(),
        repo: repo.clone(),
        common: common.clone(),
        path: path.clone(),
        branch: branch.clone(),
        base_ref: base_ref.into(),
        pinned: false,
        last_used: now(),
        removed: false,
        creation_oid: Some(commit.clone()),
        active_retirement_plan_id: None,
        pending_retirement_plan_id: None,
    };
    let project_scope = environment::scope_for_cwd(cwd)?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO managed_worktrees (id, repo, common_dir, path, branch, base_ref, last_used, creation_oid) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![id, repo, common, path, branch, base_ref, entry.last_used, commit]).map_err(|e| e.to_string())?;
    environment::mark_pending(
        &tx,
        &entry,
        environment::SetupOrigin::Fresh,
        Some(&project_scope),
        None,
    )?;
    if let Some(token) = auto_name_token {
        naming::register(&tx, &entry, token)?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    if let Err(error) = git(
        Path::new(&repo),
        &["worktree", "add", "-b", &branch, "--", &path, &creation_ref],
    ) {
        // Retain metadata if Git got far enough to create anything. Never remove
        // a user ref/directory as compensation for a failed checkout.
        if !Path::new(&path).exists()
            && resolve_commit(Path::new(&repo), &format!("refs/heads/{branch}")).is_err()
        {
            conn.execute("DELETE FROM managed_worktrees WHERE id = ?1", [id])
                .map_err(|e| e.to_string())?;
        }
        return Err(error);
    }
    verify_created_worktree(&entry, &commit)?;
    let changed = conn
        .execute(
            "UPDATE managed_worktrees SET creation_oid = NULL
              WHERE id = ?1 AND creation_oid = ?2",
            params![id, entry.creation_oid],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Saved worktree creation intent changed before completion".into());
    }
    entry.creation_oid = None;
    Ok(entry)
}

fn scoped_path(cwd: &str, checkout: &str) -> Result<String, String> {
    let source = std::fs::canonicalize(expand_home(cwd)).map_err(|e| e.to_string())?;
    let top = git(&source, &["rev-parse", "--show-toplevel"])?;
    let top = std::fs::canonicalize(top).map_err(|e| e.to_string())?;
    let relative = source
        .strip_prefix(&top)
        .map_err(|_| "Project is outside the checkout")?;
    let checkout = std::fs::canonicalize(checkout).map_err(|e| e.to_string())?;
    let target = checkout.join(relative);
    if !target.is_dir() {
        return Err("The selected base does not contain this project folder".into());
    }
    let target = std::fs::canonicalize(target).map_err(|e| e.to_string())?;
    if !target.starts_with(checkout) {
        return Err("Project folder points outside the worktree".into());
    }
    Ok(path_to_js(&target))
}

fn verify_created_worktree(entry: &Owned, commit_oid: &str) -> Result<(), String> {
    let checkout = checkouts(Path::new(&entry.repo))?
        .into_iter()
        .find(|checkout| checkout.path == entry.path)
        .ok_or("Git did not register the created worktree")?;
    if checkout.branch.as_deref() != Some(&entry.branch)
        || !Path::new(&entry.path).is_dir()
        || repository(&entry.path)?.1 != entry.common
        || resolve_commit(Path::new(&entry.path), "HEAD")? != commit_oid
        || ref_oid(
            Path::new(&entry.repo),
            &format!("refs/heads/{}", entry.branch),
        )?
        .as_deref()
            != Some(commit_oid)
    {
        return Err("Created worktree does not match its saved creation intent".into());
    }
    Ok(())
}

fn recovered_branch(
    root: &Path,
    entry: &Owned,
    commit: &str,
    checkouts: &[Checkout],
) -> Result<(String, bool), String> {
    let original_ref = format!("refs/heads/{}", entry.branch);
    match ref_oid(root, &original_ref)? {
        None => return Ok((entry.branch.clone(), true)),
        Some(oid)
            if oid == commit
                && !checkouts
                    .iter()
                    .any(|checkout| checkout.branch.as_deref() == Some(&entry.branch)) =>
        {
            return Ok((entry.branch.clone(), false));
        }
        _ => {}
    }
    let short = commit.chars().take(10).collect::<String>();
    let base = format!("monocode/recovered-{}-{short}", slug(&entry.id));
    for suffix in 0..1000 {
        let candidate = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        let reference = format!("refs/heads/{candidate}");
        match ref_oid(root, &reference)? {
            None => return Ok((candidate, true)),
            Some(oid)
                if oid == commit
                    && !checkouts
                        .iter()
                        .any(|checkout| checkout.branch.as_deref() == Some(&candidate)) =>
            {
                return Ok((candidate, false));
            }
            _ => {}
        }
    }
    Err("Could not choose an unused recovery branch name".into())
}

fn reconcile_interrupted_removal(
    conn: &Connection,
    entry: &Owned,
) -> Result<Option<String>, String> {
    if entry.creation_oid.is_some() {
        if entry.pending_retirement_plan_id.is_some() {
            return Err("Saved worktree creation state conflicts with retirement state".into());
        }
        return Ok(entry.active_retirement_plan_id.clone());
    }
    let plan_id = match entry.pending_retirement_plan_id.clone() {
        Some(plan_id) => plan_id,
        None => match sole_interrupted_removal(conn, &entry.id)? {
            Some(plan_id) => plan_id,
            None => return Ok(entry.active_retirement_plan_id.clone()),
        },
    };
    let snapshot = load_retirement_snapshot(conn, &plan_id, &entry.id)?
        .ok_or("The interrupted retirement item is missing")?;
    current_owned_for_snapshot(conn, &snapshot)?;
    claim_interrupted_removal(conn, &snapshot)?;
    complete_worktree_removal(conn, &snapshot)?;
    Ok(Some(plan_id))
}

fn open_owned(conn: &Connection, entry: &Owned) -> Result<(), String> {
    let root = Path::new(&entry.path);
    let (_, common) = repository(&entry.repo)?;
    if common != entry.common {
        return Err("Repository identity changed".into());
    }
    let all = checkouts(Path::new(&entry.repo))?;
    let registered = all.iter().find(|v| v.path == entry.path);
    let mut restored_branch = entry.branch.clone();
    if !root.exists() {
        if registered.is_some() {
            return Err("Worktree was removed outside MonoCode. Run git worktree prune in the repository, then try opening it again.".into());
        }
        if let Some(commit) = entry.creation_oid.as_deref() {
            if entry.removed
                || entry.active_retirement_plan_id.is_some()
                || entry.pending_retirement_plan_id.is_some()
            {
                return Err("Saved worktree creation state conflicts with retirement state".into());
            }
            if ref_oid(
                Path::new(&entry.repo),
                &format!("refs/heads/{}", entry.branch),
            )?
            .is_some()
            {
                return Err(
                    "The worktree branch was created after the saved creation intent; refusing to claim it"
                        .into(),
                );
            }
            let source_ref = creation_ref(&entry.id, commit);
            if direct_ref_oid(Path::new(&entry.repo), &source_ref)?.as_deref() != Some(commit) {
                return Err("The saved creation recovery ref is missing or changed".into());
            }
            git(
                Path::new(&entry.repo),
                &[
                    "worktree",
                    "add",
                    "-b",
                    &entry.branch,
                    "--",
                    &entry.path,
                    &source_ref,
                ],
            )?;
            verify_created_worktree(entry, commit)?;
        } else {
            reconcile_interrupted_removal(conn, entry)?;
            let recovery = latest_local_recovery(conn, &entry.id)?;
            let (commit, source_ref, recovery_plan_id) = match recovery {
                Some(recovery) => {
                    if direct_ref_oid(Path::new(&entry.repo), &recovery.recovery_ref)?.as_deref()
                        != Some(&recovery.commit_oid)
                    {
                        return Err("Recorded recovery ref is missing or changed".into());
                    }
                    (
                        recovery.commit_oid,
                        recovery.recovery_ref,
                        Some(recovery.plan_id),
                    )
                }
                None => {
                    let reference = format!("refs/heads/{}", entry.branch);
                    let commit = ref_oid(Path::new(&entry.repo), &reference)?
                        .ok_or("The preserved branch and recovery ref are missing")?;
                    (commit, reference, None)
                }
            };
            let (branch, create_branch) =
                recovered_branch(Path::new(&entry.repo), entry, &commit, &all)?;
            environment::mark_pending(
                conn,
                entry,
                environment::SetupOrigin::Restored,
                None,
                recovery_plan_id.as_deref(),
            )?;
            if create_branch {
                git(
                    Path::new(&entry.repo),
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &branch,
                        "--",
                        &entry.path,
                        &source_ref,
                    ],
                )?;
            } else {
                git(
                    Path::new(&entry.repo),
                    &["worktree", "add", "--", &entry.path, &branch],
                )?;
            }
            if resolve_commit(Path::new(&entry.path), "HEAD")? != commit {
                return Err("Restored worktree does not match its recovery commit".into());
            }
            restored_branch = branch;
        }
    } else {
        let checkout = registered.ok_or("Worktree path is occupied by a different checkout")?;
        if repository(&entry.path)?.1 != entry.common {
            return Err("Worktree path is occupied by a different checkout".into());
        }
        let actual_branch = checkout
            .branch
            .as_deref()
            .ok_or("Managed worktree HEAD is detached")?;
        let actual_head = resolve_commit(Path::new(&entry.path), "HEAD")?;
        if ref_oid(
            Path::new(&entry.repo),
            &format!("refs/heads/{actual_branch}"),
        )?
        .as_deref()
            != Some(&actual_head)
        {
            return Err("Managed worktree branch does not match its HEAD".into());
        }
        if entry.removed {
            let expected = latest_local_recovery(conn, &entry.id)?
                .map(|recovery| recovery.commit_oid)
                .unwrap_or_else(|| actual_head.clone());
            let recovered_prefix = format!(
                "monocode/recovered-{}-{}",
                slug(&entry.id),
                expected.chars().take(10).collect::<String>()
            );
            if actual_head != expected
                || (actual_branch != entry.branch && !actual_branch.starts_with(&recovered_prefix))
            {
                return Err("Restored worktree does not match its durable recovery record".into());
            }
            restored_branch = actual_branch.into();
        } else if actual_branch != entry.branch {
            return Err("Managed worktree branch changed".into());
        } else if entry
            .creation_oid
            .as_deref()
            .is_some_and(|commit| commit != actual_head)
        {
            return Err("Created worktree does not match its saved base commit".into());
        }
        if entry.creation_oid.is_none() {
            cancel_pending_removal_for_present_checkout(conn, entry)?;
        }
    }
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE managed_worktrees
            SET removed = 0, last_used = ?1, branch = ?2, creation_oid = NULL
          WHERE id = ?3",
        params![now(), restored_branch, entry.id],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE sessions SET branch = ?1
          WHERE id = ?2 OR COALESCE(worktree_cwd, cwd) = ?3 OR
            substr(COALESCE(worktree_cwd, cwd), 1, length(?3) + 1) = ?3 || '/'",
        params![restored_branch, entry.id, entry.path],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn worktree_list(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
) -> Result<WorktreeOverview, String> {
    let common = repository_common(&cwd)?;
    let _repository = host.repository_guard(&common)?;
    let windows = host.operation_guard()?;
    overview(
        &store.open_auxiliary_conn()?,
        &protected_windows(&app, &windows),
        &cwd,
    )
}

#[tauri::command(async)]
pub fn worktree_settings_set(
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
    settings: WorktreeSettings,
) -> Result<WorktreeSettings, String> {
    let common = repository_common(&cwd)?;
    let _repository = host.repository_guard(&common)?;
    let _windows = host.operation_guard()?;
    update_settings(&store.open_auxiliary_conn()?, &cwd, settings)
}

fn update_settings(
    conn: &Connection,
    cwd: &str,
    settings: WorktreeSettings,
) -> Result<WorktreeSettings, String> {
    let scope = environment::scope_for_cwd(cwd)?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let environment = environment::save_settings(&tx, &scope, &settings.environment)?;
    // Older databases require retention_days. Keep a compatibility value in
    // that column; retirement is always explicitly reviewed and has no timer.
    tx.execute(
        "INSERT INTO worktree_project_settings
           (common_dir, project_path, isolate_by_default, retention_days)
         VALUES (?1, ?2, ?3, 7)
         ON CONFLICT(common_dir, project_path) DO UPDATE SET
           isolate_by_default = excluded.isolate_by_default",
        params![scope.common, scope.relative, settings.isolate_by_default],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(WorktreeSettings {
        environment,
        ..settings
    })
}

#[tauri::command(async)]
pub fn worktree_create(
    window: WebviewWindow,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
    session_id: String,
    name: String,
    base_ref: Option<String>,
) -> Result<String, String> {
    let common = repository_common(&cwd)?;
    let _repository = host.repository_guard(&common)?;
    let mut windows = host.operation_guard()?;
    let entry = create(
        &store.open_auxiliary_conn()?,
        &host,
        &cwd,
        &session_id,
        &name,
        base_ref.as_deref(),
    )?;
    windows
        .entry(window.label().into())
        .or_default()
        .push(PathBuf::from(&entry.path));
    scoped_path(&cwd, &entry.path)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareWorktree {
    cwd: String,
    session_id: String,
    path: Option<String>,
    name: String,
    create_new: bool,
    #[serde(default)]
    use_worktree: Option<bool>,
    #[serde(default)]
    base_ref: Option<String>,
    #[serde(default)]
    auto_name_token: Option<String>,
}

#[tauri::command(async)]
pub fn worktree_prepare(
    window: WebviewWindow,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    request: PrepareWorktree,
) -> Result<Option<String>, String> {
    let common = repository_common(&request.cwd)?;
    let _repository = host.repository_guard(&common)?;
    let mut windows = host.operation_guard()?;
    let conn = store.open_auxiliary_conn()?;
    for changed in naming::reconcile_repository(&conn, &common)? {
        naming::emit(window.app_handle(), Ok(Some(changed)));
    }
    let work_path = prepare(&conn, &host, request)?;
    if let Some(path) = &work_path {
        let leases = windows.entry(window.label().into()).or_default();
        let path = PathBuf::from(path);
        if !leases.contains(&path) {
            leases.push(path);
        }
    }
    Ok(work_path)
}
fn prepare(
    conn: &Connection,
    host: &WorktreeHost,
    request: PrepareWorktree,
) -> Result<Option<String>, String> {
    let PrepareWorktree {
        cwd,
        session_id,
        path,
        name,
        create_new,
        use_worktree,
        base_ref,
        auto_name_token,
    } = request;
    let records = owned(conn)?;
    let entry = records.iter().find(|v| {
        path.as_deref()
            .is_some_and(|path| path_inside(Path::new(path), Path::new(&v.path)))
            || (path.is_none() && v.id == session_id)
    });
    let work_path = if let Some(entry) = entry {
        if repository(&cwd)?.1 != entry.common {
            return Err("Worktree does not belong to this project".into());
        }
        open_owned(conn, entry)?;
        let target = if let Some(path) = path {
            let target = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
            let root = std::fs::canonicalize(&entry.path).map_err(|e| e.to_string())?;
            if !target.starts_with(root) || !target.is_dir() {
                return Err("Invalid worktree project folder".into());
            }
            path_to_js(&target)
        } else {
            scoped_path(&cwd, &entry.path)?
        };
        Some(target)
    } else if let Some(path) = path {
        let (_, common) = repository(&cwd)?;
        let canonical = path_to_js(
            &std::fs::canonicalize(&path).map_err(|e| format!("Worktree unavailable: {e}"))?,
        );
        if repository(&canonical)?.1 != common
            || !checkouts(&expand_home(&cwd))?
                .iter()
                .any(|v| path_inside(Path::new(&canonical), Path::new(&v.path)))
        {
            return Err("Worktree does not belong to this project".into());
        }
        Some(canonical)
    } else if create_new {
        let root = expand_home(&cwd);
        let probe = git_output(&root, &["rev-parse", "--is-inside-work-tree"])?;
        if !probe.status.success() {
            if use_worktree == Some(true) {
                return Err("Choose a Git repository to create a worktree".into());
            }
            return Ok(None);
        } // Plain folders remain supported.
        if !use_worktree.unwrap_or(settings(conn, &cwd)?.isolate_by_default) {
            return Ok(None);
        }
        Some(scoped_path(
            &cwd,
            &create_with_naming(
                conn,
                host,
                &cwd,
                &session_id,
                &name,
                base_ref.as_deref(),
                auto_name_token.as_deref(),
            )?
            .path,
        )?)
    } else {
        None
    };
    Ok(work_path)
}

#[tauri::command(async)]
pub fn worktree_heartbeat(
    window: WebviewWindow,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    paths: Vec<String>,
) -> Result<(), String> {
    let paths: Vec<PathBuf> = paths.into_iter().map(|p| expand_home(&p)).collect();
    let conn = store.open_auxiliary_conn()?;
    let records = owned(&conn)?;
    let repository_keys = records
        .iter()
        .filter(|entry| {
            paths
                .iter()
                .any(|path| path_inside(path, Path::new(&entry.path)))
        })
        .map(|entry| entry.common.clone())
        .collect();
    let _repositories = host.repository_guards(repository_keys)?;
    let mut windows = host.operation_guard()?;
    for entry in owned(&conn)? {
        if paths
            .iter()
            .any(|path| path_inside(path, Path::new(&entry.path)))
        {
            conn.execute(
                "UPDATE managed_worktrees SET last_used = ?1 WHERE id = ?2",
                params![now(), entry.id],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    windows.insert(window.label().into(), paths);
    Ok(())
}

#[tauri::command(async)]
pub fn worktree_pin(
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    id: String,
    pinned: bool,
) -> Result<(), String> {
    let conn = store.open_auxiliary_conn()?;
    let common = owned(&conn)?
        .into_iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.common);
    let _repository = common
        .as_deref()
        .map(|common| host.repository_guard(common))
        .transpose()?;
    let _windows = host.operation_guard()?;
    conn.execute(
        "UPDATE managed_worktrees SET pinned = ?1 WHERE id = ?2",
        params![pinned, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Build and persist a review snapshot. `session_ids` is used by the archive
/// flow; `cwd` + `ids` is the explicit Settings flow. Only currently eligible
/// records are returned as entries, so an empty entry list never opens a
/// destructive confirmation dialog.
#[tauri::command(async)]
pub fn worktree_retirement_plan(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    session_ids: Vec<String>,
    cwd: Option<String>,
    ids: Vec<String>,
) -> Result<WorktreeRetirementPlan, String> {
    let windows = {
        let windows = host.operation_guard()?;
        protected_windows(&app, &windows)
    };
    build_retirement_plan_coordinated(
        &store.open_auxiliary_conn()?,
        &host,
        &windows,
        &session_ids,
        cwd.as_deref(),
        &ids,
    )
}

/// Execute only choices from a persisted review snapshot. Every destructive
/// step rechecks repository identity, active leases/sessions and exact OIDs
/// while the lifecycle guard prevents a new app lease from being acquired.
#[tauri::command(async)]
pub fn worktree_retire(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    plan_id: String,
    selections: Vec<WorktreeRetirementSelection>,
) -> Result<WorktreeRetirementReport, String> {
    if plan_id.is_empty() || plan_id.contains(['\0', '\n', '\r']) {
        return Err("Invalid retirement plan identity".into());
    }
    let result = execute_retirement_coordinated(
        &app,
        &store.open_auxiliary_conn()?,
        &host,
        &plan_id,
        &selections,
    );
    let _ = app.emit("worktree-storage-changed", ());
    storage_maintenance::schedule(&app);
    result
}

#[tauri::command(async)]
pub fn worktree_storage_get(
    store: State<'_, SessionStore>,
) -> Result<storage::RecoveryStorageUsage, String> {
    storage::usage(&store.open_auxiliary_conn()?)
}

#[tauri::command(async)]
pub fn worktree_storage_limit_set(
    app: AppHandle,
    store: State<'_, SessionStore>,
    limit_bytes: u64,
    expected_version: i64,
) -> Result<storage::RecoveryStorageUsage, String> {
    let usage = storage::set_limit(&store.open_auxiliary_conn()?, limit_bytes, expected_version)?;
    let _ = app.emit("worktree-storage-changed", ());
    storage_maintenance::schedule(&app);
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    pub(super) struct Fixture {
        pub(super) dir: PathBuf,
        pub(super) repo: PathBuf,
        pub(super) db: PathBuf,
        pub(super) conn: Connection,
        pub(super) host: WorktreeHost,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "monocode-worktree-test-{}-{}-{}",
                std::process::id(),
                now(),
                SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ));
            let repo = dir.join("repo with spaces");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "--initial-branch=main"]).unwrap();
            git(&repo, &["config", "user.name", "Worktree Test"]).unwrap();
            git(&repo, &["config", "user.email", "worktree@example.invalid"]).unwrap();
            git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
            git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
            git(
                &repo,
                &[
                    "config",
                    "core.hooksPath",
                    &path_to_js(&dir.join("no-hooks")),
                ],
            )
            .unwrap();
            std::fs::write(repo.join("tracked.txt"), "original\n").unwrap();
            std::fs::write(repo.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
            git(&repo, &["add", "."]).unwrap();
            git(&repo, &["commit", "-m", "Initial"]).unwrap();
            let db = dir.join("worktrees.sqlite");
            let conn = Connection::open(&db).unwrap();
            schema(&conn).unwrap();
            conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, worktree_cwd TEXT, branch TEXT, archived INTEGER DEFAULT 0, pinned INTEGER DEFAULT 0)").unwrap();
            let host = WorktreeHost {
                root: dir.join("owned"),
                windows: Mutex::new(HashMap::new()),
                repositories: RepositoryReservations::default(),
            };
            Self {
                dir,
                repo,
                db,
                conn,
                host,
            }
        }
        fn create(&self, id: &str) -> Owned {
            let entry = create(
                &self.conn,
                &self.host,
                &path_to_js(&self.repo),
                id,
                "Fix / a thing!",
                Some("main"),
            )
            .unwrap();
            if let environment::BeginSetup::Run(operation) =
                environment::begin_setup(&self.conn, &entry.path).unwrap()
            {
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(&self.conn, &operation, &result).unwrap();
                result.unwrap();
            }
            entry
        }
        fn reason(&self, entry: &Owned) -> Option<String> {
            blocked(&self.conn, &HashMap::new(), entry).unwrap()
        }
        fn expire(&self, entry: &Owned) {
            self.conn
                .execute(
                    "UPDATE managed_worktrees SET last_used = 0 WHERE id = ?1",
                    [&entry.id],
                )
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn project_preferences_are_isolated_and_stale_saves_are_atomic() {
        let fixture = Fixture::new();
        let web = fixture.repo.join("apps/web");
        let api = fixture.repo.join("apps/api");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::create_dir_all(&api).unwrap();
        let web = path_to_js(&web);
        let api = path_to_js(&api);
        let mut first = settings(&fixture.conn, &web).unwrap();
        first.isolate_by_default = false;
        first.environment.setup_command = "npm ci".into();
        let stale = first.clone();
        let saved = update_settings(&fixture.conn, &web, first).unwrap();
        assert_eq!(saved.environment.environment_version, 1);
        assert!(!settings(&fixture.conn, &web).unwrap().isolate_by_default);
        let sibling = settings(&fixture.conn, &api).unwrap();
        assert!(sibling.isolate_by_default);
        assert!(sibling.environment.setup_command.is_empty());
        assert_eq!(sibling.environment.environment_version, 0);
        let mut newer = saved;
        newer.isolate_by_default = true;
        newer.environment.setup_command = "pnpm install".into();
        update_settings(&fixture.conn, &web, newer).unwrap();
        assert!(update_settings(&fixture.conn, &web, stale)
            .unwrap_err()
            .contains("WORKTREE_SETTINGS_CONFLICT"));
        let current = settings(&fixture.conn, &web).unwrap();
        assert!(current.isolate_by_default);
        assert_eq!(current.environment.setup_command, "pnpm install");
        assert_eq!(current.environment.environment_version, 2);
    }

    #[test]
    fn creates_isolated_checkout_and_preserves_source_changes() {
        let fixture = Fixture::new();
        std::fs::write(fixture.repo.join("tracked.txt"), "source changes\n").unwrap();
        let entry = fixture.create("session-one");
        assert_eq!(
            std::fs::read_to_string(Path::new(&entry.path).join("tracked.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.repo.join("tracked.txt")).unwrap(),
            "source changes\n"
        );
        assert_eq!(
            git(&fixture.repo, &["branch", "--show-current"]).unwrap(),
            "main"
        );
        assert_eq!(
            git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
            entry.branch
        );
        assert_eq!(
            repository(&entry.path).unwrap().1,
            repository(&path_to_js(&fixture.repo)).unwrap().1
        );
    }

    #[test]
    fn cleanup_retains_branch_and_reopening_restores_checkout() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        fixture.expire(&entry);
        let report = cleanup(&fixture.conn, &HashMap::new(), None, None).unwrap();
        assert_eq!(
            report.removed,
            vec![entry.path.clone()],
            "{:?}",
            report.skipped
        );
        assert!(!Path::new(&entry.path).exists());
        assert!(resolve_commit(&fixture.repo, &entry.branch).is_ok());
        open_owned(&fixture.conn, &entry).unwrap();
        assert!(Path::new(&entry.path).join("tracked.txt").exists());
        assert!(!owned(&fixture.conn).unwrap()[0].removed);
    }

    #[test]
    fn reviewed_local_retirement_survives_restart_and_git_gc() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
        let plan = build_retirement_plan(
            &fixture.conn,
            &HashMap::new(),
            &[],
            Some(fixture.repo.to_str().unwrap()),
            std::slice::from_ref(&entry.id),
        )
        .unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert!(!plan.entries[0].worktree_removed);
        assert!(plan.entries[0].local_branch.allowed);
        let report = execute_retirement(
            &fixture.conn,
            &HashMap::new(),
            &plan.plan_id,
            &[WorktreeRetirementSelection {
                id: entry.id.clone(),
                delete_local_branch: true,
                delete_remote_branch: false,
            }],
        )
        .unwrap();
        assert_eq!(report.results.len(), 1);
        let result = &report.results[0];
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(result.worktree_removed);
        assert!(result.local_branch_deleted);
        let recovery_ref = result.recovery_ref.as_deref().unwrap();
        assert_eq!(
            direct_ref_oid(&fixture.repo, recovery_ref)
                .unwrap()
                .as_deref(),
            Some(tip.as_str())
        );
        git(
            &fixture.repo,
            &["reflog", "expire", "--expire=now", "--all"],
        )
        .unwrap();
        git(&fixture.repo, &["gc", "--prune=now"]).unwrap();
        assert_eq!(
            direct_ref_oid(&fixture.repo, recovery_ref)
                .unwrap()
                .as_deref(),
            Some(tip.as_str())
        );

        let restarted = Connection::open(&fixture.db).unwrap();
        schema(&restarted).unwrap();
        let retired = owned(&restarted)
            .unwrap()
            .into_iter()
            .find(|owned| owned.id == entry.id)
            .unwrap();
        open_owned(&restarted, &retired).unwrap();
        assert_eq!(resolve_commit(Path::new(&entry.path), "HEAD").unwrap(), tip);
    }

    #[test]
    fn archive_plan_is_silent_when_another_conversation_uses_the_checkout() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        fixture
            .conn
            .execute(
                "INSERT INTO sessions (id, cwd, worktree_cwd, archived, pinned)
                 VALUES (?1, ?2, ?3, 1, 0), ('other-session', ?2, ?3, 0, 0)",
                params![entry.id, entry.repo, entry.path],
            )
            .unwrap();
        let plan = build_retirement_plan(
            &fixture.conn,
            &HashMap::new(),
            std::slice::from_ref(&entry.id),
            None,
            &[],
        )
        .unwrap();
        assert!(plan.entries.is_empty());
        assert!(plan.kept.is_empty());
        assert!(Path::new(&entry.path).is_dir());
    }

    #[test]
    fn retains_tracked_untracked_and_ignored_local_data() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let root = Path::new(&entry.path);
        for file in ["tracked.txt", "new.txt", ".env"] {
            std::fs::write(root.join(file), "do not delete\n").unwrap();
            assert!(fixture.reason(&entry).unwrap().contains(file));
            let report = cleanup(&fixture.conn, &HashMap::new(), None, None).unwrap();
            assert!(report.removed.is_empty());
            if file == "tracked.txt" {
                git(root, &["restore", "tracked.txt"]).unwrap();
            } else {
                std::fs::remove_file(root.join(file)).unwrap();
            }
        }
    }

    #[test]
    fn unmerged_commits_do_not_block_clean_checkout_cleanup() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let root = Path::new(&entry.path);
        std::fs::write(root.join("tracked.txt"), "feature\n").unwrap();
        git(root, &["commit", "-am", "Feature"]).unwrap();
        assert_eq!(fixture.reason(&entry), None);
        fixture.expire(&entry);
        let report = cleanup(&fixture.conn, &HashMap::new(), None, None).unwrap();
        assert_eq!(report.removed, vec![entry.path.clone()]);
        assert!(resolve_commit(&fixture.repo, &entry.branch).is_ok());
    }

    #[test]
    fn pins_windows_and_shared_conversations_block_cleanup() {
        let fixture = Fixture::new();
        let mut entry = fixture.create("session-one");
        entry.pinned = true;
        assert_eq!(fixture.reason(&entry).as_deref(), Some("Pinned"));
        entry.pinned = false;
        let windows = HashMap::from([(
            "other-window".into(),
            vec![Path::new(&entry.path).join("tracked.txt")],
        )]);
        assert!(blocked(&fixture.conn, &windows, &entry)
            .unwrap()
            .unwrap()
            .contains("Open in a window"));
        fixture
            .conn
            .execute(
                "INSERT INTO sessions (id, cwd, worktree_cwd) VALUES ('shared-session', ?1, ?2)",
                params![entry.repo, entry.path],
            )
            .unwrap();
        assert!(fixture.reason(&entry).unwrap().contains("Archive"));
        fixture
            .conn
            .execute("UPDATE sessions SET archived = 1, pinned = 1", [])
            .unwrap();
        assert!(fixture.reason(&entry).is_some());
        fixture
            .conn
            .execute("UPDATE sessions SET pinned = 0", [])
            .unwrap();
        assert_eq!(fixture.reason(&entry), None);
        fixture
            .conn
            .execute(
                "UPDATE sessions SET cwd = ?1, worktree_cwd = NULL, archived = 0",
                [format!("{}/nested", entry.path)],
            )
            .unwrap();
        assert!(fixture.reason(&entry).is_some());
    }

    #[test]
    fn external_primary_locked_and_detached_checkouts_are_preserved() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let external_name = if cfg!(windows) {
            "external with spaces"
        } else {
            "external\nwith newline"
        };
        let external = path_to_js(&fixture.dir.join(external_name));
        git(
            &fixture.repo,
            &["worktree", "add", "-b", "external", &external, "main"],
        )
        .unwrap();
        let external = path_to_js(&std::fs::canonicalize(&external).unwrap());
        let list = overview(&fixture.conn, &HashMap::new(), &entry.path).unwrap();
        assert!(list.entries[0].main);
        assert!(list
            .entries
            .iter()
            .any(|v| v.path == external && v.id.is_none()));
        git(&fixture.repo, &["worktree", "lock", &entry.path]).unwrap();
        assert!(fixture.reason(&entry).unwrap().contains("locked"));
        git(&fixture.repo, &["worktree", "unlock", &entry.path]).unwrap();
        git(Path::new(&entry.path), &["checkout", "--detach"]).unwrap();
        assert!(fixture.reason(&entry).unwrap().contains("detached"));
        let report = cleanup(&fixture.conn, &HashMap::new(), None, None).unwrap();
        assert!(report.removed.is_empty());
        assert!(Path::new(&external).exists());
        assert!(fixture.repo.exists());
    }

    #[test]
    fn old_automatic_preferences_do_not_enable_unattended_removal() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        fixture.expire(&entry);
        fixture
            .conn
            .execute(
                "INSERT INTO worktree_settings VALUES (?1, 1, 1, 7)",
                [&entry.common],
            )
            .unwrap();
        let list = overview(&fixture.conn, &HashMap::new(), &entry.path).unwrap();
        assert!(list.settings.isolate_by_default);
        assert!(list
            .entries
            .iter()
            .any(|item| item.id.as_deref() == Some(&entry.id) && item.blocked_reason.is_none()));
        assert!(Path::new(&entry.path).exists());
        assert!(cleanup(&fixture.conn, &HashMap::new(), None, Some(&[]))
            .unwrap()
            .removed
            .is_empty());
        assert!(Path::new(&entry.path).exists());
    }

    #[test]
    fn manual_cleanup_only_removes_reviewed_ids_and_rechecks_git() {
        let fixture = Fixture::new();
        let first = fixture.create("session-one");
        let second = fixture.create("session-two");
        let ids = vec![first.id.clone()];
        std::fs::write(
            Path::new(&first.path).join("new.txt"),
            "created after preview",
        )
        .unwrap();
        assert!(cleanup(&fixture.conn, &HashMap::new(), None, Some(&ids))
            .unwrap()
            .removed
            .is_empty());
        std::fs::remove_file(Path::new(&first.path).join("new.txt")).unwrap();
        assert_eq!(
            cleanup(&fixture.conn, &HashMap::new(), None, Some(&ids))
                .unwrap()
                .removed,
            vec![first.path]
        );
        assert!(Path::new(&second.path).exists());
    }

    #[test]
    fn invalid_refs_and_duplicate_identity_do_not_change_checkouts() {
        let fixture = Fixture::new();
        for base in ["--help", "missing-branch"] {
            assert!(create(
                &fixture.conn,
                &fixture.host,
                &path_to_js(&fixture.repo),
                "session-one",
                "test",
                Some(base)
            )
            .is_err());
        }
        assert!(owned(&fixture.conn).unwrap().is_empty());
        assert!(validate_id("../escape").is_err());
        let entry = fixture.create("session-one");
        assert!(create(
            &fixture.conn,
            &fixture.host,
            &entry.repo,
            "session-one",
            "again",
            None
        )
        .is_err());
        assert!(Path::new(&entry.path).exists());
        assert_eq!(checkouts(&fixture.repo).unwrap().len(), 2);
    }

    #[test]
    fn recovery_never_overwrites_a_replacement_directory_and_restores_a_deleted_branch() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
        remove_checkout(&fixture.conn, &HashMap::new(), &entry).unwrap();
        std::fs::create_dir(&entry.path).unwrap();
        std::fs::write(Path::new(&entry.path).join("valuable.txt"), "keep").unwrap();
        assert!(open_owned(&fixture.conn, &entry).is_err());
        assert!(Path::new(&entry.path).join("valuable.txt").exists());
        std::fs::remove_dir_all(&entry.path).unwrap();
        git(&fixture.repo, &["branch", "-d", &entry.branch]).unwrap();
        open_owned(&fixture.conn, &entry).unwrap();
        assert_eq!(resolve_commit(Path::new(&entry.path), "HEAD").unwrap(), tip);
    }

    #[test]
    fn recovery_restores_exact_tip_on_a_new_branch_when_the_name_was_reused() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let preserved_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
        remove_checkout(&fixture.conn, &HashMap::new(), &entry).unwrap();
        git(&fixture.repo, &["branch", "-d", &entry.branch]).unwrap();
        std::fs::write(fixture.repo.join("later.txt"), "later\n").unwrap();
        git(&fixture.repo, &["add", "later.txt"]).unwrap();
        git(&fixture.repo, &["commit", "-m", "Later main work"]).unwrap();
        let reused_tip = resolve_commit(&fixture.repo, "HEAD").unwrap();
        git(&fixture.repo, &["branch", &entry.branch, &reused_tip]).unwrap();

        open_owned(&fixture.conn, &entry).unwrap();
        let restored = owned(&fixture.conn)
            .unwrap()
            .into_iter()
            .find(|owned| owned.id == entry.id)
            .unwrap();
        assert_ne!(restored.branch, entry.branch);
        assert!(restored.branch.starts_with("monocode/recovered-"));
        assert_eq!(
            resolve_commit(Path::new(&restored.path), "HEAD").unwrap(),
            preserved_tip
        );
        assert_eq!(
            ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
                .unwrap()
                .as_deref(),
            Some(reused_tip.as_str())
        );
    }
    #[test]
    fn preparation_is_idempotent_and_respects_repository_defaults() {
        let fixture = Fixture::new();
        let request = || PrepareWorktree {
            auto_name_token: None,
            cwd: path_to_js(&fixture.repo),
            session_id: "session-one".into(),
            path: None,
            name: "Task".into(),
            create_new: true,
            use_worktree: None,
            base_ref: None,
        };
        let first = prepare(&fixture.conn, &fixture.host, request())
            .unwrap()
            .unwrap();
        let second = prepare(&fixture.conn, &fixture.host, request())
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(checkouts(&fixture.repo).unwrap().len(), 2);
        let common = repository(&first).unwrap().1;
        fixture
            .conn
            .execute(
                "INSERT INTO worktree_settings VALUES (?1, 0, 1, 7)",
                [common],
            )
            .unwrap();
        let mut local = request();
        local.session_id = "session-two".into();
        assert_eq!(prepare(&fixture.conn, &fixture.host, local).unwrap(), None);
        let folder = fixture.dir.join("plain folder");
        std::fs::create_dir(&folder).unwrap();
        let mut plain = request();
        plain.cwd = path_to_js(&folder);
        plain.session_id = "session-three".into();
        assert_eq!(prepare(&fixture.conn, &fixture.host, plain).unwrap(), None);
    }

    #[test]
    fn repository_reservations_block_only_the_same_repository() {
        use std::sync::{mpsc, Arc};

        let host = Arc::new(WorktreeHost {
            root: PathBuf::new(),
            windows: Mutex::new(HashMap::new()),
            repositories: RepositoryReservations::default(),
        });
        let held = host.repository_guard("repo-a").unwrap();
        let (same_tx, same_rx) = mpsc::channel();
        let same_host = Arc::clone(&host);
        let same = std::thread::spawn(move || {
            let _guard = same_host.repository_guard("repo-a").unwrap();
            same_tx.send(()).unwrap();
        });
        let (other_tx, other_rx) = mpsc::channel();
        let other_host = Arc::clone(&host);
        let other = std::thread::spawn(move || {
            let _guard = other_host.repository_guard("repo-b").unwrap();
            other_tx.send(()).unwrap();
        });

        other_rx.recv_timeout(Duration::from_millis(250)).unwrap();
        assert!(same_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        same_rx.recv_timeout(Duration::from_millis(250)).unwrap();
        same.join().unwrap();
        other.join().unwrap();
    }

    #[test]
    fn explicit_draft_choice_uses_selected_base_and_overrides_default() {
        let fixture = Fixture::new();
        let cwd = path_to_js(&std::fs::canonicalize(&fixture.repo).unwrap());
        let base = git(&fixture.repo, &["rev-parse", "HEAD"]).unwrap();
        git(&fixture.repo, &["branch", "start-here"]).unwrap();
        std::fs::write(fixture.repo.join("later.txt"), "later").unwrap();
        git(&fixture.repo, &["add", "."]).unwrap();
        git(&fixture.repo, &["commit", "-m", "Later on main"]).unwrap();
        let common = repository(&cwd).unwrap().1;
        fixture
            .conn
            .execute(
                "INSERT INTO worktree_settings VALUES (?1, 0, 1, 7)",
                [&common],
            )
            .unwrap();
        let request = || PrepareWorktree {
            auto_name_token: None,
            cwd: cwd.clone(),
            session_id: "draft-one".into(),
            path: None,
            name: "Task".into(),
            create_new: true,
            use_worktree: Some(true),
            base_ref: Some("start-here".into()),
        };
        let path = prepare(&fixture.conn, &fixture.host, request())
            .unwrap()
            .unwrap();
        assert_eq!(git(Path::new(&path), &["rev-parse", "HEAD"]).unwrap(), base);
        assert_eq!(
            git(&fixture.repo, &["branch", "--show-current"]).unwrap(),
            "main"
        );
        let mut local = request();
        local.session_id = "draft-two".into();
        local.create_new = false;
        local.use_worktree = Some(false);
        local.path = Some(cwd.clone());
        assert_eq!(
            prepare(&fixture.conn, &fixture.host, local).unwrap(),
            Some(cwd.clone())
        );
        let mut reuse = request();
        reuse.session_id = "draft-three".into();
        reuse.create_new = false;
        reuse.path = Some(path.clone());
        assert_eq!(
            prepare(&fixture.conn, &fixture.host, reuse).unwrap(),
            Some(path)
        );
        assert_eq!(checkouts(&fixture.repo).unwrap().len(), 2);
    }

    #[test]
    fn preparation_preserves_subproject_cwd_and_rejects_foreign_repository() {
        let fixture = Fixture::new();
        let nested = fixture.repo.join("packages/app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("file.txt"), "app").unwrap();
        git(&fixture.repo, &["add", "."]).unwrap();
        git(&fixture.repo, &["commit", "-m", "Subproject"]).unwrap();
        let request = || PrepareWorktree {
            auto_name_token: None,
            cwd: path_to_js(&nested),
            session_id: "session-one".into(),
            path: None,
            name: "Task".into(),
            create_new: true,
            use_worktree: None,
            base_ref: None,
        };
        let path = prepare(&fixture.conn, &fixture.host, request())
            .unwrap()
            .unwrap();
        assert!(path.ends_with("/packages/app"));
        let mut resume = request();
        resume.path = Some(path.clone());
        resume.create_new = false;
        assert_eq!(
            prepare(&fixture.conn, &fixture.host, resume).unwrap(),
            Some(path.clone())
        );
        let other = Fixture::new();
        let mut foreign = request();
        foreign.cwd = path_to_js(&other.repo);
        foreign.path = Some(path);
        assert!(prepare(&fixture.conn, &fixture.host, foreign)
            .unwrap_err()
            .contains("does not belong"));
    }
    #[test]
    fn creation_does_not_claim_a_preexisting_branch() {
        let fixture = Fixture::new();
        let branch = "monocode/fix-a-thing-session-one";
        git(&fixture.repo, &["branch", branch]).unwrap();
        assert!(create(
            &fixture.conn,
            &fixture.host,
            &path_to_js(&fixture.repo),
            "session-one",
            "Fix / a thing!",
            Some("main")
        )
        .is_err());
        assert!(owned(&fixture.conn).unwrap().is_empty());
        assert!(resolve_commit(&fixture.repo, branch).is_ok());
    }
}

#[cfg(test)]
#[path = "worktree_retirement_tests.rs"]
mod retirement_tests;
