use super::*;
use crate::worktrees::tests::Fixture;

fn fixture(id: &str) -> (Fixture, Owned) {
    let f = Fixture::new();
    std::fs::write(f.repo.join("package.json"), "{}").unwrap();
    std::fs::write(
        f.repo.join(".gitignore"),
        ".env\nnode_modules/\ndist/\ntarget/\ncache/\nunknown/\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]).unwrap();
    git(&f.repo, &["commit", "-m", "Manifests"]).unwrap();
    let entry = f.create(id);
    (f, entry)
}
fn write(entry: &Owned, path: &str, contents: &str) {
    let path = Path::new(&entry.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}
fn plan(f: &Fixture, entry: &Owned) -> Review {
    review(&f.conn, &f.host, &HashMap::new(), &entry.repo, &entry.id).unwrap()
}
fn execute(f: &Fixture, plan: &Review, paths: &[&str]) -> Report {
    execute_with(
        &f.conn,
        &f.host,
        &plan.plan_id,
        &paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        Clone::clone,
        |_| Ok(()),
    )
    .unwrap()
}
fn configure(f: &Fixture, entry: &Owned, copies: &[&str], disposables: &[&str], command: &str) {
    let scope = environment::scope_for_entry(&f.conn, entry).unwrap();
    let previous = environment::load_settings(&f.conn, &scope).unwrap();
    environment::save_settings(
        &f.conn,
        &scope,
        &environment::EnvironmentSettings {
            environment_version: previous.environment_version,
            copy_paths: copies.iter().map(|p| p.to_string()).collect(),
            disposable_paths: disposables.iter().map(|p| p.to_string()).collect(),
            setup_command: command.into(),
        },
    )
    .unwrap();
}
fn setup(f: &Fixture, entry: &Owned) -> Result<(), String> {
    setup::coordinate_setup(&entry.path, || {
        let environment::BeginSetup::Run(operation) =
            environment::begin_setup(&f.conn, &entry.path)?
        else {
            return Ok(());
        };
        let result = environment::run_setup(&operation, |_| {});
        environment::finish_setup(&f.conn, &operation, &result)?;
        result
    })
}

#[test]
fn dirty_source_history_branches_unknown_files_and_selected_configuration_survive() {
    let (f, entry) = fixture("output-dirty");
    configure(
        &f,
        &entry,
        &["dist/.env", "cache/local.env", ".env"],
        &["cache"],
        "",
    );
    for (path, data) in [
        ("tracked.txt", "unfinished source"),
        ("new.txt", "untracked source"),
        ("unknown/local.db", "unknown ignored data"),
        (".env", "checkout secret"),
        ("dist/.env", "selected output secret"),
        ("cache/local.env", "selected custom secret"),
        ("dist/output.js", "generated"),
        ("cache/output", "generated"),
    ] {
        write(&entry, path, data);
    }
    f.conn.execute("INSERT INTO sessions(id, cwd, worktree_cwd, archived) VALUES ('conversation', ?1, ?2, 0)", params![entry.repo, entry.path]).unwrap();
    let head = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let usage = storage::usage(&f.conn).unwrap();
    let plan = plan(&f, &entry);
    assert!(plan.blocked_reason.is_none());
    assert_eq!(plan.candidates.len(), 2);
    let report = execute(&f, &plan, &["dist", "cache"]);
    assert_eq!(report.status, "complete");
    assert!(report.preparation_needed);
    for path in [
        "tracked.txt",
        "new.txt",
        "unknown/local.db",
        ".env",
        "dist/.env",
        "cache/local.env",
        ".git",
    ] {
        assert!(Path::new(&entry.path).join(path).exists(), "{path}");
    }
    assert!(!Path::new(&entry.path).join("dist/output.js").exists());
    assert!(!Path::new(&entry.path).join("cache/output").exists());
    assert_eq!(
        resolve_commit(Path::new(&entry.path), "HEAD").unwrap(),
        head
    );
    assert!(!owned(&f.conn).unwrap()[0].removed);
    assert_eq!(
        f.conn
            .query_row("SELECT archived FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        storage::usage(&f.conn).unwrap().used_bytes,
        usage.used_bytes
    );
    setup(&f, &entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join("tracked.txt")).unwrap(),
        "unfinished source"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join("dist/.env")).unwrap(),
        "selected output secret"
    );
}

#[test]
fn nested_manifests_and_tracked_candidates_are_reviewed_with_explanations() {
    let (f, entry) = fixture("output-nested");
    write(&entry, "nested/package.json", "{}");
    write(&entry, "rust/Cargo.toml", "[package]");
    write(&entry, "dist/source.js", "tracked source");
    git(
        Path::new(&entry.path),
        &[
            "add",
            "-f",
            "nested/package.json",
            "rust/Cargo.toml",
            "dist/source.js",
        ],
    )
    .unwrap();
    write(&entry, "nested/node_modules/dependency", "generated");
    write(&entry, "rust/target/output", "generated");
    write(&entry, "unknown/node_modules/local", "unrecognized");
    let plan = plan(&f, &entry);
    assert_eq!(plan.candidates.len(), 3);
    assert!(plan
        .candidates
        .iter()
        .find(|c| c.path == "dist")
        .unwrap()
        .blocked_reason
        .as_ref()
        .unwrap()
        .contains("tracked"));
    let report = execute(&f, &plan, &["nested/node_modules", "rust/target"]);
    assert_eq!(report.status, "complete");
    assert!(Path::new(&entry.path).join("dist/source.js").exists());
    assert!(Path::new(&entry.path)
        .join("unknown/node_modules/local")
        .exists());
}

#[test]
fn policy_identity_and_tracked_changes_after_review_block_removal() {
    let (f, entry) = fixture("output-revalidate");
    write(&entry, "dist/output", "generated");
    let first = plan(&f, &entry);
    configure(&f, &entry, &[], &[], "echo changed");
    assert!(execute_with(
        &f.conn,
        &f.host,
        &first.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| Ok(())
    )
    .unwrap_err()
    .contains("policy changed"));
    let second = plan(&f, &entry);
    git(Path::new(&entry.path), &["add", "-f", "dist/output"]).unwrap();
    assert!(execute(&f, &second, &["dist"]).results[0]
        .error
        .as_ref()
        .unwrap()
        .contains("tracked"));
    assert!(Path::new(&entry.path).join("dist/output").exists());
}

#[test]
fn pins_leases_agents_terminals_setup_reservations_and_git_locks_block() {
    let (f, entry) = fixture("output-protected");
    write(&entry, "dist/output", "generated");
    for label in ["window", "$running-processes", "$unregistered-windows"] {
        let windows = HashMap::from([(label.into(), vec![Path::new(&entry.path).join("nested")])]);
        assert!(review(&f.conn, &f.host, &windows, &entry.repo, &entry.id)
            .unwrap()
            .blocked_reason
            .is_some());
    }
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 1", [])
        .unwrap();
    assert!(plan(&f, &entry).blocked_reason.unwrap().contains("Pinned"));
    f.conn
        .execute("UPDATE managed_worktrees SET pinned = 0", [])
        .unwrap();
    f.conn
        .execute(
            "INSERT INTO sessions(id, cwd, pinned) VALUES ('pinned', ?1, 1)",
            [&entry.path],
        )
        .unwrap();
    assert!(plan(&f, &entry).blocked_reason.unwrap().contains("pinned"));
    f.conn.execute("DELETE FROM sessions", []).unwrap();
    let scope = environment::scope_for_entry(&f.conn, &entry).unwrap();
    let reservation = disk::coordinate(&f.host.disk, &f.conn, |measured| {
        f.host
            .disk
            .admit(&f.conn, &entry.path, &scope, "setup", true, measured)
    })
    .unwrap();
    assert!(plan(&f, &entry)
        .blocked_reason
        .unwrap()
        .contains("reserved"));
    drop(reservation);
    setup::coordinate_setup(&entry.path, || {
        assert!(plan(&f, &entry).blocked_reason.unwrap().contains("setup"));
        Ok(())
    })
    .unwrap();
    let git_dir = git(Path::new(&entry.path), &["rev-parse", "--absolute-git-dir"]).unwrap();
    std::fs::write(Path::new(&git_dir).join("index.lock"), "").unwrap();
    assert!(plan(&f, &entry).blocked_reason.unwrap().contains("lock"));
}

#[test]
fn live_use_arriving_after_review_or_during_removal_is_resampled() {
    let (f, entry) = fixture("output-live-race");
    write(&entry, "dist/output", "generated");
    let plan = plan(&f, &entry);
    f.host
        .operation_guard()
        .unwrap()
        .insert("window".into(), vec![entry.path.clone().into()]);
    assert!(execute_with(
        &f.conn,
        &f.host,
        &plan.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| Ok(())
    )
    .is_err());
    f.host.operation_guard().unwrap().clear();
    let live = std::cell::Cell::new(false);
    let report = execute_with(
        &f.conn,
        &f.host,
        &plan.plan_id,
        &["dist".into()],
        |windows| {
            let mut windows = windows.clone();
            if live.get() {
                windows.insert("$running-processes".into(), vec![entry.path.clone().into()]);
            }
            windows
        },
        |_| {
            live.set(true);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(report.status, "partial");
    assert!(Path::new(&entry.path).join("dist/output").exists());
}

#[cfg(unix)]
#[test]
fn symlink_candidate_parent_and_path_replacement_never_follow_external_data() {
    use std::os::unix::fs::symlink;
    let (f, entry) = fixture("output-symlink");
    let external = f.dir.join("external");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("valuable"), "keep").unwrap();
    write(&entry, "dist/output", "generated");
    symlink(&external, Path::new(&entry.path).join("dist/link")).unwrap();
    let review = plan(&f, &entry);
    std::fs::rename(
        Path::new(&entry.path).join("dist"),
        Path::new(&entry.path).join("old-dist"),
    )
    .unwrap();
    symlink(&external, Path::new(&entry.path).join("dist")).unwrap();
    assert!(execute(&f, &review, &["dist"]).results[0].error.is_some());
    std::fs::remove_file(Path::new(&entry.path).join("dist")).unwrap();
    std::fs::rename(
        Path::new(&entry.path).join("old-dist"),
        Path::new(&entry.path).join("dist"),
    )
    .unwrap();
    let review = plan(&f, &entry);
    let mut replaced = false;
    let report = execute_with(
        &f.conn,
        &f.host,
        &review.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| {
            if !replaced {
                std::fs::rename(
                    Path::new(&entry.path).join("dist"),
                    Path::new(&entry.path).join("old-dist"),
                )
                .unwrap();
                symlink(&external, Path::new(&entry.path).join("dist")).unwrap();
                replaced = true;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(report.status, "partial");
    assert_eq!(
        std::fs::read_to_string(external.join("valuable")).unwrap(),
        "keep"
    );
}

#[test]
fn partial_deletion_is_reported_and_disk_cache_invalidated() {
    let (f, entry) = fixture("output-partial");
    write(&entry, "dist/a", &"a".repeat(8192));
    write(&entry, "dist/b", &"b".repeat(8192));
    write(&entry, "node_modules/c", &"c".repeat(8192));
    let review = plan(&f, &entry);
    let report = execute_with(
        &f.conn,
        &f.host,
        &review.plan_id,
        &["dist".into(), "node_modules".into()],
        Clone::clone,
        |path| {
            if path.ends_with("b") {
                return Err("Injected deletion failure".into());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(report.status, "partial");
    assert!(report.estimated_removed_bytes > 0);
    assert!(report.results[0].error.is_some());
    assert!(report.results[1].error.is_none());
    assert!(Path::new(&entry.path).join("dist/b").exists());
    assert!(!Path::new(&entry.path).join("dist/a").exists());
    assert!(report.observed_free_space_change.is_some());
    assert_eq!(
        history(&f.conn, &entry.repo).unwrap()[0].estimated_removed_bytes,
        report.estimated_removed_bytes
    );
    assert!(environment::needs_setup(&f.conn, &entry.path).unwrap());
}

#[cfg(unix)]
#[test]
fn interrupted_cleanup_restart_and_failed_origin_setup_retry_preserve_source() {
    let (f, mut entry) = fixture("output-interrupted");
    write(&entry, "nested/package.json", "{}");
    write(&entry, "nested/dist/output", "generated");
    write(&entry, "tracked.txt", "unfinished");
    git(Path::new(&entry.path), &["add", "nested/package.json"]).unwrap();
    std::fs::create_dir_all(f.repo.join("nested")).unwrap();
    f.conn.execute("UPDATE worktree_environment_origins SET project_path = 'nested' WHERE worktree_id = ?1", [&entry.id]).unwrap();
    configure(
        &f,
        &entry,
        &[],
        &[],
        "test -f allow-setup && printf prepared > prepared",
    );
    entry.repo = path_to_js(&f.repo);
    let review = review(
        &f.conn,
        &f.host,
        &HashMap::new(),
        &path_to_js(&f.repo.join("nested")),
        &entry.id,
    )
    .unwrap();
    // Panic models termination after durable intent but before result persistence.
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_with(
            &f.conn,
            &f.host,
            &review.plan_id,
            &["nested/dist".into()],
            Clone::clone,
            |_| {
                assert!(environment::needs_setup(&f.conn, &entry.path).unwrap());
                panic!("simulated process interruption");
            },
        )
    }));
    assert!(interrupted.is_err());
    let reopened = Connection::open(&f.db).unwrap();
    super::super::schema(&reopened).unwrap();
    reset_interrupted(&reopened).unwrap();
    environment::reset_interrupted(&reopened).unwrap();
    assert!(environment::needs_setup(&reopened, &entry.path).unwrap());
    assert_eq!(
        history(&reopened, &path_to_js(&f.repo.join("nested"))).unwrap()[0].status,
        "interrupted"
    );
    assert!(Path::new(&entry.path).join("nested/dist/output").exists());
    assert!(setup(&f, &entry).is_err());
    assert!(environment::needs_setup(&reopened, &entry.path).unwrap());
    write(&entry, "nested/allow-setup", "retry");
    setup(&f, &entry).unwrap();
    assert!(!environment::needs_setup(&reopened, &entry.path).unwrap());
    assert!(Path::new(&entry.path).join("nested/prepared").exists());
    assert!(!Path::new(&entry.path).join("prepared").exists());
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join("tracked.txt")).unwrap(),
        "unfinished"
    );
}

#[test]
fn thousands_of_tiny_files_do_not_spawn_git_per_file_or_hold_global_lock() {
    let (f, entry) = fixture("output-bounded");
    write(&entry, "dist/small", "x");
    let small = plan(&f, &entry);
    let before = GIT_INVOCATIONS.with(|count| count.get());
    execute(&f, &small, &["dist"]);
    let small_count = GIT_INVOCATIONS.with(|count| count.get()) - before;
    for i in 0..2048 {
        write(&entry, &format!("dist/tiny-{i:04}"), "x");
    }
    let large = plan(&f, &entry);
    let before = GIT_INVOCATIONS.with(|count| count.get());
    let report = execute_with(
        &f.conn,
        &f.host,
        &large.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| {
            assert!(
                f.host.windows.try_lock().is_ok(),
                "global lock held during output traversal"
            );
            Ok(())
        },
    )
    .unwrap();
    let large_count = GIT_INVOCATIONS.with(|count| count.get()) - before;
    assert_eq!(report.status, "complete");
    assert!(
        large_count <= small_count + 2,
        "small: {small_count}, large: {large_count}"
    );
    assert!(large_count < 100, "{large_count} Git subprocesses");
}

#[test]
fn index_changes_during_walk_fail_closed_and_keep_newly_tracked_files() {
    let (f, entry) = fixture("output-index-race");
    write(&entry, "dist/source", "valuable");
    let review = plan(&f, &entry);
    let report = execute_with(
        &f.conn,
        &f.host,
        &review.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| {
            git(Path::new(&entry.path), &["add", "-f", "dist/source"])?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(report.status, "partial");
    assert!(report.results[0]
        .error
        .as_ref()
        .unwrap()
        .contains("Git index"));
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join("dist/source")).unwrap(),
        "valuable"
    );
}

#[test]
fn replacement_by_a_real_directory_is_rejected_and_reviews_are_single_use() {
    let (f, entry) = fixture("output-replaced-dir");
    write(&entry, "dist/output", "generated");
    let review = plan(&f, &entry);
    std::fs::rename(
        Path::new(&entry.path).join("dist"),
        Path::new(&entry.path).join("previous"),
    )
    .unwrap();
    write(&entry, "dist/output", "replacement data");
    let report = execute(&f, &review, &["dist"]);
    assert!(report.results[0]
        .error
        .as_ref()
        .unwrap()
        .contains("replaced"));
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join("dist/output")).unwrap(),
        "replacement data"
    );
    assert!(execute_with(
        &f.conn,
        &f.host,
        &review.plan_id,
        &["dist".into()],
        Clone::clone,
        |_| Ok(())
    )
    .is_err());
    assert!(execute_with(
        &f.conn,
        &f.host,
        &plan(&f, &entry).plan_id,
        &["../outside".into()],
        Clone::clone,
        |_| Ok(())
    )
    .is_err());
}

#[test]
fn pending_retirement_blocks_output_cleanup() {
    let (f, entry) = fixture("output-retirement");
    write(&entry, "dist/output", "generated");
    f.conn
        .execute(
            "UPDATE managed_worktrees SET pending_retirement_plan_id = 'pending'",
            [],
        )
        .unwrap();
    assert!(plan(&f, &entry)
        .blocked_reason
        .unwrap()
        .contains("retirement"));
}

pub(crate) fn clear_for_retirement(f: &Fixture, entry: &Owned) {
    let review = plan(f, entry);
    assert_eq!(execute(f, &review, &["node_modules"]).status, "complete");
}
