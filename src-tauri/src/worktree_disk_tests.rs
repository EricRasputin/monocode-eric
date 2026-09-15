use super::super::tests::Fixture;
use super::*;
fn admit<'a>(
    manager: &'a DiskManager,
    conn: &'a Connection,
    path: &str,
    scope: &environment::ProjectScope,
    operation: &str,
    growth: bool,
) -> Result<ReservationLease<'a>, String> {
    coordinate(manager, conn, |measured| {
        manager.admit(conn, path, scope, operation, growth, measured)
    })
}

fn snapshot() -> DiskSnapshot {
    DiskSnapshot {
        schema_version: 1,
        settings: DiskSettings {
            checkout_budget_bytes: Some(100),
            minimum_free_bytes: Some(10),
            initial_allowance_bytes: 20,
            ..DiskSettings::default()
        },
        measured_at: 123,
        complete: true,
        used_bytes: 60,
        reclaimable_bytes: 0,
        pending_bytes: 20,
        checkouts: vec![],
        volumes: vec![VolumeUsage {
            id: "a".into(),
            path: "/a".into(),
            available_bytes: 50,
            measured_at: 123,
        }],
        reservations: vec![Reservation {
            token: "one".into(),
            path: "/one".into(),
            operation: "setup".into(),
            target_bytes: 20,
            remaining_bytes: 20,
            volume_ids: vec!["a".into()],
            created_at: 120,
        }],
        limitations: vec![],
    }
}
#[test]
fn exact_thresholds_and_disabled_limits() {
    let mut s = snapshot();
    let affected = HashSet::from(["a".into()]);
    assert_eq!(capacity_reason(&s, 20, &affected, true), None);
    assert_eq!(
        capacity_reason(&s, 21, &affected, true).unwrap().0,
        "checkoutBudget"
    );
    s.settings.checkout_budget_bytes = None;
    s.volumes[0].available_bytes = 49;
    assert_eq!(
        capacity_reason(&s, 20, &affected, true).unwrap().0,
        "freeSpace"
    );
    s.settings.minimum_free_bytes = None;
    assert_eq!(capacity_reason(&s, 20, &affected, true), None);
    s.volumes[0].available_bytes = 39;
    assert!(capacity_reason(&s, 20, &affected, true).is_some()); // physical space still required
    s.volumes[0].id = "other-volume".into();
    assert_eq!(capacity_reason(&s, 20, &affected, true), None);
    s.complete = false;
    assert_eq!(
        capacity_reason(&s, 0, &affected, true).unwrap().0,
        "measurementUnavailable"
    );
}
#[test]
fn defaults_and_version_conflicts_are_separate_from_recovery_storage() {
    let f = Fixture::new();
    let defaults = settings(&f.conn).unwrap();
    assert_eq!(defaults.checkout_budget_bytes, Some(30 * GIB));
    assert_eq!(defaults.minimum_free_bytes, Some(10 * GIB));
    assert_eq!(defaults.initial_allowance_bytes, 5 * GIB);
    let recovery = super::super::storage::usage(&f.conn).unwrap();
    let mut next = defaults.clone();
    next.checkout_budget_bytes = None;
    next.minimum_free_bytes = None;
    let saved = save_settings(&f.host.disk, &f.conn, next).unwrap();
    assert_eq!(saved.version, 1);
    assert!(save_settings(&f.host.disk, &f.conn, defaults)
        .unwrap_err()
        .contains("CONFLICT"));
    assert_eq!(
        super::super::storage::usage(&f.conn).unwrap().limit_bytes,
        recovery.limit_bytes
    );
    for value in [0, MAX_BYTES + 1] {
        let invalid = DiskSettings {
            initial_allowance_bytes: value,
            ..saved.clone()
        };
        assert!(save_settings(&f.host.disk, &f.conn, invalid).is_err());
    }
}
#[test]
fn concurrent_repositories_cannot_spend_the_same_capacity() {
    let f = Fixture::new();
    let next = DiskSettings {
        checkout_budget_bytes: Some(30 * GIB),
        minimum_free_bytes: None,
        initial_allowance_bytes: 20 * GIB,
        ..DiskSettings::default()
    };
    save_settings(&f.host.disk, &f.conn, next).unwrap();
    let scope = environment::scope_for_cwd(f.repo.to_str().unwrap()).unwrap();
    let path = path_to_js(&f.dir.join("first"));
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let first = threads.spawn(|| {
            let conn = Connection::open(&f.db).unwrap();
            let reservation = admit(&f.host.disk, &conn, &path, &scope, "create", true).unwrap();
            barrier.wait();
            barrier.wait();
            drop(reservation);
        });
        barrier.wait();
        let other = environment::ProjectScope {
            common: path_to_js(&f.dir),
            relative: String::new(),
            main_path: path_to_js(&f.dir),
        };
        let error = admit(
            &f.host.disk,
            &f.conn,
            &path_to_js(&f.dir.join("second")),
            &other,
            "create",
            true,
        )
        .err()
        .unwrap();
        assert!(matches!(
            WorkspaceError::from(error),
            WorkspaceError::Capacity(_)
        ));
        barrier.wait();
        first.join().unwrap();
    });
    assert!(admit(&f.host.disk, &f.conn, &path, &scope, "create", true).is_ok());
}
#[test]
fn retry_subtracts_partial_bytes_and_releases_on_error_and_unwind() {
    let f = Fixture::new();
    let entry = f.create("disk-retry");
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _lease = admit(&f.host.disk, &f.conn, &entry.path, &scope, "setup", true).unwrap();
        std::fs::write(Path::new(&entry.path).join("partial-build"), vec![7; 8192]).unwrap();
        panic!("interrupted operation");
    }));
    let lease = admit(&f.host.disk, &f.conn, &entry.path, &scope, "setup", true).unwrap();
    let s = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(s.reservations.len(), 1);
    assert_eq!(s.pending_bytes, (5 * GIB).saturating_sub(s.used_bytes));
    drop(lease);
    assert!(f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap()
        .reservations
        .is_empty());
}
#[test]
fn startup_reconciles_dead_owners_but_keeps_surviving_setup_and_live_owners() {
    let f = Fixture::new();
    let entry = f.create("disk-restart");
    let reservation = Reservation {
        token: "dead".into(),
        path: entry.path.clone(),
        operation: "setup".into(),
        target_bytes: 5 * GIB,
        remaining_bytes: 5 * GIB,
        volume_ids: vec![],
        created_at: 1,
    };
    let json = serde_json::to_string(&reservation).unwrap();
    f.conn
        .execute(
            "INSERT INTO worktree_disk_reservations VALUES ('dead', ?1, 0, ?2)",
            params![entry.path, json],
        )
        .unwrap();
    f.conn
        .execute(
            "UPDATE worktree_environment_setup SET status = 'running' WHERE worktree_id = ?1",
            [&entry.id],
        )
        .unwrap();
    let manager = DiskManager::default();
    assert_eq!(
        manager
            .snapshot(&f.conn, &HashMap::new(), &[], true)
            .unwrap()
            .reservations
            .len(),
        1
    );
    f.conn
        .execute(
            "UPDATE worktree_environment_setup SET status = 'failed' WHERE worktree_id = ?1",
            [&entry.id],
        )
        .unwrap();
    std::fs::write(Path::new(&entry.path).join("survived"), vec![1; 4096]).unwrap();
    let restarted = DiskManager::default();
    let s = restarted
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert!(s.reservations.is_empty());
    assert!(s.used_bytes > 0);
    f.conn
        .execute(
            "INSERT INTO worktree_disk_reservations VALUES ('live', ?1, ?2, ?3)",
            params![entry.path, std::process::id(), json],
        )
        .unwrap();
    assert_eq!(
        DiskManager::default()
            .snapshot(&f.conn, &HashMap::new(), &[], true)
            .unwrap()
            .reservations
            .len(),
        1
    );
}
#[test]
fn missing_directories_cached_refresh_and_completed_footprints() {
    let f = Fixture::new();
    let entry = f.create("disk-observed");
    std::fs::write(Path::new(&entry.path).join("build"), vec![1; 16384]).unwrap();
    let first = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert!(first.used_bytes >= 16384);
    std::fs::write(Path::new(&entry.path).join("build"), vec![1; 32768]).unwrap();
    assert_eq!(
        f.host
            .disk
            .snapshot(&f.conn, &HashMap::new(), &[], false)
            .unwrap()
            .used_bytes,
        first.used_bytes
    );
    let fresh = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert!(fresh.used_bytes > first.used_bytes);
    let next = DiskSettings {
        initial_allowance_bytes: 1,
        minimum_free_bytes: None,
        ..DiskSettings::default()
    };
    save_settings(&f.host.disk, &f.conn, next).unwrap();
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    let new_path = path_to_js(&f.dir.join("new"));
    let lease = admit(&f.host.disk, &f.conn, &new_path, &scope, "create", true).unwrap();
    let s = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(
        s.reservations[0].target_bytes,
        fresh.checkouts[0].estimated_bytes
    );
    drop(lease);
    std::fs::remove_dir_all(&entry.path).unwrap();
    let missing = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert!(missing.complete);
    assert!(missing.checkouts[0].missing);
    assert_eq!(missing.used_bytes, 0);
}
#[cfg(unix)]
#[test]
fn excludes_primary_git_and_symlinks_deduplicates_hardlinks_and_roots() {
    let f = Fixture::new();
    let first = f.create("disk-first");
    let second = f.create("disk-second");
    let root = Path::new(&first.path);
    let file = root.join("linked");
    std::fs::write(&file, vec![1; 8192]).unwrap();
    std::fs::hard_link(&file, Path::new(&second.path).join("linked")).unwrap();
    std::fs::hard_link(&file, root.join("linked-again")).unwrap();
    let before = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    std::fs::write(f.repo.join("unmanaged"), vec![1; 65536]).unwrap();
    std::fs::write(f.repo.join(".git").join("unaccounted"), vec![1; 65536]).unwrap();
    std::os::unix::fs::symlink(&f.repo, root.join("external")).unwrap();
    std::os::unix::fs::symlink(root, root.join("cycle")).unwrap();
    let after = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    // Directory allocation may grow when adding symlinks; their targets do not.
    assert!(after.used_bytes < before.used_bytes + 8192);
    assert!(after
        .checkouts
        .iter()
        .all(|c| c.limitations.iter().any(|s| s.contains("hard links"))));
    assert!(
        after
            .checkouts
            .iter()
            .map(|c| c.estimated_bytes)
            .sum::<u64>()
            >= after.used_bytes + 8192
    );
    let mut measurement = Measurement::default();
    let mut dirs = HashSet::new();
    let mut files = HashSet::new();
    walk(
        root,
        &HashSet::new(),
        &mut dirs,
        &mut files,
        &mut HashSet::new(),
        &mut measurement,
    )
    .unwrap();
    let counted = measurement.accounted;
    walk(
        root,
        &HashSet::new(),
        &mut dirs,
        &mut files,
        &mut HashSet::new(),
        &mut measurement,
    )
    .unwrap();
    assert_eq!(measurement.accounted, counted);
    assert!(measurement.exclusive < measurement.accounted);
    let missing = volume(&f.dir.join("not-created/yet")).unwrap();
    assert_eq!(missing.id, volume(&f.dir).unwrap().id);
}
#[test]
fn setup_flight_owner_is_the_only_request_admitted() {
    use super::super::setup;
    let f = Fixture::new();
    let entry = f.create("disk-flight");
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let owner = threads.spawn(|| {
            let conn = Connection::open(&f.db).unwrap();
            setup::coordinate_setup(&entry.path, || {
                let _lease = admit(&f.host.disk, &conn, &entry.path, &scope, "setup", true)?;
                barrier.wait();
                while !setup::setup_has_waiter(&entry.path) {
                    std::thread::yield_now();
                }
                Err("shared failure".into())
            })
        });
        barrier.wait();
        let joined =
            setup::coordinate_setup(&entry.path, || panic!("joined request must not reserve"));
        assert_eq!(joined, owner.join().unwrap());
    });
    assert!(f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap()
        .reservations
        .is_empty());
}

#[test]
fn slow_scan_does_not_hold_conversation_writer_or_window_heartbeat() {
    let f = Fixture::new();
    f.create("slow-scan");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let (written_tx, written_rx) = std::sync::mpsc::channel();
    std::thread::scope(|threads| {
        let db = &f.db;
        let host = &f.host;
        let scanner = threads.spawn(move || {
            let conn = Connection::open(db).unwrap();
            host.disk
                .measure_with(&conn, &HashMap::new(), &[], true, || {
                    started_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                })
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        threads.spawn(move || {
            let conn = Connection::open(db).unwrap();
            conn.execute(
                "INSERT INTO sessions(id, cwd) VALUES ('unrelated', '/project')",
                [],
            )
            .unwrap();
            host.operation_guard()
                .unwrap()
                .insert("other-window".into(), vec![PathBuf::from("/other-project")]);
            written_tx.send(()).unwrap();
        });
        let wrote_while_scanning = written_rx.recv_timeout(Duration::from_secs(2));
        coordinate(&f.host.disk, &f.conn, |_| {
            release_handoff(&f.host.disk, &f.conn, "/ready-unrelated")
        })
        .unwrap();
        resume_tx.send(()).unwrap();
        scanner.join().unwrap();
        assert!(
            f.host.disk.state.lock().unwrap().cached.is_some(),
            "ready access invalidated an in-flight scan"
        );
        assert!(
            wrote_while_scanning.is_ok(),
            "scan blocked database writer or heartbeat"
        );
    });
}

#[test]
fn preparation_reservation_survives_ipc_gap_and_setup_takes_it_over() {
    let f = Fixture::new();
    let entry = super::super::tests::create(
        &f.conn,
        &f.host,
        f.repo.to_str().unwrap(),
        "ipc-gap-task",
        "Gap",
        Some("main"),
    )
    .unwrap();
    let before = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(before.reservations.len(), 1);
    assert_eq!(before.reservations[0].operation, "awaitingSetup");
    assert_eq!(before.used_bytes + before.pending_bytes, 5 * GIB);
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    let lease = admit(&f.host.disk, &f.conn, &entry.path, &scope, "setup", true).unwrap();
    let during = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(during.reservations.len(), 1);
    assert_eq!(during.reservations[0].token, before.reservations[0].token);
    assert_eq!(during.pending_bytes, before.pending_bytes);
    drop(lease); // setup failed before running; partial checkout still accounted
    let after = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(after.pending_bytes, 0);
    assert_eq!(after.used_bytes, before.used_bytes);
}

#[test]
fn admission_rechecks_settings_journal_and_root_identity_after_measurement() {
    let f = Fixture::new();
    let entry = f.create("identity");
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    f.host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    let updated = DiskSettings {
        checkout_budget_bytes: Some(1),
        ..DiskSettings::default()
    };
    // Another native process updates policy without touching this cache.
    save_settings(&DiskManager::default(), &f.conn, updated).unwrap();
    let error = f
        .host
        .disk
        .admit(&f.conn, &entry.path, &scope, "setup", true, true)
        .err()
        .unwrap();
    assert!(matches!(
        WorkspaceError::from(error),
        WorkspaceError::Capacity(_)
    ));
    std::fs::rename(&entry.path, f.dir.join("replaced")).unwrap();
    std::fs::create_dir(&entry.path).unwrap();
    assert_eq!(
        f.host
            .disk
            .admit(&f.conn, &entry.path, &scope, "setup", true, true)
            .err()
            .unwrap(),
        RETRY_MEASUREMENT
    );
}

#[test]
fn ready_workspace_access_is_allowed_under_pressure_but_pending_setup_is_checked() {
    let mut s = snapshot();
    s.used_bytes = 200;
    s.volumes[0].available_bytes = 0;
    let affected = HashSet::from(["a".into()]);
    assert_eq!(capacity_reason(&s, 0, &affected, false), None);
    assert_eq!(
        capacity_reason(&s, 0, &affected, true).unwrap().0,
        "checkoutBudget"
    );
    s.settings.checkout_budget_bytes = None;
    assert_eq!(
        capacity_reason(&s, 0, &affected, true).unwrap().0,
        "freeSpace"
    );
    let f = Fixture::new();
    let entry = f.create("ready-pressure");
    save_settings(
        &f.host.disk,
        &f.conn,
        DiskSettings {
            checkout_budget_bytes: Some(1),
            ..DiskSettings::default()
        },
    )
    .unwrap();
    assert!(!environment::needs_setup(&f.conn, &entry.path).unwrap());
    // Native ready-access path never requests measurement or admission.
    coordinate(&f.host.disk, &f.conn, |_| {
        super::super::setup::validate_checkout(&entry, &entry.path)?;
        release_handoff(&f.host.disk, &f.conn, &entry.path)
    })
    .unwrap();
    assert!(f.host.disk.state.lock().unwrap().cached.is_none());
}

#[test]
fn abandoned_handoff_expires_in_live_app_only_after_grace_and_last_lease() {
    let f = Fixture::new();
    let entry = super::super::tests::create(
        &f.conn,
        &f.host,
        f.repo.to_str().unwrap(),
        "abandoned",
        "Abandoned",
        None,
    )
    .unwrap();
    let rows = reservations(&f.conn, &[]).unwrap();
    let time = rows[0].created_at;
    reap_handoffs(&f.host.disk, &f.conn, &HashMap::new(), time + 59_999).unwrap();
    assert_eq!(reservations(&f.conn, &[]).unwrap().len(), 1);
    let windows = HashMap::from([(
        "window".into(),
        vec![PathBuf::from(&entry.path).join("subdirectory")],
    )]);
    reap_handoffs(&f.host.disk, &f.conn, &windows, time + 60_000).unwrap();
    assert_eq!(reservations(&f.conn, &[]).unwrap().len(), 1);
    reap_handoffs(&f.host.disk, &f.conn, &HashMap::new(), time + 60_000).unwrap();
    assert!(reservations(&f.conn, &[]).unwrap().is_empty());
    assert!(Path::new(&entry.path).exists());
}

#[test]
fn ready_access_reuses_cache_and_idle_monitor_does_not_schedule_traversals() {
    let f = Fixture::new();
    let entry = f.create("cached-ready");
    let initial = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    let generation = f.host.disk.state.lock().unwrap().generation;
    for _ in 0..3 {
        coordinate(&f.host.disk, &f.conn, |measured| {
            assert!(!measured);
            release_handoff(&f.host.disk, &f.conn, &entry.path)
        })
        .unwrap();
        let cached = f
            .host
            .disk
            .measure_with(&f.conn, &HashMap::new(), &[], false, || {
                panic!("ready access started a traversal")
            })
            .unwrap();
        assert_eq!(cached.measured_at, initial.measured_at);
    }
    assert_eq!(f.host.disk.state.lock().unwrap().generation, generation);
    assert!(!should_schedule_scan(false, false));
    assert!(should_schedule_scan(true, false));
    assert!(should_schedule_scan(false, true));
    f.host
        .disk
        .state
        .lock()
        .unwrap()
        .cached
        .as_mut()
        .unwrap()
        .measured_at = now() - SCAN_CACHE_MS;
    let scans = std::cell::Cell::new(0);
    for _ in 0..2 {
        f.host
            .disk
            .measure_with(&f.conn, &HashMap::new(), &[], false, || {
                scans.set(scans.get() + 1)
            })
            .unwrap();
    }
    assert_eq!(
        scans.get(),
        1,
        "queued refreshes should share the completed scan"
    );
}

#[test]
fn reclaimable_estimates_require_a_window_snapshot_and_exclude_active_paths() {
    let f = Fixture::new();
    let entry = f.create("reclaimable-view");
    let admission = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert_eq!(admission.reclaimable_bytes, 0);
    let idle = HashMap::from([("native-processes".into(), Vec::new())]);
    let snapshot = f.host.disk.snapshot(&f.conn, &idle, &[], true).unwrap();
    assert!(snapshot.reclaimable_bytes > 0);
    let active = HashMap::from([("editor-window".into(), vec![PathBuf::from(&entry.path)])]);
    assert_eq!(
        f.host
            .disk
            .snapshot(&f.conn, &active, &[], true)
            .unwrap()
            .reclaimable_bytes,
        0
    );
}
