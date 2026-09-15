//! Run project setup without keeping the global worktree lifecycle lock held.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State, WebviewWindow};

use super::{
    checkouts, disk, environment, owned, path_inside, path_to_js, ref_oid, repository,
    resolve_commit, Owned, WorktreeHost,
};
use crate::session_store::SessionStore;

#[derive(Default)]
struct SetupFlight {
    result: Mutex<Option<Result<(), String>>>,
    finished: Condvar,
}

static ACTIVE: LazyLock<Mutex<HashMap<String, Arc<SetupFlight>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Setup remains protected when a window heartbeat changes or the window closes.
pub(super) fn active_paths() -> Vec<PathBuf> {
    ACTIVE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .keys()
        .map(PathBuf::from)
        .collect()
}

struct SetupLease {
    path: String,
    flight: Arc<SetupFlight>,
}

impl Drop for SetupLease {
    fn drop(&mut self) {
        let mut result = self
            .flight
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if result.is_none() {
            *result = Some(Err(
                "Workspace setup was interrupted; retry preparation".into()
            ));
        }
        ACTIVE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.path);
        self.flight.finished.notify_all();
    }
}

/// Share one setup result per canonical managed checkout across sessions,
/// subdirectories and windows. Wait without holding repository/lifecycle locks.
/// Only in-flight work is cached; every later request checks native setup again.
pub(super) fn coordinate_setup(
    path: &str,
    operation: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let (flight, owner) = {
        let mut active = ACTIVE.lock().map_err(|error| error.to_string())?;
        if let Some(flight) = active.get(path) {
            (Arc::clone(flight), false)
        } else {
            let flight = Arc::new(SetupFlight::default());
            active.insert(path.into(), Arc::clone(&flight));
            (flight, true)
        }
    };
    if !owner {
        let mut result = flight.result.lock().map_err(|error| error.to_string())?;
        while result.is_none() {
            result = flight
                .finished
                .wait(result)
                .map_err(|error| error.to_string())?;
        }
        return result.clone().unwrap();
    }
    let lease = SetupLease {
        path: path.into(),
        flight,
    };
    let result = operation();
    *lease
        .flight
        .result
        .lock()
        .map_err(|error| error.to_string())? = Some(result.clone());
    result
}

#[cfg(test)]
pub(super) fn setup_has_waiter(path: &str) -> bool {
    ACTIVE
        .lock()
        .unwrap()
        .get(path)
        .is_some_and(|flight| Arc::strong_count(flight) > 2)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupProgress<'a> {
    path: &'a str,
    phase: &'a str,
}

/// A delayed setup request must never write into a retired or replaced path.
/// Called while holding the same lifecycle guard as prepare and retirement.
pub(super) fn validate_checkout(entry: &Owned, requested_path: &str) -> Result<(), String> {
    if entry.removed {
        return Err("This worktree was retired. Open it again before running setup.".into());
    }
    let root = Path::new(&entry.path);
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|_| "Worktree folder is unavailable. Open it again before running setup.")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Worktree path was replaced; setup was not run.".into());
    }
    let canonical = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    if path_to_js(&canonical) != entry.path {
        return Err("Worktree path changed; setup was not run.".into());
    }
    let requested = std::fs::canonicalize(requested_path).map_err(|error| error.to_string())?;
    if !requested.is_dir() || !requested.starts_with(&canonical) {
        return Err("Setup project folder is outside the managed worktree.".into());
    }
    if repository(&entry.repo)?.1 != entry.common || repository(&entry.path)?.1 != entry.common {
        return Err("Worktree repository changed; setup was not run.".into());
    }
    let all = checkouts(Path::new(&entry.repo))?;
    if all
        .first()
        .is_some_and(|checkout| checkout.path == entry.path)
    {
        return Err("Setup is only prepared automatically for managed worktrees.".into());
    }
    let checkout = all
        .iter()
        .find(|checkout| checkout.path == entry.path)
        .ok_or("Worktree is no longer registered with Git; setup was not run.")?;
    if checkout.prunable || checkout.branch.as_deref() != Some(&entry.branch) {
        return Err("Worktree branch changed; setup was not run.".into());
    }
    let head = resolve_commit(root, "HEAD")?;
    if ref_oid(
        Path::new(&entry.repo),
        &format!("refs/heads/{}", entry.branch),
    )?
    .as_deref()
        != Some(&head)
    {
        return Err("Worktree branch no longer matches its checkout; setup was not run.".into());
    }
    Ok(())
}

#[tauri::command(async)]
pub fn worktree_setup(
    app: AppHandle,
    window: WebviewWindow,
    store: State<'_, SessionStore>,
    host: State<'_, WorktreeHost>,
    path: String,
) -> Result<(), disk::WorkspaceError> {
    let conn = store.open_auxiliary_conn()?;
    let Some(candidate) = owned(&conn)?
        .into_iter()
        .find(|entry| path_inside(Path::new(&path), Path::new(&entry.path)))
    else {
        // Local and externally managed checkouts keep their existing setup.
        return Ok(());
    };
    let common = candidate.common.clone();
    let result = coordinate_setup(&candidate.path, || {
        let Some((operation, capacity)) = disk::coordinate(&host.disk, &conn, |measured| {
            let _repository = host.repository_guard(&common)?;
            let mut windows = host.operation_guard()?;
            for changed in super::naming::reconcile_repository(&conn, &common)? {
                super::naming::emit(&app, Ok(Some(changed)));
            }
            let Some(entry) = owned(&conn)?.into_iter().find(|entry| {
                entry.id == candidate.id && path_inside(Path::new(&path), Path::new(&entry.path))
            }) else {
                return Err("Worktree ownership changed before setup could start".into());
            };
            validate_checkout(&entry, &path)?;
            if !environment::needs_setup(&conn, &path)? {
                disk::release_handoff(&host.disk, &conn, &entry.path)?;
                super::naming::emit(&app, super::naming::apply_pending(&conn, &entry.id));
                return Ok(None);
            }
            let scope = environment::scope_for_entry(&conn, &entry)?;
            let capacity = host
                .disk
                .admit(&conn, &entry.path, &scope, "setup", true, measured)?;
            let operation = match environment::begin_setup(&conn, &path)? {
                environment::BeginSetup::Skip => {
                    super::naming::emit(&app, super::naming::apply_pending(&conn, &entry.id));
                    return Ok(None);
                }
                environment::BeginSetup::Run(operation) => operation,
            };
            let leases = windows.entry(window.label().into()).or_default();
            let root = PathBuf::from(operation.root_path());
            if !leases.contains(&root) {
                leases.push(root);
            }
            Ok(Some((operation, capacity)))
        })?
        else {
            return Ok(());
        };

        let progress = |phase: &str| {
            let _ = app.emit(
                "worktree-setup-progress",
                SetupProgress { path: &path, phase },
            );
        };
        let result = environment::run_setup(&operation, progress);
        // Persist completion before releasing the setup lease. The running command
        // never holds this lock, so other projects and transcript writes stay live.
        let completion = {
            let _repository = host.repository_guard(&common).map_err(|error| {
                format!("Setup state could not be saved. Restart Monocode before retrying: {error}")
            })?;
            let _windows = host.operation_guard().map_err(|error| {
                format!("Setup state could not be saved. Restart Monocode before retrying: {error}")
            })?;
            let completion =
                environment::finish_setup(&conn, &operation, &result).map_err(|error| {
                    format!(
                        "Setup state could not be saved. Restart Monocode before retrying: {error}"
                    )
                })?;
            super::naming::emit(&app, super::naming::apply_pending(&conn, &candidate.id));
            completion
        };
        let result = match (result, completion) {
            (Ok(()), environment::FinishSetup::Retry(error)) => Err(error),
            (result, _) => result,
        };
        drop(capacity);
        let _ = app.emit("worktree-disk-changed", ());
        progress(if result.is_ok() { "ready" } else { "failed" });
        result
    });
    super::automatic::schedule(&app);
    disk::refresh(&app);
    result?;
    let _repository = host.repository_guard(&common)?;
    let mut windows = host.operation_guard()?;
    let entry = owned(&conn)?
        .into_iter()
        .find(|entry| entry.id == candidate.id)
        .ok_or("Worktree ownership changed during setup")?;
    validate_checkout(&entry, &path)?;
    let leases = windows.entry(window.label().into()).or_default();
    let path = PathBuf::from(path);
    if !leases.contains(&path) {
        leases.push(path);
    }
    Ok(())
}
