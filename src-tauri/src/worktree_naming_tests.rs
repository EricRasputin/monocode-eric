use super::*;
use crate::worktrees::tests::{create, Fixture};

const TOKEN: &str = "naming-request-one";

#[test]
fn failed_generation_can_retry_but_never_reopens_a_frozen_or_completed_request() {
    let f = Fixture::new();
    let entry = draft(&f, "retry-session");
    setup_ready(&f, &entry);
    // Generation failure performs no Git mutation and leaves this request
    // waiting. A status read neither consumes nor renews its original token.
    assert_eq!(
        status(&f.conn, &entry.id, TOKEN).unwrap(),
        WorktreeNameStatus::Waiting
    );
    assert_eq!(
        status(&f.conn, &entry.id, "stale-token").unwrap(),
        WorktreeNameStatus::Skipped
    );
    assert_eq!(current(&f, &entry.id).branch, entry.branch);
    let named = suggest(&f.conn, &entry.id, TOKEN, Some("retry-naming"))
        .unwrap()
        .unwrap();
    assert_eq!(named.branch, "monocode/retry-naming");
    assert_eq!(
        status(&f.conn, &entry.id, TOKEN).unwrap(),
        WorktreeNameStatus::Named
    );
    assert!(suggest(&f.conn, &entry.id, TOKEN, Some("rename-again"))
        .unwrap()
        .is_none());

    let frozen = draft(&f, "frozen-retry-session");
    setup_ready(&f, &frozen);
    freeze_pending(&f.conn, &frozen).unwrap();
    assert_eq!(
        status(&f.conn, &frozen.id, TOKEN).unwrap(),
        WorktreeNameStatus::Skipped
    );
    assert!(suggest(&f.conn, &frozen.id, TOKEN, Some("late-retry"))
        .unwrap()
        .is_none());
    assert_eq!(current(&f, &frozen.id).branch, frozen.branch);
}

#[test]
fn explicit_retry_reapplies_only_the_original_saved_suggestion() {
    let f = Fixture::new();
    let entry = draft(&f, "saved-retry-session");
    assert!(suggest(&f.conn, &entry.id, TOKEN, Some("original-name"))
        .unwrap()
        .is_none());
    assert_eq!(
        status(&f.conn, &entry.id, TOKEN).unwrap(),
        WorktreeNameStatus::Pending
    );
    setup_ready(&f, &entry);
    assert!(
        suggest(&f.conn, &entry.id, "wrong-token", Some("wrong-name"))
            .unwrap()
            .is_none()
    );
    let named = suggest(&f.conn, &entry.id, TOKEN, Some("replacement-name"))
        .unwrap()
        .unwrap();
    assert_eq!(named.branch, "monocode/original-name");
}

#[test]
fn naming_freezes_before_publish_or_manual_checkout_without_waiting_for_ai() {
    for suggestion_ready in [false, true] {
        let f = Fixture::new();
        let entry = draft(&f, "session-one");
        if suggestion_ready {
            suggest(&f.conn, &entry.id, TOKEN, Some("pending-name")).unwrap();
        }
        assert!(freeze_pending(&f.conn, &entry).unwrap().is_none());
        setup_ready(&f, &entry);
        assert!(suggest(&f.conn, &entry.id, TOKEN, Some("late-name"))
            .unwrap()
            .is_none());
        assert!(apply_pending(&f.conn, &entry.id).unwrap().is_none());
        assert_eq!(current(&f, &entry.id).branch, entry.branch);
    }
}

#[test]
fn explicit_retry_reuses_the_suggestion_after_a_non_mutating_git_failure() {
    let f = Fixture::new();
    let entry = draft(&f, "git-failure-retry-session");
    setup_ready(&f, &entry);
    git(&f.repo, &["pack-refs", "--all"]).unwrap();
    let lock = f.repo.join(".git/packed-refs.lock");
    std::fs::write(&lock, "held by another Git operation").unwrap();
    let result = suggest(&f.conn, &entry.id, TOKEN, Some("retry-original-name"));
    std::fs::remove_file(lock).unwrap();
    assert!(result.is_err());
    assert_eq!(
        git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
        entry.branch
    );
    assert_eq!(
        status(&f.conn, &entry.id, TOKEN).unwrap(),
        WorktreeNameStatus::Pending
    );
    let named = suggest(&f.conn, &entry.id, TOKEN, Some("replacement-name"))
        .unwrap()
        .unwrap();
    assert_eq!(named.branch, "monocode/retry-original-name");
    setup::validate_checkout(&current(&f, &entry.id), &entry.path).unwrap();
}

#[test]
fn naming_serializes_two_windows_competing_for_the_same_branch() {
    let f = Fixture::new();
    let one = draft(&f, "session-one");
    let two = draft(&f, "session-two");
    setup_ready(&f, &one);
    setup_ready(&f, &two);
    let barrier = std::sync::Barrier::new(2);
    let host = &f.host;
    let db = &f.db;
    let names = std::thread::scope(|scope| {
        let run = |entry: &Owned| {
            let conn = Connection::open(db).unwrap();
            barrier.wait();
            let _repository = host.repository_guard(&entry.common).unwrap();
            let _windows = host.operation_guard().unwrap();
            suggest(&conn, &entry.id, TOKEN, Some("same-task"))
                .unwrap()
                .unwrap()
                .branch
        };
        let first = scope.spawn(move || run(&one));
        let second = scope.spawn(move || run(&two));
        vec![first.join().unwrap(), second.join().unwrap()]
    });
    assert_ne!(names[0], names[1]);
    assert!(names.contains(&"monocode/same-task".to_string()));
    assert!(names.contains(&"monocode/same-task-1".to_string()));
}

fn draft(f: &Fixture, id: &str) -> Owned {
    crate::worktrees::disk::coordinate(&f.host.disk, &f.conn, || {
        create_with_naming(
            &f.conn,
            &f.host,
            &path_to_js(&f.repo),
            id,
            "Raw verbose request",
            Some("main"),
            Some(TOKEN),
        )
    })
    .unwrap()
}

fn setup_ready(f: &Fixture, entry: &Owned) {
    if let environment::BeginSetup::Run(operation) =
        environment::begin_setup(&f.conn, &entry.path).unwrap()
    {
        let result = environment::run_setup(&operation, |_| {});
        environment::finish_setup(&f.conn, &operation, &result).unwrap();
        result.unwrap();
    }
}

fn current(f: &Fixture, id: &str) -> Owned {
    owned(&f.conn)
        .unwrap()
        .into_iter()
        .find(|entry| entry.id == id)
        .unwrap()
}

#[test]
fn naming_preserves_checkout_and_full_retirement_recovery() {
    let f = Fixture::new();
    let original = draft(&f, "aabbccdd-1234-5678-long-session-identity");
    assert_eq!(original.branch, "monocode/task-aabbccdd");
    setup_ready(&f, &original);
    let head = resolve_commit(Path::new(&original.path), "HEAD").unwrap();
    let named = suggest(&f.conn, &original.id, TOKEN, Some("AI worktree naming"))
        .unwrap()
        .unwrap();
    assert_eq!(named.branch, "monocode/ai-worktree-naming");
    let entry = current(&f, &original.id);
    assert_eq!(entry.path, original.path);
    assert_eq!(entry.base_ref, original.base_ref);
    assert_eq!(
        resolve_commit(Path::new(&entry.path), "HEAD").unwrap(),
        head
    );
    assert!(ref_oid(&f.repo, &format!("refs/heads/{}", original.branch))
        .unwrap()
        .is_none());
    setup::validate_checkout(&entry, &entry.path).unwrap();
    open_owned(&f.conn, &entry).unwrap();
    let plan = build_retirement_plan(
        &f.conn,
        &HashMap::new(),
        &[],
        Some(&path_to_js(&f.repo)),
        std::slice::from_ref(&entry.id),
    )
    .unwrap();
    assert_eq!(plan.entries.len(), 1);
    let report = execute_retirement(
        &f.conn,
        &HashMap::new(),
        &plan.plan_id,
        &[WorktreeRetirementSelection {
            id: entry.id.clone(),
            delete_local_branch: false,
            delete_remote_branch: false,
        }],
    )
    .unwrap();
    assert!(
        report.results[0].worktree_removed,
        "{:?}",
        report.results[0].error
    );
    let removed = current(&f, &entry.id);
    open_owned(&f.conn, &removed).unwrap();
    let restored = current(&f, &entry.id);
    assert_eq!(restored.branch, named.branch);
    assert_eq!(
        resolve_commit(Path::new(&restored.path), "HEAD").unwrap(),
        head
    );
}

#[test]
fn naming_survives_failed_setup_and_consumes_the_first_suggestion_once() {
    let f = Fixture::new();
    let entry = draft(&f, "session-one");
    f.conn
        .execute(
            "UPDATE worktree_environment_setup SET status = 'failed' WHERE worktree_id = ?1",
            [&entry.id],
        )
        .unwrap();
    assert!(suggest(&f.conn, &entry.id, TOKEN, Some("Fix startup"))
        .unwrap()
        .is_none());
    assert_eq!(current(&f, &entry.id).branch, entry.branch);
    // Reopen the database to prove this is durable, not an in-memory callback.
    let conn = Connection::open(&f.db).unwrap();
    assert_eq!(record(&conn, &entry.id).unwrap().unwrap().state, "ready");
    assert!(suggest(&conn, &entry.id, TOKEN, Some("Different followup"))
        .unwrap()
        .is_none());
    setup_ready(&f, &entry);
    assert_eq!(
        apply_pending(&conn, &entry.id).unwrap().unwrap().branch,
        "monocode/fix-startup"
    );
    assert!(apply_pending(&conn, &entry.id).unwrap().is_none());
}

#[test]
fn naming_allocates_collisions_without_claiming_existing_refs() {
    let f = Fixture::new();
    let first = draft(&f, "session-one");
    let second = draft(&f, "session-two");
    assert_ne!(first.branch, second.branch); // identical first eight ID characters
    setup_ready(&f, &first);
    setup_ready(&f, &second);
    git(&f.repo, &["branch", "monocode/add-search"]).unwrap();
    let one = suggest(&f.conn, &first.id, TOKEN, Some("add-search"))
        .unwrap()
        .unwrap();
    let two = suggest(&f.conn, &second.id, TOKEN, Some("add-search"))
        .unwrap()
        .unwrap();
    assert_eq!(one.branch, "monocode/add-search-1");
    assert_eq!(two.branch, "monocode/add-search-2");
    assert!(ref_oid(&f.repo, "refs/heads/monocode/add-search")
        .unwrap()
        .is_some());
}

#[test]
fn naming_rejects_unowned_reused_and_stale_requests() {
    let f = Fixture::new();
    let manual = create(
        &f.conn,
        &f.host,
        &path_to_js(&f.repo),
        "manual-session",
        "Chosen name",
        Some("main"),
    )
    .unwrap();
    assert!(suggest(&f.conn, &manual.id, TOKEN, Some("ignored"))
        .unwrap()
        .is_none());
    let entry = draft(&f, "session-one");
    setup_ready(&f, &entry);
    assert!(
        suggest(&f.conn, &entry.id, "another-token", Some("ignored"))
            .unwrap()
            .is_none()
    );
    git(Path::new(&entry.path), &["branch", "-m", "user-chosen"]).unwrap();
    assert!(suggest(&f.conn, &entry.id, TOKEN, Some("ignored"))
        .unwrap()
        .is_none());
    assert_eq!(
        git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
        "user-chosen"
    );
    assert_eq!(
        record(&f.conn, &entry.id).unwrap().unwrap().state,
        "skipped"
    );
}

#[test]
fn naming_preserves_published_shared_archived_and_locked_worktrees() {
    for reason in ["published", "shared", "archived", "locked", "retired"] {
        let f = Fixture::new();
        let entry = draft(&f, "session-one");
        setup_ready(&f, &entry);
        match reason {
            "published" => {
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
                git(
                    &f.repo,
                    &[
                        "remote",
                        "add",
                        "origin",
                        "https://example.invalid/repo.git",
                    ],
                )
                .unwrap();
            }
            "shared" => {
                f.conn.execute("INSERT INTO sessions(id, cwd, worktree_cwd) VALUES ('another-session', ?1, ?2)", params![path_to_js(&f.repo), format!("{}/nested", entry.path)]).unwrap();
            }
            "archived" => {
                f.conn.execute("INSERT INTO sessions(id, cwd, worktree_cwd, archived) VALUES (?1, ?2, ?3, 1)", params![entry.id, path_to_js(&f.repo), entry.path]).unwrap();
            }
            "locked" => {
                git(&f.repo, &["worktree", "lock", &entry.path]).unwrap();
            }
            "retired" => {
                f.conn
                    .execute(
                        "UPDATE managed_worktrees SET removed = 1 WHERE id = ?1",
                        [&entry.id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            suggest(&f.conn, &entry.id, TOKEN, Some("ignored"))
                .unwrap()
                .is_none(),
            "{reason}"
        );
        assert_eq!(
            record(&f.conn, &entry.id).unwrap().unwrap().state,
            "skipped",
            "{reason}"
        );
        assert_eq!(
            git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
            entry.branch
        );
    }
}

#[test]
fn naming_reconciles_git_success_after_database_failure_including_nested_sessions() {
    let f = Fixture::new();
    let entry = draft(&f, "session-one");
    setup_ready(&f, &entry);
    f.conn
        .execute(
            "INSERT INTO sessions(id, cwd, worktree_cwd, branch) VALUES (?1, ?2, ?3, ?4)",
            params![entry.id, path_to_js(&f.repo), entry.path, entry.branch],
        )
        .unwrap();
    // Fail the metadata transaction after the Git operation, as a disk error would.
    f.conn.execute_batch("CREATE TRIGGER fail_name BEFORE UPDATE OF branch ON managed_worktrees BEGIN SELECT RAISE(FAIL, 'simulated storage failure'); END;").unwrap();
    assert!(suggest(&f.conn, &entry.id, TOKEN, Some("recover-name")).is_err());
    assert_eq!(
        record(&f.conn, &entry.id).unwrap().unwrap().state,
        "renaming"
    );
    assert_eq!(
        git(Path::new(&entry.path), &["branch", "--show-current"]).unwrap(),
        "monocode/recover-name"
    );
    f.conn.execute_batch("DROP TRIGGER fail_name;").unwrap();
    f.conn.execute("INSERT INTO sessions(id, cwd, worktree_cwd, branch) VALUES ('nested-session', ?1, ?2, ?3)", params![path_to_js(&f.repo), format!("{}/nested", entry.path), entry.branch]).unwrap();
    git(
        Path::new(&entry.path),
        &["commit", "--allow-empty", "-m", "Agent continued"],
    )
    .unwrap();
    let reopened = Connection::open(&f.db).unwrap();
    let named = apply_pending(&reopened, &entry.id).unwrap().unwrap();
    assert!(named.session_ids.contains(&"nested-session".to_string()));
    let branches: i64 = reopened
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE branch = 'monocode/recover-name'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(branches, 2);
    assert!(apply_pending(&reopened, &entry.id).unwrap().is_none());
    setup::validate_checkout(&current(&f, &entry.id), &entry.path).unwrap();
}

#[test]
fn naming_does_not_replay_an_interrupted_intent_or_adopt_an_unrelated_checkout() {
    for renamed_elsewhere in [false, true] {
        let f = Fixture::new();
        let entry = draft(&f, "session-one");
        setup_ready(&f, &entry);
        let oid = resolve_commit(Path::new(&entry.path), "HEAD").unwrap();
        f.conn.execute("UPDATE worktree_naming SET state = 'renaming', target_branch = 'monocode/proposed', rename_oid = ?1 WHERE worktree_id = ?2", params![oid, entry.id]).unwrap();
        if renamed_elsewhere {
            git(Path::new(&entry.path), &["branch", "-m", "user-branch"]).unwrap();
            assert!(apply_pending(&f.conn, &entry.id).is_err());
            assert_eq!(current(&f, &entry.id).branch, entry.branch);
        } else {
            assert!(apply_pending(&f.conn, &entry.id).unwrap().is_none());
            assert_eq!(
                record(&f.conn, &entry.id).unwrap().unwrap().state,
                "skipped"
            );
        }
        assert!(ref_oid(&f.repo, "refs/heads/monocode/proposed")
            .unwrap()
            .is_none());
    }
}

#[test]
fn naming_failure_keeps_fallback_and_normalization_is_git_safe() {
    let f = Fixture::new();
    for (i, suggestion) in [None, Some("... / "), Some("")].into_iter().enumerate() {
        let entry = draft(&f, &format!("session-{i}"));
        setup_ready(&f, &entry);
        assert!(suggest(&f.conn, &entry.id, TOKEN, suggestion)
            .unwrap()
            .is_none());
        assert_eq!(current(&f, &entry.id).branch, entry.branch);
        assert!(suggest(&f.conn, &entry.id, TOKEN, Some("late-result"))
            .unwrap()
            .is_none());
    }
    assert_eq!(
        normalize("refs/heads/monocode/Fix `Search`... @ UI"),
        Some("monocode/fix-search-ui".into())
    );
    let long = normalize(&"very-long-name-".repeat(20)).unwrap();
    assert!(long.len() <= "monocode/".len() + 64);
    git(&f.repo, &["check-ref-format", "--branch", &long]).unwrap();
}
