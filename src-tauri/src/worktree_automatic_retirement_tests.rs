use super::*;
use crate::worktrees::tests::Fixture;

fn configure(f: &Fixture, mode: Mode) -> Policy {
    let scope = environment::scope_for_cwd(&path_to_js(&f.repo)).unwrap();
    save_policy(
        &f.conn,
        &scope,
        Policy {
            mode,
            ..policy(&f.conn, &scope).unwrap()
        },
    )
    .unwrap()
}

fn session(f: &Fixture, entry: &Owned, id: &str, archived: bool) {
    f.conn
        .execute(
            "INSERT INTO sessions(id, cwd, worktree_cwd, archived) VALUES (?1, ?2, ?3, ?4)",
            params![id, entry.repo, format!("{}/nested", entry.path), archived],
        )
        .unwrap();
}

fn run(f: &Fixture) {
    maintain_with(&f.conn, &f.host, Clone::clone).unwrap();
}
fn item(f: &Fixture) -> Pending {
    pending(&f.conn).unwrap().into_iter().next().unwrap()
}
fn plan_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM worktree_retirement_plans", [], |r| {
        r.get(0)
    })
    .unwrap()
}

fn prepare(f: &Fixture, entry: &Owned) -> String {
    discover(&f.conn, &f.host).unwrap();
    prepare_attempt(&f.conn, &f.host, &entry.id, &Clone::clone)
        .unwrap()
        .unwrap()
}

fn execute(f: &Fixture, entry: &Owned, plan: &str) -> WorktreeRetirementResult {
    execute_retirement_coordinated_with(
        &f.conn,
        &f.host,
        plan,
        &[WorktreeRetirementSelection {
            id: entry.id.clone(),
            delete_local_branch: false,
            delete_remote_branch: false,
        }],
        Clone::clone,
    )
    .unwrap()
    .results
    .remove(0)
}

#[test]
fn legacy_and_initial_preferences_require_an_explicit_versioned_opt_in() {
    let f = Fixture::new();
    let scope = environment::scope_for_cwd(&path_to_js(&f.repo)).unwrap();
    f.conn
        .execute(
            "INSERT INTO worktree_settings VALUES (?1, 1, 1, 1)",
            [&scope.common],
        )
        .unwrap();
    schema(&f.conn).unwrap();
    assert_eq!(policy(&f.conn, &scope).unwrap(), Policy::default());
    let entry = f.create("automatic-default");
    session(&f, &entry, "archived", true);
    run(&f);
    assert!(Path::new(&entry.path).exists());
    assert_eq!(plan_count(&f.conn), 0);
    let saved = configure(&f, Mode::Automatic);
    assert_eq!(saved.version, 1);
    assert!(save_policy(&f.conn, &scope, Policy::default())
        .unwrap_err()
        .contains("another window"));
    let nested = environment::ProjectScope {
        relative: "nested".into(),
        ..scope
    };
    assert_eq!(policy(&f.conn, &nested).unwrap().mode, Mode::Manual);
    run(&f);
    assert!(!Path::new(&entry.path).exists());
}

#[test]
fn shared_and_bulk_archives_retire_each_checkout_once_and_keep_history() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let one = f.create("automatic-shared");
    let two = f.create("automatic-bulk");
    session(&f, &one, "first", true);
    session(&f, &one, "second", false);
    session(&f, &two, "third", false);
    run(&f);
    assert!(pending(&f.conn).unwrap().is_empty());
    f.conn
        .execute("UPDATE sessions SET archived = 1", [])
        .unwrap();
    run(&f);
    run(&f);
    assert_eq!(plan_count(&f.conn), 2);
    assert!(pending(&f.conn)
        .unwrap()
        .iter()
        .all(|p| p.status == "complete"));
    assert!(!Path::new(&one.path).exists());
    assert!(!Path::new(&two.path).exists());
    assert_eq!(
        f.conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE archived = 1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    assert!(ref_oid(&f.repo, &format!("refs/heads/{}", one.branch))
        .unwrap()
        .is_some());
}

#[test]
fn exact_unmerged_code_and_selected_configuration_survive_restart_and_gc() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let scope = environment::scope_for_cwd(&path_to_js(&f.repo)).unwrap();
    environment::save_settings(
        &f.conn,
        &scope,
        &environment::EnvironmentSettings {
            copy_paths: vec![".env".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let entry = f.create("automatic-recovery");
    std::fs::write(
        Path::new(&entry.path).join("tracked.txt"),
        "unmerged exact code",
    )
    .unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "Unmerged work"]).unwrap();
    std::fs::write(Path::new(&entry.path).join(".env"), "private configuration").unwrap();
    let head = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    session(&f, &entry, "history", true);
    run(&f);
    assert_eq!(item(&f).status, "complete");
    let conn = Connection::open(&f.db).unwrap();
    schema(&conn).unwrap();
    git(&f.repo, &["gc", "--prune=now"]).unwrap();
    let current = owned(&conn).unwrap().remove(0);
    open_owned(&conn, &current).unwrap();
    if let environment::BeginSetup::Run(operation) =
        environment::begin_setup(&conn, &entry.path).unwrap()
    {
        let result = environment::run_setup(&operation, |_| {});
        environment::finish_setup(&conn, &operation, &result).unwrap();
        result.unwrap();
    }
    assert_eq!(
        resolve_commit(Path::new(&entry.path), "HEAD").unwrap(),
        head
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join(".env")).unwrap(),
        "private configuration"
    );
    assert!(f
        .conn
        .query_row(
            "SELECT archived FROM sessions WHERE id = 'history'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}

#[test]
fn disabling_saved_policy_prevents_a_prepared_but_unstarted_removal() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-policy-race");
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    configure(&f, Mode::Manual);
    assert!(execute(&f, &entry, &plan)
        .error
        .unwrap()
        .contains("disabled"));
    run(&f);
    assert_eq!(item(&f).status, "paused");
    assert!(Path::new(&entry.path).exists());
    configure(&f, Mode::Automatic);
    run(&f);
    assert_eq!(item(&f).plan_id.as_deref(), Some(plan.as_str()));
    assert_eq!(plan_count(&f.conn), 1);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn late_window_file_terminal_session_and_pin_activity_are_revalidated() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-activity-race");
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    for (label, suffix) in [
        ("session-window", ""),
        ("editor-window", "/tracked.txt"),
        ("terminal-window", "/nested"),
    ] {
        f.host.windows.lock().unwrap().insert(
            label.into(),
            vec![PathBuf::from(format!("{}{suffix}", entry.path))],
        );
        assert!(execute(&f, &entry, &plan).error.unwrap().contains("window"));
        f.host.windows.lock().unwrap().clear();
    }
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 1", [])
        .unwrap();
    assert!(execute(&f, &entry, &plan).error.unwrap().contains("Pinned"));
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 0", [])
        .unwrap();
    f.conn
        .execute("UPDATE sessions SET pinned = 1", [])
        .unwrap();
    assert!(execute(&f, &entry, &plan).error.unwrap().contains("pinned"));
    f.conn
        .execute("UPDATE sessions SET pinned = 0, archived = 0", [])
        .unwrap();
    assert!(execute(&f, &entry, &plan).error.unwrap().contains("active"));
    f.conn
        .execute("UPDATE sessions SET archived = 1", [])
        .unwrap();
    run(&f);
    assert_eq!(plan_count(&f.conn), 1);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn startup_waits_for_all_windows_and_coalesces_concurrent_passes() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-windows");
    session(&f, &entry, "history", true);
    maintain_with(&f.conn, &f.host, |_| {
        HashMap::from([("$unregistered-windows".into(), vec![])])
    })
    .unwrap();
    assert!(item(&f).reason.unwrap().contains("register"));
    assert_eq!(plan_count(&f.conn), 0);
    std::thread::scope(|scope| {
        let host = &f.host;
        let db = &f.db;
        let first =
            scope.spawn(move || maintain_with(&Connection::open(db).unwrap(), host, Clone::clone));
        let second =
            scope.spawn(move || maintain_with(&Connection::open(db).unwrap(), host, Clone::clone));
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
    });
    assert_eq!(plan_count(&f.conn), 1);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn a_window_opening_between_phases_or_after_preservation_blocks_removal() {
    use std::cell::Cell;
    for barrier_call in [1, 4] {
        let f = Fixture::new();
        configure(&f, Mode::Automatic);
        let entry = f.create("automatic-new-window");
        session(&f, &entry, "history", true);
        let plan = prepare(&f, &entry);
        let calls = Cell::new(0);
        let report = execute_retirement_coordinated_with(
            &f.conn,
            &f.host,
            &plan,
            &[WorktreeRetirementSelection {
                id: entry.id.clone(),
                delete_local_branch: false,
                delete_remote_branch: false,
            }],
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() >= barrier_call {
                    HashMap::from([("$unregistered-windows".into(), vec![])])
                } else {
                    HashMap::new()
                }
            },
        )
        .unwrap();
        assert!(report.results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("register"));
        assert!(Path::new(&entry.path).exists());
        if barrier_call == 4 {
            assert!(f
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM worktree_environment_archives)",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap());
        }
        run(&f);
        assert_eq!(item(&f).status, "complete");
        assert_eq!(plan_count(&f.conn), 1);
    }
}

#[test]
fn damaged_candidate_ownership_and_scope_do_not_stop_other_retirements() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let bad = f.create("automatic-bad-identity");
    let scope_bad = f.create("automatic-bad-scope");
    let good = f.create("automatic-good");
    for (id, entry) in [("bad", &bad), ("scope", &scope_bad), ("good", &good)] {
        session(&f, entry, id, true);
    }
    f.conn
        .execute(
            "UPDATE managed_worktrees SET common_dir = '' WHERE id = ?1",
            [&bad.id],
        )
        .unwrap();
    f.conn.execute("UPDATE worktree_environment_origins SET project_path = '../outside' WHERE worktree_id = ?1", [&scope_bad.id]).unwrap();
    run(&f);
    assert!(!Path::new(&good.path).exists());
    assert!(Path::new(&bad.path).exists());
    assert!(Path::new(&scope_bad.path).exists());
    let items = pending(&f.conn).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.status == "blocked" && i.reason.is_some())
            .count(),
        2
    );
    let overview = overview(&f.conn, &HashMap::new(), &good.repo).unwrap();
    assert!(overview
        .automatic_retirement
        .iter()
        .any(|i| i.id == scope_bad.id && i.reason.is_some()));
}

#[test]
fn preparation_handoff_and_coordinated_setup_protect_the_ipc_gap() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = crate::worktrees::tests::create(
        &f.conn,
        &f.host,
        &path_to_js(&f.repo),
        "automatic-setup-gap",
        "Setup",
        Some("main"),
    )
    .unwrap();
    session(&f, &entry, "history", true);
    run(&f);
    assert!(item(&f).reason.unwrap().contains("reserved"));
    disk::release_handoff(&f.host.disk, &f.conn, &entry.path).unwrap();
    setup::coordinate_setup(&entry.path, || {
        run(&f);
        assert!(item(&f).reason.unwrap().contains("setup"));
        Ok(())
    })
    .unwrap();
    run(&f);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn dirty_unknown_and_git_locked_checkouts_remain_protected_without_extra_plans() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-local-data");
    session(&f, &entry, "history", true);
    for file in ["tracked.txt", "unknown.txt", ".env"] {
        let path = Path::new(&entry.path).join(file);
        std::fs::write(&path, "do not remove").unwrap();
        run(&f);
        assert!(item(&f).reason.unwrap().contains(file));
        assert_eq!(plan_count(&f.conn), 0);
        if file == "tracked.txt" {
            git(Path::new(&entry.path), &["restore", "tracked.txt"]).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
    git(&f.repo, &["worktree", "lock", &entry.path]).unwrap();
    run(&f);
    assert!(item(&f).reason.unwrap().contains("locked"));
    git(&f.repo, &["worktree", "unlock", &entry.path]).unwrap();
    let lock = f.repo.join(".git/packed-refs.lock");
    std::fs::write(&lock, "locked").unwrap();
    run(&f);
    assert!(item(&f).reason.unwrap().contains("Git lock"));
    std::fs::remove_file(lock).unwrap();
    run(&f);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn partial_git_removal_retries_the_same_journal_after_restart() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-partial");
    session(&f, &entry, "history", true);
    let before = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], true)
        .unwrap();
    assert!(before.used_bytes > 0);
    f.conn.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE OF removed ON managed_worktrees WHEN NEW.removed = 1 BEGIN SELECT RAISE(FAIL, 'completion unavailable'); END;").unwrap();
    run(&f);
    assert!(!Path::new(&entry.path).exists());
    let after = f
        .host
        .disk
        .snapshot(&f.conn, &HashMap::new(), &[], false)
        .unwrap();
    assert!(after.used_bytes < before.used_bytes);
    assert!(
        after
            .checkouts
            .iter()
            .find(|c| c.id == entry.id)
            .unwrap()
            .missing
    );
    assert_eq!(item(&f).status, "failed");
    assert!(item(&f).reason.unwrap().contains("completion unavailable"));
    let plan = item(&f).plan_id;
    f.conn
        .execute_batch("DROP TRIGGER fail_completion;")
        .unwrap();
    let conn = Connection::open(&f.db).unwrap();
    schema(&conn).unwrap();
    maintain_with(&conn, &f.host, Clone::clone).unwrap();
    assert_eq!(item(&f).plan_id, plan);
    assert_eq!(item(&f).status, "complete");
    assert_eq!(plan_count(&f.conn), 1);
    assert!(owned(&conn).unwrap()[0].removed);
}

#[test]
fn recovery_quota_failure_keeps_checkout_and_reuses_plan_when_limit_increases() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    std::fs::write(f.repo.join(".gitignore"), ".env\n.env.local\n").unwrap();
    git(&f.repo, &["commit", "-am", "Ignore configuration"]).unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&f.repo)).unwrap();
    environment::save_settings(
        &f.conn,
        &scope,
        &environment::EnvironmentSettings {
            copy_paths: vec![".env".into(), ".env.local".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let entry = f.create("automatic-quota");
    std::fs::write(Path::new(&entry.path).join(".env"), vec![b'a'; 700_000]).unwrap();
    std::fs::write(
        Path::new(&entry.path).join(".env.local"),
        vec![b'b'; 700_000],
    )
    .unwrap();
    let usage = storage::usage(&f.conn).unwrap();
    storage::set_limit(&f.conn, 1024 * 1024, usage.version).unwrap();
    session(&f, &entry, "history", true);
    run(&f);
    run(&f);
    assert!(Path::new(&entry.path).exists());
    assert!(item(&f).reason.unwrap().contains("storage"));
    assert_eq!(plan_count(&f.conn), 1);
    let usage = storage::usage(&f.conn).unwrap();
    storage::set_limit(&f.conn, 2 * 1024 * 1024, usage.version).unwrap();
    run(&f);
    assert_eq!(item(&f).status, "complete");
    assert_eq!(plan_count(&f.conn), 1);
}

#[test]
fn automatic_plan_cannot_be_used_to_request_branch_deletion() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-no-delete");
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    let report = execute_retirement_coordinated_with(
        &f.conn,
        &f.host,
        &plan,
        &[WorktreeRetirementSelection {
            id: entry.id.clone(),
            delete_local_branch: true,
            delete_remote_branch: true,
        }],
        Clone::clone,
    )
    .unwrap();
    assert!(report.results[0]
        .error
        .as_ref()
        .unwrap()
        .contains("never deletes branches"));
    assert!(Path::new(&entry.path).exists());
    let requested: bool = f
        .conn
        .query_row(
            "SELECT local_requested OR remote_requested FROM worktree_retirement_items",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!requested);
    run(&f);
    assert_eq!(item(&f).status, "complete");
}

#[test]
fn disabled_automatic_attempt_can_be_resumed_by_a_fresh_manual_review() {
    for folder_removed in [false, true] {
        let f = Fixture::new();
        configure(&f, Mode::Automatic);
        let entry = f.create("automatic-manual-takeover");
        session(&f, &entry, "history", true);
        let plan = prepare(&f, &entry);
        let snapshot = load_retirement_snapshot(&f.conn, &plan, &entry.id)
            .unwrap()
            .unwrap();
        ensure_local_recovery(&f.conn, &snapshot).unwrap();
        environment::preserve(&f.conn, &entry, &plan).unwrap();
        begin_worktree_removal(&f.conn, &snapshot).unwrap();
        if folder_removed {
            git(&f.repo, &["worktree", "remove", "--", &entry.path]).unwrap();
        }
        configure(&f, Mode::Manual);
        run(&f);
        assert_eq!(item(&f).status, "paused");
        let inventory = overview(&f.conn, &HashMap::new(), &entry.repo).unwrap();
        assert!(
            inventory
                .entries
                .iter()
                .find(|e| e.id.as_deref() == Some(&entry.id))
                .unwrap()
                .retirement_pending
        );
        let manual = build_retirement_plan_coordinated(
            &f.conn,
            &f.host,
            &HashMap::new(),
            &[],
            Some(&entry.repo),
            std::slice::from_ref(&entry.id),
        )
        .unwrap();
        assert_eq!(manual.entries.len(), 1, "{:?}", manual.kept);
        assert_ne!(manual.plan_id, plan);
        assert_eq!(
            Path::new(&entry.path).exists(),
            !folder_removed,
            "review itself cannot remove a folder"
        );
        let result = execute(&f, &entry, &manual.plan_id);
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(result.worktree_removed);
        assert!(!result.local_branch_deleted && !result.remote_branch_deleted);
        assert!(ref_oid(&f.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .is_some());
        assert_eq!(
            direct_ref_oid(&f.repo, &snapshot.recovery_ref)
                .unwrap()
                .as_deref(),
            Some(snapshot.commit_oid.as_str())
        );
        let requests: i64 = f
            .conn
            .query_row(
                "SELECT SUM(local_requested + remote_requested) FROM worktree_retirement_items",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(requests, 0);
    }
}

#[test]
fn manual_takeover_keeps_pending_recovery_when_ownership_or_activity_changed() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-takeover-pin");
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    let snapshot = load_retirement_snapshot(&f.conn, &plan, &entry.id)
        .unwrap()
        .unwrap();
    ensure_local_recovery(&f.conn, &snapshot).unwrap();
    environment::preserve(&f.conn, &entry, &plan).unwrap();
    begin_worktree_removal(&f.conn, &snapshot).unwrap();
    configure(&f, Mode::Manual);
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 1", [])
        .unwrap();
    let manual = build_retirement_plan_coordinated(
        &f.conn,
        &f.host,
        &HashMap::new(),
        &[],
        Some(&entry.repo),
        std::slice::from_ref(&entry.id),
    )
    .unwrap();
    assert!(manual.entries.is_empty());
    assert!(manual.kept[0].reason.contains("Pinned"));
    assert_eq!(
        owned(&f.conn).unwrap()[0]
            .pending_retirement_plan_id
            .as_deref(),
        Some(plan.as_str())
    );
}

#[test]
fn changed_commits_and_saved_environment_get_one_new_review_without_replacing_old_recovery() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-new-state");
    session(&f, &entry, "history", true);
    let original = prepare(&f, &entry);
    let snapshot = load_retirement_snapshot(&f.conn, &original, &entry.id)
        .unwrap()
        .unwrap();
    ensure_local_recovery(&f.conn, &snapshot).unwrap();
    environment::preserve(&f.conn, &entry, &original).unwrap();
    std::fs::write(
        Path::new(&entry.path).join("tracked.txt"),
        "new committed code",
    )
    .unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "New work"]).unwrap();
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    environment::save_settings(
        &f.conn,
        &scope,
        &environment::EnvironmentSettings {
            copy_paths: vec![".env".into()],
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(Path::new(&entry.path).join(".env"), "new configuration").unwrap();
    let new_head = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    run(&f);
    run(&f);
    assert_eq!(item(&f).status, "complete");
    assert_eq!(plan_count(&f.conn), 2);
    assert_ne!(item(&f).plan_id.as_deref(), Some(original.as_str()));
    assert_eq!(
        direct_ref_oid(&f.repo, &snapshot.recovery_ref)
            .unwrap()
            .as_deref(),
        Some(snapshot.commit_oid.as_str())
    );
    assert_eq!(
        latest_local_recovery(&f.conn, &entry.id)
            .unwrap()
            .unwrap()
            .commit_oid,
        new_head
    );
}

#[test]
fn archive_reporting_keeps_pinned_automatic_failures_visible_without_empty_plans() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-archive-report");
    session(&f, &entry, "history", true);
    f.conn
        .execute("UPDATE sessions SET pinned = 1", [])
        .unwrap();
    run(&f);
    let result = archive_result(&f.conn, &f.host, &["history".into()], &HashMap::new()).unwrap();
    assert!(result.review.entries.is_empty() && result.review.kept.is_empty());
    assert_eq!(result.automatic.len(), 1);
    assert!(result.automatic[0]
        .reason
        .as_ref()
        .unwrap()
        .contains("pinned"));
    assert_eq!(plan_count(&f.conn), 0);
    f.conn
        .execute("UPDATE sessions SET pinned = 0", [])
        .unwrap();
    run(&f);
    for _ in 0..2 {
        let result =
            archive_result(&f.conn, &f.host, &["history".into()], &HashMap::new()).unwrap();
        assert_eq!(result.automatic[0].status, "complete");
    }
    assert_eq!(plan_count(&f.conn), 1);
}

#[test]
fn pausing_automatic_cleanup_keeps_the_last_failure_explanation_visible() {
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let entry = f.create("automatic-pause-failure");
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    record_status(
        &f.conn,
        &entry.id,
        "failed",
        Some("Recovery storage is full"),
    )
    .unwrap();
    configure(&f, Mode::Manual);
    run(&f);
    run(&f);
    assert_eq!(item(&f).status, "paused");
    assert_eq!(item(&f).reason.as_deref(), Some("Recovery storage is full"));
    assert_eq!(item(&f).plan_id.as_deref(), Some(plan.as_str()));
    assert!(Path::new(&entry.path).exists());
}

#[test]
fn bulk_archive_routes_each_repository_to_its_saved_mode() {
    let f = Fixture::new();
    let other = Fixture::new();
    configure(&f, Mode::Automatic);
    let automatic = f.create("automatic-mixed");
    let manual = crate::worktrees::tests::create(
        &f.conn,
        &f.host,
        &path_to_js(&other.repo),
        "manual-mixed",
        "Manual task",
        Some("main"),
    )
    .unwrap();
    disk::release_handoff(&f.host.disk, &f.conn, &manual.path).unwrap();
    session(&f, &automatic, "automatic-history", true);
    session(&f, &manual, "manual-history", true);
    run(&f);
    let report = archive_result(
        &f.conn,
        &f.host,
        &[
            "automatic-history".into(),
            "manual-history".into(),
            "automatic-history".into(),
        ],
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(report.automatic.len(), 1);
    assert_eq!(report.automatic[0].status, "complete");
    assert_eq!(report.review.entries.len(), 1);
    assert_eq!(report.review.entries[0].id, manual.id);
    assert!(Path::new(&manual.path).exists());
    assert!(!Path::new(&automatic.path).exists());
    assert_eq!(plan_count(&f.conn), 2);
}

#[cfg(unix)]
#[test]
fn automatic_success_and_failure_never_contact_a_configured_remote() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    configure(&f, Mode::Automatic);
    let helper = f.dir.join("ssh-helper");
    let marker = f.dir.join("remote-contacted");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &f.repo,
        &["config", "core.sshCommand", helper.to_str().unwrap()],
    )
    .unwrap();
    git(
        &f.repo,
        &["remote", "add", "origin", "ssh://invalid.example/repo"],
    )
    .unwrap();
    git(&f.repo, &["config", "branch.main.remote", "origin"]).unwrap();
    git(&f.repo, &["config", "branch.main.merge", "refs/heads/main"]).unwrap();
    let entry = f.create("automatic-no-network");
    git(
        &f.repo,
        &[
            "config",
            &format!("branch.{}.remote", entry.branch),
            "origin",
        ],
    )
    .unwrap();
    git(
        &f.repo,
        &[
            "config",
            &format!("branch.{}.merge", entry.branch),
            &format!("refs/heads/{}", entry.branch),
        ],
    )
    .unwrap();
    session(&f, &entry, "history", true);
    let plan = prepare(&f, &entry);
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 1", [])
        .unwrap();
    assert!(execute(&f, &entry, &plan).error.is_some());
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 0", [])
        .unwrap();
    run(&f);
    assert_eq!(item(&f).status, "complete");
    assert!(!marker.exists());
}

#[test]
fn cleared_outputs_can_retire_after_archive_without_reinstalling_dependencies() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("package.json"), "{}").unwrap();
    git(&f.repo, &["add", "package.json"]).unwrap();
    git(&f.repo, &["commit", "-m", "Manifest"]).unwrap();
    let entry = f.create("cleared-then-archived");
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    environment::save_settings(
        &f.conn,
        &scope,
        &environment::EnvironmentSettings {
            copy_paths: vec!["node_modules/local.env".into()],
            setup_command: "exit 99".into(),
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::create_dir_all(Path::new(&entry.path).join("node_modules")).unwrap();
    std::fs::write(
        Path::new(&entry.path).join("node_modules/local.env"),
        "local secret",
    )
    .unwrap();
    std::fs::write(
        Path::new(&entry.path).join("node_modules/output"),
        "generated",
    )
    .unwrap();
    session(&f, &entry, "unarchived", false);
    super::super::output_cleanup::tests::clear_for_retirement(&f, &entry);
    assert!(environment::needs_setup(&f.conn, &entry.path).unwrap());
    configure(&f, Mode::Automatic);
    f.conn
        .execute("UPDATE sessions SET archived = 1", [])
        .unwrap();
    run(&f);
    assert_eq!(item(&f).status, "complete");
    assert!(!Path::new(&entry.path).exists());
    assert!(ref_oid(&f.repo, &format!("refs/heads/{}", entry.branch))
        .unwrap()
        .is_some());
    let attempts: i64 = f
        .conn
        .query_row(
            "SELECT attempts FROM worktree_environment_setup WHERE worktree_id = ?1",
            [&entry.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 0);
    assert!(storage::usage(&f.conn).unwrap().used_bytes > 0);
    assert_eq!(
        f.conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE archived = 1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
