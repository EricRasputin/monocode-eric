//! Real-Git regression coverage for the recovery-storage boundary.
//!
//! These tests intentionally use the same create, review, retire, reopen, and
//! setup seams as the native commands. Storage-only assertions go through the
//! storage API, except for physical blob counts and deliberate orphan creation.

use super::tests::create;
use super::*;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

const MIB: u64 = 1024 * 1024;
static STORAGE_TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct StorageFixture {
    dir: PathBuf,
    repo: PathBuf,
    database: PathBuf,
    conn: Option<Connection>,
    host: WorktreeHost,
}

impl StorageFixture {
    fn new(copy_paths: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "monocode-storage-integration-{}-{}-{}",
            std::process::id(),
            now(),
            STORAGE_TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        let repo = dir.join("project with spaces");
        let database = dir.join("state.sqlite");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]).unwrap();
        git(&repo, &["config", "user.name", "Storage Test"]).unwrap();
        git(&repo, &["config", "user.email", "storage@example.invalid"]).unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        git(
            &repo,
            &[
                "config",
                "core.hooksPath",
                &path_to_js(&dir.join("no hooks")),
            ],
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n.env.local\n").unwrap();
        std::fs::write(repo.join("code.txt"), "initial code\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "Initial"]).unwrap();
        std::fs::write(repo.join(".env"), b"source configuration\n").unwrap();

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                worktree_cwd TEXT,
                branch TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0
             )",
        )
        .unwrap();
        let scope = environment::scope_for_cwd(&path_to_js(&repo)).unwrap();
        environment::save_settings(
            &conn,
            &scope,
            &environment::EnvironmentSettings {
                environment_version: 0,
                setup_command: "true".into(),
                copy_paths: copy_paths.iter().map(|path| (*path).into()).collect(),
                disposable_paths: Vec::new(),
            },
        )
        .unwrap();

        Self {
            host: WorktreeHost {
                root: dir.join("owned checkouts"),
                windows: Mutex::new(HashMap::new()),
                repositories: RepositoryReservations::default(),
                disk: disk::DiskManager::default(),
            },
            dir,
            repo,
            database,
            conn: Some(conn),
        }
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }

    fn reopen_database(&mut self) {
        drop(self.conn.take());
        let conn = Connection::open(&self.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema(&conn).unwrap();
        self.conn = Some(conn);
    }

    fn create(&self, id: &str) -> Owned {
        let entry = create(
            self.conn(),
            &self.host,
            &path_to_js(&self.repo),
            id,
            "Storage lifecycle",
            Some("main"),
        )
        .unwrap();
        self.setup(&entry);
        entry
    }

    fn current(&self, id: &str) -> Owned {
        owned(self.conn())
            .unwrap()
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap()
    }

    fn setup(&self, entry: &Owned) {
        match environment::begin_setup(self.conn(), &entry.path).unwrap() {
            environment::BeginSetup::Skip => {}
            environment::BeginSetup::Run(operation) => {
                let result = environment::run_setup(&operation, |_| {});
                environment::finish_setup(self.conn(), &operation, &result).unwrap();
                result.unwrap();
            }
        }
    }

    fn restore(&self, id: &str) -> Owned {
        let entry = self.current(id);
        open_owned(self.conn(), &entry).unwrap();
        self.setup(&entry);
        self.current(id)
    }

    fn plan(&self, entry: &Owned) -> WorktreeRetirementPlan {
        let plan = build_retirement_plan(
            self.conn(),
            &HashMap::new(),
            &[],
            Some(&path_to_js(&self.repo)),
            std::slice::from_ref(&entry.id),
        )
        .unwrap();
        assert_eq!(plan.entries.len(), 1, "{:?}", plan.kept);
        plan
    }

    fn retire(&self, plan: &WorktreeRetirementPlan, entry: &Owned) -> WorktreeRetirementResult {
        execute_retirement(
            self.conn(),
            &HashMap::new(),
            &plan.plan_id,
            &[WorktreeRetirementSelection {
                id: entry.id.clone(),
                delete_local_branch: false,
                delete_remote_branch: false,
            }],
        )
        .unwrap()
        .results
        .remove(0)
    }

    fn retire_successfully(
        &self,
        plan: &WorktreeRetirementPlan,
        entry: &Owned,
    ) -> WorktreeRetirementResult {
        let result = self.retire(plan, entry);
        assert_eq!(result.error, None);
        assert!(result.worktree_removed);
        assert!(!Path::new(&entry.path).exists());
        result
    }

    fn archive_id(&self, plan: &WorktreeRetirementPlan, entry: &Owned) -> String {
        self.conn()
            .query_row(
                "SELECT archive_id FROM worktree_environment_archives
                  WHERE plan_id = ?1 AND worktree_id = ?2",
                params![plan.plan_id, entry.id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn physical_blob_count(&self) -> i64 {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM worktree_environment_blobs",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn assert_setup_uses(&self, id: &str, plan: &WorktreeRetirementPlan) {
        let selected_plan: String = self
            .conn()
            .query_row(
                "SELECT archive.plan_id
                   FROM worktree_environment_setup setup
                   JOIN worktree_environment_archives archive
                     ON archive.archive_id = setup.archive_id
                  WHERE setup.worktree_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(selected_plan, plan.plan_id);
    }
}

impl Drop for StorageFixture {
    fn drop(&mut self) {
        drop(self.conn.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn archived_contents(conn: &Connection, archive_id: &str, path: &str) -> Option<Vec<u8>> {
    storage::load_archive(conn, archive_id)
        .unwrap()
        .into_iter()
        .find(|file| file.path == path)
        .unwrap()
        .contents
}

fn direct_archive(id: &str, project_cwd: &str, contents: Vec<u8>) -> storage::ArchiveManifest {
    storage::ArchiveManifest {
        archive_id: format!("archive-{id}"),
        plan_id: format!("plan-{id}"),
        worktree_id: format!("worktree-{id}"),
        common_dir: format!("{project_cwd}/.git"),
        created_at: now(),
        project_path: String::new(),
        project_cwd: project_cwd.into(),
        settings_json: "{}".into(),
        files: vec![storage::ArchiveFile {
            path: ".env".into(),
            contents: Some(contents),
            unix_mode: Some(0o600),
        }],
    }
}

#[test]
fn repeated_real_git_retirements_share_identical_bytes_and_restore_the_latest_archive() {
    let fixture = StorageFixture::new(&[".env"]);
    let entry = fixture.create("storage-dedup");
    let saved = b"one immutable local configuration\n";
    std::fs::write(Path::new(&entry.path).join(".env"), saved).unwrap();

    let first_plan = fixture.plan(&entry);
    fixture.retire_successfully(&first_plan, &entry);
    let first_usage = storage::usage(fixture.conn()).unwrap();
    assert_eq!(first_usage.used_bytes, saved.len() as u64);
    assert_eq!(first_usage.limit_bytes, 64 * MIB);
    assert_eq!(fixture.physical_blob_count(), 1);

    let restored = fixture.restore(&entry.id);
    assert_eq!(
        std::fs::read(Path::new(&restored.path).join(".env")).unwrap(),
        saved
    );
    let second_plan = fixture.plan(&restored);
    fixture.retire_successfully(&second_plan, &restored);

    let second_usage = storage::usage(fixture.conn()).unwrap();
    assert_eq!(second_usage.used_bytes, first_usage.used_bytes);
    assert_eq!(fixture.physical_blob_count(), 1);
    assert_eq!(second_usage.projects.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&second_usage.projects[0].project_cwd).unwrap(),
        std::fs::canonicalize(&fixture.repo).unwrap()
    );
    assert_eq!(second_usage.projects[0].used_bytes, saved.len() as u64);

    let restored = fixture.restore(&entry.id);
    fixture.assert_setup_uses(&entry.id, &second_plan);
    assert_eq!(
        std::fs::read(Path::new(&restored.path).join(".env")).unwrap(),
        saved
    );
}

#[test]
fn changed_bytes_and_absence_survive_a_database_reopen() {
    let mut fixture = StorageFixture::new(&[".env", ".env.local"]);
    let entry = fixture.create("storage-reopen");
    std::fs::write(Path::new(&entry.path).join(".env"), b"first env\n").unwrap();
    std::fs::write(Path::new(&entry.path).join(".env.local"), b"first local\n").unwrap();
    let first_plan = fixture.plan(&entry);
    fixture.retire_successfully(&first_plan, &entry);

    let restored = fixture.restore(&entry.id);
    std::fs::remove_file(Path::new(&restored.path).join(".env")).unwrap();
    let changed = b"changed\0local\xffbytes\n";
    std::fs::write(Path::new(&restored.path).join(".env.local"), changed).unwrap();
    let second_plan = fixture.plan(&restored);
    fixture.retire_successfully(&second_plan, &restored);
    let second_archive = fixture.archive_id(&second_plan, &restored);
    assert_eq!(
        archived_contents(fixture.conn(), &second_archive, ".env"),
        None
    );
    assert_eq!(
        archived_contents(fixture.conn(), &second_archive, ".env.local"),
        Some(changed.to_vec())
    );

    fixture.reopen_database();
    let restored = fixture.restore(&entry.id);
    fixture.assert_setup_uses(&entry.id, &second_plan);
    assert!(!Path::new(&restored.path).join(".env").exists());
    assert_eq!(
        std::fs::read(Path::new(&restored.path).join(".env.local")).unwrap(),
        changed
    );
}

#[test]
fn capacity_failure_is_atomic_and_the_same_retirement_can_be_retried_after_a_cas_increase() {
    let fixture = StorageFixture::new(&[".env"]);
    let entry = fixture.create("storage-capacity");
    let first = vec![b'a'; 700 * 1024];
    std::fs::write(Path::new(&entry.path).join(".env"), &first).unwrap();
    let usage = storage::usage(fixture.conn()).unwrap();
    storage::set_limit(fixture.conn(), MIB, usage.version).unwrap();
    let first_plan = fixture.plan(&entry);
    fixture.retire_successfully(&first_plan, &entry);

    let restored = fixture.restore(&entry.id);
    let root = Path::new(&restored.path);
    std::fs::write(root.join("code.txt"), "capacity retry code\n").unwrap();
    git(root, &["commit", "-am", "Capacity retry code"]).unwrap();
    let tip = resolve_commit(root, "HEAD").unwrap();
    let second = vec![b'b'; 700 * 1024];
    std::fs::write(root.join(".env"), &second).unwrap();
    let second_plan = fixture.plan(&restored);

    let rejected = fixture.retire(&second_plan, &restored);
    let error = rejected.error.unwrap();
    assert!(error.contains("limit"), "{error}");
    assert!(!rejected.worktree_removed);
    assert!(root.is_dir());
    assert_eq!(resolve_commit(root, "HEAD").unwrap(), tip);
    assert_eq!(
        std::fs::read(root.join("code.txt")).unwrap(),
        b"capacity retry code\n"
    );
    assert_eq!(std::fs::read(root.join(".env")).unwrap(), second);
    assert_eq!(fixture.physical_blob_count(), 1);
    let failed_archive: i64 = fixture
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM worktree_environment_archives
              WHERE plan_id = ?1 AND worktree_id = ?2",
            params![second_plan.plan_id, entry.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_archive, 0);

    let usage = storage::usage(fixture.conn()).unwrap();
    storage::set_limit(fixture.conn(), 2 * MIB, usage.version).unwrap();
    fixture.retire_successfully(&second_plan, &restored);
    assert_eq!(fixture.physical_blob_count(), 2);
    let restored = fixture.restore(&entry.id);
    assert_eq!(
        resolve_commit(Path::new(&restored.path), "HEAD").unwrap(),
        tip
    );
    assert_eq!(
        std::fs::read(Path::new(&restored.path).join(".env")).unwrap(),
        second
    );
}

#[test]
fn concurrent_archives_serialize_the_quota_and_dedup_still_succeeds_at_the_cap() {
    let fixture = StorageFixture::new(&[".env"]);
    let initial = storage::usage(fixture.conn()).unwrap();
    storage::set_limit(fixture.conn(), MIB, initial.version).unwrap();
    let gate = Arc::new(Barrier::new(3));
    let database = fixture.database.clone();
    let project_cwd = path_to_js(&fixture.repo);

    let contenders = [("alpha", b'a'), ("beta", b'b')]
        .into_iter()
        .map(|(id, byte)| {
            let gate = Arc::clone(&gate);
            let database = database.clone();
            let project_cwd = project_cwd.clone();
            std::thread::spawn(move || {
                let conn = Connection::open(database).unwrap();
                conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                gate.wait();
                let result = storage::store_archive(
                    &conn,
                    &direct_archive(id, &project_cwd, vec![byte; MIB as usize]),
                );
                (id, byte, result)
            })
        })
        .collect::<Vec<_>>();
    gate.wait();
    let results = contenders
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(_, _, result)| result.is_ok())
            .count(),
        1
    );
    let (_, winning_byte, _) = results
        .iter()
        .find(|(_, _, result)| result.is_ok())
        .unwrap();
    let error = results
        .iter()
        .find_map(|(_, _, result)| result.as_ref().err())
        .unwrap();
    assert_eq!(error, storage::STORAGE_FULL);
    assert_eq!(storage::usage(fixture.conn()).unwrap().used_bytes, MIB);

    storage::store_archive(
        fixture.conn(),
        &direct_archive(
            "same-at-cap",
            &project_cwd,
            vec![*winning_byte; MIB as usize],
        ),
    )
    .unwrap();
    assert_eq!(storage::usage(fixture.conn()).unwrap().used_bytes, MIB);
    assert_eq!(fixture.physical_blob_count(), 1);
}

#[test]
fn maintenance_removes_only_orphans_and_preserves_every_recovery_state() {
    let fixture = StorageFixture::new(&[".env"]);
    let entry = fixture.create("storage-maintenance");
    let historical = b"historical recovery bytes\n";
    std::fs::write(Path::new(&entry.path).join(".env"), historical).unwrap();
    let historical_plan = fixture.plan(&entry);
    fixture.retire_successfully(&historical_plan, &entry);
    let historical_archive = fixture.archive_id(&historical_plan, &entry);

    let restored = fixture.restore(&entry.id);
    let active = b"active recovery and setup bytes\n";
    std::fs::write(Path::new(&restored.path).join(".env"), active).unwrap();
    let active_plan = fixture.plan(&restored);
    fixture.retire_successfully(&active_plan, &restored);
    let active_archive = fixture.archive_id(&active_plan, &restored);

    let restored = fixture.restore(&entry.id);
    fixture.assert_setup_uses(&entry.id, &active_plan);
    let pending = b"pending retirement bytes\n";
    std::fs::write(Path::new(&restored.path).join(".env"), pending).unwrap();
    let pending_plan = fixture.plan(&restored);
    let snapshot = load_retirement_snapshot(fixture.conn(), &pending_plan.plan_id, &restored.id)
        .unwrap()
        .unwrap();
    ensure_local_recovery(fixture.conn(), &snapshot).unwrap();
    let pending_archive =
        environment::preserve(fixture.conn(), &restored, &pending_plan.plan_id).unwrap();
    begin_worktree_removal(fixture.conn(), &snapshot).unwrap();

    let orphan = b"unreferenced temporary blob";
    fixture
        .conn()
        .execute(
            "INSERT INTO worktree_environment_blobs
               (content_hash, contents, byte_length) VALUES (?1, ?2, ?3)",
            params![
                "fd0410221927fcac5928b5ce409bece5f9c685a1c82de5a860c592e26ce2603f",
                orphan,
                orphan.len() as i64
            ],
        )
        .unwrap();
    assert_eq!(fixture.physical_blob_count(), 4);

    let report = storage::maintenance(fixture.conn()).unwrap();
    assert_eq!(report.reclaimed_bytes, orphan.len() as u64);
    assert_eq!(fixture.physical_blob_count(), 3);
    for (archive, expected) in [
        (&historical_archive, historical.as_slice()),
        (&active_archive, active.as_slice()),
        (&pending_archive, pending.as_slice()),
    ] {
        assert_eq!(
            archived_contents(fixture.conn(), archive, ".env"),
            Some(expected.to_vec())
        );
    }

    fixture.retire_successfully(&pending_plan, &restored);
    let restored = fixture.restore(&entry.id);
    fixture.assert_setup_uses(&entry.id, &pending_plan);
    assert_eq!(
        std::fs::read(Path::new(&restored.path).join(".env")).unwrap(),
        pending
    );
}
