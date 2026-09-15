//! Per-project worktree setup and local-only file preservation.
//!
//! The project policy is deliberately narrow: copied paths are literal ignored
//! files and disposable paths are literal ignored directories. Known generated
//! directories are also disposable when a tracked project manifest identifies
//! them. Archives live in the private application database and are immutable
//! for a retirement review.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::Owned;

const MAX_COPY_PATHS: usize = 64;
const MAX_DISPOSABLE_PATHS: usize = 32;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SETUP_COMMAND_BYTES: usize = 32 * 1024;
const SETUP_TIMEOUT: Duration = Duration::from_secs(20 * 60);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSettings {
    /// Optimistic-concurrency token for this project's environment policy.
    #[serde(default)]
    pub environment_version: i64,
    #[serde(default)]
    pub setup_command: String,
    #[serde(default)]
    pub copy_paths: Vec<String>,
    #[serde(default)]
    pub disposable_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectScope {
    pub(super) common: String,
    pub(super) relative: String,
    pub(super) main_path: String,
}

impl ProjectScope {
    pub(super) fn project_path_in(&self, checkout: &Path) -> PathBuf {
        if self.relative.is_empty() {
            checkout.to_path_buf()
        } else {
            checkout.join(&self.relative)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SetupOrigin {
    Fresh,
    Restored,
    Cleaned,
}

impl SetupOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Restored => "restored",
            Self::Cleaned => "cleaned",
        }
    }
}

pub(super) enum BeginSetup {
    Skip,
    Run(Box<SetupOperation>),
}

pub(super) enum FinishSetup {
    Complete,
    Retry(String),
}

pub(super) struct SetupOperation {
    worktree_id: String,
    root_path: String,
    path: String,
    generation: i64,
    attempt: i64,
    settings_version: i64,
    setup_command: String,
    files: Vec<FileSnapshot>,
    log_path: Option<PathBuf>,
    database_path: Option<PathBuf>,
    process_may_be_live: AtomicBool,
}

impl SetupOperation {
    pub(super) fn root_path(&self) -> &str {
        &self.root_path
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    path: String,
    contents: Option<Vec<u8>>,
    unix_mode: Option<u32>,
}

#[derive(Debug)]
struct SetupRow {
    worktree_id: String,
    common: String,
    repo: String,
    path: String,
    origin: SetupOrigin,
    archive_id: Option<String>,
    status: String,
    attempts: i64,
    generation: i64,
    project_path: String,
}

pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_environment_settings (
            common_dir TEXT PRIMARY KEY,
            setup_command TEXT NOT NULL DEFAULT '',
            copy_paths_json TEXT NOT NULL DEFAULT '[]',
            disposable_paths_json TEXT NOT NULL DEFAULT '[]',
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS worktree_environment_reviews (
            plan_id TEXT NOT NULL,
            worktree_id TEXT NOT NULL,
            common_dir TEXT NOT NULL,
            settings_json TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (plan_id, worktree_id)
         );
         CREATE TABLE IF NOT EXISTS worktree_environment_archives (
            archive_id TEXT PRIMARY KEY,
            plan_id TEXT NOT NULL,
            worktree_id TEXT NOT NULL,
            common_dir TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE (plan_id, worktree_id)
         );
         CREATE INDEX IF NOT EXISTS worktree_environment_archive_latest
           ON worktree_environment_archives(worktree_id, created_at DESC);
         CREATE TABLE IF NOT EXISTS worktree_environment_setup (
            worktree_id TEXT PRIMARY KEY,
            common_dir TEXT NOT NULL,
            path TEXT NOT NULL,
            origin TEXT NOT NULL,
            archive_id TEXT,
            status TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            files_applied INTEGER NOT NULL DEFAULT 0,
            process_id INTEGER,
            last_error TEXT,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY (worktree_id) REFERENCES managed_worktrees(id) ON DELETE CASCADE
         );",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_environment_project_settings (
            common_dir TEXT NOT NULL,
            project_path TEXT NOT NULL,
            setup_command TEXT NOT NULL DEFAULT '',
            copy_paths_json TEXT NOT NULL DEFAULT '[]',
            disposable_paths_json TEXT NOT NULL DEFAULT '[]',
            version INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (common_dir, project_path)
         );
         CREATE TABLE IF NOT EXISTS worktree_environment_origins (
            worktree_id TEXT PRIMARY KEY,
            common_dir TEXT NOT NULL,
            project_path TEXT NOT NULL,
            FOREIGN KEY (worktree_id) REFERENCES managed_worktrees(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS worktree_environment_setup_files (
            worktree_id TEXT NOT NULL,
            setup_generation INTEGER NOT NULL,
            path TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            PRIMARY KEY (worktree_id, setup_generation, path),
            FOREIGN KEY (worktree_id) REFERENCES managed_worktrees(id) ON DELETE CASCADE
         );",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO worktree_environment_project_settings
           (common_dir, project_path, setup_command, copy_paths_json,
            disposable_paths_json, version, updated_at)
         SELECT common_dir, '', setup_command, copy_paths_json,
                disposable_paths_json, 1, updated_at
           FROM worktree_environment_settings",
        [],
    )?;
    ensure_setup_column(conn, "files_applied", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_setup_column(conn, "setup_generation", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_setup_column(conn, "process_id", "INTEGER")?;
    ensure_column(
        conn,
        "worktree_environment_reviews",
        "project_path",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "worktree_environment_archives",
        "project_path",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "worktree_environment_archives",
        "settings_json",
        "TEXT",
    )?;
    super::storage::schema(conn)?;
    tighten_database_permissions(conn)?;
    Ok(())
}

fn ensure_setup_column(conn: &Connection, column: &str, declaration: &str) -> rusqlite::Result<()> {
    ensure_column(conn, "worktree_environment_setup", column, declaration)
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
            [],
        )?;
    }
    Ok(())
}

/// Call once during application initialization, after schema creation. A
/// running row at that point belongs to the previous process and is retryable.
pub(super) fn reset_interrupted(conn: &Connection) -> Result<(), String> {
    let mut statement = conn
        .prepare(
            "SELECT worktree_id, process_id FROM worktree_environment_setup
              WHERE status = 'running'",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<u32>>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (worktree_id, process_id) in rows {
        let retryable = process_id.is_none_or(setup_group_confirmed_absent);
        if retryable {
            conn.execute(
                "UPDATE worktree_environment_setup
                    SET status = 'failed', process_id = NULL,
                        last_error = 'Setup was interrupted. Retry setup.',
                        updated_at = ?1
                  WHERE worktree_id = ?2 AND status = 'running'",
                params![super::now(), worktree_id],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub(super) fn scope_for_cwd(cwd: &str) -> Result<ProjectScope, String> {
    let source = std::fs::canonicalize(crate::fs::expand_home(cwd)).map_err(|e| e.to_string())?;
    let checkout = super::git(&source, &["rev-parse", "--show-toplevel"])?;
    let checkout = std::fs::canonicalize(checkout).map_err(|e| e.to_string())?;
    let (main, common) = super::repository(cwd)?;
    let main = std::fs::canonicalize(main).map_err(|e| e.to_string())?;
    let relative = source
        .strip_prefix(&checkout)
        .map_err(|_| "Project is outside the checkout")?;
    let relative = relative
        .to_str()
        .ok_or("Project path must be valid UTF-8")?
        .replace('\\', "/");
    if !relative.is_empty() {
        normalize_relative_path(&relative)?;
    }
    let main_project = std::fs::canonicalize(main.join(&relative))
        .map_err(|_| "The primary checkout does not contain this project folder")?;
    if !main_project.starts_with(&main) || !main_project.is_dir() {
        return Err("Project folder points outside the primary checkout".into());
    }
    Ok(ProjectScope {
        common,
        relative,
        main_path: super::path_to_js(&main_project),
    })
}

fn root_scope(entry: &Owned) -> ProjectScope {
    ProjectScope {
        common: entry.common.clone(),
        relative: String::new(),
        main_path: entry.repo.clone(),
    }
}

pub(super) fn scope_for_entry(conn: &Connection, entry: &Owned) -> Result<ProjectScope, String> {
    Ok(explicit_scope_for_entry(conn, entry)?.unwrap_or_else(|| root_scope(entry)))
}

pub(super) fn explicit_scope_for_entry(
    conn: &Connection,
    entry: &Owned,
) -> Result<Option<ProjectScope>, String> {
    let stored = conn
        .query_row(
            "SELECT common_dir, project_path FROM worktree_environment_origins
              WHERE worktree_id = ?1",
            [&entry.id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((common, relative)) = stored else {
        return Ok(None);
    };
    if common != entry.common {
        return Err("Worktree environment origin repository changed".into());
    }
    if !relative.is_empty() {
        normalize_relative_path(&relative)?;
    }
    Ok(Some(ProjectScope {
        common,
        main_path: super::path_to_js(&Path::new(&entry.repo).join(&relative)),
        relative,
    }))
}

pub(super) fn load_settings(
    conn: &Connection,
    scope: &ProjectScope,
) -> Result<EnvironmentSettings, String> {
    let stored = conn
        .query_row(
            "SELECT setup_command, copy_paths_json, disposable_paths_json, version
               FROM worktree_environment_project_settings
              WHERE common_dir = ?1 AND project_path = ?2",
            params![scope.common, scope.relative],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((setup_command, copy_paths, disposable_paths, environment_version)) = stored else {
        return Ok(EnvironmentSettings::default());
    };
    normalize_settings(EnvironmentSettings {
        environment_version,
        setup_command,
        copy_paths: serde_json::from_str(&copy_paths)
            .map_err(|_| "Saved copy paths are invalid".to_string())?,
        disposable_paths: serde_json::from_str(&disposable_paths)
            .map_err(|_| "Saved disposable paths are invalid".to_string())?,
    })
}

pub(super) fn save_settings(
    conn: &Connection,
    scope: &ProjectScope,
    settings: &EnvironmentSettings,
) -> Result<EnvironmentSettings, String> {
    let settings = normalize_settings(settings.clone())?;
    if settings.environment_version < 0 {
        return Err("Environment settings version is invalid".into());
    }
    let copy_paths = serde_json::to_string(&settings.copy_paths).map_err(|e| e.to_string())?;
    let disposable_paths =
        serde_json::to_string(&settings.disposable_paths).map_err(|e| e.to_string())?;
    let next_version = settings
        .environment_version
        .checked_add(1)
        .ok_or("Environment settings version is exhausted")?;
    let changed = if settings.environment_version == 0 {
        conn.execute(
            "INSERT INTO worktree_environment_project_settings
               (common_dir, project_path, setup_command, copy_paths_json,
                disposable_paths_json, version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(common_dir, project_path) DO NOTHING",
            params![
                scope.common,
                scope.relative,
                settings.setup_command,
                copy_paths,
                disposable_paths,
                next_version,
                super::now()
            ],
        )
    } else {
        conn.execute(
            "UPDATE worktree_environment_project_settings
                SET setup_command = ?1, copy_paths_json = ?2,
                    disposable_paths_json = ?3, version = ?4, updated_at = ?5
              WHERE common_dir = ?6 AND project_path = ?7 AND version = ?8",
            params![
                settings.setup_command,
                copy_paths,
                disposable_paths,
                next_version,
                super::now(),
                scope.common,
                scope.relative,
                settings.environment_version
            ],
        )
    }
    .map_err(|error| error.to_string())?;
    if changed != 1 {
        let current = load_settings(conn, scope)?;
        return Err(format!(
            "WORKTREE_SETTINGS_CONFLICT: environment settings changed (current version {})",
            current.environment_version
        ));
    }
    load_settings(conn, scope)
}

pub(super) fn settings_at_checkout_root(
    scope: &ProjectScope,
    settings: &EnvironmentSettings,
) -> EnvironmentSettings {
    let prefix = |path: &str| {
        if scope.relative.is_empty() {
            path.to_string()
        } else {
            format!("{}/{path}", scope.relative)
        }
    };
    EnvironmentSettings {
        environment_version: settings.environment_version,
        setup_command: settings.setup_command.clone(),
        copy_paths: settings
            .copy_paths
            .iter()
            .map(|path| prefix(path))
            .collect(),
        disposable_paths: settings
            .disposable_paths
            .iter()
            .map(|path| prefix(path))
            .collect(),
    }
}

/// Return an actionable reason when anything in the checkout falls outside
/// the saved copy policy and explicit or recognized disposable directories.
pub(super) fn check_cleanup(conn: &Connection, entry: &Owned) -> Result<Option<String>, String> {
    let setup: Option<(String, String, bool, Option<u32>)> = conn
        .query_row(
            "SELECT status, origin, files_applied, process_id
               FROM worktree_environment_setup
              WHERE worktree_id = ?1",
            [&entry.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((status, origin, files_applied, process_id)) = setup {
        match status.as_str() {
            "running" => {
                let detail = process_id
                    .filter(|pid| !setup_group_confirmed_absent(*pid))
                    .map(|pid| format!(" (process group {pid})"))
                    .unwrap_or_default();
                return Ok(Some(format!(
                    "Worktree setup is still running or could not be safely verified{detail}. Let it finish or restart MonoCode before cleanup."
                )));
            }
            "pending" | "failed" if origin == "restored" && !files_applied => {
                return Ok(Some(
                    "Archived local configuration has not been restored yet. Retry setup before cleanup."
                        .into(),
                ));
            }
            _ => {}
        }
    }
    let scope = scope_for_entry(conn, entry)?;
    let settings = load_settings(conn, &scope)?;
    check_cleanup_with_settings(
        Path::new(&entry.path),
        &settings_at_checkout_root(&scope, &settings),
    )
}

/// Freeze the policy that was reviewed. Execution must use this exact policy,
/// even if another settings window edits the project in the meantime.
pub(super) fn record_review(conn: &Connection, entry: &Owned, plan_id: &str) -> Result<(), String> {
    let scope = scope_for_entry(conn, entry)?;
    let settings = load_settings(conn, &scope)?;
    let root_settings = settings_at_checkout_root(&scope, &settings);
    if Path::new(&entry.path).is_dir() {
        if let Some(reason) = check_cleanup_with_settings(Path::new(&entry.path), &root_settings)? {
            return Err(reason);
        }
    }
    let json = serde_json::to_string(&settings).map_err(|error| error.to_string())?;
    conn.execute(
        "INSERT OR IGNORE INTO worktree_environment_reviews
           (plan_id, worktree_id, common_dir, settings_json, created_at, project_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            plan_id,
            entry.id,
            entry.common,
            json,
            super::now(),
            scope.relative
        ],
    )
    .map_err(|error| error.to_string())?;
    let saved: String = conn
        .query_row(
            "SELECT settings_json FROM worktree_environment_reviews
              WHERE plan_id = ?1 AND worktree_id = ?2",
            params![plan_id, entry.id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if saved != json {
        return Err("Worktree environment policy changed during review; review it again".into());
    }
    Ok(())
}

/// Detect new code-preservation inputs without changing any immutable archive.
pub(super) fn review_is_current(
    conn: &Connection,
    entry: &Owned,
    plan_id: &str,
) -> Result<bool, String> {
    let scope = scope_for_entry(conn, entry)?;
    let reviewed: String = conn.query_row(
        "SELECT settings_json FROM worktree_environment_reviews WHERE plan_id = ?1 AND worktree_id = ?2 AND common_dir = ?3 AND project_path = ?4",
        params![plan_id, entry.id, scope.common, scope.relative], |r| r.get(0),
    ).map_err(|e| e.to_string())?;
    let settings = load_settings(conn, &scope)?;
    if serde_json::from_str::<EnvironmentSettings>(&reviewed).map_err(|e| e.to_string())?
        != settings
    {
        return Ok(false);
    }
    let archive: Option<String> = conn.query_row("SELECT archive_id FROM worktree_environment_archives WHERE plan_id = ?1 AND worktree_id = ?2", params![plan_id, entry.id], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
    match archive {
        Some(archive) => Ok(load_archive(conn, &archive)?
            == snapshot_copy_files(&scope.project_path_in(Path::new(&entry.path)), &settings)?),
        None => Ok(true),
    }
}

/// Preserve an immutable snapshot before Git removes the checkout. Existing
/// archives for the same review are verified rather than overwritten.
pub(super) fn preserve(conn: &Connection, entry: &Owned, plan_id: &str) -> Result<String, String> {
    let scope = scope_for_entry(conn, entry)?;
    let reviewed_json: String = conn
        .query_row(
            "SELECT settings_json FROM worktree_environment_reviews
              WHERE plan_id = ?1 AND worktree_id = ?2 AND common_dir = ?3
                AND project_path = ?4",
            params![plan_id, entry.id, entry.common, scope.relative],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or("Worktree environment policy was not reviewed")?;
    let reviewed: EnvironmentSettings = serde_json::from_str(&reviewed_json)
        .map_err(|_| "Reviewed worktree environment policy is invalid".to_string())?;
    let reviewed = normalize_settings(reviewed)?;
    if load_settings(conn, &scope)? != reviewed {
        return Err("Worktree environment settings changed after review; review it again".into());
    }
    let root_settings = settings_at_checkout_root(&scope, &reviewed);
    if let Some(reason) = check_cleanup_with_settings(Path::new(&entry.path), &root_settings)? {
        return Err(reason);
    }

    let archive_id = format!("{plan_id}:{}", entry.id);
    let project = scope.project_path_in(Path::new(&entry.path));
    let first = snapshot_copy_files(&project, &reviewed)?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT archive_id FROM worktree_environment_archives
              WHERE plan_id = ?1 AND worktree_id = ?2",
            params![plan_id, entry.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(existing) = existing {
        if load_archive(conn, &existing)? != first {
            return Err(
                "Configured local files changed after they were preserved; review again".into(),
            );
        }
        return Ok(existing);
    }

    super::storage::store_archive(
        conn,
        &super::storage::ArchiveManifest {
            archive_id: archive_id.clone(),
            plan_id: plan_id.to_string(),
            worktree_id: entry.id.clone(),
            common_dir: entry.common.clone(),
            created_at: super::now(),
            project_path: scope.relative.clone(),
            project_cwd: scope.main_path.clone(),
            settings_json: reviewed_json,
            files: first
                .iter()
                .map(|file| super::storage::ArchiveFile {
                    path: file.path.clone(),
                    contents: file.contents.clone(),
                    unix_mode: file.unix_mode,
                })
                .collect(),
        },
    )?;

    // Bound the remaining filesystem race: a file that changed while its blob
    // was being archived forces a new review before removal.
    let second = snapshot_copy_files(&project, &reviewed)?;
    if first != second {
        return Err("Configured local files changed while being preserved; review again".into());
    }
    if let Some(reason) = check_cleanup_with_settings(Path::new(&entry.path), &root_settings)? {
        return Err(reason);
    }
    Ok(archive_id)
}

/// Mark only newly-created or newly-restored worktrees. Existing legacy
/// worktrees have no row and therefore skip setup.
pub(super) fn mark_pending(
    conn: &Connection,
    entry: &Owned,
    origin: SetupOrigin,
    scope: Option<&ProjectScope>,
    retirement_plan_id: Option<&str>,
) -> Result<(), String> {
    if origin == SetupOrigin::Fresh && scope.is_none() {
        return Err("A new worktree requires a project environment origin".into());
    }
    let explicit = explicit_scope_for_entry(conn, entry)?;
    let requested_scope = scope.cloned();
    let scope = requested_scope
        .clone()
        .or_else(|| explicit.clone())
        .unwrap_or_else(|| root_scope(entry));
    if scope.common != entry.common {
        return Err("Worktree environment origin repository changed".into());
    }
    if requested_scope.is_some() {
        conn.execute(
            "INSERT OR IGNORE INTO worktree_environment_origins
               (worktree_id, common_dir, project_path) VALUES (?1, ?2, ?3)",
            params![entry.id, scope.common, scope.relative],
        )
        .map_err(|error| error.to_string())?;
    }
    if explicit.as_ref().is_some_and(|persisted| {
        persisted.common != scope.common || persisted.relative != scope.relative
    }) {
        return Err("This worktree is already bound to a different project origin".into());
    }
    let archive_id = if origin == SetupOrigin::Restored {
        latest_archive_id(conn, &entry.id, retirement_plan_id)?
    } else {
        None
    };
    let existing = conn
        .query_row(
            "SELECT origin, archive_id, status FROM worktree_environment_setup
              WHERE worktree_id = ?1",
            [&entry.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((saved_origin, saved_archive, saved_status)) = existing {
        let same_attempt = saved_origin == origin.as_str() && saved_archive == archive_id;
        if same_attempt && matches!(saved_status.as_str(), "pending" | "running" | "failed") {
            return Ok(());
        }
    }
    conn.execute(
        "INSERT INTO worktree_environment_setup
           (worktree_id, common_dir, path, origin, archive_id, status,
            attempts, files_applied, process_id, last_error, updated_at, setup_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, 0, NULL, NULL, ?6, 1)
         ON CONFLICT(worktree_id) DO UPDATE SET
           common_dir = excluded.common_dir,
           path = excluded.path,
           origin = excluded.origin,
           archive_id = excluded.archive_id,
           status = 'pending',
           attempts = 0,
           files_applied = 0,
           process_id = NULL,
           last_error = NULL,
           setup_generation = worktree_environment_setup.setup_generation + 1,
           updated_at = excluded.updated_at",
        params![
            entry.id,
            scope.common,
            entry.path,
            origin.as_str(),
            archive_id,
            super::now()
        ],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Snapshot everything the unlocked runner needs, then atomically record the
/// attempt. The returned operation owns any secret bytes; callers need not keep
/// a database or lifecycle guard while it runs.
pub(super) fn needs_setup(conn: &Connection, path: &str) -> Result<bool, String> {
    Ok(find_setup_row(conn, path)?.is_some_and(|row| row.status != "ready"))
}

pub(super) fn begin_setup(conn: &Connection, requested_path: &str) -> Result<BeginSetup, String> {
    let Some(row) = find_setup_row(conn, requested_path)? else {
        return Ok(BeginSetup::Skip);
    };
    if row.status == "ready" {
        return Ok(BeginSetup::Skip);
    }
    if row.status == "running" {
        return Err("Worktree setup is already running".into());
    }
    if !matches!(row.status.as_str(), "pending" | "failed") {
        return Err("Saved worktree setup state is invalid".into());
    }

    let operation = build_operation(conn, &row);
    let (setup_command, files, log_path, settings_version) = match operation {
        Ok(value) => value,
        Err(error) => {
            conn.execute(
                "UPDATE worktree_environment_setup
                    SET status = 'failed', attempts = attempts + 1, process_id = NULL,
                        last_error = ?1, updated_at = ?2
                  WHERE worktree_id = ?3",
                params![error, super::now(), row.worktree_id],
            )
            .map_err(|db_error| db_error.to_string())?;
            return Err(error);
        }
    };
    let attempt = row.attempts + 1;
    let changed = conn
        .execute(
            "UPDATE worktree_environment_setup
                SET status = 'running', attempts = ?1, process_id = NULL, last_error = NULL,
                    updated_at = ?2
              WHERE worktree_id = ?3 AND status IN ('pending', 'failed')",
            params![attempt, super::now(), row.worktree_id],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("Worktree setup state changed; retry setup".into());
    }
    Ok(BeginSetup::Run(Box::new(SetupOperation {
        worktree_id: row.worktree_id,
        root_path: row.path.clone(),
        path: super::path_to_js(&Path::new(&row.path).join(&row.project_path)),
        generation: row.generation,
        attempt,
        settings_version,
        setup_command,
        files,
        log_path,
        database_path: conn
            .path()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
        process_may_be_live: AtomicBool::new(false),
    })))
}

/// Run file restoration/copying and the optional command without a database or
/// lifecycle lock. The callback is intentionally phase-only so command output
/// and copied secrets cannot enter generic UI events.
pub(super) fn run_setup(operation: &SetupOperation, progress: impl Fn(&str)) -> Result<(), String> {
    if !operation.files.is_empty() {
        progress("copying");
        apply_snapshots(operation)?;
    }
    record_files_applied(operation)?;
    if operation.setup_command.trim().is_empty() {
        return Ok(());
    }
    progress("running");
    run_setup_command(operation)
}

pub(super) fn finish_setup(
    conn: &Connection,
    operation: &SetupOperation,
    result: &Result<(), String>,
) -> Result<FinishSetup, String> {
    let process_may_be_live = operation.process_may_be_live.load(Ordering::Acquire);
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(settings.version, 0)
               FROM worktree_environment_setup setup
               LEFT JOIN worktree_environment_origins origins
                 ON origins.worktree_id = setup.worktree_id
               LEFT JOIN worktree_environment_project_settings settings
                 ON settings.common_dir = setup.common_dir
                AND settings.project_path = COALESCE(origins.project_path, '')
              WHERE setup.worktree_id = ?1",
            [&operation.worktree_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let settings_changed = current_version != operation.settings_version;
    let retry = "Worktree environment settings changed while setup was running. Retry setup.";
    let (status, error) = match result {
        Ok(()) if settings_changed => ("failed", Some(retry)),
        Ok(()) => ("ready", None),
        Err(error) if process_may_be_live => ("running", Some(error.as_str())),
        Err(error) => ("failed", Some(error.as_str())),
    };
    let changed = conn
        .execute(
            "UPDATE worktree_environment_setup
                SET status = ?1,
                    process_id = CASE WHEN ?6 THEN process_id ELSE NULL END,
                    last_error = ?2, updated_at = ?3
              WHERE worktree_id = ?4 AND status = 'running' AND attempts = ?5
                AND setup_generation = ?7",
            params![
                status,
                error,
                super::now(),
                operation.worktree_id,
                operation.attempt,
                process_may_be_live,
                operation.generation
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("Worktree setup attempt is no longer current".into());
    }
    if result.is_ok() && settings_changed {
        Ok(FinishSetup::Retry(retry.into()))
    } else {
        Ok(FinishSetup::Complete)
    }
}

fn normalize_settings(mut settings: EnvironmentSettings) -> Result<EnvironmentSettings, String> {
    if settings.setup_command.len() > MAX_SETUP_COMMAND_BYTES
        || settings.setup_command.contains('\0')
    {
        return Err("Setup command is invalid or too long".into());
    }
    if settings.copy_paths.len() > MAX_COPY_PATHS {
        return Err(format!("Copy paths are limited to {MAX_COPY_PATHS} files"));
    }
    if settings.disposable_paths.len() > MAX_DISPOSABLE_PATHS {
        return Err(format!(
            "Disposable paths are limited to {MAX_DISPOSABLE_PATHS} directories"
        ));
    }
    settings.copy_paths = settings
        .copy_paths
        .iter()
        .map(|path| normalize_relative_path(path))
        .collect::<Result<_, _>>()?;
    settings.disposable_paths = settings
        .disposable_paths
        .iter()
        .map(|path| normalize_relative_path(path))
        .collect::<Result<_, _>>()?;

    let mut seen = HashSet::new();
    for path in settings
        .copy_paths
        .iter()
        .chain(settings.disposable_paths.iter())
    {
        if !seen.insert(path.clone()) {
            return Err(format!("Configured path is duplicated: {path}"));
        }
    }
    for copy in &settings.copy_paths {
        for disposable in &settings.disposable_paths {
            if copy == disposable || disposable.starts_with(&format!("{copy}/")) {
                return Err(format!(
                    "Copy path '{copy}' overlaps disposable path '{disposable}'"
                ));
            }
        }
    }
    for (index, first) in settings.disposable_paths.iter().enumerate() {
        if settings
            .disposable_paths
            .iter()
            .skip(index + 1)
            .any(|second| paths_overlap(first, second))
        {
            return Err(format!("Disposable paths overlap at '{first}'"));
        }
    }
    Ok(settings)
}

fn normalize_relative_path(value: &str) -> Result<String, String> {
    let value = value.trim_end_matches('/');
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\0')
        || value.contains('\\')
        || value.starts_with('/')
        || value.starts_with("//")
        || (value.as_bytes().get(1) == Some(&b':') && value.as_bytes()[0].is_ascii_alphabetic())
    {
        return Err(format!("Invalid project-relative path: {value}"));
    }
    let mut parts = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or("Configured paths must be valid UTF-8")?;
                if part.eq_ignore_ascii_case(".git") {
                    return Err("Configured paths cannot access Git internals".into());
                }
                parts.push(part);
            }
            _ => return Err(format!("Invalid project-relative path: {value}")),
        }
    }
    if parts.is_empty() {
        return Err("Configured path cannot be empty".into());
    }
    Ok(parts.join("/"))
}

fn paths_overlap(first: &str, second: &str) -> bool {
    first == second
        || first
            .strip_prefix(second)
            .is_some_and(|rest| rest.starts_with('/'))
        || second
            .strip_prefix(first)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Recognize outputs beside tracked project manifests, including nested
/// projects, without walking dependency/build trees. A folder name alone is
/// not evidence that its contents are generated. Require an ignored, real
/// directory with no tracked files and no symlink ancestors as well.
pub(super) fn recognized_output_paths(root: &Path) -> Result<Vec<String>, String> {
    let raw = super::git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--",
            ":(glob)**/package.json",
            ":(glob)**/Cargo.toml",
            ":(glob)**/tauri.conf.json",
            ":(glob)**/tauri.conf.json5",
            ":(glob)**/Tauri.toml",
        ],
    )?;
    let manifests: HashSet<&str> = raw.split_terminator('\0').collect();
    let mut candidates = HashSet::new();
    for manifest in &manifests {
        let (directory, name) = manifest.rsplit_once('/').unwrap_or(("", manifest));
        let relative = |path: &str| {
            if directory.is_empty() {
                path.to_string()
            } else {
                format!("{directory}/{path}")
            }
        };
        let outputs: &[&str] = match name {
            "package.json" => &["node_modules", "dist"],
            "Cargo.toml" => &["target"],
            "tauri.conf.json" | "tauri.conf.json5" | "Tauri.toml"
                if manifests.contains(relative("Cargo.toml").as_str()) =>
            {
                &["gen/schemas"]
            }
            _ => continue,
        };
        if !matches!(safe_metadata(root, manifest)?, Some(metadata) if metadata.is_file()) {
            continue;
        }
        for output in outputs {
            candidates.insert(relative(output));
        }
    }
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort();
    Ok(candidates)
}

fn automatic_disposable_paths(root: &Path) -> Result<Vec<String>, String> {
    let mut disposable = Vec::new();
    for path in recognized_output_paths(root)? {
        if matches!(safe_metadata(root, &path)?, Some(metadata) if metadata.is_dir())
            && validate_git_policy(root, &path, true).is_ok()
        {
            disposable.push(path);
        }
    }
    disposable.sort();
    Ok(disposable)
}

fn check_cleanup_with_settings(
    root: &Path,
    settings: &EnvironmentSettings,
) -> Result<Option<String>, String> {
    let mut settings = normalize_settings(settings.clone())?;
    validate_policy_paths(root, &settings)?;
    // These are derived at every review/removal check, not saved as user
    // settings. Copy paths within a recognized output still get archived.
    settings
        .disposable_paths
        .extend(automatic_disposable_paths(root)?);
    let output = super::git_output(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "Git status contains a filename that is not valid UTF-8".to_string())?;
    let mut blockers = Vec::new();
    let mut records = raw.split_terminator('\0');
    while let Some(record) = records.next() {
        if record.len() < 4 || record.as_bytes().get(2) != Some(&b' ') {
            blockers.push("an unreadable Git status entry".to_string());
            continue;
        }
        let status = &record[..2];
        let ignored_directory = status == "!!" && record[3..].ends_with('/');
        let path = record[3..].trim_end_matches('/');
        let allowed = status == "!!"
            && (settings.copy_paths.iter().any(|copy| copy == path)
                || settings
                    .disposable_paths
                    .iter()
                    .any(|dir| path == dir || path.starts_with(&format!("{dir}/"))));
        if !allowed
            && ignored_directory
            && configured_descends_from(path, &settings.copy_paths, &settings.disposable_paths)
        {
            blockers.extend(expand_ignored_directory(root, path, &settings)?);
        } else if !allowed {
            let kind = match status {
                "!!" => "ignored",
                "??" => "untracked",
                _ => "modified",
            };
            blockers.push(format!("{path} ({kind})"));
        }
        if status
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'R' | b'C'))
        {
            let _ = records.next();
        }
    }
    blockers.sort();
    blockers.dedup();
    if blockers.is_empty() {
        return Ok(None);
    }
    let extra = blockers.len().saturating_sub(12);
    blockers.truncate(12);
    let mut detail = blockers.join(", ");
    if extra > 0 {
        detail.push_str(&format!(", and {extra} more"));
    }
    Ok(Some(format!(
        "Local files need attention: {detail}. Commit code changes, remove unknown files, or add ignored configuration files to Copy local files and custom generated directories to Disposable folders in Settings → Worktrees."
    )))
}

fn configured_descends_from(path: &str, copies: &[String], disposables: &[String]) -> bool {
    let prefix = format!("{path}/");
    copies
        .iter()
        .chain(disposables.iter())
        .any(|configured| configured.starts_with(&prefix))
}

/// `git status --ignored=matching` intentionally collapses ignored
/// directories. Expand only a collapsed directory that contains a configured
/// descendant, excluding approved disposable trees so large dependency folders
/// are never walked.
fn expand_ignored_directory(
    root: &Path,
    directory: &str,
    settings: &EnvironmentSettings,
) -> Result<Vec<String>, String> {
    let mut arguments = vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "--ignored".to_string(),
        "--exclude-standard".to_string(),
        "-z".to_string(),
        "--".to_string(),
        format!(":(top,literal){directory}"),
    ];
    for disposable in &settings.disposable_paths {
        if disposable == directory || disposable.starts_with(&format!("{directory}/")) {
            arguments.push(format!(":(top,exclude,literal){disposable}"));
        }
    }
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = super::git_output(root, &arguments)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "Git found an ignored filename that is not valid UTF-8".to_string())?;
    let mut blockers = Vec::new();
    for file in raw.split_terminator('\0') {
        let file = file.trim_end_matches('/');
        if settings.copy_paths.iter().any(|copy| copy == file)
            || settings
                .disposable_paths
                .iter()
                .any(|dir| file == dir || file.starts_with(&format!("{dir}/")))
        {
            continue;
        }
        blockers.push(format!("{file} (ignored)"));
    }
    Ok(blockers)
}

fn validate_policy_paths(root: &Path, settings: &EnvironmentSettings) -> Result<(), String> {
    for path in &settings.copy_paths {
        validate_git_policy(root, path, false)?;
        match safe_metadata(root, path)? {
            Some(metadata) if metadata.is_file() => {}
            Some(_) => return Err(format!("Copy path must be a regular file: {path}")),
            None => {}
        }
    }
    for path in &settings.disposable_paths {
        validate_git_policy(root, path, true)?;
        match safe_metadata(root, path)? {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => return Err(format!("Disposable path must be a directory: {path}")),
            None => {}
        }
    }
    Ok(())
}

pub(super) fn validate_git_policy(root: &Path, path: &str, directory: bool) -> Result<(), String> {
    let tracked = super::git(
        root,
        &["ls-files", "-z", "--", &format!(":(literal){path}")],
    )?;
    if !tracked.is_empty() {
        return Err(format!("Configured local path is tracked by Git: {path}"));
    }
    let probe = if directory {
        format!("{path}/")
    } else {
        path.to_string()
    };
    let ignored = super::git_output(
        root,
        &["check-ignore", "--quiet", "--no-index", "--", &probe],
    )?;
    match ignored.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(format!(
            "Configured local path is not ignored by Git: {path}. Add it to .gitignore first."
        )),
        _ => Err(String::from_utf8_lossy(&ignored.stderr).trim().to_string()),
    }
}

/// Inspect each existing ancestor but never enter a configured directory. This
/// permits ordinary symlinks inside disposable dependency trees.
pub(super) fn safe_metadata(
    root: &Path,
    relative: &str,
) -> Result<Option<std::fs::Metadata>, String> {
    let mut current = root.to_path_buf();
    let parts: Vec<_> = Path::new(relative).components().collect();
    for (index, component) in parts.iter().enumerate() {
        let Component::Normal(part) = component else {
            return Err(format!("Invalid project-relative path: {relative}"));
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!("Configured path crosses a symlink: {relative}"));
                }
                if index + 1 < parts.len() && !metadata.is_dir() {
                    return Err(format!(
                        "Configured path has a non-directory parent: {relative}"
                    ));
                }
                if index + 1 == parts.len() {
                    return Ok(Some(metadata));
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "Could not inspect configured path '{relative}': {error}"
                ))
            }
        }
    }
    Ok(None)
}

fn snapshot_copy_files(
    root: &Path,
    settings: &EnvironmentSettings,
) -> Result<Vec<FileSnapshot>, String> {
    validate_policy_paths(root, settings)?;
    let mut result = Vec::with_capacity(settings.copy_paths.len());
    let mut total = 0u64;
    for relative in &settings.copy_paths {
        let path = root.join(relative);
        let metadata = safe_metadata(root, relative)?;
        let Some(metadata) = metadata else {
            result.push(FileSnapshot {
                path: relative.clone(),
                contents: None,
                unix_mode: None,
            });
            continue;
        };
        if !metadata.is_file() {
            return Err(format!("Copy path must be a regular file: {relative}"));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(format!(
                "Copy file exceeds the {} MiB limit: {relative}",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        total = total.saturating_add(metadata.len());
        if total > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "Copied local files exceed the {} MiB total limit",
                MAX_ARCHIVE_BYTES / 1024 / 1024
            ));
        }
        let contents = read_regular_nofollow(&path, relative)?;
        let _ = safe_metadata(root, relative)?;
        if contents.len() as u64 != metadata.len() {
            return Err(format!(
                "Configured local file changed while reading: {relative}"
            ));
        }
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(metadata.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        result.push(FileSnapshot {
            path: relative.clone(),
            contents: Some(contents),
            unix_mode,
        });
    }
    Ok(result)
}

fn latest_archive_id(
    conn: &Connection,
    worktree_id: &str,
    retirement_plan_id: Option<&str>,
) -> Result<Option<String>, String> {
    match retirement_plan_id {
        Some(plan_id) => conn
            .query_row(
                "SELECT archive_id FROM worktree_environment_archives
                  WHERE worktree_id = ?1 AND plan_id = ?2",
                params![worktree_id, plan_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string()),
        None => conn
            .query_row(
                "SELECT archive_id FROM worktree_environment_archives
                  WHERE worktree_id = ?1
                  ORDER BY created_at DESC, archive_order DESC, archive_id DESC LIMIT 1",
                [worktree_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string()),
    }
}

fn load_archive(conn: &Connection, archive_id: &str) -> Result<Vec<FileSnapshot>, String> {
    let files = super::storage::load_archive(conn, archive_id)?
        .into_iter()
        .map(|file| FileSnapshot {
            path: file.path,
            contents: file.contents,
            unix_mode: file.unix_mode,
        })
        .collect::<Vec<_>>();
    let total: usize = files
        .iter()
        .map(|file| file.contents.as_ref().map_or(0, Vec::len))
        .sum();
    if files.len() > MAX_COPY_PATHS || total as u64 > MAX_ARCHIVE_BYTES {
        return Err("Saved worktree environment archive exceeds safety limits".into());
    }
    for file in &files {
        normalize_relative_path(&file.path)?;
        if file
            .contents
            .as_ref()
            .is_some_and(|contents| contents.len() as u64 > MAX_FILE_BYTES)
        {
            return Err(format!(
                "Saved copy file exceeds safety limits: {}",
                file.path
            ));
        }
    }
    Ok(files)
}

fn find_setup_row(conn: &Connection, requested_path: &str) -> Result<Option<SetupRow>, String> {
    let requested = Path::new(requested_path);
    let mut statement = conn
        .prepare(
            "SELECT setup.worktree_id, setup.common_dir, managed.repo, setup.path,
                    setup.origin, setup.archive_id, setup.status, setup.attempts,
                    setup.setup_generation,
                    COALESCE(origins.project_path, '')
               FROM worktree_environment_setup setup
               JOIN managed_worktrees managed ON managed.id = setup.worktree_id
               LEFT JOIN worktree_environment_origins origins
                 ON origins.worktree_id = setup.worktree_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let origin: String = row.get(4)?;
            Ok(SetupRow {
                worktree_id: row.get(0)?,
                common: row.get(1)?,
                repo: row.get(2)?,
                path: row.get(3)?,
                origin: match origin.as_str() {
                    "restored" => SetupOrigin::Restored,
                    "cleaned" => SetupOrigin::Cleaned,
                    _ => SetupOrigin::Fresh,
                },
                archive_id: row.get(5)?,
                status: row.get(6)?,
                attempts: row.get(7)?,
                generation: row.get(8)?,
                project_path: row.get(9)?,
            })
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let row = row.map_err(|error| error.to_string())?;
        if super::path_inside(requested, Path::new(&row.path)) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

fn build_operation(
    conn: &Connection,
    row: &SetupRow,
) -> Result<(String, Vec<FileSnapshot>, Option<PathBuf>, i64), String> {
    let scope = ProjectScope {
        common: row.common.clone(),
        relative: row.project_path.clone(),
        main_path: super::path_to_js(&Path::new(&row.repo).join(&row.project_path)),
    };
    let project = scope.project_path_in(Path::new(&row.path));
    let (settings, mut files) = match row.origin {
        // Output cleanup keeps all selected configuration in place. Never copy
        // primary-checkout files or replay tombstones over unfinished work.
        SetupOrigin::Cleaned => (load_settings(conn, &scope)?, Vec::new()),
        SetupOrigin::Fresh => {
            let settings = load_settings(conn, &scope)?;
            let files = snapshot_copy_files(Path::new(&scope.main_path), &settings)?
                .into_iter()
                .filter(|file| file.contents.is_some())
                .collect();
            (settings, files)
        }
        SetupOrigin::Restored => match row.archive_id.as_deref() {
            // File bytes always come from the exact retirement archive. The
            // command remains editable so a failed restored setup can be fixed
            // in Settings and retried without replacing archived config.
            Some(archive_id) => {
                let settings = load_settings(conn, &scope)?;
                let mut files = load_archive(conn, archive_id)?;
                let archived: HashSet<_> = files.iter().map(|file| file.path.clone()).collect();
                let additional_paths: Vec<_> = settings
                    .copy_paths
                    .iter()
                    .filter(|path| !archived.contains(path.as_str()))
                    .cloned()
                    .collect();
                if !additional_paths.is_empty() {
                    let additional = EnvironmentSettings {
                        environment_version: settings.environment_version,
                        setup_command: String::new(),
                        copy_paths: additional_paths,
                        disposable_paths: Vec::new(),
                    };
                    files.extend(
                        snapshot_copy_files(Path::new(&scope.main_path), &additional)?
                            .into_iter()
                            .filter(|file| file.contents.is_some()),
                    );
                    validate_snapshot_limits(&files)?;
                }
                (settings, files)
            }
            // Worktrees retired before environment archives existed remain
            // recoverable; never substitute today's main-checkout secrets.
            None => (load_settings(conn, &scope)?, Vec::new()),
        },
    };
    let mut statement = conn
        .prepare(
            "SELECT path FROM worktree_environment_setup_files
              WHERE worktree_id = ?1 AND setup_generation = ?2",
        )
        .map_err(|error| error.to_string())?;
    let applied = statement
        .query_map(params![row.worktree_id, row.generation], |record| {
            record.get::<_, String>(0)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut pending = Vec::with_capacity(files.len());
    for file in files {
        let metadata = safe_metadata(&project, &file.path)?;
        // Generation zero predates the per-file ledger. Existing files may
        // already have been copied and edited after a partial attempt.
        let attempted = applied.contains(&file.path) || (row.generation == 0 && metadata.is_some());
        match (&file.contents, metadata, attempted) {
            (Some(_), None, _) => pending.push(file),
            (Some(_), Some(metadata), true) if metadata.is_file() => {}
            (Some(_), Some(_), true) => {
                return Err(format!(
                    "Previously copied config is no longer a regular file: {}",
                    file.path
                ))
            }
            (_, _, false) => pending.push(file),
            // A recorded tombstone is complete. A user-created file after a
            // failed command is their data and is never deleted on retry.
            (None, _, true) => {}
        }
    }
    files = pending;
    let log_path = database_log_path(conn, &row.worktree_id)?;
    Ok((
        settings.setup_command,
        files,
        log_path,
        settings.environment_version,
    ))
}

fn validate_snapshot_limits(files: &[FileSnapshot]) -> Result<(), String> {
    if files.len() > MAX_COPY_PATHS {
        return Err(format!("Copy paths are limited to {MAX_COPY_PATHS} files"));
    }
    let total: u64 = files
        .iter()
        .map(|file| file.contents.as_ref().map_or(0, Vec::len) as u64)
        .sum();
    if total > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "Copied local files exceed the {} MiB total limit",
            MAX_ARCHIVE_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

fn apply_snapshots(operation: &SetupOperation) -> Result<(), String> {
    let root = Path::new(&operation.path);
    let files = &operation.files;
    // Preflight the complete set before writing anything. A selected base may
    // track a file that the primary checkout ignores, and a retry may contain a
    // user edit made after a partial copy. Neither may be overwritten.
    for file in files {
        let relative = normalize_relative_path(&file.path)?;
        validate_git_policy(root, &relative, false)?;
        let metadata = safe_metadata(root, &relative)?;
        match (&file.contents, metadata) {
            (Some(expected), Some(metadata)) if metadata.is_file() => {
                let existing = read_regular_nofollow(&root.join(&relative), &relative)?;
                let _ = safe_metadata(root, &relative)?;
                if &existing != expected {
                    return Err(format!(
                        "Configured file changed after setup copying began: {relative}. Move or reconcile it, then retry setup."
                    ));
                }
            }
            (Some(_), Some(_)) => {
                return Err(format!("Cannot restore config over a non-file: {relative}"))
            }
            (None, Some(_)) => {
                return Err(format!(
                    "A config file archived as deleted now exists: {relative}. Move it, then retry setup."
                ))
            }
            (_, None) => {}
        }
    }
    for file in files {
        let relative = normalize_relative_path(&file.path)?;
        let destination = root.join(&relative);
        let metadata = safe_metadata(root, &relative)?;
        // Persist intent before the write. If the process stops immediately
        // afterward, a missing destination is retried; an existing destination
        // is treated as user-owned and preserved.
        record_file_started(operation, &relative)?;
        match &file.contents {
            None => {}
            Some(contents) => {
                if contents.len() as u64 > MAX_FILE_BYTES {
                    return Err(format!("Saved copy file exceeds safety limits: {relative}"));
                }
                if metadata.is_none() {
                    let parent = destination
                        .parent()
                        .ok_or("Configured file has no parent directory")?;
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("Could not create config directory: {error}"))?;
                    // Recheck after directory creation so a concurrent symlink
                    // swap is caught before opening the destination.
                    let _ = safe_metadata(root, &relative)?;
                    write_atomic(&destination, contents, file.unix_mode)
                        .map_err(|error| format!("Could not restore '{relative}': {error}"))?;
                }
            }
        }
    }
    Ok(())
}

fn read_regular_nofollow(path: &Path, relative: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not safely read '{relative}': {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect '{relative}': {error}"))?;
    if !metadata.is_file() {
        return Err(format!("Copy path must be a regular file: {relative}"));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!("Copy file exceeds safety limits: {relative}"));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut contents)
        .map_err(|error| format!("Could not read '{relative}': {error}"))?;
    if contents.len() as u64 != metadata.len() {
        return Err(format!(
            "Configured local file changed while reading: {relative}"
        ));
    }
    Ok(contents)
}

fn open_runtime_connection(operation: &SetupOperation) -> Result<Connection, String> {
    let path = operation
        .database_path
        .as_deref()
        .ok_or("Worktree setup requires a persistent session database")?;
    let conn = Connection::open(path).map_err(|error| error.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|error| error.to_string())?;
    Ok(conn)
}

fn record_files_applied(operation: &SetupOperation) -> Result<(), String> {
    let conn = open_runtime_connection(operation)?;
    let changed = conn
        .execute(
            "UPDATE worktree_environment_setup
                SET files_applied = 1, updated_at = ?1
              WHERE worktree_id = ?2 AND status = 'running' AND attempts = ?3
                AND setup_generation = ?4",
            params![
                super::now(),
                operation.worktree_id,
                operation.attempt,
                operation.generation
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("Worktree setup attempt changed while copying files".into());
    }
    Ok(())
}

fn record_file_started(operation: &SetupOperation, path: &str) -> Result<(), String> {
    let conn = open_runtime_connection(operation)?;
    let current: bool = conn
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM worktree_environment_setup
                WHERE worktree_id = ?1 AND status = 'running' AND attempts = ?2
                  AND setup_generation = ?3
             )",
            params![
                operation.worktree_id,
                operation.attempt,
                operation.generation
            ],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !current {
        return Err("Worktree setup attempt changed while copying files".into());
    }
    conn.execute(
        "INSERT OR IGNORE INTO worktree_environment_setup_files
           (worktree_id, setup_generation, path, started_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            operation.worktree_id,
            operation.generation,
            path,
            super::now()
        ],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn record_setup_process(operation: &SetupOperation, process_id: u32) -> Result<(), String> {
    let conn = open_runtime_connection(operation)?;
    let changed = conn
        .execute(
            "UPDATE worktree_environment_setup
                SET process_id = ?1, updated_at = ?2
              WHERE worktree_id = ?3 AND status = 'running' AND attempts = ?4
                AND setup_generation = ?5 AND process_id IS NULL",
            params![
                process_id,
                super::now(),
                operation.worktree_id,
                operation.attempt,
                operation.generation
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("Worktree setup attempt changed while starting the command".into());
    }
    Ok(())
}

fn write_atomic(path: &Path, contents: &[u8], unix_mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(not(unix))]
    let _ = unix_mode;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "destination has no parent"))?;
    let mut last_error = None;
    for _ in 0..32 {
        let name = format!(
            ".monocode-setup-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let temporary = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(unix_mode.unwrap_or(0o600));
        }
        match options.open(&temporary) {
            Ok(mut output) => {
                let result = (|| {
                    output.write_all(contents)?;
                    output.sync_all()?;
                    #[cfg(unix)]
                    if let Some(mode) = unix_mode {
                        use std::os::unix::fs::PermissionsExt;
                        output.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
                    }
                    drop(output);
                    #[cfg(windows)]
                    if path.exists() {
                        std::fs::remove_file(path)?;
                    }
                    std::fs::rename(&temporary, path)
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temporary);
                }
                return result;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate temporary file",
        )
    }))
}

fn run_setup_command(operation: &SetupOperation) -> Result<(), String> {
    let (stdout, stderr) = setup_log_files(operation.log_path.as_deref())?;
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C", &operation.setup_command]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .or_else(|| crate::passwd_identity().map(|identity| identity.shell))
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut command = Command::new(shell);
        command.args(["-lc", &operation.setup_command]);
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        command
    };
    command
        .current_dir(&operation.path)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    crate::harness::apply_gui_env(&mut command);
    #[cfg(windows)]
    let spawned = crate::windows::spawn_scoped_job(&mut command);
    #[cfg(not(windows))]
    let spawned = command.spawn();
    #[cfg(windows)]
    let (mut child, setup_job) =
        spawned.map_err(|error| setup_failure(operation, &format!("could not start: {error}")))?;
    #[cfg(not(windows))]
    let mut child =
        spawned.map_err(|error| setup_failure(operation, &format!("could not start: {error}")))?;
    let process_id = child.id();
    operation.process_may_be_live.store(true, Ordering::Release);
    if let Err(error) = record_setup_process(operation, process_id) {
        terminate_setup_process(&mut child);
        #[cfg(windows)]
        let stopped = setup_job
            .terminate_and_wait(Duration::from_secs(4))
            .unwrap_or(false);
        #[cfg(not(windows))]
        let stopped = setup_group_confirmed_absent(process_id);
        if stopped {
            operation
                .process_may_be_live
                .store(false, Ordering::Release);
        }
        return Err(setup_failure(
            operation,
            &format!("could not record its process: {error}"),
        ));
    }
    let started = Instant::now();
    let command_result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => {
                let detail = status
                    .code()
                    .map(|code| format!("exit code {code}"))
                    .unwrap_or_else(|| "a process signal".into());
                break Err(setup_failure(operation, &detail));
            }
            Ok(None) if started.elapsed() < SETUP_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                terminate_setup_process(&mut child);
                break Err(setup_failure(operation, "the 20 minute timeout"));
            }
            Err(error) => {
                terminate_setup_process(&mut child);
                break Err(setup_failure(
                    operation,
                    &format!("could not be monitored: {error}"),
                ));
            }
        }
    };
    #[cfg(windows)]
    let stopped = setup_job
        .terminate_and_wait(Duration::from_secs(4))
        .unwrap_or(false);
    #[cfg(not(windows))]
    let stopped = {
        if !setup_group_confirmed_absent(process_id) {
            terminate_setup_group(process_id);
        }
        setup_group_confirmed_absent(process_id)
    };
    if stopped {
        operation
            .process_may_be_live
            .store(false, Ordering::Release);
        command_result
    } else {
        Err(setup_failure(
            operation,
            "background processes could not be stopped",
        ))
    }
}

fn setup_log_files(path: Option<&Path>) -> Result<(Stdio, Stdio), String> {
    let Some(path) = path else {
        return Ok((Stdio::null(), Stdio::null()));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create private setup log: {error}"))?;
        tighten_directory_permissions(parent)
            .map_err(|error| format!("Could not protect setup log directory: {error}"))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let output = options
        .open(path)
        .map_err(|error| format!("Could not open private setup log: {error}"))?;
    tighten_file_permissions(path)
        .map_err(|error| format!("Could not protect setup log: {error}"))?;
    let errors = output
        .try_clone()
        .map_err(|error| format!("Could not open private setup log: {error}"))?;
    Ok((Stdio::from(output), Stdio::from(errors)))
}

fn setup_failure(operation: &SetupOperation, detail: &str) -> String {
    match operation.log_path.as_deref() {
        Some(path) => format!(
            "Worktree setup failed ({detail}). Retry setup. Details were written to {}.",
            path.display()
        ),
        None => format!("Worktree setup failed ({detail}). Retry setup."),
    }
}

fn terminate_setup_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as i32;
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        crate::hide_window_console(&mut taskkill);
        let _ = taskkill
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(windows))]
fn terminate_setup_group(process_id: u32) {
    #[cfg(unix)]
    unsafe {
        let group = -(process_id as i32);
        libc::kill(group, libc::SIGTERM);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if setup_group_confirmed_absent(process_id) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        libc::kill(group, libc::SIGKILL);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if setup_group_confirmed_absent(process_id) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    #[cfg(not(unix))]
    let _ = process_id;
}

#[cfg(unix)]
fn setup_group_confirmed_absent(process_id: u32) -> bool {
    let result = unsafe { libc::kill(-(process_id as i32), 0) };
    result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn setup_group_confirmed_absent(process_id: u32) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
        if raw.is_null() {
            return std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_INVALID_PARAMETER as i32);
        }
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        (unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }) == WAIT_OBJECT_0
    }
    #[cfg(not(windows))]
    {
        let _ = process_id;
        false
    }
}

fn database_log_path(conn: &Connection, worktree_id: &str) -> Result<Option<PathBuf>, String> {
    let Some(database) = conn.path().filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let Some(parent) = Path::new(database).parent() else {
        return Ok(None);
    };
    let safe_id: String = worktree_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    Ok(Some(
        parent
            .join("worktree-setup-logs")
            .join(format!("{safe_id}.log")),
    ))
}

fn tighten_database_permissions(conn: &Connection) -> rusqlite::Result<()> {
    if let Some(path) = conn.path().filter(|path| !path.is_empty()) {
        tighten_file_permissions(Path::new(path))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let wal = PathBuf::from(format!("{path}-wal"));
        if wal.exists() {
            tighten_file_permissions(&wal)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let shm = PathBuf::from(format!("{path}-shm"));
        if shm.exists() {
            tighten_file_permissions(&shm)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn tighten_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn tighten_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn tighten_directory_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        worktree: PathBuf,
        conn: Connection,
        entry: Owned,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "monocode-worktree-env-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let repo = dir.join("repo");
            let worktree = dir.join("worktree");
            std::fs::create_dir(&repo).unwrap();
            super::super::git(&repo, &["init", "-b", "main"]).unwrap();
            super::super::git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
            super::super::git(&repo, &["config", "user.name", "Test"]).unwrap();
            std::fs::write(
                repo.join(".gitignore"),
                ".env\n.env.local\n.config/\nnode_modules/\nbuild/\n",
            )
            .unwrap();
            std::fs::write(repo.join("tracked.txt"), "tracked\n").unwrap();
            super::super::git(&repo, &["add", "."]).unwrap();
            super::super::git(&repo, &["commit", "-m", "Initial"]).unwrap();
            super::super::git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "monocode/test-session-one",
                    "--",
                    worktree.to_str().unwrap(),
                    "main",
                ],
            )
            .unwrap();
            let common = super::super::repository(repo.to_str().unwrap()).unwrap().1;
            let conn = Connection::open(dir.join("monocode.db")).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE managed_worktrees (
                   id TEXT PRIMARY KEY, repo TEXT NOT NULL, common_dir TEXT NOT NULL,
                   path TEXT NOT NULL UNIQUE, branch TEXT NOT NULL, base_ref TEXT NOT NULL,
                   pinned INTEGER NOT NULL DEFAULT 0, last_used INTEGER NOT NULL,
                   removed INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
            schema(&conn).unwrap();
            let entry = Owned {
                id: "session-one".into(),
                repo: repo.to_string_lossy().into_owned(),
                common,
                path: worktree.to_string_lossy().into_owned(),
                branch: "monocode/test-session-one".into(),
                base_ref: "main".into(),
                pinned: false,
                last_used: 0,
                removed: false,
                creation_oid: None,
                active_retirement_plan_id: None,
                pending_retirement_plan_id: None,
            };
            conn.execute(
                "INSERT INTO managed_worktrees
                   (id, repo, common_dir, path, branch, base_ref, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    entry.id,
                    entry.repo,
                    entry.common,
                    entry.path,
                    entry.branch,
                    entry.base_ref
                ],
            )
            .unwrap();
            Self {
                dir,
                worktree,
                conn,
                entry,
            }
        }

        fn settings(&self) -> EnvironmentSettings {
            EnvironmentSettings {
                environment_version: 0,
                setup_command: String::new(),
                copy_paths: vec![".env".into()],
                disposable_paths: vec!["node_modules".into(), "build".into()],
            }
        }

        fn scope(&self) -> ProjectScope {
            root_scope(&self.entry)
        }

        fn save(&self, mut settings: EnvironmentSettings) -> EnvironmentSettings {
            let scope = self.scope();
            settings.environment_version = load_settings(&self.conn, &scope)
                .unwrap()
                .environment_version;
            save_settings(&self.conn, &scope, &settings).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn automatic_disposal_requires_tracked_manifests_and_ignored_untracked_directories() {
        let fixture = Fixture::new();
        let root = &fixture.worktree;
        let settings = EnvironmentSettings::default();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/generated"), "output").unwrap();
        // A folder name or an untracked manifest cannot classify local data.
        assert!(check_cleanup_with_settings(root, &settings)
            .unwrap()
            .unwrap()
            .contains("node_modules"));
        std::fs::write(root.join("package.json"), "{}\n").unwrap();
        assert!(check_cleanup_with_settings(root, &settings)
            .unwrap()
            .unwrap()
            .contains("node_modules"));
        super::super::git(root, &["add", "package.json"]).unwrap();
        super::super::git(root, &["commit", "-m", "Add Node project"]).unwrap();
        assert_eq!(check_cleanup_with_settings(root, &settings).unwrap(), None);

        // A same-named ordinary file is not a generated directory.
        std::fs::write(root.join("dist"), "user data").unwrap();
        assert!(check_cleanup_with_settings(root, &settings)
            .unwrap()
            .unwrap()
            .contains("dist (untracked)"));
        std::fs::remove_file(root.join("dist")).unwrap();
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/authored.txt"), "authored output").unwrap();
        assert!(check_cleanup_with_settings(root, &settings)
            .unwrap()
            .unwrap()
            .contains("dist/authored.txt (untracked)"));
        super::super::git(root, &["add", "dist/authored.txt"]).unwrap();
        super::super::git(root, &["commit", "-m", "Track authored output"]).unwrap();
        std::fs::write(root.join("dist/authored.txt"), "modified code").unwrap();
        assert!(check_cleanup_with_settings(root, &settings)
            .unwrap()
            .unwrap()
            .contains("dist/authored.txt (modified)"));
    }

    #[test]
    #[cfg(unix)]
    fn automatic_disposal_rejects_symlink_roots_without_touching_the_target() {
        let fixture = Fixture::new();
        let root = &fixture.worktree;
        std::fs::write(root.join("package.json"), "{}\n").unwrap();
        super::super::git(root, &["add", "package.json"]).unwrap();
        super::super::git(root, &["commit", "-m", "Add Node project"]).unwrap();
        let external = fixture.dir.join("external-dependencies");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("keep"), "external data").unwrap();
        std::os::unix::fs::symlink(&external, root.join("node_modules")).unwrap();
        assert!(
            check_cleanup_with_settings(root, &EnvironmentSettings::default())
                .unwrap_err()
                .contains("symlink")
        );
        assert_eq!(
            std::fs::read_to_string(external.join("keep")).unwrap(),
            "external data"
        );
    }

    #[test]
    fn cleanup_allows_only_approved_ignored_paths() {
        let fixture = Fixture::new();
        fixture.save(fixture.settings());
        std::fs::write(fixture.worktree.join(".env"), "SECRET=worktree\n").unwrap();
        std::fs::create_dir(fixture.worktree.join("node_modules")).unwrap();
        std::fs::write(fixture.worktree.join("node_modules/cache"), "generated").unwrap();
        assert_eq!(check_cleanup(&fixture.conn, &fixture.entry).unwrap(), None);

        std::fs::write(fixture.worktree.join("unknown.txt"), "keep").unwrap();
        let reason = check_cleanup(&fixture.conn, &fixture.entry)
            .unwrap()
            .unwrap();
        assert!(reason.contains("unknown.txt (untracked)"), "{reason}");
        std::fs::remove_file(fixture.worktree.join("unknown.txt")).unwrap();
        std::fs::write(fixture.worktree.join("tracked.txt"), "changed").unwrap();
        assert!(check_cleanup(&fixture.conn, &fixture.entry)
            .unwrap()
            .unwrap()
            .contains("tracked.txt (modified)"));
    }

    #[test]
    fn collapsed_ignored_directory_allows_nested_copy_but_lists_unknown_siblings() {
        let fixture = Fixture::new();
        let settings = EnvironmentSettings {
            environment_version: 0,
            setup_command: String::new(),
            copy_paths: vec![".config/project.json".into()],
            disposable_paths: vec![".config/generated".into()],
        };
        fixture.save(settings);
        std::fs::create_dir(fixture.worktree.join(".config")).unwrap();
        std::fs::write(
            fixture.worktree.join(".config/project.json"),
            "private config",
        )
        .unwrap();
        std::fs::create_dir(fixture.worktree.join(".config/generated")).unwrap();
        std::fs::write(
            fixture.worktree.join(".config/generated/cache.bin"),
            "generated",
        )
        .unwrap();
        let status = super::super::git(
            &fixture.worktree,
            &[
                "status",
                "--porcelain=v1",
                "--ignored=matching",
                "--untracked-files=all",
            ],
        )
        .unwrap();
        assert!(status.contains("!! .config/"), "{status:?}");
        assert_eq!(check_cleanup(&fixture.conn, &fixture.entry).unwrap(), None);

        std::fs::write(
            fixture.worktree.join(".config/unknown.sqlite"),
            "valuable unknown data",
        )
        .unwrap();
        let reason = check_cleanup(&fixture.conn, &fixture.entry)
            .unwrap()
            .unwrap();
        assert!(
            reason.contains(".config/unknown.sqlite (ignored)"),
            "{reason}"
        );
    }

    #[test]
    fn preservation_restores_private_bytes_and_tombstones_after_restart() {
        let fixture = Fixture::new();
        fixture.save(fixture.settings());
        std::fs::write(fixture.worktree.join(".env"), b"SECRET=archived\0binary\n").unwrap();
        record_review(&fixture.conn, &fixture.entry, "plan-one").unwrap();
        preserve(&fixture.conn, &fixture.entry, "plan-one").unwrap();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Restored,
            None,
            Some("plan-one"),
        )
        .unwrap();
        std::fs::remove_file(fixture.worktree.join(".env")).unwrap();
        let operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("setup unexpectedly skipped"),
            };
        let result = run_setup(&operation, |_| {});
        finish_setup(&fixture.conn, &operation, &result).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read(fixture.worktree.join(".env")).unwrap(),
            b"SECRET=archived\0binary\n"
        );

        // A later archive of an absent configured file is a deletion marker,
        // not an instruction to copy the primary checkout's current file.
        std::fs::remove_file(fixture.worktree.join(".env")).unwrap();
        record_review(&fixture.conn, &fixture.entry, "plan-two").unwrap();
        preserve(&fixture.conn, &fixture.entry, "plan-two").unwrap();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Restored,
            None,
            Some("plan-two"),
        )
        .unwrap();
        let operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("setup unexpectedly skipped"),
            };
        assert!(run_setup(&operation, |_| {}).is_ok());
        assert!(!fixture.worktree.join(".env").exists());
    }

    #[test]
    fn policy_change_after_review_requires_another_review() {
        let fixture = Fixture::new();
        fixture.save(fixture.settings());
        record_review(&fixture.conn, &fixture.entry, "plan-one").unwrap();
        let mut changed = load_settings(&fixture.conn, &fixture.scope()).unwrap();
        changed.copy_paths.clear();
        fixture.save(changed);
        assert!(preserve(&fixture.conn, &fixture.entry, "plan-one")
            .unwrap_err()
            .contains("settings changed"));
    }

    #[test]
    fn restored_setup_selects_archive_from_authoritative_retirement_plan() {
        let fixture = Fixture::new();
        fixture.save(fixture.settings());
        std::fs::write(fixture.worktree.join(".env"), "successful retirement\n").unwrap();
        record_review(&fixture.conn, &fixture.entry, "successful-plan").unwrap();
        preserve(&fixture.conn, &fixture.entry, "successful-plan").unwrap();

        std::fs::write(fixture.worktree.join(".env"), "newer unrelated archive\n").unwrap();
        record_review(&fixture.conn, &fixture.entry, "newer-plan").unwrap();
        preserve(&fixture.conn, &fixture.entry, "newer-plan").unwrap();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Restored,
            None,
            Some("successful-plan"),
        )
        .unwrap();
        std::fs::remove_file(fixture.worktree.join(".env")).unwrap();

        let operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("setup unexpectedly skipped"),
            };
        let result = run_setup(&operation, |_| {});
        finish_setup(&fixture.conn, &operation, &result).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join(".env")).unwrap(),
            "successful retirement\n"
        );
    }

    #[test]
    fn traversal_git_internals_tracked_files_and_symlinks_are_refused() {
        for path in ["../.env", "/tmp/.env", ".git/config", "C:/secret"] {
            let settings = EnvironmentSettings {
                copy_paths: vec![path.into()],
                ..EnvironmentSettings::default()
            };
            assert!(normalize_settings(settings).is_err(), "accepted {path}");
        }
        let fixture = Fixture::new();
        let tracked = EnvironmentSettings {
            copy_paths: vec!["tracked.txt".into()],
            ..EnvironmentSettings::default()
        };
        assert!(check_cleanup_with_settings(&fixture.worktree, &tracked)
            .unwrap_err()
            .contains("tracked"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::write(fixture.worktree.join(".env-real"), "secret").unwrap();
            symlink(".env-real", fixture.worktree.join(".env")).unwrap();
            assert!(
                check_cleanup_with_settings(&fixture.worktree, &fixture.settings())
                    .unwrap_err()
                    .contains("symlink")
            );
        }
    }

    #[test]
    fn failed_setup_persists_and_can_be_retried() {
        let fixture = Fixture::new();
        let mut settings = fixture.settings();
        settings.setup_command = "exit 7".into();
        let mut settings = fixture.save(settings);
        let scope = fixture.scope();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Fresh,
            Some(&scope),
            None,
        )
        .unwrap();
        let first = match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
            BeginSetup::Run(operation) => operation,
            BeginSetup::Skip => panic!("setup unexpectedly skipped"),
        };
        let failed = run_setup(&first, |_| {});
        assert!(failed.as_ref().unwrap_err().contains("Retry setup"));
        finish_setup(&fixture.conn, &first, &failed).unwrap();

        settings.setup_command = "printf ready > build-result".into();
        let _settings = fixture.save(settings);
        let retry = match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
            BeginSetup::Run(operation) => operation,
            BeginSetup::Skip => panic!("retry unexpectedly skipped"),
        };
        let result = run_setup(&retry, |_| {});
        finish_setup(&fixture.conn, &retry, &result).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join("build-result")).unwrap(),
            "ready"
        );
    }

    #[test]
    fn fresh_copy_never_overwrites_a_path_tracked_by_the_selected_base() {
        let fixture = Fixture::new();
        std::fs::write(fixture.worktree.join(".env"), "tracked base value\n").unwrap();
        super::super::git(&fixture.worktree, &["add", "-f", ".env"]).unwrap();
        super::super::git(&fixture.worktree, &["commit", "-m", "Track env on base"]).unwrap();
        std::fs::write(
            Path::new(&fixture.entry.repo).join(".env"),
            "private main value\n",
        )
        .unwrap();
        fixture.save(fixture.settings());
        let scope = fixture.scope();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Fresh,
            Some(&scope),
            None,
        )
        .unwrap();
        let operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("setup unexpectedly skipped"),
            };
        let error = run_setup(&operation, |_| {}).unwrap_err();
        assert!(error.contains("tracked by Git"), "{error}");
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join(".env")).unwrap(),
            "tracked base value\n"
        );
    }

    #[test]
    fn interrupted_running_attempt_becomes_retryable_only_on_init_reset() {
        let fixture = Fixture::new();
        fixture.save(fixture.settings());
        let scope = fixture.scope();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Fresh,
            Some(&scope),
            None,
        )
        .unwrap();
        let _operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("setup unexpectedly skipped"),
            };
        assert!(begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).is_err());
        reset_interrupted(&fixture.conn).unwrap();
        assert!(matches!(
            begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap(),
            BeginSetup::Run(_)
        ));
    }

    #[test]
    fn partial_copy_ledger_preserves_an_edited_file_and_retries_a_missing_file() {
        let fixture = Fixture::new();
        std::fs::write(Path::new(&fixture.entry.repo).join(".env"), "first\n").unwrap();
        std::fs::write(
            Path::new(&fixture.entry.repo).join(".env.local"),
            "second\n",
        )
        .unwrap();
        let mut settings = fixture.settings();
        settings.copy_paths.push(".env.local".into());
        fixture.save(settings);
        let scope = fixture.scope();
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Fresh,
            Some(&scope),
            None,
        )
        .unwrap();
        let first = match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
            BeginSetup::Run(operation) => operation,
            BeginSetup::Skip => panic!("setup unexpectedly skipped"),
        };
        assert_eq!(first.files.len(), 2);

        // Model a stop after the durable per-file intent and first write.
        record_file_started(&first, ".env").unwrap();
        std::fs::write(fixture.worktree.join(".env"), "first\n").unwrap();
        let interrupted = Err("simulated interruption".to_string());
        finish_setup(&fixture.conn, &first, &interrupted).unwrap();
        std::fs::write(fixture.worktree.join(".env"), "user edit\n").unwrap();

        let retry = match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
            BeginSetup::Run(operation) => operation,
            BeginSetup::Skip => panic!("retry unexpectedly skipped"),
        };
        assert_eq!(
            retry
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            [".env.local"]
        );
        let result = run_setup(&retry, |_| {});
        finish_setup(&fixture.conn, &retry, &result).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join(".env")).unwrap(),
            "user edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join(".env.local")).unwrap(),
            "second\n"
        );
    }

    #[test]
    fn legacy_environment_settings_migrate_only_to_root_scope() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE managed_worktrees (id TEXT PRIMARY KEY);
             CREATE TABLE worktree_environment_settings (
               common_dir TEXT PRIMARY KEY,
               setup_command TEXT NOT NULL DEFAULT '',
               copy_paths_json TEXT NOT NULL DEFAULT '[]',
               disposable_paths_json TEXT NOT NULL DEFAULT '[]',
               updated_at INTEGER NOT NULL
             );
             INSERT INTO worktree_environment_settings VALUES
               ('common', 'legacy command', '[\".env\"]', '[\"dist\"]', 10);",
        )
        .unwrap();
        schema(&conn).unwrap();
        let root = ProjectScope {
            common: "common".into(),
            relative: String::new(),
            main_path: "/repo".into(),
        };
        let nested = ProjectScope {
            common: "common".into(),
            relative: "apps/web".into(),
            main_path: "/repo/apps/web".into(),
        };
        let root_settings = load_settings(&conn, &root).unwrap();
        assert_eq!(root_settings.environment_version, 1);
        assert_eq!(root_settings.setup_command, "legacy command");
        assert_eq!(
            load_settings(&conn, &nested).unwrap(),
            EnvironmentSettings::default()
        );

        let mut changed = root_settings;
        changed.setup_command = "new root command".into();
        save_settings(&conn, &root, &changed).unwrap();
        schema(&conn).unwrap();
        let reloaded = load_settings(&conn, &root).unwrap();
        assert_eq!(reloaded.environment_version, 2);
        assert_eq!(reloaded.setup_command, "new root command");
    }

    #[test]
    fn migration_freezes_legacy_archive_order_across_vacuum() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE managed_worktrees (id TEXT PRIMARY KEY);
             CREATE TABLE worktree_environment_archives (
               archive_id TEXT PRIMARY KEY,
               plan_id TEXT NOT NULL,
               worktree_id TEXT NOT NULL,
               common_dir TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               project_path TEXT NOT NULL DEFAULT '',
               settings_json TEXT
             );
             CREATE TABLE worktree_environment_files (
               archive_id TEXT NOT NULL,
               path TEXT NOT NULL,
               present INTEGER NOT NULL,
               contents BLOB,
               unix_mode INTEGER,
               PRIMARY KEY (archive_id, path)
             );
             INSERT INTO worktree_environment_archives VALUES
               ('z-older', 'plan-older', 'worktree', '/repo/.git', 10, '', '{}');
             INSERT INTO worktree_environment_archives VALUES
               ('a-newer', 'plan-newer', 'worktree', '/repo/.git', 10, '', '{}');
             INSERT INTO worktree_environment_files VALUES
               ('z-older', '.env', 1, X'6f6c646572', 384),
               ('a-newer', '.env', 1, X'6e65776572', 384);",
        )
        .unwrap();
        let legacy_choice: String = conn
            .query_row(
                "SELECT archive_id FROM worktree_environment_archives
                  WHERE worktree_id = 'worktree'
                  ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_choice, "a-newer");

        schema(&conn).unwrap();
        assert_eq!(
            latest_archive_id(&conn, "worktree", None)
                .unwrap()
                .as_deref(),
            Some("a-newer")
        );
        conn.execute_batch("VACUUM").unwrap();
        assert_eq!(
            latest_archive_id(&conn, "worktree", None)
                .unwrap()
                .as_deref(),
            Some("a-newer")
        );
    }

    #[test]
    fn legacy_restoration_keeps_implicit_root_origin_implicit() {
        let fixture = Fixture::new();
        std::fs::write(
            Path::new(&fixture.entry.repo).join(".env"),
            "today's unrelated secret\n",
        )
        .unwrap();
        let mut settings = fixture.settings();
        settings.setup_command = "printf legacy > legacy-setup-ran".into();
        fixture.save(settings);
        assert!(explicit_scope_for_entry(&fixture.conn, &fixture.entry)
            .unwrap()
            .is_none());
        mark_pending(
            &fixture.conn,
            &fixture.entry,
            SetupOrigin::Restored,
            None,
            None,
        )
        .unwrap();
        assert!(explicit_scope_for_entry(&fixture.conn, &fixture.entry)
            .unwrap()
            .is_none());
        assert_eq!(
            scope_for_entry(&fixture.conn, &fixture.entry)
                .unwrap()
                .relative,
            ""
        );
        let operation =
            match begin_setup(&fixture.conn, fixture.worktree.to_str().unwrap()).unwrap() {
                BeginSetup::Run(operation) => operation,
                BeginSetup::Skip => panic!("legacy restored setup unexpectedly skipped"),
            };
        let result = run_setup(&operation, |_| {});
        finish_setup(&fixture.conn, &operation, &result).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join("legacy-setup-ran")).unwrap(),
            "legacy"
        );
        assert!(!fixture.worktree.join(".env").exists());
    }
}
