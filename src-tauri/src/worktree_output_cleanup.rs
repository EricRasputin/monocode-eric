//! Manual, reviewed output cleanup. It never removes a checkout or a Git ref.
//! Plans are single-use; restart preserves partial results and requires a new
//! review. Preparation-needed state commits before the first filesystem write.
use super::*;
use rusqlite::{Transaction, TransactionBehavior};

#[path = "worktree_output_fs.rs"]
mod output_fs;
use output_fs::{estimated_bytes, is_link, Directory};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    path: String,
    estimated_bytes: u64,
    preserved_paths: Vec<String>,
    blocked_reason: Option<String>,
    identity: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    plan_id: String,
    id: String,
    path: String,
    branch: String,
    candidates: Vec<Candidate>,
    blocked_reason: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    review: Review,
    repo: String,
    common: String,
    project: String,
    root_identity: String,
    head: String,
    settings: environment::EnvironmentSettings,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemResult {
    path: String,
    estimated_removed_bytes: u64,
    error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    plan_id: String,
    id: String,
    path: String,
    status: String,
    #[serde(default)]
    selected_paths: Vec<String>,
    results: Vec<ItemResult>,
    estimated_removed_bytes: u64,
    observed_free_space_change: Option<i64>,
    measurement_error: Option<String>,
    preparation_needed: bool,
}

pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_output_cleanups (
        plan_id TEXT PRIMARY KEY, worktree_id TEXT NOT NULL, project_cwd TEXT NOT NULL,
        snapshot_json TEXT NOT NULL, status TEXT NOT NULL, report_json TEXT,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
    );",
    )
}

pub(super) fn reset_interrupted(conn: &Connection) -> Result<(), String> {
    conn.execute("UPDATE worktree_output_cleanups SET status = 'interrupted', updated_at = ?1 WHERE status = 'executing'", [now()]).map_err(|e| e.to_string())?;
    // There is deliberately no automatic deletion retry and no ready-state write.
    Ok(())
}

fn validate_entry(
    conn: &Connection,
    host: &WorktreeHost,
    windows: &HashMap<String, Vec<PathBuf>>,
    entry: &Owned,
) -> Result<(), String> {
    setup::validate_checkout(entry, &entry.path)?;
    let managed_root = std::fs::canonicalize(&host.root).map_err(|e| e.to_string())?;
    let root = Path::new(&entry.path);
    if !root.starts_with(&managed_root) || root == managed_root {
        return Err("Checkout is outside the managed worktree root".into());
    }
    if owned(conn)?
        .iter()
        .filter(|e| e.path == entry.path && !e.removed)
        .count()
        != 1
    {
        return Err("Checkout ownership is ambiguous".into());
    }
    if let Some(reason) = live_use_reason(conn, windows, entry, false)? {
        return Err(reason);
    }
    if entry.pending_retirement_plan_id.is_some() {
        return Err("Checkout retirement is pending; finish or review retirement first".into());
    }
    if checkouts(Path::new(&entry.repo))?
        .iter()
        .any(|c| c.path == entry.path && c.locked)
    {
        return Err("Git worktree is locked".into());
    }
    check_git_locks(entry)?;
    let state: Option<(String, String)> = conn
        .query_row(
            "SELECT status, origin FROM worktree_environment_setup WHERE worktree_id = ?1",
            [&entry.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if state.is_some_and(|(status, origin)| {
        status == "running" || (status != "ready" && origin != "cleaned")
    }) {
        return Err("Workspace setup must finish before output cleanup".into());
    }
    Ok(())
}

fn policy(
    conn: &Connection,
    entry: &Owned,
) -> Result<
    (
        environment::ProjectScope,
        environment::EnvironmentSettings,
        Vec<String>,
    ),
    String,
> {
    let scope = environment::scope_for_entry(conn, entry)?;
    if !scope.relative.is_empty() {
        environment::safe_metadata(Path::new(&entry.path), &scope.relative)?
            .filter(|m| m.is_dir())
            .ok_or("Originating project folder is unavailable")?;
    }
    let settings = environment::load_settings(conn, &scope)?;
    let root_settings = environment::settings_at_checkout_root(&scope, &settings);
    let mut paths = environment::recognized_output_paths(Path::new(&entry.path))?;
    paths.extend(root_settings.disposable_paths);
    paths.sort();
    paths.dedup();
    // Only the outermost directory is a candidate. Tracked files anywhere in
    // it block that entire candidate, never authorize a smaller surprise delete.
    let all = paths.clone();
    paths.retain(|p| {
        !all.iter()
            .any(|other| p != other && p.starts_with(&format!("{other}/")))
    });
    Ok((scope, settings, paths))
}

fn candidate_policy(root: &Path, path: &str) -> Result<(), String> {
    environment::validate_git_policy(root, path, true)?;
    let tracked_head = git(
        root,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            "HEAD",
            "--",
            &format!(":(literal){path}"),
        ],
    )?;
    if !tracked_head.is_empty() {
        return Err(format!("Candidate contains tracked files in HEAD: {path}"));
    }
    Ok(())
}

/// Sample Git identities once per candidate, then detect index/ref/policy
/// writes cheaply during its walk. Running Git for every dependency file would
/// turn a large node_modules cleanup into hours of subprocess work.
struct GitWatch {
    files: Vec<(PathBuf, Option<FileStamp>)>,
    locks: Vec<PathBuf>,
}
impl GitWatch {
    fn new(entry: &Owned) -> Result<Self, String> {
        let root = Path::new(&entry.path);
        let private = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?);
        let mut files = vec![root.join(".git")];
        let mut locks = Vec::new();
        for directory in [PathBuf::from(&entry.common), private] {
            for name in [
                "index",
                "HEAD",
                "packed-refs",
                "config",
                "shallow",
                "commondir",
                "gitdir",
            ] {
                files.push(directory.join(name));
                locks.push(directory.join(format!("{name}.lock")));
            }
            files.push(directory.join(format!("refs/heads/{}", entry.branch)));
            locks.push(directory.join(format!("refs/heads/{}.lock", entry.branch)));
            files.push(directory.join("info/exclude"));
        }
        for path in git(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--",
                ":(glob)**/.gitignore",
                ":(glob)**/package.json",
                ":(glob)**/Cargo.toml",
                ":(glob)**/tauri.conf.json",
                ":(glob)**/tauri.conf.json5",
                ":(glob)**/Tauri.toml",
            ],
        )?
        .split_terminator('\0')
        {
            files.push(root.join(path));
        }
        let files = files
            .into_iter()
            .map(|p| fingerprint(&p).map(|m| (p, m)))
            .collect::<Result<_, _>>()?;
        Ok(Self { files, locks })
    }
    fn check(&self) -> Result<(), String> {
        for lock in &self.locks {
            if std::fs::symlink_metadata(lock).is_ok() {
                return Err("Git lock appeared during cleanup; remaining outputs kept".into());
            }
        }
        for (path, previous) in &self.files {
            if &fingerprint(path)? != previous {
                return Err(
                    "Git index, branch or disposal policy changed during cleanup; review again"
                        .into(),
                );
            }
        }
        Ok(())
    }
}
#[derive(PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: std::time::SystemTime,
    identity: String,
}
fn fingerprint(path: &Path) -> Result<Option<FileStamp>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(FileStamp {
            bytes: m.len(),
            modified: m.modified().map_err(|e| e.to_string())?,
            identity: output_fs::path_identity(path, &m)?,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn selected_path(path: &Path, selected: &str, ancestor: bool) -> bool {
    // Conservative on case-sensitive filesystems too: case aliases must not
    // erase selected configuration on the default macOS/Windows filesystems.
    let path = path.to_string_lossy().replace('\\', "/").to_lowercase();
    let selected = selected.to_lowercase();
    path == selected || (ancestor && selected.starts_with(&format!("{path}/")))
}

fn copies(
    scope: &environment::ProjectScope,
    settings: &environment::EnvironmentSettings,
) -> Vec<String> {
    environment::settings_at_checkout_root(scope, settings).copy_paths
}

struct Preservation {
    paths: Vec<String>,
    identities: HashSet<String>,
}
impl Preservation {
    fn new(root: &Path, paths: &[String]) -> Result<Self, String> {
        let mut identities = HashSet::new();
        for path in paths {
            if let Some(metadata) = environment::safe_metadata(root, path)? {
                if !metadata.is_file() {
                    return Err(format!(
                        "Selected configuration is not a regular file: {path}"
                    ));
                }
                identities.insert(output_fs::path_identity(&root.join(path), &metadata)?);
            }
        }
        Ok(Self {
            paths: paths.to_vec(),
            identities,
        })
    }
}

fn inspect_candidate(root: &Path, path: String, copies: &[String]) -> Candidate {
    let preserved_paths = copies
        .iter()
        .filter(|p| selected_path(Path::new(&path), p, true))
        .cloned()
        .collect::<Vec<_>>();
    let mut candidate = Candidate {
        path,
        estimated_bytes: 0,
        preserved_paths,
        blocked_reason: None,
        identity: None,
    };
    let checked = (|| {
        candidate_policy(root, &candidate.path)?;
        environment::safe_metadata(root, &candidate.path)?
            .filter(|m| m.is_dir())
            .ok_or("Output directory is absent or is not a directory")?;
        let directory = Directory::open(&root.join(&candidate.path))?;
        if !directory.same_filesystem(&Directory::open(root)?)? {
            return Err("Output crosses a filesystem mount; kept".into());
        }
        candidate.identity = Some(directory.identity()?);
        walk(
            &directory,
            Path::new(&candidate.path),
            &Preservation::new(root, copies)?,
            false,
            &mut candidate.estimated_bytes,
            &mut |_| Ok(()),
        )
    })();
    candidate.blocked_reason = checked.err();
    candidate
}

/// Walk only this candidate. Preserve selected files and their ancestors in
/// place, unlink links without following them, and remove only empty directories.
fn walk(
    directory: &Directory,
    relative: &Path,
    preserved: &Preservation,
    delete: bool,
    bytes: &mut u64,
    before_remove: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<bool, String> {
    let mut retained = false;
    for name in directory.names()? {
        let path = relative.join(&name);
        if preserved
            .paths
            .iter()
            .any(|p| selected_path(&path, p, false))
        {
            retained = true;
            continue;
        }
        let metadata = directory.metadata(&name)?;
        if metadata.is_file()
            && !is_link(&metadata)
            && !preserved.identities.is_empty()
            && preserved
                .identities
                .contains(&directory.entry_identity(&name)?)
        {
            retained = true;
            continue;
        }
        if metadata.is_dir() && !is_link(&metadata) {
            let child = directory.child(&name)?;
            let child_retained = walk(&child, &path, preserved, delete, bytes, before_remove)?;
            child.check()?;
            drop(child);
            if child_retained
                || preserved
                    .paths
                    .iter()
                    .any(|p| selected_path(&path, p, true))
            {
                retained = true;
                continue;
            }
            if delete {
                before_remove(&path)?;
                directory.remove(&name, true)?;
            }
        } else {
            if !metadata.is_file() && !is_link(&metadata) {
                return Err(format!("Special filesystem entry kept: {}", path.display()));
            }
            if delete {
                let identity = directory.entry_identity(&name)?;
                before_remove(&path)?;
                let current = directory.metadata(&name)?;
                if directory.entry_identity(&name)? != identity
                    || current.len() != metadata.len()
                    || current.modified().ok() != metadata.modified().ok()
                {
                    return Err(format!(
                        "Output changed before deletion: {}",
                        path.display()
                    ));
                }
                directory.remove(&name, false)?;
            }
            *bytes = bytes.saturating_add(estimated_bytes(&metadata));
        }
    }
    Ok(retained)
}

fn review(
    conn: &Connection,
    host: &WorktreeHost,
    windows: &HashMap<String, Vec<PathBuf>>,
    cwd: &str,
    id: &str,
) -> Result<Review, String> {
    let entry = owned(conn)?
        .into_iter()
        .find(|e| e.id == id)
        .ok_or("Managed checkout no longer exists")?;
    let requested = environment::scope_for_cwd(cwd)?;
    let (scope, settings, paths) = policy(conn, &entry)?;
    if requested.common != scope.common || requested.relative != scope.relative {
        return Err("Checkout belongs to a different project".into());
    }
    let root = Directory::open(Path::new(&entry.path))?;
    let blocked_reason = validate_entry(conn, host, windows, &entry).err();
    let result = Review {
        plan_id: format!("outputs-{}", uuid::Uuid::new_v4()),
        id: entry.id.clone(),
        path: entry.path.clone(),
        branch: entry.branch.clone(),
        candidates: paths
            .into_iter()
            .filter(|p| std::fs::symlink_metadata(Path::new(&entry.path).join(p)).is_ok())
            .map(|p| inspect_candidate(Path::new(&entry.path), p, &copies(&scope, &settings)))
            .collect(),
        blocked_reason,
    };
    let snapshot = Snapshot {
        review: result.clone(),
        repo: entry.repo,
        common: entry.common,
        project: scope.relative,
        root_identity: root.identity()?,
        head: resolve_commit(Path::new(&entry.path), "HEAD")?,
        settings,
    };
    conn.execute("INSERT INTO worktree_output_cleanups(plan_id, worktree_id, project_cwd, snapshot_json, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'reviewed', ?5, ?5)", params![result.plan_id, result.id, requested.main_path, serde_json::to_string(&snapshot).map_err(|e| e.to_string())?, now()]).map_err(|e| e.to_string())?;
    Ok(result)
}

fn current(
    conn: &Connection,
    host: &WorktreeHost,
    windows: &HashMap<String, Vec<PathBuf>>,
    snapshot: &Snapshot,
) -> Result<Owned, String> {
    let entry = owned(conn)?
        .into_iter()
        .find(|e| e.id == snapshot.review.id)
        .ok_or("Checkout ownership changed")?;
    if entry.path != snapshot.review.path
        || entry.repo != snapshot.repo
        || entry.common != snapshot.common
        || entry.branch != snapshot.review.branch
    {
        return Err("Checkout identity changed; review again".into());
    }
    validate_entry(conn, host, windows, &entry)?;
    if Directory::open(Path::new(&entry.path))?.identity()? != snapshot.root_identity
        || resolve_commit(Path::new(&entry.path), "HEAD")? != snapshot.head
    {
        return Err("Checkout was replaced or HEAD changed; review again".into());
    }
    let (scope, settings, _) = policy(conn, &entry)?;
    if scope.relative != snapshot.project || settings != snapshot.settings {
        return Err("Environment policy changed; review again".into());
    }
    Ok(entry)
}

fn save_report(conn: &Connection, report: &Report) -> Result<(), String> {
    conn.execute("UPDATE worktree_output_cleanups SET status = ?1, report_json = ?2, updated_at = ?3 WHERE plan_id = ?4", params![report.status, serde_json::to_string(report).map_err(|e| e.to_string())?, now(), report.plan_id]).map_err(|e| e.to_string())?;
    Ok(())
}

fn execute_with(
    conn: &Connection,
    host: &WorktreeHost,
    plan_id: &str,
    selected: &[String],
    protect: impl Fn(&HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>>,
    mut before_remove: impl FnMut(&Path) -> Result<(), String>,
) -> Result<Report, String> {
    let (json, status): (String, String) = conn
        .query_row(
            "SELECT snapshot_json, status FROM worktree_output_cleanups WHERE plan_id = ?1",
            [plan_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if status != "reviewed" {
        return Err(
            "Cleanup already started or was interrupted; review remaining outputs again".into(),
        );
    }
    let snapshot: Snapshot = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let _repository = host.repository_guard(&snapshot.common)?;
    let windows = host.operation_guard()?;
    let selected_set: HashSet<_> = selected.iter().collect();
    if selected.is_empty()
        || selected_set.len() != selected.len()
        || selected.iter().any(|path| {
            !snapshot
                .review
                .candidates
                .iter()
                .any(|c| &c.path == path && c.blocked_reason.is_none())
        })
        || snapshot.review.blocked_reason.is_some()
    {
        return Err("Select only eligible directories from this review".into());
    }
    let entry = current(conn, host, &protect(&windows), &snapshot)?;
    let before = disk::volume(Path::new(&entry.path));
    let mut report = Report {
        plan_id: plan_id.into(),
        id: entry.id.clone(),
        path: entry.path.clone(),
        status: "executing".into(),
        selected_paths: selected.to_vec(),
        results: Vec::new(),
        estimated_removed_bytes: 0,
        observed_free_space_change: None,
        measurement_error: None,
        preparation_needed: true,
    };
    // Both records commit before any deletion. Even failed/interrupted cleanup
    // cannot reuse a previous ready bit or replay primary-checkout configuration.
    {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let status: String = tx
            .query_row(
                "SELECT status FROM worktree_output_cleanups WHERE plan_id = ?1",
                [plan_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if status != "reviewed" {
            return Err("Cleanup already started; review again".into());
        }
        environment::mark_pending(&tx, &entry, environment::SetupOrigin::Cleaned, None, None)?;
        save_report(&tx, &report)?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    // The repository reservation prevents new workspace/agent/terminal/pin or
    // settings changes here. Release the app-wide lock during the large walk;
    // unrelated repositories and windows must remain usable.
    drop(windows);
    // Invalidation also happens on unwinding/errors after partial filesystem work.
    struct Invalidate<'a>(&'a disk::DiskManager);
    impl Drop for Invalidate<'_> {
        fn drop(&mut self) {
            self.0.invalidate();
        }
    }
    let _invalidate = Invalidate(&host.disk);
    for path in selected {
        let candidate = snapshot
            .review
            .candidates
            .iter()
            .find(|c| &c.path == path)
            .unwrap();
        let mut removed = 0;
        let result = (|| {
            let entry = {
                let windows = host.operation_guard()?;
                current(conn, host, &protect(&windows), &snapshot)?
            };
            let (_, _, allowed) = policy(conn, &entry)?;
            if !allowed.contains(path) {
                return Err(
                    "Disposal policy no longer recognizes this directory; review again".into(),
                );
            }
            candidate_policy(Path::new(&entry.path), path)?;
            environment::safe_metadata(Path::new(&entry.path), path)?
                .ok_or("Output directory no longer exists")?;
            let directory = Directory::open(&Path::new(&entry.path).join(path))?;
            if !directory.same_filesystem(&Directory::open(Path::new(&entry.path))?)? {
                return Err("Output crosses a filesystem mount; kept".into());
            }
            if Some(directory.identity()?) != candidate.identity {
                return Err("Output directory was replaced; review again".into());
            }
            let git_watch = GitWatch::new(&entry)?;
            candidate_policy(Path::new(&entry.path), path)?;
            let mut guarded_remove = |relative: &Path| {
                before_remove(relative)?;
                // Leases/agent starts/pins/setup use the held lifecycle guard.
                // Resample native activity and Git locks before each write too.
                {
                    let windows = host.operation_guard()?;
                    if let Some(reason) = live_use_reason(conn, &protect(&windows), &entry, false)?
                    {
                        return Err(reason);
                    }
                }
                git_watch.check()?;
                directory.check()?;
                Ok(())
            };
            walk(
                &directory,
                Path::new(path),
                &Preservation::new(
                    Path::new(&entry.path),
                    &copies(
                        &environment::scope_for_entry(conn, &entry)?,
                        &snapshot.settings,
                    ),
                )?,
                true,
                &mut removed,
                &mut guarded_remove,
            )
        })();
        report.estimated_removed_bytes = report.estimated_removed_bytes.saturating_add(removed);
        report.results.push(ItemResult {
            path: path.clone(),
            estimated_removed_bytes: removed,
            error: result.err(),
        });
        save_report(conn, &report)?;
    }
    match (before, disk::volume(Path::new(&entry.path))) {
        (Ok(before), Ok(after)) if before.id == after.id => {
            report.observed_free_space_change = i64::try_from(
                i128::from(after.available_bytes) - i128::from(before.available_bytes),
            )
            .ok();
        }
        (Err(error), _) | (_, Err(error)) => report.measurement_error = Some(error),
        _ => {
            report.measurement_error = Some("Filesystem identity changed during measurement".into())
        }
    }
    report.status = if report.results.iter().any(|r| r.error.is_some()) {
        "partial"
    } else {
        "complete"
    }
    .into();
    save_report(conn, &report)?;
    Ok(report)
}

fn history(conn: &Connection, cwd: &str) -> Result<Vec<Report>, String> {
    let scope = environment::scope_for_cwd(cwd)?;
    let mut statement = conn.prepare("SELECT report_json, status FROM worktree_output_cleanups WHERE project_cwd = ?1 AND report_json IS NOT NULL ORDER BY created_at DESC, rowid DESC LIMIT 20").map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([scope.main_path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|r| {
        let (json, status) = r.map_err(|e| e.to_string())?;
        let mut report: Report = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        report.status = status;
        report.preparation_needed = environment::needs_setup(conn, &report.path)?;
        Ok(report)
    })
    .collect()
}

#[tauri::command(async)]
pub fn worktree_output_review(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    cwd: String,
    id: String,
) -> Result<Review, String> {
    let common = environment::scope_for_cwd(&cwd)?.common;
    let _repository = host.repository_guard(&common)?;
    let windows = automatic::protection(&app, &*host.operation_guard()?);
    review(&store.open_auxiliary_conn()?, &host, &windows, &cwd, &id)
}
#[tauri::command(async)]
pub fn worktree_output_execute(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    plan_id: String,
    paths: Vec<String>,
) -> Result<Report, String> {
    let result = execute_with(
        &store.open_auxiliary_conn()?,
        &host,
        &plan_id,
        &paths,
        |windows| automatic::protection(&app, windows),
        |_| Ok(()),
    );
    host.disk.invalidate();
    disk::refresh(&app);
    automatic::schedule(&app);
    let _ = app.emit("worktree-output-changed", ());
    let _ = app.emit("worktree-retirement-changed", ());
    result
}
#[tauri::command(async)]
pub fn worktree_output_history(
    store: State<'_, SessionStore>,
    cwd: String,
) -> Result<Vec<Report>, String> {
    history(&store.open_auxiliary_conn()?, &cwd)
}

#[cfg(test)]
#[path = "worktree_output_cleanup_tests.rs"]
pub(super) mod tests;
