//! Exercise the environment through the same create/review/retire/open seams
//! used by the native commands, with real Git and an on-disk database.
use super::tests::{create, prepare};
use super::*;

struct EnvironmentFixture {
    dir: PathBuf,
    repo: PathBuf,
    conn: Connection,
    host: WorktreeHost,
}

impl EnvironmentFixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "monocode-environment-integration-{}-{}",
            std::process::id(),
            RETIREMENT_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        let repo = dir.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]).unwrap();
        git(&repo, &["config", "user.name", "Environment Test"]).unwrap();
        git(
            &repo,
            &["config", "user.email", "environment@example.invalid"],
        )
        .unwrap();
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
        std::fs::write(
            repo.join(".gitignore"),
            ".env\n.env.local\nnode_modules/\ndist/\nlocal.sqlite\n",
        )
        .unwrap();
        std::fs::write(repo.join("code.txt"), "original\n").unwrap();
        std::fs::create_dir_all(repo.join("apps/web")).unwrap();
        std::fs::create_dir_all(repo.join("apps/api")).unwrap();
        std::fs::write(repo.join("apps/web/app.txt"), "web\n").unwrap();
        std::fs::write(repo.join("apps/api/app.txt"), "api\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "Initial"]).unwrap();
        std::fs::write(repo.join(".env"), "initial configuration\n").unwrap();
        let conn = Connection::open(dir.join("state.sqlite")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema(&conn).unwrap();
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, worktree_cwd TEXT, branch TEXT, archived INTEGER DEFAULT 0, pinned INTEGER DEFAULT 0)").unwrap();
        let host = WorktreeHost {
            root: dir.join("worktrees"),
            windows: Mutex::new(HashMap::new()),
            repositories: RepositoryReservations::default(),
            disk: disk::DiskManager::default(),
        };
        let fixture = Self {
            dir,
            repo,
            conn,
            host,
        };
        fixture.configure("mkdir -p node_modules dist && printf generated > dist/result.txt");
        fixture
    }

    fn configure(&self, command: &str) {
        self.configure_at(&self.repo, command, &[".env"]);
    }

    fn configure_at(&self, project: &Path, command: &str, copy_paths: &[&str]) {
        let scope = environment::scope_for_cwd(&path_to_js(project)).unwrap();
        let version = environment::load_settings(&self.conn, &scope)
            .unwrap()
            .environment_version;
        environment::save_settings(
            &self.conn,
            &scope,
            &environment::EnvironmentSettings {
                environment_version: version,
                setup_command: command.into(),
                copy_paths: copy_paths.iter().map(|path| (*path).into()).collect(),
                disposable_paths: vec!["node_modules".into(), "dist".into()],
            },
        )
        .unwrap();
    }

    fn create(&self) -> Owned {
        self.create_at(&self.repo)
    }

    fn create_at(&self, project: &Path) -> Owned {
        create(
            &self.conn,
            &self.host,
            &path_to_js(project),
            "environment-task",
            "Environment",
            Some("main"),
        )
        .unwrap()
    }

    fn setup(&self, entry: &Owned) -> Result<(), String> {
        self.setup_at(&entry.path)
    }

    fn setup_at(&self, requested_path: &str) -> Result<(), String> {
        let result = match environment::begin_setup(&self.conn, requested_path)? {
            environment::BeginSetup::Skip => Ok(()),
            environment::BeginSetup::Run(operation) => {
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(&self.conn, &operation, &result)?;
                result
            }
        };
        if let Some(entry) = owned(&self.conn)?
            .into_iter()
            .find(|entry| path_inside(Path::new(requested_path), Path::new(&entry.path)))
        {
            disk::release_handoff(&self.host.disk, &self.conn, &entry.path)?;
        }
        result
    }

    fn plan(&self, entry: &Owned) -> WorktreeRetirementPlan {
        build_retirement_plan(
            &self.conn,
            &HashMap::new(),
            &[],
            Some(&entry.repo),
            std::slice::from_ref(&entry.id),
        )
        .unwrap()
    }

    fn retire(&self, plan: &WorktreeRetirementPlan, entry: &Owned) -> WorktreeRetirementReport {
        execute_retirement(
            &self.conn,
            &HashMap::new(),
            &plan.plan_id,
            &[WorktreeRetirementSelection {
                id: entry.id.clone(),
                delete_local_branch: false,
                delete_remote_branch: false,
            }],
        )
        .unwrap()
    }
}

impl Drop for EnvironmentFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn access_request(cwd: &str) -> PrepareWorktree {
    PrepareWorktree {
        cwd: cwd.into(),
        session_id: "filesystem-access".into(),
        path: Some(cwd.into()),
        name: "Filesystem access".into(),
        create_new: false,
        use_worktree: Some(false),
        base_ref: None,
        auto_name_token: None,
    }
}

#[test]
fn standalone_access_accepts_plain_directories_but_never_a_file_as_checkout() {
    let fixture = EnvironmentFixture::new();
    let folder = fixture.dir.join("ordinary folder");
    std::fs::create_dir(&folder).unwrap();
    let file = folder.join("notes.txt");
    std::fs::write(&file, "ordinary file").unwrap();
    for cwd in [&folder, &fixture.repo] {
        let request = access_request(&path_to_js(cwd));
        preparation_common(&fixture.conn, &request).unwrap();
        let prepared = prepare(&fixture.conn, &fixture.host, request)
            .unwrap()
            .unwrap();
        assert_eq!(prepared, path_to_js(&std::fs::canonicalize(cwd).unwrap()));
    }
    let request = access_request(&path_to_js(&file));
    assert!(preparation_common(&fixture.conn, &request).is_err());
    assert!(prepare(&fixture.conn, &fixture.host, request).is_err());
    assert!(owned(&fixture.conn).unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(file).unwrap(), "ordinary file");
}

#[test]
fn archived_access_resolves_missing_checkout_and_retries_setup_without_activating_history() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create_at(&fixture.repo.join("apps/web"));
    fixture.setup(&entry).unwrap();
    fixture.conn.execute(
        "INSERT INTO sessions (id, cwd, worktree_cwd, branch, archived) VALUES ('saved', ?1, ?2, ?3, 1)",
        params![path_to_js(&fixture.repo.join("apps/web")), format!("{}/apps/web", entry.path), entry.branch],
    ).unwrap();
    let plan = fixture.plan(&entry);
    assert_eq!(fixture.retire(&plan, &entry).results[0].error, None);
    assert!(!Path::new(&entry.path).exists());
    // Looking up saved history has no lifecycle side effects, even after restart.
    let reopened = Connection::open(fixture.dir.join("state.sqlite")).unwrap();
    let saved: (String, bool) = reopened
        .query_row(
            "SELECT worktree_cwd, archived FROM sessions WHERE id = 'saved'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(saved.1);
    assert!(!Path::new(&entry.path).exists());
    // Recovery must preserve the saved commit when its old branch name is reused.
    git(&fixture.repo, &["branch", "-D", &entry.branch]).unwrap();
    std::fs::write(fixture.repo.join("later.txt"), "later main work").unwrap();
    git(&fixture.repo, &["add", "later.txt"]).unwrap();
    git(&fixture.repo, &["commit", "-m", "Reuse branch name"]).unwrap();
    let reused_tip = resolve_commit(&fixture.repo, "HEAD").unwrap();
    git(&fixture.repo, &["branch", &entry.branch]).unwrap();
    fixture.configure_at(&fixture.repo.join("apps/web"), "exit 9", &[]);
    let request = access_request(&saved.0);
    assert_eq!(
        preparation_common(&fixture.conn, &request).unwrap(),
        entry.common
    );
    let prepared = prepare(&fixture.conn, &fixture.host, request)
        .unwrap()
        .unwrap();
    assert!(prepared.ends_with("/apps/web"));
    let recovered_branch: String = fixture
        .conn
        .query_row(
            "SELECT branch FROM sessions WHERE id = 'saved'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(recovered_branch, entry.branch);
    assert!(recovered_branch.starts_with("monocode/recovered-"));
    assert_eq!(
        resolve_commit(&fixture.repo, &entry.branch).unwrap(),
        reused_tip
    );

    assert!(fixture.setup_at(&prepared).is_err());
    assert!(fixture
        .conn
        .query_row(
            "SELECT archived FROM sessions WHERE id = 'saved'",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap());
    fixture.configure_at(
        &fixture.repo.join("apps/web"),
        "printf ready > setup-proof",
        &[],
    );
    let retried = prepare(&fixture.conn, &fixture.host, access_request(&saved.0))
        .unwrap()
        .unwrap();
    assert_eq!(prepared, retried);
    fixture.setup_at(&retried).unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&retried).join("setup-proof")).unwrap(),
        "ready"
    );
    assert!(!fixture.repo.join("apps/web/setup-proof").exists());
    // Only the successful conversation preparation caller may activate it.
    assert!(fixture
        .conn
        .query_row(
            "SELECT archived FROM sessions WHERE id = 'saved'",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap());
}

#[test]
fn subsequent_workspace_access_observes_native_pending_setup() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    std::fs::remove_file(Path::new(&entry.path).join("dist/result.txt")).unwrap();
    // Simulate a future generated-output cleanup invalidating native setup.
    fixture
        .conn
        .execute(
            "UPDATE worktree_environment_setup SET status = 'pending' WHERE worktree_id = ?1",
            [&entry.id],
        )
        .unwrap();
    let prepared = prepare(&fixture.conn, &fixture.host, access_request(&entry.path))
        .unwrap()
        .unwrap();
    fixture.setup_at(&prepared).unwrap();
    assert!(Path::new(&prepared).join("dist/result.txt").is_file());
}

#[test]
fn shared_checkout_setup_waits_across_sessions_and_subdirectories_and_shares_failures() {
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    for fail in [false, true] {
        let fixture = EnvironmentFixture::new();
        fixture.configure(if fail {
            "exit 9"
        } else {
            "printf once >> setup-count"
        });
        let entry = fixture.create();
        let first = prepare(
            &fixture.conn,
            &fixture.host,
            access_request(&format!("{}/apps/web", entry.path)),
        )
        .unwrap()
        .unwrap();
        let second = prepare(
            &fixture.conn,
            &fixture.host,
            access_request(&format!("{}/apps/api", entry.path)),
        )
        .unwrap()
        .unwrap();
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let database = fixture.dir.join("state.sqlite");
        let path = entry.path.clone();
        let owner_runs = Arc::clone(&runs);
        let owner = std::thread::spawn(move || {
            setup::coordinate_setup(&path, || {
                owner_runs.fetch_add(1, Ordering::SeqCst);
                let conn = Connection::open(database).unwrap();
                let environment::BeginSetup::Run(operation) =
                    environment::begin_setup(&conn, &first)?
                else {
                    panic!("expected pending setup")
                };
                started_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(&conn, &operation, &result)?;
                result
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let path = entry.path.clone();
        let waiter_runs = Arc::clone(&runs);
        let waiter = std::thread::spawn(move || {
            setup::coordinate_setup(&path, || {
                waiter_runs.fetch_add(1, Ordering::SeqCst);
                panic!("second subdirectory {second} ran setup concurrently")
            })
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !setup::setup_has_waiter(&entry.path) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(setup::setup_has_waiter(&entry.path));
        assert!(setup::active_paths().contains(&PathBuf::from(&entry.path)));
        finish_tx.send(()).unwrap();
        let result = owner.join().unwrap();
        assert_eq!(result, waiter.join().unwrap());
        assert_eq!(result.is_err(), fail);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(!setup::active_paths().contains(&PathBuf::from(&entry.path)));
        if fail {
            fixture.configure("printf retried > setup-count");
            setup::coordinate_setup(&entry.path, || fixture.setup(&entry)).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(Path::new(&entry.path).join("setup-count")).unwrap(),
            if fail { "retried" } else { "once" }
        );
    }
}

fn automatic_output_fixture() -> EnvironmentFixture {
    let fixture = EnvironmentFixture::new();
    for directory in ["", "apps/web", "apps/api"] {
        std::fs::write(fixture.repo.join(directory).join("package.json"), "{}\n").unwrap();
    }
    git(&fixture.repo, &["add", "."]).unwrap();
    git(
        &fixture.repo,
        &["commit", "-m", "Add Node project manifests"],
    )
    .unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&fixture.repo)).unwrap();
    let mut settings = environment::load_settings(&fixture.conn, &scope).unwrap();
    settings.disposable_paths.clear();
    environment::save_settings(&fixture.conn, &scope, &settings).unwrap();
    fixture
}

#[test]
fn automatic_outputs_preserve_configured_files_and_cover_nested_projects() {
    let fixture = automatic_output_fixture();
    // An explicitly copied file within an automatic output still has to be
    // archived. Automatic disposal never becomes a conflicting saved policy.
    std::fs::create_dir(fixture.repo.join("dist")).unwrap();
    std::fs::write(fixture.repo.join("dist/local.json"), "initial local data\n").unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&fixture.repo)).unwrap();
    let mut settings = environment::load_settings(&fixture.conn, &scope).unwrap();
    settings.copy_paths.push("dist/local.json".into());
    environment::save_settings(&fixture.conn, &scope, &settings).unwrap();

    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    let root = Path::new(&entry.path);
    for directory in ["apps/web/node_modules", "apps/web/dist", "apps/api/dist"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
        std::fs::write(root.join(directory).join("generated"), "build output").unwrap();
    }
    std::fs::write(root.join(".env"), "last worktree configuration\n").unwrap();
    std::fs::write(root.join("dist/local.json"), "last local data\n").unwrap();
    let plan = fixture.plan(&entry);
    assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);
    assert_eq!(fixture.retire(&plan, &entry).results[0].error, None);
    assert!(!root.exists());
    assert!(environment::load_settings(&fixture.conn, &scope)
        .unwrap()
        .disposable_paths
        .is_empty());

    open_owned(&fixture.conn, &entry).unwrap();
    fixture.setup(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(".env")).unwrap(),
        "last worktree configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dist/local.json")).unwrap(),
        "last local data\n"
    );
    assert!(!root.join("apps/web/node_modules").exists());
    assert!(!root.join("apps/api/dist").exists());
}

#[test]
fn automatic_outputs_do_not_hide_local_data_or_changes_added_after_review() {
    let fixture = automatic_output_fixture();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    let root = Path::new(&entry.path);
    let plan = fixture.plan(&entry);
    assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);

    for path in [
        ".env.local",
        "local.sqlite",
        "notes.txt",
        "code.txt",
        "apps/api/local.sqlite",
    ] {
        std::fs::write(root.join(path), "local data to keep\n").unwrap();
        let report = fixture.retire(&plan, &entry);
        assert!(!report.results[0].worktree_removed);
        assert!(report.results[0].error.as_deref().unwrap().contains(path));
        assert_eq!(
            std::fs::read_to_string(root.join(path)).unwrap(),
            "local data to keep\n"
        );
        if path == "code.txt" {
            std::fs::write(root.join(path), "original\n").unwrap();
        } else {
            std::fs::remove_file(root.join(path)).unwrap();
        }
    }
    assert_eq!(fixture.retire(&plan, &entry).results[0].error, None);
    assert!(!root.exists());
}

#[test]
fn branch_cleanup_after_restart_preserves_the_original_configuration_recovery() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    std::fs::write(
        Path::new(&entry.path).join(".env"),
        "retired configuration\n",
    )
    .unwrap();
    let first = fixture.plan(&entry);
    assert_eq!(fixture.retire(&first, &entry).results[0].error, None);
    std::fs::write(fixture.repo.join(".env"), "new primary configuration\n").unwrap();

    let review = fixture.plan(&entry);
    let restarted = Connection::open(fixture.dir.join("state.sqlite")).unwrap();
    schema(&restarted).unwrap();
    let completed = execute_retirement(
        &restarted,
        &HashMap::new(),
        &review.plan_id,
        &[WorktreeRetirementSelection {
            id: entry.id.clone(),
            delete_local_branch: true,
            delete_remote_branch: false,
        }],
    )
    .unwrap();
    assert_eq!(completed.results[0].error, None);
    assert!(completed.results[0].local_branch_deleted);
    assert_eq!(
        latest_local_recovery(&restarted, &entry.id)
            .unwrap()
            .unwrap()
            .plan_id,
        first.plan_id
    );
    open_owned(&restarted, &entry).unwrap();
    fixture.setup(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join(".env")).unwrap(),
        "retired configuration\n"
    );
}

#[test]
fn unmerged_checkout_roundtrip_restores_last_local_config_and_rebuilds_generated_files() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    let root = Path::new(&entry.path);
    assert_eq!(
        std::fs::read_to_string(root.join(".env")).unwrap(),
        "initial configuration\n"
    );
    assert!(root.join("dist/result.txt").is_file());
    std::fs::write(root.join("code.txt"), "unmerged feature\n").unwrap();
    git(root, &["commit", "-am", "Unmerged feature"]).unwrap();
    let tip = resolve_commit(root, "HEAD").unwrap();
    let plan = fixture.plan(&entry);
    assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);
    assert!(!plan.entries[0].local_branch.allowed);
    // Changes made since the review must be preserved at retirement time.
    std::fs::write(root.join(".env"), "last worktree configuration\n").unwrap();
    let report = fixture.retire(&plan, &entry);
    assert_eq!(report.results[0].error, None);
    assert!(report.results[0].worktree_removed);
    assert!(!root.exists());
    assert_eq!(resolve_commit(&fixture.repo, &entry.branch).unwrap(), tip);
    std::fs::write(fixture.repo.join(".env"), "different main configuration\n").unwrap();
    std::fs::write(
        fixture.repo.join(".env.local"),
        "today's local configuration\n",
    )
    .unwrap();
    fixture.configure_at(
        &fixture.repo,
        "printf current > current-policy-ran",
        &[".env.local"],
    );
    let restarted = Connection::open(fixture.dir.join("state.sqlite")).unwrap();
    schema(&restarted).unwrap();
    let retired = owned(&restarted).unwrap().remove(0);
    open_owned(&restarted, &retired).unwrap();
    fixture.setup(&retired).unwrap();
    assert_eq!(resolve_commit(root, "HEAD").unwrap(), tip);
    assert_eq!(
        std::fs::read_to_string(root.join(".env")).unwrap(),
        "last worktree configuration\n"
    );
    assert!(!root.join("dist/result.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join(".env.local")).unwrap(),
        "today's local configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("current-policy-ran")).unwrap(),
        "current"
    );
}

#[test]
fn changed_environment_policy_requires_another_retirement_review() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    let plan = fixture.plan(&entry);
    assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);
    fixture.configure("true");
    let report = fixture.retire(&plan, &entry);
    assert!(!report.results[0].worktree_removed);
    assert!(report.results[0]
        .error
        .as_deref()
        .unwrap()
        .contains("changed after review"));
    assert!(Path::new(&entry.path).join(".env").is_file());
}

#[test]
fn failed_setup_retry_keeps_local_config_edits_and_ready_setup_does_not_repeat() {
    let fixture = EnvironmentFixture::new();
    fixture.configure(
        "test \"$(cat .env)\" = repaired && mkdir -p dist && printf complete > dist/result.txt",
    );
    let entry = fixture.create();
    assert!(fixture.setup(&entry).is_err());
    std::fs::write(Path::new(&entry.path).join(".env"), "repaired\n").unwrap();
    fixture.setup(&entry).unwrap();
    let result = Path::new(&entry.path).join("dist/result.txt");
    std::fs::write(&result, "retained existing setup\n").unwrap();
    fixture.setup(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(&result).unwrap(),
        "retained existing setup\n"
    );
}

#[test]
fn failed_setup_retry_applies_new_copy_paths_without_overwriting_previous_files() {
    let fixture = EnvironmentFixture::new();
    fixture.configure("exit 7");
    let entry = fixture.create();
    assert!(fixture.setup(&entry).is_err());

    let worktree = Path::new(&entry.path);
    std::fs::write(worktree.join(".env"), "user repaired configuration\n").unwrap();
    std::fs::write(fixture.repo.join(".env"), "new source configuration\n").unwrap();
    std::fs::write(fixture.repo.join(".env.local"), "new local configuration\n").unwrap();
    let scope = environment::scope_for_cwd(&path_to_js(&fixture.repo)).unwrap();
    let version = environment::load_settings(&fixture.conn, &scope)
        .unwrap()
        .environment_version;
    environment::save_settings(
        &fixture.conn,
        &scope,
        &environment::EnvironmentSettings {
            environment_version: version,
            setup_command:
                "test \"$(cat .env)\" = 'user repaired configuration' && test \"$(cat .env.local)\" = 'new local configuration'"
                    .into(),
            copy_paths: vec![".env".into(), ".env.local".into()],
            disposable_paths: vec!["node_modules".into(), "dist".into()],
        },
    )
    .unwrap();

    fixture.setup(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(worktree.join(".env")).unwrap(),
        "user repaired configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join(".env.local")).unwrap(),
        "new local configuration\n"
    );
}

#[test]
fn restored_setup_retry_uses_repaired_command_without_replacing_archived_files() {
    let fixture = EnvironmentFixture::new();
    fixture.configure("true");
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    std::fs::write(
        Path::new(&entry.path).join(".env"),
        "archived configuration\n",
    )
    .unwrap();
    let plan = fixture.plan(&entry);
    let report = fixture.retire(&plan, &entry);
    assert_eq!(report.results[0].error, None);

    fixture.configure("exit 9");
    let retired = owned(&fixture.conn).unwrap().remove(0);
    open_owned(&fixture.conn, &retired).unwrap();
    assert!(fixture.setup(&retired).is_err());
    let restored = Path::new(&retired.path).join(".env");
    assert_eq!(
        std::fs::read_to_string(&restored).unwrap(),
        "archived configuration\n"
    );

    std::fs::write(&restored, "user repaired configuration\n").unwrap();
    std::fs::write(fixture.repo.join(".env.local"), "new retry configuration\n").unwrap();
    fixture.configure_at(
        &fixture.repo,
        "test \"$(cat .env)\" = 'user repaired configuration' && test \"$(cat .env.local)\" = 'new retry configuration' && printf repaired > restored-setup",
        &[".env", ".env.local"],
    );
    fixture.setup(&retired).unwrap();
    assert_eq!(
        std::fs::read_to_string(restored).unwrap(),
        "user repaired configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&retired.path).join(".env.local")).unwrap(),
        "new retry configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&retired.path).join("restored-setup")).unwrap(),
        "repaired"
    );
}

#[test]
fn nested_projects_keep_independent_versioned_environment_settings() {
    let fixture = EnvironmentFixture::new();
    let web = fixture.repo.join("apps/web");
    let api = fixture.repo.join("apps/api");
    let web_scope = environment::scope_for_cwd(&path_to_js(&web)).unwrap();
    let api_scope = environment::scope_for_cwd(&path_to_js(&api)).unwrap();
    assert_eq!(web_scope.relative, "apps/web");
    assert_eq!(api_scope.relative, "apps/api");

    let stale_web = environment::load_settings(&fixture.conn, &web_scope).unwrap();
    assert_eq!(stale_web, environment::EnvironmentSettings::default());
    fixture.configure_at(&web, "printf web > setup-marker", &[".env"]);
    fixture.configure_at(&api, "printf api > setup-marker", &[".env.local"]);

    let saved_web = environment::load_settings(&fixture.conn, &web_scope).unwrap();
    let saved_api = environment::load_settings(&fixture.conn, &api_scope).unwrap();
    assert_eq!(saved_web.setup_command, "printf web > setup-marker");
    assert_eq!(saved_web.copy_paths, [".env"]);
    assert_eq!(saved_api.setup_command, "printf api > setup-marker");
    assert_eq!(saved_api.copy_paths, [".env.local"]);
    assert_eq!(saved_web.environment_version, 1);
    assert_eq!(saved_api.environment_version, 1);

    let conflict = environment::save_settings(&fixture.conn, &web_scope, &stale_web).unwrap_err();
    assert!(
        conflict.starts_with("WORKTREE_SETTINGS_CONFLICT:"),
        "{conflict}"
    );
    assert_eq!(
        environment::load_settings(&fixture.conn, &web_scope)
            .unwrap()
            .setup_command,
        "printf web > setup-marker"
    );
}

#[test]
fn settings_saved_during_setup_keep_completion_retryable_for_new_copy_paths() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    let operation = match environment::begin_setup(&fixture.conn, &entry.path).unwrap() {
        environment::BeginSetup::Run(operation) => operation,
        environment::BeginSetup::Skip => panic!("setup unexpectedly skipped"),
    };

    std::fs::write(fixture.repo.join(".env.local"), "late configuration\n").unwrap();
    fixture.configure_at(&fixture.repo, "true", &[".env", ".env.local"]);
    let first = environment::run_setup(&operation, |_| {});
    let completion = environment::finish_setup(&fixture.conn, &operation, &first).unwrap();
    first.unwrap();
    assert!(matches!(completion, environment::FinishSetup::Retry(_)));

    fixture.setup(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&entry.path).join(".env.local")).unwrap(),
        "late configuration\n"
    );
}

#[test]
fn setup_uses_first_origin_project_when_requested_from_a_sibling() {
    let fixture = EnvironmentFixture::new();
    let web = fixture.repo.join("apps/web");
    let api = fixture.repo.join("apps/api");
    std::fs::write(web.join(".env"), "web configuration\n").unwrap();
    std::fs::write(api.join(".env"), "api configuration\n").unwrap();
    fixture.configure_at(
        &web,
        "test \"$(cat .env)\" = 'web configuration' && printf web > setup-marker",
        &[".env"],
    );
    fixture.configure_at(
        &api,
        "test \"$(cat .env)\" = 'api configuration' && printf api > setup-marker",
        &[".env"],
    );
    let entry = fixture.create_at(&web);
    let requested_api = Path::new(&entry.path).join("apps/api");

    fixture.setup_at(&path_to_js(&requested_api)).unwrap();

    let root = Path::new(&entry.path);
    assert_eq!(
        std::fs::read_to_string(root.join("apps/web/.env")).unwrap(),
        "web configuration\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("apps/web/setup-marker")).unwrap(),
        "web"
    );
    assert!(!root.join("apps/api/.env").exists());
    assert!(!root.join("apps/api/setup-marker").exists());
}

#[test]
fn nested_project_policy_still_blocks_unknown_files_elsewhere_in_checkout() {
    let fixture = EnvironmentFixture::new();
    let web = fixture.repo.join("apps/web");
    std::fs::write(web.join(".env"), "web configuration\n").unwrap();
    fixture.configure_at(&web, "true", &[".env"]);
    let entry = fixture.create_at(&web);
    fixture.setup(&entry).unwrap();
    std::fs::write(
        Path::new(&entry.path).join("apps/api/local.sqlite"),
        "valuable sibling data",
    )
    .unwrap();

    let reason = environment::check_cleanup(&fixture.conn, &entry)
        .unwrap()
        .unwrap();
    assert!(reason.contains("apps/api/local.sqlite"), "{reason}");
}

#[test]
fn unknown_ignored_database_still_blocks_retirement_with_approved_generated_folders() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    std::fs::write(Path::new(&entry.path).join("local.sqlite"), "local data").unwrap();
    let plan = fixture.plan(&entry);
    assert!(plan.entries.is_empty());
    assert_eq!(plan.kept.len(), 1);
    assert!(plan.kept[0].reason.contains("local.sqlite"));
    assert!(Path::new(&entry.path).is_dir());
}

#[test]
fn delayed_setup_rejects_retired_or_replaced_checkout_paths() {
    let fixture = EnvironmentFixture::new();
    let entry = fixture.create();
    fixture.setup(&entry).unwrap();
    setup::validate_checkout(&entry, &entry.path).unwrap();
    let plan = fixture.plan(&entry);
    let report = fixture.retire(&plan, &entry);
    assert!(report.results[0].worktree_removed);
    let retired = owned(&fixture.conn).unwrap().remove(0);
    assert!(setup::validate_checkout(&retired, &retired.path).is_err());
    std::fs::create_dir_all(&entry.path).unwrap();
    std::fs::write(Path::new(&entry.path).join("keep.txt"), "unrelated data").unwrap();
    assert!(setup::validate_checkout(&entry, &entry.path).is_err());
    assert!(!Path::new(&entry.path).join(".env").exists());
}
