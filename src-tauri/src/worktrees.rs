//! Owned worktree lifecycle. Git is the source of truth; SQLite records ownership,
//! retention and recovery information. Cleanup never force-removes or drops refs.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, WebviewWindow};

use crate::fs::{expand_home, path_to_js as fs_path_to_js};
use crate::session_store::SessionStore;

pub struct WorktreeHost {
    root: PathBuf,
    // All lifecycle operations and window leases share one lock. A checkout
    // cannot be opened between the last safety check and git worktree remove.
    windows: Mutex<HashMap<String, Vec<PathBuf>>>,
}

impl WorktreeHost {
    pub(crate) fn operation_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<PathBuf>>>, String> {
        self.windows.lock().map_err(|e| e.to_string())
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
        ]
        .concat(),
    );
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSettings {
    pub isolate_by_default: bool,
    pub auto_cleanup: bool,
    pub retention_days: i64,
}

impl Default for WorktreeSettings {
    fn default() -> Self {
        Self {
            isolate_by_default: true,
            auto_cleanup: false,
            retention_days: 7,
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEntry {
    pub id: Option<String>,
    pub path: String,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub main: bool,
    pub pinned: bool,
    pub missing: bool,
    pub last_used: Option<i64>,
    pub blocked_reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeOverview {
    pub repo: String,
    pub settings: WorktreeSettings,
    pub entries: Vec<WorktreeEntry>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
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
            removed INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS worktree_settings (
            common_dir TEXT PRIMARY KEY, isolate_by_default INTEGER NOT NULL,
            auto_cleanup INTEGER NOT NULL, retention_days INTEGER NOT NULL
         );",
    )
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
    });
    // Cleanup is only invoked with the IDs the user reviewed and confirmed.
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    let mut cmd = Command::new("git");
    crate::hide_window_console(&mut cmd);
    cmd.arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd.output().map_err(|e| format!("Could not run Git: {e}"))
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

fn settings(conn: &Connection, common: &str) -> Result<WorktreeSettings, String> {
    conn.query_row("SELECT isolate_by_default, auto_cleanup, retention_days FROM worktree_settings WHERE common_dir = ?1", [common], |row| {
        Ok(WorktreeSettings { isolate_by_default: row.get(0)?, auto_cleanup: false, retention_days: row.get(2)? })
    }).optional().map(|v| v.unwrap_or_default()).map_err(|e| e.to_string())
}

fn owned(conn: &Connection) -> Result<Vec<Owned>, String> {
    let mut stmt = conn.prepare("SELECT id, repo, common_dir, path, branch, base_ref, pinned, last_used, removed FROM managed_worktrees ORDER BY last_used DESC").map_err(|e| e.to_string())?;
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
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn path_inside(path: &Path, parent: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
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
    // Include ignored files: .env and other local-only data must never vanish
    // in an unattended sweep. Git's normal remove can discard ignored files.
    if !git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?
    .is_empty()
    {
        return Ok(Some("Contains changes, untracked or ignored files".into()));
    }
    let base = match resolve_commit(Path::new(&entry.repo), &entry.base_ref) {
        Ok(base) => base,
        Err(_) => {
            return Ok(Some(
                "Base ref unavailable; cannot verify merged commits".into(),
            ))
        }
    };
    if !git_output(root, &["merge-base", "--is-ancestor", "HEAD", &base])?
        .status
        .success()
    {
        return Ok(Some(format!("Commits not merged into {}", entry.base_ref)));
    }
    Ok(None)
}

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
        match remove_checkout(conn, &entry) {
            Ok(()) => report.removed.push(entry.path),
            Err(error) => report.skipped.push(format!("{}: {error}", entry.branch)),
        }
    }
    Ok(report)
}

fn remove_checkout(conn: &Connection, entry: &Owned) -> Result<(), String> {
    git(
        Path::new(&entry.repo),
        &["worktree", "remove", "--", &entry.path],
    )?;
    conn.execute(
        "UPDATE managed_worktrees SET removed = 1 WHERE id = ?1",
        [&entry.id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
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
            branch: checkout.branch.clone(),
            base_ref: record.map(|v| v.base_ref.clone()),
            main: index == 0,
            pinned: record.is_some_and(|v| v.pinned),
            missing: !Path::new(&checkout.path).is_dir(),
            last_used: record.map(|v| v.last_used),
            blocked_reason: reason,
        });
    }
    for entry in records
        .iter()
        .filter(|v| v.common == common && !all.iter().any(|c| c.path == v.path))
    {
        entries.push(WorktreeEntry {
            id: Some(entry.id.clone()),
            path: entry.path.clone(),
            branch: Some(entry.branch.clone()),
            base_ref: Some(entry.base_ref.clone()),
            main: false,
            pinned: entry.pinned,
            missing: true,
            last_used: Some(entry.last_used),
            blocked_reason: Some("Cleaned up; branch preserved for recovery".into()),
        });
    }
    Ok(WorktreeOverview {
        repo,
        settings: settings(conn, &common)?,
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
    validate_id(id)?;
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
    let branch = format!("monocode/{}-{}", slug(name), id);
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
    // Persist intent first, so interruption/crash cannot leave an unowned folder.
    conn.execute("INSERT INTO managed_worktrees (id, repo, common_dir, path, branch, base_ref, last_used) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![id, repo, common, path, branch, base_ref, now()]).map_err(|e| e.to_string())?;
    if let Err(error) = git(
        Path::new(&repo),
        &["worktree", "add", "-b", &branch, "--", &path, &commit],
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
    owned(conn)?
        .into_iter()
        .find(|v| v.id == id)
        .ok_or("Worktree metadata missing".into())
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

fn open_owned(conn: &Connection, entry: &Owned) -> Result<(), String> {
    let root = Path::new(&entry.path);
    let (_, common) = repository(&entry.repo)?;
    if common != entry.common {
        return Err("Repository identity changed".into());
    }
    let registered = checkouts(Path::new(&entry.repo))?
        .into_iter()
        .find(|v| v.path == entry.path);
    if !root.exists() {
        if registered.is_some() {
            return Err("Worktree was removed outside MonoCode. Run git worktree prune in the repository, then try opening it again.".into());
        }
        resolve_commit(
            Path::new(&entry.repo),
            &format!("refs/heads/{}", entry.branch),
        )?;
        git(
            Path::new(&entry.repo),
            &["worktree", "add", "--", &entry.path, &entry.branch],
        )?;
    } else if registered.is_none() || repository(&entry.path)?.1 != entry.common {
        return Err("Worktree path is occupied by a different checkout".into());
    }
    conn.execute(
        "UPDATE managed_worktrees SET removed = 0, last_used = ?1 WHERE id = ?2",
        params![now(), entry.id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command(async)]
pub fn worktree_list(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
) -> Result<WorktreeOverview, String> {
    let windows = host.windows.lock().map_err(|e| e.to_string())?;
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
) -> Result<(), String> {
    if !(1..=365).contains(&settings.retention_days) {
        return Err("Retention must be between 1 and 365 days".into());
    }
    let _windows = host.windows.lock().map_err(|e| e.to_string())?;
    let (_, common) = repository(&cwd)?;
    store.open_auxiliary_conn()?.execute("INSERT INTO worktree_settings VALUES (?1, ?2, ?3, ?4) ON CONFLICT(common_dir) DO UPDATE SET isolate_by_default = excluded.isolate_by_default, auto_cleanup = excluded.auto_cleanup, retention_days = excluded.retention_days", params![common, settings.isolate_by_default, false, settings.retention_days]).map_err(|e| e.to_string())?;
    Ok(())
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
    let mut windows = host.windows.lock().map_err(|e| e.to_string())?;
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
}

#[tauri::command(async)]
pub fn worktree_prepare(
    window: WebviewWindow,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    request: PrepareWorktree,
) -> Result<Option<String>, String> {
    let mut windows = host.windows.lock().map_err(|e| e.to_string())?;
    let work_path = prepare(&store.open_auxiliary_conn()?, &host, request)?;
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
        let (_, common) = repository(&cwd)?;
        if !use_worktree.unwrap_or(settings(conn, &common)?.isolate_by_default) {
            return Ok(None);
        }
        Some(scoped_path(
            &cwd,
            &create(conn, host, &cwd, &session_id, &name, base_ref.as_deref())?.path,
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
    let mut windows = host.windows.lock().map_err(|e| e.to_string())?;
    let paths: Vec<PathBuf> = paths.into_iter().map(|p| expand_home(&p)).collect();
    let conn = store.open_auxiliary_conn()?;
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
    let _windows = host.windows.lock().map_err(|e| e.to_string())?;
    store
        .open_auxiliary_conn()?
        .execute(
            "UPDATE managed_worktrees SET pinned = ?1 WHERE id = ?2",
            params![pinned, id],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command(async)]
pub fn worktree_cleanup(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
    ids: Vec<String>,
) -> Result<CleanupReport, String> {
    let windows = host.windows.lock().map_err(|e| e.to_string())?;
    let (_, common) = repository(&cwd)?;
    cleanup(
        &store.open_auxiliary_conn()?,
        &protected_windows(&app, &windows),
        Some(&common),
        Some(&ids),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        dir: PathBuf,
        repo: PathBuf,
        conn: Connection,
        host: WorktreeHost,
    }

    impl Fixture {
        fn new() -> Self {
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
            let conn = Connection::open_in_memory().unwrap();
            schema(&conn).unwrap();
            conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, worktree_cwd TEXT, archived INTEGER DEFAULT 0, pinned INTEGER DEFAULT 0)").unwrap();
            let host = WorktreeHost {
                root: dir.join("owned"),
                windows: Mutex::new(HashMap::new()),
            };
            Self {
                dir,
                repo,
                conn,
                host,
            }
        }
        fn create(&self, id: &str) -> Owned {
            create(
                &self.conn,
                &self.host,
                &path_to_js(&self.repo),
                id,
                "Fix / a thing!",
                Some("main"),
            )
            .unwrap()
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
        assert_eq!(report.removed, vec![entry.path.clone()]);
        assert!(!Path::new(&entry.path).exists());
        assert!(resolve_commit(&fixture.repo, &entry.branch).is_ok());
        open_owned(&fixture.conn, &entry).unwrap();
        assert!(Path::new(&entry.path).join("tracked.txt").exists());
        assert!(!owned(&fixture.conn).unwrap()[0].removed);
    }

    #[test]
    fn retains_tracked_untracked_and_ignored_local_data() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let root = Path::new(&entry.path);
        for file in ["tracked.txt", "new.txt", ".env"] {
            std::fs::write(root.join(file), "do not delete\n").unwrap();
            assert!(fixture.reason(&entry).unwrap().contains("Contains changes"));
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
    fn unmerged_commits_block_until_base_contains_them() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        let root = Path::new(&entry.path);
        std::fs::write(root.join("tracked.txt"), "feature\n").unwrap();
        git(root, &["commit", "-am", "Feature"]).unwrap();
        assert!(fixture.reason(&entry).unwrap().contains("not merged"));
        git(&fixture.repo, &["merge", "--ff-only", &entry.branch]).unwrap();
        assert_eq!(fixture.reason(&entry), None);
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
        let external = path_to_js(&fixture.dir.join("external\nwith newline"));
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
        assert!(!list.settings.auto_cleanup);
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
    fn recovery_never_overwrites_a_replacement_directory_or_deleted_branch() {
        let fixture = Fixture::new();
        let entry = fixture.create("session-one");
        remove_checkout(&fixture.conn, &entry).unwrap();
        std::fs::create_dir(&entry.path).unwrap();
        std::fs::write(Path::new(&entry.path).join("valuable.txt"), "keep").unwrap();
        assert!(open_owned(&fixture.conn, &entry).is_err());
        assert!(Path::new(&entry.path).join("valuable.txt").exists());
        std::fs::remove_dir_all(&entry.path).unwrap();
        git(&fixture.repo, &["branch", "-d", &entry.branch]).unwrap();
        assert!(open_owned(&fixture.conn, &entry).is_err());
        assert!(!Path::new(&entry.path).exists());
    }
    #[test]
    fn preparation_is_idempotent_and_respects_repository_defaults() {
        let fixture = Fixture::new();
        let request = || PrepareWorktree {
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
