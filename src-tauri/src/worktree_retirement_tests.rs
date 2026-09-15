use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn wait_for_remote_helper<'scope, T: std::fmt::Debug>(
    marker: &Path,
    release: &Path,
    operation: &str,
    worker: std::thread::ScopedJoinHandle<'scope, Result<T, String>>,
) -> std::thread::ScopedJoinHandle<'scope, Result<T, String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.exists() && std::time::Instant::now() < deadline {
        if worker.is_finished() {
            std::fs::write(release, "release").unwrap();
            let result = worker.join();
            panic!("{operation} finished before its helper started: {result:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !marker.exists() {
        std::fs::write(release, "release").unwrap();
        let result = worker.join();
        panic!("{operation} helper did not start within 60 seconds: {result:?}");
    }
    worker
}

struct RetirementFixture {
    dir: PathBuf,
    repo: PathBuf,
    remote: PathBuf,
    alternate_remote: PathBuf,
    conn: Connection,
    host: WorktreeHost,
}

impl RetirementFixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "monocode-retirement-git-test-{}-{}-{}",
            std::process::id(),
            now(),
            TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        let repo = dir.join("source repo with spaces");
        let remote = dir.join("remote one.git");
        let alternate_remote = dir.join("remote two.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]).unwrap();
        git(&repo, &["config", "user.name", "Retirement Test"]).unwrap();
        git(
            &repo,
            &["config", "user.email", "retirement@example.invalid"],
        )
        .unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        git(
            &repo,
            &[
                "config",
                "core.hooksPath",
                &path_to_js(&dir.join("no client hooks")),
            ],
        )
        .unwrap();
        std::fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        git(&repo, &["add", "tracked.txt"]).unwrap();
        git(&repo, &["commit", "-m", "Initial"]).unwrap();

        for bare in [&remote, &alternate_remote] {
            std::fs::create_dir_all(bare).unwrap();
            git(bare, &["init", "--bare", "--initial-branch=main"]).unwrap();
            git(
                &repo,
                &["push", &path_to_js(bare), "refs/heads/main:refs/heads/main"],
            )
            .unwrap();
            git(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        }
        git(&repo, &["remote", "add", "origin", &path_to_js(&remote)]).unwrap();

        let conn = Connection::open(dir.join("worktrees.sqlite")).unwrap();
        schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                worktree_cwd TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                branch TEXT
             )",
        )
        .unwrap();
        let host = WorktreeHost {
            root: dir.join("owned checkouts"),
            windows: Mutex::new(HashMap::new()),
            repositories: RepositoryReservations::default(),
        };
        Self {
            dir,
            repo,
            remote,
            alternate_remote,
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
            "Remote retirement",
            Some("main"),
        )
        .unwrap();
        match environment::begin_setup(&self.conn, &entry.path).unwrap() {
            environment::BeginSetup::Run(operation) => {
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(&self.conn, &operation, &result).unwrap();
                result.unwrap();
            }
            environment::BeginSetup::Skip => {}
        }
        entry
    }

    fn create_remote_branch(&self, id: &str) -> Owned {
        let entry = self.create(id);
        git(
            Path::new(&entry.path),
            &["push", "--set-upstream", "origin", &entry.branch],
        )
        .unwrap();
        entry
    }

    fn plan_ids(&self, ids: &[String]) -> WorktreeRetirementPlan {
        build_retirement_plan(
            &self.conn,
            &HashMap::new(),
            &[],
            Some(&path_to_js(&self.repo)),
            ids,
        )
        .unwrap()
    }

    fn advance_main(&self, name: &str) -> String {
        std::fs::write(self.repo.join(name), format!("{name}\n")).unwrap();
        git(&self.repo, &["add", name]).unwrap();
        git(&self.repo, &["commit", "-m", name]).unwrap();
        resolve_commit(&self.repo, "HEAD").unwrap()
    }

    fn remote_tip(&self, remote: &Path, branch: &str) -> Option<String> {
        ref_oid(remote, &format!("refs/heads/{branch}")).unwrap()
    }
}

impl Drop for RetirementFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn selection(
    entry: &Owned,
    delete_local_branch: bool,
    delete_remote_branch: bool,
) -> WorktreeRetirementSelection {
    WorktreeRetirementSelection {
        id: entry.id.clone(),
        delete_local_branch,
        delete_remote_branch,
    }
}

#[cfg(unix)]
#[test]
fn delayed_remote_review_releases_global_lifecycle_lock() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-slow-review");
    let marker = fixture.dir.join("remote-started");
    let release = fixture.dir.join("remote-release");
    let remote_link = fixture.dir.join("remote.git");
    std::os::unix::fs::symlink(&fixture.remote, &remote_link).unwrap();
    let helper = fixture.dir.join("delayed-remote");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf ready > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nexec \"$GIT_EXT_SERVICE\" \"$1\"\n",
            marker.display(),
            release.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).unwrap();
    git(&fixture.repo, &["config", "protocol.ext.allow", "always"]).unwrap();
    let remote_url = format!("ext::{} {}", path_to_js(&helper), path_to_js(&remote_link));
    git(
        &fixture.repo,
        &["remote", "set-url", "--push", "origin", &remote_url],
    )
    .unwrap();

    let common = entry.common.clone();
    let ids = vec![entry.id.clone()];
    let cwd = path_to_js(&fixture.repo);
    let db = fixture.dir.join("worktrees.sqlite");
    std::thread::scope(|scope| {
        let host = &fixture.host;
        let review = scope.spawn(move || {
            let conn = Connection::open(db).unwrap();
            build_retirement_plan_coordinated(&conn, host, &HashMap::new(), &[], Some(&cwd), &ids)
        });
        let review = wait_for_remote_helper(&marker, &release, "remote review", review);

        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        scope.spawn(move || {
            let _guard = host.operation_guard().unwrap();
            lifecycle_tx.send(()).unwrap();
        });
        lifecycle_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("remote review held the global lifecycle lock");

        let unrelated = fixture
            .host
            .repository_guard("unrelated-repository")
            .unwrap();
        drop(unrelated);
        let (same_tx, same_rx) = mpsc::channel();
        scope.spawn(move || {
            let _guard = host.repository_guard(&common).unwrap();
            same_tx.send(()).unwrap();
        });
        assert!(same_rx.recv_timeout(Duration::from_millis(50)).is_err());
        std::fs::write(&release, "release").unwrap();
        same_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        review.join().unwrap().unwrap();
    });
}

#[cfg(unix)]
#[test]
fn delayed_remote_delete_releases_global_lifecycle_lock() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-slow-delete");
    let marker = fixture.dir.join("delete-started");
    let release = fixture.dir.join("delete-release");
    let remote_link = fixture.dir.join("delete-remote.git");
    std::os::unix::fs::symlink(&fixture.remote, &remote_link).unwrap();
    let helper = fixture.dir.join("delayed-delete-remote");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nif [ \"$GIT_EXT_SERVICE\" = git-receive-pack ]; then printf ready > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; fi\nexec \"$GIT_EXT_SERVICE\" \"$1\"\n",
            marker.display(),
            release.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).unwrap();
    git(&fixture.repo, &["config", "protocol.ext.allow", "always"]).unwrap();
    let remote_url = format!("ext::{} {}", path_to_js(&helper), path_to_js(&remote_link));
    git(
        &fixture.repo,
        &["remote", "set-url", "--push", "origin", &remote_url],
    )
    .unwrap();
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(plan.entries[0].remote_branch.as_ref().unwrap().allowed);

    let common = entry.common.clone();
    let entry_id = entry.id.clone();
    let plan_id = plan.plan_id.clone();
    let db = fixture.dir.join("worktrees.sqlite");
    std::thread::scope(|scope| {
        let host = &fixture.host;
        let retirement = scope.spawn(move || {
            let conn = Connection::open(db).unwrap();
            execute_retirement_coordinated_with(
                &conn,
                host,
                &plan_id,
                &[WorktreeRetirementSelection {
                    id: entry_id,
                    delete_local_branch: true,
                    delete_remote_branch: true,
                }],
                |windows| windows.clone(),
            )
        });
        let retirement = wait_for_remote_helper(&marker, &release, "remote delete", retirement);

        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        scope.spawn(move || {
            let _guard = host.operation_guard().unwrap();
            lifecycle_tx.send(()).unwrap();
        });
        lifecycle_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("remote delete held the global lifecycle lock");
        let (same_tx, same_rx) = mpsc::channel();
        scope.spawn(move || {
            let _guard = host.repository_guard(&common).unwrap();
            same_tx.send(()).unwrap();
        });
        assert!(same_rx.recv_timeout(Duration::from_millis(50)).is_err());
        std::fs::write(&release, "release").unwrap();
        same_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let report = retirement.join().unwrap().unwrap();
        assert_eq!(report.results[0].error, None);
        assert!(report.results[0].worktree_removed);
        assert!(report.results[0].local_branch_deleted);
        assert!(report.results[0].remote_branch_deleted);
    });
    assert_eq!(fixture.remote_tip(&fixture.remote, &entry.branch), None);
}

#[test]
fn coordinated_partial_failure_retains_all_requested_branch_steps() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-partial-intent");
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(plan.entries[0].local_branch.allowed);
    assert!(plan.entries[0].remote_branch.as_ref().unwrap().allowed);

    fixture.advance_main("base-moved-after-review.txt");
    let report = execute_retirement_coordinated_with(
        &fixture.conn,
        &fixture.host,
        &plan.plan_id,
        &[selection(&entry, true, true)],
        |windows| windows.clone(),
    )
    .unwrap();

    assert!(report.results[0].worktree_removed);
    assert!(report.results[0].error.is_some());
    assert!(!report.results[0].local_branch_deleted);
    assert!(!report.results[0].remote_branch_deleted);
    let snapshot = load_retirement_snapshot(&fixture.conn, &plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();
    assert!(snapshot.local_requested);
    assert!(snapshot.remote_requested);
    assert!(!snapshot.local_deleted);
    assert!(!snapshot.remote_deleted);
    assert!(fixture.remote_tip(&fixture.remote, &entry.branch).is_some());
}

#[test]
fn stale_plan_cannot_replace_the_recovery_selected_by_a_completed_retirement() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-stale-recovery");
    let first_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let stale_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));

    std::fs::write(Path::new(&entry.path).join("tracked.txt"), "second\n").unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "Second"]).unwrap();
    let second_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    assert_ne!(second_tip, first_tip);
    let current_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let retired = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &current_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(retired.results[0].error, None);
    assert!(retired.results[0].worktree_removed);

    // Make the stale insert deterministically newer than the successful one.
    std::thread::sleep(Duration::from_millis(2));
    let rejected = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &stale_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert!(rejected.results[0].error.is_some());
    let stale_recovery_rows: i64 = fixture
        .conn
        .query_row(
            "SELECT COUNT(*) FROM worktree_recoveries
              WHERE plan_id = ?1 AND worktree_id = ?2 AND kind = 'local'",
            params![stale_plan.plan_id, entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_recovery_rows, 0);

    let active_plan: String = fixture
        .conn
        .query_row(
            "SELECT active_retirement_plan_id FROM managed_worktrees WHERE id = ?1",
            [&entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_plan, current_plan.plan_id);

    // Recreate the shape left by the older implementation: a newer recovery
    // row from a failed stale plan alongside an older completed retirement.
    let stale_snapshot = load_retirement_snapshot(&fixture.conn, &stale_plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();
    ensure_recovery_record(
        &fixture.conn,
        &stale_snapshot,
        "local",
        &stale_snapshot.branch,
        &stale_snapshot.commit_oid,
        &stale_snapshot.recovery_ref,
    )
    .unwrap();
    fixture
        .conn
        .execute(
            "UPDATE worktree_retirement_items SET worktree_removed = 1, updated_at = ?1
              WHERE plan_id = ?2 AND worktree_id = ?3",
            params![now() + 10, stale_plan.plan_id, entry.id],
        )
        .unwrap();
    fixture
        .conn
        .execute(
            "UPDATE managed_worktrees SET active_retirement_plan_id = NULL WHERE id = ?1",
            [&entry.id],
        )
        .unwrap();
    schema(&fixture.conn).unwrap();

    let selected = latest_local_recovery(&fixture.conn, &entry.id)
        .unwrap()
        .unwrap();
    assert_eq!(selected.commit_oid, second_tip);
    open_owned(&fixture.conn, &entry).unwrap();
    assert_eq!(
        resolve_commit(Path::new(&entry.path), "HEAD").unwrap(),
        second_tip
    );
    let restored_archive: Option<String> = fixture
        .conn
        .query_row(
            "SELECT archive_id FROM worktree_environment_setup WHERE worktree_id = ?1",
            [&entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        restored_archive.as_deref(),
        Some(format!("{}:{}", current_plan.plan_id, entry.id).as_str())
    );
}

#[test]
fn archived_failed_plan_cannot_replace_a_later_completed_retirement() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-archived-stale-recovery");
    let stale_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let stale_snapshot = load_retirement_snapshot(&fixture.conn, &stale_plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();

    ensure_local_recovery(&fixture.conn, &stale_snapshot).unwrap();
    environment::preserve(&fixture.conn, &entry, &stale_plan.plan_id).unwrap();
    journal_step(
        &fixture.conn,
        &stale_snapshot,
        "worktree",
        "failed",
        Some("simulated Git removal failure"),
    )
    .unwrap();

    std::fs::write(Path::new(&entry.path).join("tracked.txt"), "second\n").unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "Second"]).unwrap();
    let second_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let current_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let retired = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &current_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(retired.results[0].error, None);

    let rejected = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &stale_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert!(rejected.results[0].error.is_some());
    let active: String = fixture
        .conn
        .query_row(
            "SELECT active_retirement_plan_id FROM managed_worktrees WHERE id = ?1",
            [&entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, current_plan.plan_id);

    // Migrate the state produced by the old failure path, which could mark the
    // earlier archived plan removed after the later plan had already won.
    fixture
        .conn
        .execute(
            "UPDATE worktree_retirement_items
                SET worktree_removed = 1, updated_at = ?1
              WHERE plan_id = ?2 AND worktree_id = ?3",
            params![now() + 10, stale_plan.plan_id, entry.id],
        )
        .unwrap();
    fixture
        .conn
        .execute(
            "UPDATE managed_worktrees SET active_retirement_plan_id = NULL WHERE id = ?1",
            [&entry.id],
        )
        .unwrap();
    schema(&fixture.conn).unwrap();
    let migrated_stale_retry = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &stale_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert!(migrated_stale_retry.results[0]
        .error
        .as_deref()
        .unwrap()
        .contains("different retirement"));
    assert_eq!(
        latest_local_recovery(&fixture.conn, &entry.id)
            .unwrap()
            .unwrap()
            .commit_oid,
        second_tip
    );
}

#[test]
fn opening_reconciles_a_crash_after_git_removed_the_checkout() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-removal-crash");
    std::fs::write(Path::new(&entry.path).join("tracked.txt"), "preserve me\n").unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "Preserve me"]).unwrap();
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let snapshot = load_retirement_snapshot(&fixture.conn, &plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();

    recheck_stable_safety(&fixture.conn, &HashMap::new(), &snapshot).unwrap();
    assert!(!recheck_checkout(&fixture.conn, &snapshot).unwrap());
    ensure_local_recovery(&fixture.conn, &snapshot).unwrap();
    environment::preserve(&fixture.conn, &entry, &plan.plan_id).unwrap();
    begin_worktree_removal(&fixture.conn, &snapshot).unwrap();
    git(&fixture.repo, &["worktree", "remove", "--", &entry.path]).unwrap();
    assert!(!Path::new(&entry.path).exists());

    // Simulate process death before managed_worktrees and the retirement item
    // are marked complete. Opening must reconcile the pending removal first.
    open_owned(&fixture.conn, &entry).unwrap();
    assert_eq!(resolve_commit(Path::new(&entry.path), "HEAD").unwrap(), tip);
    let completed: bool = fixture
        .conn
        .query_row(
            "SELECT worktree_removed FROM worktree_retirement_items
              WHERE plan_id = ?1 AND worktree_id = ?2",
            params![plan.plan_id, entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(completed);
    let status: String = fixture
        .conn
        .query_row(
            "SELECT status FROM worktree_retirement_journal
              WHERE plan_id = ?1 AND worktree_id = ?2 AND step = 'worktree'",
            params![plan.plan_id, entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "complete");
}

#[test]
fn opening_promotes_a_second_interrupted_retirement_over_the_prior_active_recovery() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-second-removal-crash");
    let first_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let first = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &first_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(first.results[0].error, None);
    open_owned(&fixture.conn, &entry).unwrap();
    if let environment::BeginSetup::Run(operation) =
        environment::begin_setup(&fixture.conn, &entry.path).unwrap()
    {
        let result = environment::run_setup(&operation, |_| {});
        environment::finish_setup(&fixture.conn, &operation, &result).unwrap();
        result.unwrap();
    }

    std::fs::write(Path::new(&entry.path).join("tracked.txt"), "second cycle\n").unwrap();
    git(Path::new(&entry.path), &["commit", "-am", "Second cycle"]).unwrap();
    let second_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let current_entry = owned(&fixture.conn)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id == entry.id)
        .unwrap();
    assert_eq!(
        current_entry.active_retirement_plan_id.as_deref(),
        Some(first_plan.plan_id.as_str())
    );
    let second_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let snapshot = load_retirement_snapshot(&fixture.conn, &second_plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();
    recheck_stable_safety(&fixture.conn, &HashMap::new(), &snapshot).unwrap();
    assert!(!recheck_checkout(&fixture.conn, &snapshot).unwrap());
    ensure_local_recovery(&fixture.conn, &snapshot).unwrap();
    environment::preserve(&fixture.conn, &current_entry, &second_plan.plan_id).unwrap();
    begin_worktree_removal(&fixture.conn, &snapshot).unwrap();
    git(&fixture.repo, &["worktree", "remove", "--", &entry.path]).unwrap();

    fixture
        .conn
        .execute(
            "INSERT INTO sessions (id, cwd, worktree_cwd, archived, pinned)
             VALUES (?1, ?2, ?2, 0, 0)",
            params![entry.id, entry.path],
        )
        .unwrap();
    let stale_retry = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &first_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert!(stale_retry.results[0].error.is_some());
    fixture
        .conn
        .execute("DELETE FROM sessions WHERE id = ?1", [&entry.id])
        .unwrap();
    let (still_active, still_pending): (Option<String>, Option<String>) = fixture
        .conn
        .query_row(
            "SELECT active_retirement_plan_id, pending_retirement_plan_id
               FROM managed_worktrees WHERE id = ?1",
            [&entry.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(still_active.as_deref(), Some(first_plan.plan_id.as_str()));
    assert_eq!(still_pending.as_deref(), Some(second_plan.plan_id.as_str()));

    open_owned(&fixture.conn, &current_entry).unwrap();
    assert_eq!(
        resolve_commit(Path::new(&entry.path), "HEAD").unwrap(),
        second_tip
    );
    let active: String = fixture
        .conn
        .query_row(
            "SELECT active_retirement_plan_id FROM managed_worktrees WHERE id = ?1",
            [&entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, second_plan.plan_id);
}

#[test]
fn interrupted_fresh_creation_resumes_only_when_its_branch_and_path_are_still_unused() {
    let fixture = RetirementFixture::new();
    let id = "session-interrupted-create";
    let nested_project = fixture.repo.join("apps/web");
    std::fs::create_dir_all(&nested_project).unwrap();
    std::fs::write(nested_project.join("package.json"), "{}\n").unwrap();
    git(&fixture.repo, &["add", "apps/web/package.json"]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Add nested project"]).unwrap();
    let (_, common) = repository(&path_to_js(&fixture.repo)).unwrap();
    let base_oid = resolve_commit(&fixture.repo, "main").unwrap();
    let recovery_ref = ensure_creation_ref(&fixture.repo, id, &base_oid).unwrap();
    std::fs::create_dir_all(&fixture.host.root).unwrap();
    let path = path_to_js(
        &std::fs::canonicalize(&fixture.host.root)
            .unwrap()
            .join("interrupted-create"),
    );
    let entry = Owned {
        id: id.into(),
        repo: path_to_js(&fixture.repo),
        common: common.clone(),
        path: path.clone(),
        branch: format!("monocode/interrupted-{id}"),
        base_ref: "main".into(),
        pinned: false,
        last_used: now(),
        removed: false,
        creation_oid: Some(base_oid.clone()),
        active_retirement_plan_id: None,
        pending_retirement_plan_id: None,
    };
    fixture
        .conn
        .execute(
            "INSERT INTO managed_worktrees
               (id, repo, common_dir, path, branch, base_ref, last_used, creation_oid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.id,
                entry.repo,
                entry.common,
                entry.path,
                entry.branch,
                entry.base_ref,
                entry.last_used,
                entry.creation_oid
            ],
        )
        .unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&nested_project)).unwrap();
    environment::mark_pending(
        &fixture.conn,
        &entry,
        environment::SetupOrigin::Fresh,
        Some(&scope),
        None,
    )
    .unwrap();

    std::fs::write(fixture.repo.join("after-intent.txt"), "new base\n").unwrap();
    git(&fixture.repo, &["add", "after-intent.txt"]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Move base after intent"]).unwrap();

    open_owned(&fixture.conn, &entry).unwrap();
    assert_eq!(
        direct_ref_oid(&fixture.repo, &recovery_ref)
            .unwrap()
            .as_deref(),
        Some(base_oid.as_str())
    );
    assert_eq!(resolve_commit(Path::new(&path), "HEAD").unwrap(), base_oid);
    assert_eq!(
        git(Path::new(&path), &["branch", "--show-current"]).unwrap(),
        entry.branch
    );
    assert_eq!(
        environment::scope_for_entry(&fixture.conn, &entry)
            .unwrap()
            .relative,
        "apps/web"
    );
}

#[test]
fn interrupted_fresh_creation_does_not_claim_a_branch_created_after_its_intent() {
    let fixture = RetirementFixture::new();
    let id = "session-interrupted-occupied";
    let (_, common) = repository(&path_to_js(&fixture.repo)).unwrap();
    let base_oid = resolve_commit(&fixture.repo, "main").unwrap();
    ensure_creation_ref(&fixture.repo, id, &base_oid).unwrap();
    std::fs::create_dir_all(&fixture.host.root).unwrap();
    let path = path_to_js(
        &std::fs::canonicalize(&fixture.host.root)
            .unwrap()
            .join("interrupted-occupied"),
    );
    let entry = Owned {
        id: id.into(),
        repo: path_to_js(&fixture.repo),
        common,
        path: path.clone(),
        branch: format!("monocode/interrupted-{id}"),
        base_ref: "main".into(),
        pinned: false,
        last_used: now(),
        removed: false,
        creation_oid: Some(base_oid),
        active_retirement_plan_id: None,
        pending_retirement_plan_id: None,
    };
    fixture
        .conn
        .execute(
            "INSERT INTO managed_worktrees
               (id, repo, common_dir, path, branch, base_ref, last_used, creation_oid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.id,
                entry.repo,
                entry.common,
                entry.path,
                entry.branch,
                entry.base_ref,
                entry.last_used,
                entry.creation_oid
            ],
        )
        .unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&fixture.repo)).unwrap();
    environment::mark_pending(
        &fixture.conn,
        &entry,
        environment::SetupOrigin::Fresh,
        Some(&scope),
        None,
    )
    .unwrap();
    git(&fixture.repo, &["branch", &entry.branch, "main"]).unwrap();

    let error = open_owned(&fixture.conn, &entry).unwrap_err();
    assert!(
        error.contains("branch") || error.contains("creation"),
        "{error}"
    );
    assert!(!Path::new(&path).exists());
}

#[test]
fn local_and_remote_retirement_keeps_a_real_recovery_ref_and_restores_the_exact_tip() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-one");
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(plan.entries.len(), 1);
    let tracking_ref = format!("refs/remotes/origin/{}", entry.branch);
    assert_eq!(
        ref_oid(&fixture.repo, &tracking_ref).unwrap().as_deref(),
        Some(tip.as_str())
    );
    let reviewed_remote = plan.entries[0].remote_branch.as_ref().unwrap();
    assert_eq!(reviewed_remote.name, entry.branch);
    assert_eq!(reviewed_remote.remote, "origin");
    assert_eq!(reviewed_remote.destination, path_to_js(&fixture.remote));
    assert!(reviewed_remote.allowed);

    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, true)],
    )
    .unwrap();
    let result = &report.results[0];
    assert_eq!(result.error, None);
    assert!(result.worktree_removed);
    assert!(result.local_branch_deleted);
    assert!(result.remote_branch_deleted);
    assert!(!Path::new(&entry.path).exists());
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch)).unwrap(),
        None
    );
    assert_eq!(fixture.remote_tip(&fixture.remote, &entry.branch), None);
    assert_eq!(ref_oid(&fixture.repo, &tracking_ref).unwrap(), None);
    let recovery_ref = result.recovery_ref.as_ref().unwrap();
    assert_eq!(
        ref_oid(&fixture.repo, recovery_ref).unwrap().as_deref(),
        Some(tip.as_str())
    );

    open_owned(&fixture.conn, &entry).unwrap();
    assert_eq!(resolve_commit(Path::new(&entry.path), "HEAD").unwrap(), tip);
    assert_eq!(
        git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
        entry.branch
    );
}

#[test]
fn unresolved_remote_head_keeps_branches_but_retires_the_checkout() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-one");
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    git(
        &fixture.remote,
        &["symbolic-ref", "HEAD", "refs/heads/missing-default"],
    )
    .unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(plan.entries.len(), 1);
    let reviewed_remote = plan.entries[0].remote_branch.as_ref().unwrap();
    assert!(!reviewed_remote.allowed);
    assert!(reviewed_remote
        .reason
        .as_deref()
        .unwrap()
        .contains("default branch could not be determined"));

    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, true)],
    )
    .unwrap();
    let result = &report.results[0];
    assert!(result
        .error
        .as_deref()
        .unwrap()
        .contains("default branch could not be determined"));
    assert!(result.worktree_removed);
    assert!(!result.local_branch_deleted);
    assert!(!result.remote_branch_deleted);
    assert!(!Path::new(&entry.path).exists());
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        fixture
            .remote_tip(&fixture.remote, &entry.branch)
            .as_deref(),
        Some(tip.as_str())
    );
}

#[test]
fn remote_move_after_review_keeps_remote_work_after_retiring_the_checkout() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-one");
    let reviewed_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let moved_tip = fixture.advance_main("later-main.txt");
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    git(
        &fixture.repo,
        &[
            "push",
            &path_to_js(&fixture.remote),
            &format!("{moved_tip}:refs/heads/{}", entry.branch),
        ],
    )
    .unwrap();

    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, true)],
    )
    .unwrap();
    let result = &report.results[0];
    assert!(result
        .error
        .as_deref()
        .unwrap()
        .contains("changed after review"));
    assert!(result.worktree_removed);
    assert!(result.local_branch_deleted);
    assert!(!result.remote_branch_deleted);
    assert!(!Path::new(&entry.path).exists());
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        None
    );
    assert_eq!(
        fixture
            .remote_tip(&fixture.remote, &entry.branch)
            .as_deref(),
        Some(moved_tip.as_str())
    );
    assert_eq!(
        result.recovery_ref.as_deref().and_then(|reference| {
            ref_oid(&fixture.repo, reference)
                .unwrap()
                .as_deref()
                .map(str::to_owned)
        }),
        Some(reviewed_tip)
    );
}

#[test]
fn changing_the_push_url_after_review_preserves_both_remote_destinations() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-one");
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    git(
        &fixture.repo,
        &[
            "push",
            &path_to_js(&fixture.alternate_remote),
            &format!("{tip}:refs/heads/{}", entry.branch),
        ],
    )
    .unwrap();
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            &path_to_js(&fixture.alternate_remote),
        ],
    )
    .unwrap();

    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, true)],
    )
    .unwrap();
    let result = &report.results[0];
    assert!(result
        .error
        .as_deref()
        .unwrap()
        .contains("destination changed"));
    assert!(result.worktree_removed);
    assert!(!result.local_branch_deleted);
    assert!(!result.remote_branch_deleted);
    assert!(!Path::new(&entry.path).exists());
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        fixture
            .remote_tip(&fixture.remote, &entry.branch)
            .as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        fixture
            .remote_tip(&fixture.alternate_remote, &entry.branch)
            .as_deref(),
        Some(tip.as_str())
    );
}

#[test]
fn fresh_review_can_delete_branches_after_checkout_only_retirement() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-fresh-review");
    let first_plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let first = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &first_plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(first.results[0].error, None);
    assert!(first.results[0].worktree_removed);

    let review = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(review.entries.len(), 1);
    assert!(review.entries[0].worktree_removed);
    let completed = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &review.plan_id,
        &[selection(&entry, true, true)],
    )
    .unwrap();
    assert_eq!(completed.results[0].error, None);
    assert!(completed.results[0].local_branch_deleted);
    assert!(completed.results[0].remote_branch_deleted);
    assert_eq!(
        latest_local_recovery(&fixture.conn, &entry.id)
            .unwrap()
            .unwrap()
            .plan_id,
        first_plan.plan_id
    );
}

#[test]
fn branch_only_review_cannot_cross_a_restore_and_second_retirement_at_the_same_commit() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-branch-review-stale");
    let first = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let retired = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &first.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(retired.results[0].error, None);
    let stale = fixture.plan_ids(std::slice::from_ref(&entry.id));

    open_owned(&fixture.conn, &entry).unwrap();
    if let environment::BeginSetup::Run(operation) =
        environment::begin_setup(&fixture.conn, &entry.path).unwrap()
    {
        let result = environment::run_setup(&operation, |_| {});
        environment::finish_setup(&fixture.conn, &operation, &result).unwrap();
        result.unwrap();
    }
    let second = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let retired = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &second.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(retired.results[0].error, None);

    let rejected = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &stale.plan_id,
        &[selection(&entry, true, false)],
    )
    .unwrap();
    assert!(rejected.results[0]
        .error
        .as_deref()
        .unwrap()
        .contains("different retirement"));
    assert!(!rejected.results[0].local_branch_deleted);
    assert_eq!(
        latest_local_recovery(&fixture.conn, &entry.id)
            .unwrap()
            .unwrap()
            .plan_id,
        second.plan_id
    );
}

#[test]
fn last_archived_session_can_retire_recognized_build_folders_without_configuration() {
    let fixture = RetirementFixture::new();
    std::fs::write(
        fixture.repo.join(".gitignore"),
        "node_modules/\ndist/\ntarget/\nsrc-tauri/gen/\n",
    )
    .unwrap();
    std::fs::write(fixture.repo.join("package.json"), "{}\n").unwrap();
    std::fs::write(fixture.repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::create_dir(fixture.repo.join("src-tauri")).unwrap();
    std::fs::write(fixture.repo.join("src-tauri/Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(fixture.repo.join("src-tauri/tauri.conf.json"), "{}\n").unwrap();
    git(&fixture.repo, &["add", "."]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Add project manifests"]).unwrap();
    let entry = fixture.create("last-session-build-folders");
    for dir in ["node_modules", "dist", "target", "src-tauri/gen/schemas"] {
        let root = Path::new(&entry.path).join(dir);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("generated"), "build output").unwrap();
    }
    fixture
        .conn
        .execute(
            "INSERT INTO sessions (id, cwd, worktree_cwd, archived) VALUES (?1, ?2, ?3, 1)",
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
    assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);
    assert!(plan.kept.is_empty());
    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert!(!Path::new(&entry.path).exists());
    assert!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .is_some()
    );
}

#[test]
fn rejected_remote_delete_is_retryable_from_the_same_plan() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-one");
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let hook = fixture.remote.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'test server rejected deletion' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
    }

    let first = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, true)],
    )
    .unwrap();
    let failed = &first.results[0];
    assert!(failed.error.as_deref().unwrap().contains("rejected"));
    assert!(failed.worktree_removed);
    assert!(failed.local_branch_deleted);
    assert!(!failed.remote_branch_deleted);
    assert_eq!(
        fixture
            .remote_tip(&fixture.remote, &entry.branch)
            .as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch)).unwrap(),
        None
    );

    std::fs::remove_file(&hook).unwrap();
    let retry = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, true)],
    )
    .unwrap();
    let completed = &retry.results[0];
    assert_eq!(completed.error, None);
    assert!(completed.worktree_removed);
    assert!(completed.local_branch_deleted);
    assert!(completed.remote_branch_deleted);
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch)).unwrap(),
        None
    );
    assert_eq!(fixture.remote_tip(&fixture.remote, &entry.branch), None);
}

#[test]
fn duplicate_bulk_ids_and_selections_execute_each_worktree_once() {
    let fixture = RetirementFixture::new();
    let first = fixture.create_remote_branch("session-one");
    let second = fixture.create_remote_branch("session-two");
    let ids = vec![
        first.id.clone(),
        first.id.clone(),
        second.id.clone(),
        second.id.clone(),
    ];
    let plan = fixture.plan_ids(&ids);
    assert_eq!(plan.entries.len(), 2);
    assert_eq!(
        plan.entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from([first.id.as_str(), second.id.as_str()])
    );

    let selections = vec![
        selection(&first, true, false),
        selection(&first, false, true),
        selection(&second, false, true),
        selection(&second, true, false),
    ];
    let report =
        execute_retirement(&fixture.conn, &HashMap::new(), &plan.plan_id, &selections).unwrap();
    assert_eq!(report.results.len(), 2);
    assert!(report.results.iter().all(|result| {
        result.error.is_none()
            && result.worktree_removed
            && result.local_branch_deleted
            && result.remote_branch_deleted
    }));
    assert_eq!(fixture.remote_tip(&fixture.remote, &first.branch), None);
    assert_eq!(fixture.remote_tip(&fixture.remote, &second.branch), None);
}

#[test]
fn an_active_conversation_sharing_the_checkout_suppresses_an_empty_review() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("managed-owner");
    fixture
        .conn
        .execute(
            "INSERT INTO sessions (id, cwd, worktree_cwd, archived, pinned, branch)
             VALUES (?1, ?2, ?3, 1, 0, ?4)",
            params![
                "archived-conversation",
                entry.repo,
                entry.path,
                entry.branch
            ],
        )
        .unwrap();
    fixture
        .conn
        .execute(
            "INSERT INTO sessions (id, cwd, worktree_cwd, archived, pinned, branch)
             VALUES (?1, ?2, ?3, 0, 0, ?4)",
            params!["active-conversation", entry.repo, entry.path, entry.branch],
        )
        .unwrap();
    let session_ids = vec![
        "archived-conversation".to_string(),
        "archived-conversation".to_string(),
    ];

    let plan =
        build_retirement_plan(&fixture.conn, &HashMap::new(), &session_ids, None, &[]).unwrap();
    assert!(plan.entries.is_empty());
    assert!(plan.kept.is_empty());
    assert!(Path::new(&entry.path).exists());
    assert!(fixture.remote_tip(&fixture.remote, &entry.branch).is_some());
}

#[test]
fn a_local_branch_moved_after_review_is_preserved_and_recovery_uses_the_old_tip() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-one");
    let reviewed_tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let moved_tip = fixture.advance_main("later-main.txt");
    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    let removed = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(removed.results[0].error, None);
    assert!(removed.results[0].worktree_removed);
    git(
        &fixture.repo,
        &[
            "update-ref",
            &format!("refs/heads/{}", entry.branch),
            &moved_tip,
            &reviewed_tip,
        ],
    )
    .unwrap();

    let attempted = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, false)],
    )
    .unwrap();
    let preserved = &attempted.results[0];
    assert!(preserved
        .error
        .as_deref()
        .unwrap()
        .contains("changed after review"));
    assert!(preserved.worktree_removed);
    assert!(!preserved.local_branch_deleted);
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(moved_tip.as_str())
    );

    open_owned(&fixture.conn, &entry).unwrap();
    let restored = owned(&fixture.conn)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id == entry.id)
        .unwrap();
    assert_ne!(restored.branch, entry.branch);
    assert!(restored.branch.starts_with("monocode/recovered-"));
    assert_eq!(
        resolve_commit(Path::new(&restored.path), "HEAD").unwrap(),
        reviewed_tip
    );
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(moved_tip.as_str())
    );
}

#[test]
fn stale_local_base_uses_its_exact_upstream_and_accepts_a_squash_merge() {
    let fixture = RetirementFixture::new();
    git(&fixture.repo, &["config", "branch.main.remote", "origin"]).unwrap();
    git(
        &fixture.repo,
        &["config", "branch.main.merge", "refs/heads/main"],
    )
    .unwrap();
    let entry = fixture.create_remote_branch("session-squash");
    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join("squashed.txt"), "reviewed content\n").unwrap();
    git(worktree, &["add", "squashed.txt"]).unwrap();
    git(worktree, &["commit", "-m", "Feature to squash"]).unwrap();
    git(worktree, &["push", "origin", &entry.branch]).unwrap();
    let feature_tip = resolve_commit(worktree, "HEAD").unwrap();
    let stale_main = resolve_commit(&fixture.repo, "main").unwrap();

    git(&fixture.repo, &["merge", "--squash", &entry.branch]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Squashed feature"]).unwrap();
    let remote_target = resolve_commit(&fixture.repo, "HEAD").unwrap();
    git(&fixture.repo, &["push", "origin", "main"]).unwrap();
    git(&fixture.repo, &["reset", "--hard", &stale_main]).unwrap();
    git(
        &fixture.repo,
        &["update-ref", "refs/remotes/origin/main", &stale_main],
    )
    .unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(resolve_commit(&fixture.repo, "main").unwrap(), stale_main);
    assert_eq!(
        resolve_commit(&fixture.repo, "refs/remotes/origin/main").unwrap(),
        stale_main
    );
    let snapshot = load_retirement_snapshot(&fixture.conn, &plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.base_oid, remote_target);
    assert!(plan.entries[0].local_branch.allowed);
    assert!(plan.entries[0].remote_branch.as_ref().unwrap().allowed);

    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, true)],
    )
    .unwrap();
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert!(report.results[0].local_branch_deleted);
    assert!(report.results[0].remote_branch_deleted);
    assert_eq!(
        direct_ref_oid(
            &fixture.repo,
            report.results[0].recovery_ref.as_deref().unwrap()
        )
        .unwrap()
        .as_deref(),
        Some(feature_tip.as_str())
    );
}

#[test]
fn rebased_patch_series_is_accepted_when_target_content_is_unchanged() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-rebase");
    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join("feature.txt"), "one\n").unwrap();
    git(worktree, &["add", "feature.txt"]).unwrap();
    git(worktree, &["commit", "-m", "First feature commit"]).unwrap();
    std::fs::write(worktree.join("feature.txt"), "one\ntwo\n").unwrap();
    git(worktree, &["commit", "-am", "Second feature commit"]).unwrap();
    let original_commits = git(worktree, &["rev-list", "--reverse", "main..HEAD"]).unwrap();
    fixture.advance_main("unrelated-main.txt");
    for commit in original_commits.lines() {
        git(&fixture.repo, &["cherry-pick", commit]).unwrap();
    }

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(plan.entries[0].local_branch.allowed);
    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, true, false)],
    )
    .unwrap();
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert!(report.results[0].local_branch_deleted);
}

#[test]
fn squash_merge_does_not_cover_a_later_branch_commit() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create_remote_branch("session-later");
    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join("first.txt"), "merged\n").unwrap();
    git(worktree, &["add", "first.txt"]).unwrap();
    git(worktree, &["commit", "-m", "Merged part"]).unwrap();
    git(worktree, &["push", "origin", &entry.branch]).unwrap();
    git(&fixture.repo, &["merge", "--squash", &entry.branch]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Squashed first part"]).unwrap();

    std::fs::write(worktree.join("later.txt"), "still unique\n").unwrap();
    git(worktree, &["add", "later.txt"]).unwrap();
    git(worktree, &["commit", "-m", "Later unique work"]).unwrap();
    let later_tip = resolve_commit(worktree, "HEAD").unwrap();
    git(worktree, &["push", "origin", &entry.branch]).unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(!plan.entries[0].local_branch.allowed);
    assert!(!plan.entries[0].remote_branch.as_ref().unwrap().allowed);
    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(later_tip.as_str())
    );
    assert_eq!(
        fixture
            .remote_tip(&fixture.remote, &entry.branch)
            .as_deref(),
        Some(later_tip.as_str())
    );
}

#[test]
fn whitespace_different_squash_is_not_integration_evidence() {
    let fixture = RetirementFixture::new();
    std::fs::write(fixture.repo.join("script.py"), "if True:\n    pass\n").unwrap();
    git(&fixture.repo, &["add", "script.py"]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Add script"]).unwrap();
    let entry = fixture.create("session-whitespace");
    let worktree = Path::new(&entry.path);
    std::fs::write(
        worktree.join("script.py"),
        "if True:\n    print('feature')\n",
    )
    .unwrap();
    git(worktree, &["commit", "-am", "Feature indentation"]).unwrap();
    git(&fixture.repo, &["merge", "--squash", &entry.branch]).unwrap();
    std::fs::write(
        fixture.repo.join("script.py"),
        "if True:\n        print('feature')\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "script.py"]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Different indentation"]).unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(!plan.entries[0].local_branch.allowed);
    assert!(plan.entries[0]
        .local_branch
        .reason
        .as_deref()
        .unwrap()
        .contains("not integrated"));
}

#[test]
fn unmerged_clean_checkout_retires_with_branch_and_recovery_kept() {
    let fixture = RetirementFixture::new();
    let entry = fixture.create("session-unmerged");
    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join("unique.txt"), "unique work\n").unwrap();
    git(worktree, &["add", "unique.txt"]).unwrap();
    git(worktree, &["commit", "-m", "Unmerged work"]).unwrap();
    let tip = resolve_commit(worktree, "HEAD").unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(plan.entries.len(), 1);
    assert!(!plan.entries[0].local_branch.allowed);
    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    let result = &report.results[0];
    assert_eq!(result.error, None);
    assert!(result.worktree_removed);
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        direct_ref_oid(&fixture.repo, result.recovery_ref.as_deref().unwrap())
            .unwrap()
            .as_deref(),
        Some(tip.as_str())
    );
}

#[test]
fn offline_base_disables_branch_deletion_without_blocking_checkout_retirement() {
    let fixture = RetirementFixture::new();
    git(&fixture.repo, &["config", "branch.main.remote", "origin"]).unwrap();
    git(
        &fixture.repo,
        &["config", "branch.main.merge", "refs/heads/main"],
    )
    .unwrap();
    let entry = fixture.create_remote_branch("session-offline");
    let tip = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
    let offline = fixture.dir.join("remote temporarily offline.git");
    std::fs::rename(&fixture.remote, &offline).unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert_eq!(plan.entries.len(), 1);
    assert!(!plan.entries[0].local_branch.allowed);
    assert!(!plan.entries[0].remote_branch.as_ref().unwrap().allowed);
    let report = execute_retirement(
        &fixture.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[selection(&entry, false, false)],
    )
    .unwrap();
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert_eq!(
        ref_oid(&fixture.repo, &format!("refs/heads/{}", entry.branch))
            .unwrap()
            .as_deref(),
        Some(tip.as_str())
    );
    std::fs::rename(&offline, &fixture.remote).unwrap();
}

#[test]
fn merge_review_uses_the_recorded_base_upstream_instead_of_origin() {
    let fixture = RetirementFixture::new();
    git(
        &fixture.repo,
        &[
            "remote",
            "add",
            "base",
            &path_to_js(&fixture.alternate_remote),
        ],
    )
    .unwrap();
    git(&fixture.repo, &["config", "branch.main.remote", "base"]).unwrap();
    git(
        &fixture.repo,
        &["config", "branch.main.merge", "refs/heads/main"],
    )
    .unwrap();
    let entry = fixture.create_remote_branch("session-correct-base");
    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join("only-on-origin.txt"), "feature\n").unwrap();
    git(worktree, &["add", "only-on-origin.txt"]).unwrap();
    git(worktree, &["commit", "-m", "Feature"]).unwrap();
    git(worktree, &["push", "origin", &entry.branch]).unwrap();
    git(worktree, &["push", "origin", "HEAD:refs/heads/main"]).unwrap();

    let plan = fixture.plan_ids(std::slice::from_ref(&entry.id));
    assert!(!plan.entries[0].local_branch.allowed);
    assert!(!plan.entries[0].remote_branch.as_ref().unwrap().allowed);
    let snapshot = load_retirement_snapshot(&fixture.conn, &plan.plan_id, &entry.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.base_oid,
        fixture
            .remote_tip(&fixture.alternate_remote, "main")
            .unwrap()
    );
    assert_ne!(
        snapshot.base_oid,
        fixture.remote_tip(&fixture.remote, "main").unwrap()
    );
}
