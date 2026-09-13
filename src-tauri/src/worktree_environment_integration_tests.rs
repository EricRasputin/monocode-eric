//! Exercise the environment through the same create/review/retire/open seams
//! used by the native commands, with real Git and an on-disk database.
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
        match environment::begin_setup(&self.conn, requested_path)? {
            environment::BeginSetup::Skip => Ok(()),
            environment::BeginSetup::Run(operation) => {
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(&self.conn, &operation, &result)?;
                result
            }
        }
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
