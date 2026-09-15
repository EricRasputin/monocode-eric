//! App-wide checkout admission and monitoring, not a filesystem quota.
//! Lock order: repository -> lifecycle -> disk. Disk never acquires either outer
//! lock. All callers run on native workers; snapshots are cached between scans.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use super::{environment, now, owned, path_to_js, WorktreeHost};
use crate::session_store::SessionStore;

pub const GIB: u64 = 1024 * 1024 * 1024;
const MAX_BYTES: u64 = 1024 * 1024 * GIB;
const SCAN_CACHE_MS: i64 = 5 * 60 * 1000;
const RETRY_MEASUREMENT: &str = "WORKTREE_DISK_REMEASURE";
const FAILURE_PREFIX: &str = "WORKTREE_CAPACITY:";
const LIMITATIONS: &[&str] = &[
    "Estimates count managed checkout directories once, excluding primary checkouts and .git metadata. Symlinks are not followed.",
    "Hard-linked files are counted once app-wide; per-checkout estimates may overlap. Shared hard links are excluded from reclaimable estimates. Filesystem clones, snapshots, compression and shared storage pools prevent exact physical attribution or guaranteed reclaimed space.",
    "Scans are not atomic with builds. Admission control and 30-second monitoring do not limit later growth, kill processes or remove active work. Unmanaged files and Git metadata still consume volume free space.",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiskSettings {
    pub schema_version: u32,
    pub version: i64,
    pub checkout_budget_bytes: Option<u64>,
    pub minimum_free_bytes: Option<u64>,
    pub initial_allowance_bytes: u64,
}
impl Default for DiskSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            version: 0,
            checkout_budget_bytes: Some(30 * GIB),
            minimum_free_bytes: Some(10 * GIB),
            initial_allowance_bytes: 5 * GIB,
        }
    }
}
impl DiskSettings {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 || self.version < 0 {
            return Err("Unsupported disk settings version".into());
        }
        for bytes in [
            self.checkout_budget_bytes,
            self.minimum_free_bytes,
            Some(self.initial_allowance_bytes),
        ]
        .into_iter()
        .flatten()
        {
            if !(1..=MAX_BYTES).contains(&bytes) {
                return Err("Disk settings must be between 1 byte and 1 PiB; use null to disable budget or reserve".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeUsage {
    pub id: String,
    pub path: String,
    pub available_bytes: u64,
    pub measured_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutUsage {
    pub id: String,
    pub path: String,
    pub project_cwd: String,
    pub estimated_bytes: u64,
    pub accounted_bytes: u64,
    pub reclaimable_bytes: u64,
    pub missing: bool,
    pub limitations: Vec<String>,
    pub volume_ids: Vec<String>,
    #[serde(skip)]
    identity: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reservation {
    pub token: String,
    pub path: String,
    pub operation: String,
    pub target_bytes: u64,
    pub remaining_bytes: u64,
    pub volume_ids: Vec<String>,
    pub created_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSnapshot {
    pub schema_version: u32,
    pub settings: DiskSettings,
    pub measured_at: i64,
    pub complete: bool,
    pub used_bytes: u64,
    pub reclaimable_bytes: u64,
    pub pending_bytes: u64,
    pub checkouts: Vec<CheckoutUsage>,
    pub volumes: Vec<VolumeUsage>,
    pub reservations: Vec<Reservation>,
    pub limitations: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityFailure {
    pub code: String,
    pub reason: String,
    pub operation: String,
    pub path: String,
    pub required_bytes: u64,
    pub volume_id: Option<String>,
    pub snapshot: DiskSnapshot,
    pub guidance: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum WorkspaceError {
    Capacity(Box<CapacityFailure>),
    Message(String),
}
impl From<String> for WorkspaceError {
    fn from(error: String) -> Self {
        error
            .strip_prefix(FAILURE_PREFIX)
            .and_then(|json| serde_json::from_str(json).ok())
            .map(Self::Capacity)
            .unwrap_or(Self::Message(error))
    }
}
impl From<&str> for WorkspaceError {
    fn from(error: &str) -> Self {
        Self::Message(error.into())
    }
}

pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS worktree_disk_settings (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1), version INTEGER NOT NULL, settings_json TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS worktree_disk_reservations (
        token TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, owner_pid INTEGER NOT NULL, reservation_json TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS worktree_disk_footprints (
        common_dir TEXT NOT NULL, project_path TEXT NOT NULL, bytes INTEGER NOT NULL, measured_at INTEGER NOT NULL,
        PRIMARY KEY(common_dir, project_path)
    );")
}
fn settings(conn: &Connection) -> Result<DiskSettings, String> {
    let json: Option<String> = conn
        .query_row(
            "SELECT settings_json FROM worktree_disk_settings WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let settings = json
        .map(|s| serde_json::from_str::<DiskSettings>(&s).map_err(|e| e.to_string()))
        .transpose()?
        .unwrap_or_default();
    settings.validate()?;
    Ok(settings)
}

#[derive(Default)]
pub(super) struct DiskManager {
    state: Mutex<DiskState>,
    scanner: Mutex<()>,
}
#[derive(Default)]
struct DiskState {
    reconciled: bool,
    cached: Option<DiskSnapshot>,
    generation: u64,
    ownership: Vec<(String, String, bool)>,
}

impl DiskManager {
    /// Only discard interrupted operations whose native owner is gone. A setup
    /// process surviving a restart keeps its reservation and existing protection.
    fn reconcile(&self, conn: &Connection, state: &mut DiskState) -> Result<(), String> {
        conn.execute("DELETE FROM worktree_disk_reservations WHERE path IN (SELECT path FROM managed_worktrees WHERE removed = 1) AND json_extract(reservation_json, '$.operation') = 'awaitingSetup'", []).map_err(|e| e.to_string())?;
        if state.reconciled {
            return Ok(());
        }
        let mut stmt = conn
            .prepare("SELECT token, path, owner_pid FROM worktree_disk_reservations")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        for (token, path, pid) in rows {
            let running: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM worktree_environment_setup WHERE path = ?1 AND status = 'running')", [&path], |r| r.get(0)).map_err(|e| e.to_string())?;
            if !process_alive(pid) && !running {
                conn.execute(
                    "DELETE FROM worktree_disk_reservations WHERE token = ?1",
                    [token],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        state.reconciled = true;
        state.cached = None;
        Ok(())
    }

    pub(super) fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cached = None;
        state.generation += 1;
    }

    pub(super) fn snapshot(
        &self,
        conn: &Connection,
        windows: &HashMap<String, Vec<PathBuf>>,
        paths: &[PathBuf],
        refresh: bool,
    ) -> Result<DiskSnapshot, String> {
        self.measure_with(conn, windows, paths, refresh, || {})
    }

    fn measure_with(
        &self,
        conn: &Connection,
        windows: &HashMap<String, Vec<PathBuf>>,
        paths: &[PathBuf],
        refresh: bool,
        before_walk: impl FnOnce(),
    ) -> Result<DiskSnapshot, String> {
        // The scan mutex only coalesces scans. Never hold state, SQLite write,
        // repository or window/lifecycle locks while walking the filesystem.
        let _scanner = self.scanner.lock().map_err(|e| e.to_string())?;
        let generation = {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            self.reconcile(conn, &mut state)?;
            if !refresh {
                if let Some(cached) = &state.cached {
                    if now() - cached.measured_at < SCAN_CACHE_MS {
                        return Ok(cached.clone());
                    }
                }
            }
            state.generation
        };
        let ownership = ownership(conn)?;
        before_walk();
        let snapshot = scan(conn, windows, paths, !windows.is_empty())?;
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if state.generation == generation && ownership == self::ownership(conn)? {
            state.cached = Some(snapshot.clone());
            state.ownership = ownership;
        }
        Ok(snapshot)
    }

    pub(super) fn admit<'a>(
        &'a self,
        conn: &'a Connection,
        path: &str,
        scope: &environment::ProjectScope,
        operation: &str,
        needs_growth: bool,
        measured: bool,
    ) -> Result<ReservationLease<'a>, String> {
        if !measured {
            return Err(RETRY_MEASUREMENT.into());
        }
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        let mut snapshot = state.cached.clone().ok_or(RETRY_MEASUREMENT)?;
        // Only cheap identity checks, current policy and journal updates occur
        // under this writer transaction. The directory walk already finished.
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE worktree_disk_settings SET version = version WHERE singleton = 1",
            [],
        )
        .map_err(|e| e.to_string())?;
        if state.ownership != ownership(&tx)?
            || snapshot
                .checkouts
                .iter()
                .any(|entry| entry.identity != root_identity(Path::new(&entry.path)))
        {
            state.cached = None;
            return Err(RETRY_MEASUREMENT.into());
        }
        snapshot.settings = settings(&tx)?;
        snapshot.reservations = reservations(&tx, &snapshot.checkouts)?;
        let previous = snapshot
            .reservations
            .iter()
            .find(|r| r.path == path)
            .cloned();
        if previous
            .as_ref()
            .is_some_and(|r| r.operation != "awaitingSetup")
        {
            return Err("Workspace preparation is already in progress".into());
        }
        snapshot.reservations.retain(|r| r.path != path);
        snapshot.pending_bytes = snapshot
            .reservations
            .iter()
            .map(|r| r.remaining_bytes)
            .sum();
        let paths = [PathBuf::from(path), PathBuf::from(&scope.common)];
        let observed: i64 = tx.query_row("SELECT bytes FROM worktree_disk_footprints WHERE common_dir = ?1 AND project_path = ?2", params![scope.common, scope.relative], |r| r.get(0)).optional().map_err(|e| e.to_string())?.unwrap_or(0);
        let accounted = snapshot
            .checkouts
            .iter()
            .find(|entry| entry.path == path)
            .map_or(0, |entry| entry.accounted_bytes);
        let target = if needs_growth {
            snapshot
                .settings
                .initial_allowance_bytes
                .max(observed.max(0) as u64)
                .max(previous.as_ref().map_or(0, |r| r.target_bytes))
        } else {
            accounted
        };
        let remaining = target.saturating_sub(accounted);
        let mut affected: HashSet<String> = paths
            .iter()
            .map(|path| volume(path).map(|v| v.id))
            .collect::<Result<_, _>>()?;
        if let Some(checkout) = snapshot.checkouts.iter().find(|entry| entry.path == path) {
            affected.extend(checkout.volume_ids.iter().cloned());
        }
        for path in &paths {
            let v = volume(path)?;
            if !snapshot.volumes.iter().any(|existing| existing.id == v.id) {
                snapshot.volumes.push(v);
            }
        }
        // Probe again after the expensive walk, immediately before admission.
        for volume_usage in &mut snapshot.volumes {
            *volume_usage = volume(Path::new(&volume_usage.path))?;
        }
        if let Some((reason, volume_id)) =
            capacity_reason(&snapshot, remaining, &affected, needs_growth)
        {
            let failure = CapacityFailure {
                code: "WORKTREE_CAPACITY".into(), reason, operation: operation.into(), path: path.into(), required_bytes: remaining, volume_id, snapshot,
                guidance: vec!["Review finished worktrees in Settings → Worktrees → Cleanup.".into(), "Adjust or disable the checkout budget or free-space reserve in Worktrees settings.".into(), "Explicitly choose an existing workspace for a new task, or free space and retry this workspace.".into()],
            };
            return Err(format!(
                "{FAILURE_PREFIX}{}",
                serde_json::to_string(&failure).map_err(|e| e.to_string())?
            ));
        }
        let reservation = Reservation {
            token: previous.map_or_else(|| uuid::Uuid::new_v4().to_string(), |r| r.token),
            path: path.into(),
            operation: operation.into(),
            target_bytes: target,
            remaining_bytes: remaining,
            volume_ids: affected.into_iter().collect(),
            created_at: now(),
        };
        tx.execute("INSERT INTO worktree_disk_reservations (token, path, owner_pid, reservation_json) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(token) DO UPDATE SET owner_pid = excluded.owner_pid, reservation_json = excluded.reservation_json", params![reservation.token, path, std::process::id(), serde_json::to_string(&reservation).map_err(|e| e.to_string())?]).map_err(|e| format!("Workspace admission is already in progress or unavailable: {e}"))?;
        tx.commit().map_err(|e| e.to_string())?;
        state.cached = None;
        state.generation += 1;
        Ok(ReservationLease {
            manager: self,
            conn,
            token: reservation.token,
            handed_off: false,
        })
    }
}

pub(super) struct ReservationLease<'a> {
    manager: &'a DiskManager,
    conn: &'a Connection,
    token: String,
    handed_off: bool,
}
impl ReservationLease<'_> {
    pub(super) fn handoff(mut self) -> Result<(), String> {
        let mut state = self.manager.state.lock().map_err(|e| e.to_string())?;
        let json: String = self
            .conn
            .query_row(
                "SELECT reservation_json FROM worktree_disk_reservations WHERE token = ?1",
                [&self.token],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let mut reservation: Reservation =
            serde_json::from_str(&json).map_err(|e| e.to_string())?;
        reservation.operation = "awaitingSetup".into();
        self.conn
            .execute(
                "UPDATE worktree_disk_reservations SET reservation_json = ?1 WHERE token = ?2",
                params![
                    serde_json::to_string(&reservation).map_err(|e| e.to_string())?,
                    self.token
                ],
            )
            .map_err(|e| e.to_string())?;
        state.cached = None;
        state.generation += 1;
        self.handed_off = true;
        Ok(())
    }
}
impl Drop for ReservationLease<'_> {
    fn drop(&mut self) {
        if self.handed_off {
            return;
        }
        let mut state = self.manager.state.lock().unwrap_or_else(|e| e.into_inner());
        let running = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM worktree_environment_setup setup JOIN worktree_disk_reservations reservation ON setup.path = reservation.path WHERE reservation.token = ?1 AND setup.status = 'running')",
            [&self.token], |row| row.get::<_, bool>(0),
        ).unwrap_or(true);
        if running {
            state.cached = None;
            state.generation += 1;
            return;
        }
        if let Err(error) = self.conn.execute(
            "DELETE FROM worktree_disk_reservations WHERE token = ?1",
            [&self.token],
        ) {
            eprintln!("Disk reservation release deferred until restart: {error}");
        }
        // Partial filesystem writes remain owned and will be measured on the next
        // admission; a retry reserves only growth beyond those accounted bytes.
        state.cached = None;
        state.generation += 1;
    }
}

pub(super) fn coordinate<T>(
    manager: &DiskManager,
    conn: &Connection,
    operation: impl Fn(bool) -> Result<T, String>,
) -> Result<T, String> {
    // Ready/local access performs no growth admission and needs no scan. A
    // growing operation asks for measurement before its first mutation.
    let mut measured = false;
    loop {
        match operation(measured) {
            Err(error) if error == RETRY_MEASUREMENT => {
                manager.snapshot(conn, &HashMap::new(), &[], true)?;
                measured = true;
            }
            result => return result,
        }
    }
}

pub(super) fn release_handoff(
    manager: &DiskManager,
    conn: &Connection,
    path: &str,
) -> Result<(), String> {
    let mut state = manager.state.lock().map_err(|e| e.to_string())?;
    let changed = conn.execute("DELETE FROM worktree_disk_reservations WHERE path = ?1 AND json_extract(reservation_json, '$.operation') = 'awaitingSetup'", [path]).map_err(|e| e.to_string())?;
    if changed > 0 {
        state.cached = None;
        state.generation += 1;
    }
    Ok(())
}

fn reap_handoffs(
    manager: &DiskManager,
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    current_time: i64,
) -> Result<(), String> {
    let mut state = manager.state.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT reservation_json FROM worktree_disk_reservations WHERE owner_pid = ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([std::process::id()], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    for json in rows {
        let r: Reservation = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        if r.operation != "awaitingSetup"
            || current_time - r.created_at < 60_000
            || windows
                .values()
                .flatten()
                .any(|path| super::path_inside(path, Path::new(&r.path)))
        {
            continue;
        }
        conn.execute("DELETE FROM worktree_disk_reservations WHERE token = ?1 AND NOT EXISTS(SELECT 1 FROM worktree_environment_setup WHERE path = ?2 AND status = 'running')", params![r.token, r.path]).map_err(|e| e.to_string())?;
        state.cached = None;
        state.generation += 1;
    }
    Ok(())
}

fn ownership(conn: &Connection) -> Result<Vec<(String, String, bool)>, String> {
    let mut result: Vec<_> = owned(conn)?
        .into_iter()
        .map(|entry| (entry.id, entry.path, entry.removed))
        .collect();
    result.sort();
    Ok(result)
}
fn root_identity(path: &Path) -> Option<String> {
    std::fs::symlink_metadata(path).ok().map(|metadata| {
        let (id, _, _) = file_accounting(path, &metadata);
        format!(
            "{id}:{}:{}",
            metadata.is_dir(),
            metadata.file_type().is_symlink()
        )
    })
}

fn capacity_reason(
    snapshot: &DiskSnapshot,
    growth: u64,
    affected: &HashSet<String>,
    needs_growth: bool,
) -> Option<(String, Option<String>)> {
    if !needs_growth {
        return None;
    }
    if !snapshot.complete {
        return Some(("measurementUnavailable".into(), None));
    }
    if snapshot
        .settings
        .checkout_budget_bytes
        .is_some_and(|limit| {
            snapshot
                .used_bytes
                .saturating_add(snapshot.pending_bytes)
                .saturating_add(growth)
                > limit
        })
    {
        return Some(("checkoutBudget".into(), None));
    }
    for volume in &snapshot.volumes {
        if !affected.contains(&volume.id) {
            continue;
        }
        let pending = snapshot
            .reservations
            .iter()
            .filter(|r| r.volume_ids.contains(&volume.id))
            .fold(0u64, |sum, r| sum.saturating_add(r.remaining_bytes));
        if volume.available_bytes
            < snapshot
                .settings
                .minimum_free_bytes
                .unwrap_or(0)
                .saturating_add(pending)
                .saturating_add(growth)
        {
            return Some(("freeSpace".into(), Some(volume.id.clone())));
        }
    }
    None
}

#[derive(Default)]
struct Measurement {
    estimate: u64,
    accounted: u64,
    exclusive: u64,
    volumes: HashMap<String, PathBuf>,
    limitations: Vec<String>,
}

// Directory traversal uses symlink_metadata and never follows symlinks. A
// canonical path set deduplicates aliases/nested managed roots; file identities
// deduplicate hard links globally while retaining per-checkout estimates.
fn walk(
    path: &Path,
    excluded: &HashSet<PathBuf>,
    directories: &mut HashSet<PathBuf>,
    files: &mut HashSet<String>,
    local_files: &mut HashSet<String>,
    measurement: &mut Measurement,
) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let canonical = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if path_to_js(&canonical) != path_to_js(path) {
        return Err(format!(
            "Path changed or contains a symlink: {}",
            path.display()
        ));
    }
    if excluded.contains(&canonical) {
        return Ok(());
    }
    if metadata.is_dir() && !directories.insert(canonical.clone()) {
        return Ok(());
    }
    let (identity, bytes, shared) = file_accounting(&canonical, &metadata);
    measurement
        .volumes
        .entry(volume_id(&canonical, &metadata)?)
        .or_insert_with(|| canonical.clone());
    if local_files.insert(identity.clone()) {
        measurement.estimate = measurement.estimate.saturating_add(bytes);
    }
    if files.insert(identity) {
        measurement.accounted = measurement.accounted.saturating_add(bytes);
        if !shared {
            measurement.exclusive = measurement.exclusive.saturating_add(bytes);
        }
    }
    if shared
        && !measurement
            .limitations
            .iter()
            .any(|v| v == "Contains shared hard links")
    {
        measurement
            .limitations
            .push("Contains shared hard links".into());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name() != ".git" {
                walk(
                    &entry.path(),
                    excluded,
                    directories,
                    files,
                    local_files,
                    measurement,
                )?;
            }
        }
    }
    Ok(())
}

fn scan(
    conn: &Connection,
    windows: &HashMap<String, Vec<PathBuf>>,
    extra_paths: &[PathBuf],
    check_reclaimable: bool,
) -> Result<DiskSnapshot, String> {
    let mut result = DiskSnapshot {
        schema_version: 1,
        settings: settings(conn)?,
        measured_at: now(),
        complete: true,
        used_bytes: 0,
        reclaimable_bytes: 0,
        pending_bytes: 0,
        checkouts: vec![],
        volumes: vec![],
        reservations: vec![],
        limitations: LIMITATIONS.iter().map(|s| (*s).into()).collect(),
    };
    let mut records = owned(conn)?;
    records.sort_by_key(|entry| (entry.path.len(), entry.path.clone()));
    let mut excluded = HashSet::new();
    let mut volume_paths = extra_paths.to_vec();
    for entry in &records {
        excluded.insert(
            std::fs::canonicalize(&entry.repo).unwrap_or_else(|_| PathBuf::from(&entry.repo)),
        );
        excluded.insert(
            std::fs::canonicalize(&entry.common).unwrap_or_else(|_| PathBuf::from(&entry.common)),
        );
        volume_paths.push(PathBuf::from(&entry.common));
    }
    let mut directories = HashSet::new();
    let mut files = HashSet::new();
    for entry in records {
        let path = Path::new(&entry.path);
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if excluded.contains(&canonical) || directories.contains(&canonical) {
            continue;
        }
        let scope = environment::scope_for_entry(conn, &entry)?;
        let mut measurement = Measurement::default();
        if let Err(error) = walk(
            path,
            &excluded,
            &mut directories,
            &mut files,
            &mut HashSet::new(),
            &mut measurement,
        ) {
            result.complete = false;
            measurement.limitations.push(error);
        }
        // Walk collected device identities, and probe all mount points encountered
        // below a checkout rather than inferring a volume from its path prefix.
        volume_paths.push(path.to_path_buf());
        volume_paths.extend(measurement.volumes.values().cloned());
        let reclaimable = check_reclaimable
            && !entry.removed
            && super::blocked(conn, windows, &entry).is_ok_and(|reason| reason.is_none());
        let reclaimable_bytes = if reclaimable {
            measurement.exclusive
        } else {
            0
        };
        let ready: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM worktree_environment_setup WHERE worktree_id = ?1 AND status = 'ready')", [&entry.id], |r| r.get(0)).map_err(|e| e.to_string())?;
        if ready && result.complete && measurement.estimate > 0 {
            conn.execute("INSERT INTO worktree_disk_footprints (common_dir, project_path, bytes, measured_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(common_dir, project_path) DO UPDATE SET bytes = MAX(bytes, excluded.bytes), measured_at = excluded.measured_at", params![scope.common, scope.relative, measurement.estimate.min(i64::MAX as u64) as i64, now()]).map_err(|e| e.to_string())?;
        }
        result.used_bytes = result.used_bytes.saturating_add(measurement.accounted);
        result.reclaimable_bytes = result.reclaimable_bytes.saturating_add(reclaimable_bytes);
        result.checkouts.push(CheckoutUsage {
            identity: root_identity(path),
            id: entry.id,
            path: entry.path,
            project_cwd: scope.main_path,
            estimated_bytes: measurement.estimate,
            accounted_bytes: measurement.accounted,
            reclaimable_bytes,
            missing: !path_exists(&canonical),
            limitations: measurement.limitations,
            volume_ids: measurement.volumes.into_keys().collect(),
        });
    }
    let mut volume_ids = HashSet::new();
    // Probe once per native volume, not once per directory.
    for path in volume_paths {
        let ancestor = existing_ancestor(&path)?;
        let metadata = std::fs::metadata(&ancestor).map_err(|e| e.to_string())?;
        let id = volume_id(&ancestor, &metadata)?;
        if volume_ids.insert(id) {
            result.volumes.push(volume(&ancestor)?);
        }
    }
    result.reservations = reservations(conn, &result.checkouts)?;
    result.pending_bytes = result.reservations.iter().map(|r| r.remaining_bytes).sum();
    result.measured_at = now();
    Ok(result)
}
fn reservations(
    conn: &Connection,
    checkouts: &[CheckoutUsage],
) -> Result<Vec<Reservation>, String> {
    let mut stmt = conn
        .prepare("SELECT reservation_json FROM worktree_disk_reservations ORDER BY token")
        .map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for json in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
    {
        let mut reservation: Reservation =
            serde_json::from_str(&json.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let accounted = checkouts
            .iter()
            .find(|c| c.path == reservation.path)
            .map_or(0, |c| c.accounted_bytes);
        reservation.remaining_bytes = reservation.target_bytes.saturating_sub(accounted);
        result.push(reservation);
    }
    Ok(result)
}
fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
fn existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut path = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&path) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !path.pop() {
                    return Err(error.to_string());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(unix)]
fn file_accounting(_path: &Path, metadata: &std::fs::Metadata) -> (String, u64, bool) {
    use std::os::unix::fs::MetadataExt;
    (
        format!("{}:{}", metadata.dev(), metadata.ino()),
        metadata.blocks().saturating_mul(512),
        metadata.is_file() && metadata.nlink() > 1,
    )
}
#[cfg(unix)]
fn volume_id(_path: &Path, metadata: &std::fs::Metadata) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("device:{}", metadata.dev()))
}
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // statvfs field widths differ between Unix targets.
pub(super) fn volume(path: &Path) -> Result<VolumeUsage, String> {
    use std::os::unix::ffi::OsStrExt;
    let ancestor = existing_ancestor(path)?;
    let encoded =
        std::ffi::CString::new(ancestor.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: a NUL-terminated existing path and valid writable statvfs storage.
    if unsafe { libc::statvfs(encoded.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(VolumeUsage {
        id: volume_id(
            &ancestor,
            &std::fs::metadata(&ancestor).map_err(|e| e.to_string())?,
        )?,
        path: path_to_js(&ancestor),
        available_bytes: (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        measured_at: now(),
    })
}
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    pid != 0 && i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

#[cfg(windows)]
#[path = "worktree_disk_windows.rs"]
mod windows;
#[cfg(windows)]
pub(super) use windows::volume;
#[cfg(windows)]
use windows::{file_accounting, process_alive, volume_id};

#[tauri::command(async)]
pub fn worktree_disk_get(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    refresh: bool,
) -> Result<DiskSnapshot, String> {
    let windows = super::protected_windows(&app, &*host.operation_guard()?);
    let conn = store.open_auxiliary_conn()?;
    reap_handoffs(&host.disk, &conn, &windows, now())?;
    host.disk
        .snapshot(&conn, &windows, std::slice::from_ref(&host.root), refresh)
}
#[tauri::command(async)]
pub fn worktree_disk_settings_set(
    app: AppHandle,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    settings: DiskSettings,
) -> Result<DiskSettings, String> {
    let saved = save_settings(&host.disk, &store.open_auxiliary_conn()?, settings)?;
    let _ = app.emit("worktree-disk-changed", ());
    Ok(saved)
}
fn save_settings(
    manager: &DiskManager,
    conn: &Connection,
    mut next: DiskSettings,
) -> Result<DiskSettings, String> {
    next.validate()?;
    let mut state = manager.state.lock().map_err(|e| e.to_string())?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE worktree_disk_settings SET version = version WHERE singleton = 1",
        [],
    )
    .map_err(|e| e.to_string())?;
    if settings(&tx)?.version != next.version {
        return Err("WORKTREE_DISK_CONFLICT: Disk settings changed in another window. Reload before saving.".into());
    }
    next.version += 1;
    tx.execute("INSERT INTO worktree_disk_settings(singleton, version, settings_json) VALUES(1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET version = excluded.version, settings_json = excluded.settings_json", params![next.version, serde_json::to_string(&next).map_err(|e| e.to_string())?]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    state.cached = None;
    state.generation += 1;
    Ok(next)
}

fn should_schedule_scan(active: bool, pending: bool) -> bool {
    active || pending
}

static SCAN_WORKER: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

pub(super) fn refresh(app: &AppHandle) {
    if app
        .state::<WorktreeHost>()
        .disk
        .state
        .lock()
        .is_ok_and(|state| state.cached.is_some())
    {
        return;
    }
    if let Some(worker) = SCAN_WORKER.get() {
        let _ = worker.try_send(());
    }
    let _ = app.emit("worktree-disk-changed", ());
}

/// Free-space probes have their own timer; an expensive checkout scan cannot
/// delay them. The bounded scan queue coalesces lifecycle and monitor refreshes.
pub(super) fn start_monitor(app: &AppHandle) {
    let (sender, receiver) = mpsc::sync_channel(1);
    if SCAN_WORKER.set(sender).is_err() {
        return;
    }
    let scan_app = app.clone();
    std::thread::spawn(move || {
        while receiver.recv().is_ok() {
            let result = (|| -> Result<(), String> {
                let host = scan_app.state::<WorktreeHost>();
                let windows = super::protected_windows(&scan_app, &*host.operation_guard()?);
                let conn = scan_app.state::<SessionStore>().open_auxiliary_conn()?;
                reap_handoffs(&host.disk, &conn, &windows, now())?;
                let snapshot =
                    host.disk
                        .snapshot(&conn, &windows, std::slice::from_ref(&host.root), false)?;
                let _ = scan_app.emit("worktree-disk-snapshot", snapshot);
                Ok(())
            })();
            if let Err(error) = result {
                let _ = scan_app.emit("worktree-disk-monitor-error", error);
            }
        }
    });
    if let Some(worker) = SCAN_WORKER.get() {
        let _ = worker.try_send(());
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        let result = (|| -> Result<(), String> {
            let host = app.state::<WorktreeHost>();
            let conn = app.state::<SessionStore>().open_auxiliary_conn()?;
            let mut paths = vec![host.root.clone()];
            // Never wait behind checkout mutation to deliver a free-space probe.
            if let Ok(windows) = host.windows.try_lock() {
                paths.extend(windows.values().flatten().cloned());
            }
            paths.extend(app.state::<crate::harness::HarnessHost>().active_workdirs());
            paths.extend(app.state::<crate::pty::PtyHost>().active_workdirs());
            let active = paths.len() > 1;
            for entry in owned(&conn)? {
                paths.extend([PathBuf::from(entry.path), PathBuf::from(entry.common)]);
            }
            // Include nested mounts discovered by the latest scan.
            if let Ok(state) = host.disk.state.try_lock() {
                if let Some(snapshot) = &state.cached {
                    paths.extend(snapshot.volumes.iter().map(|v| PathBuf::from(&v.path)));
                }
            }
            let settings = settings(&conn)?;
            let mut seen = HashSet::new();
            let mut volumes = vec![];
            for path in &paths {
                let ancestor = existing_ancestor(path)?;
                let metadata = std::fs::metadata(&ancestor).map_err(|e| e.to_string())?;
                if seen.insert(volume_id(&ancestor, &metadata)?) {
                    volumes.push(volume(&ancestor)?);
                }
            }
            let _ = app.emit(
                "worktree-disk-pressure",
                serde_json::json!({"settings": settings, "volumes": volumes, "measuredAt": now()}),
            );
            if should_schedule_scan(active, !reservations(&conn, &[])?.is_empty()) {
                if let Some(worker) = SCAN_WORKER.get() {
                    let _ = worker.try_send(());
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = app.emit("worktree-disk-monitor-error", error);
        }
        std::thread::sleep(Duration::from_secs(30));
    });
}

#[cfg(test)]
#[path = "worktree_disk_tests.rs"]
mod tests;
