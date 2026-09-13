//! Content-addressed storage for immutable worktree recovery archives.
//!
//! Archive identity and lifecycle metadata remain in
//! `worktree_environment_archives`. This module owns the manifest-to-blob
//! mapping, its migration from inline payloads, quota accounting, and safe
//! garbage collection.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;
const DEFAULT_LIMIT_BYTES: u64 = 64 * MIB;
const MIN_LIMIT_BYTES: u64 = MIB;
const MAX_LIMIT_BYTES: u64 = 4096 * MIB;
const MAX_FILE_BYTES: u64 = MIB;
const MAX_ARCHIVE_BYTES: u64 = 4 * MIB;
const MAX_ARCHIVE_FILES: usize = 64;

pub(super) const STORAGE_FULL: &str =
    "Recovery storage is full. Increase the limit in Settings, then retry. The worktree was kept.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArchiveFile {
    pub(super) path: String,
    pub(super) contents: Option<Vec<u8>>,
    pub(super) unix_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArchiveManifest {
    pub(super) archive_id: String,
    pub(super) plan_id: String,
    pub(super) worktree_id: String,
    pub(super) common_dir: String,
    pub(super) created_at: i64,
    pub(super) project_path: String,
    pub(super) project_cwd: String,
    pub(super) settings_json: String,
    pub(super) files: Vec<ArchiveFile>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryStorageUsage {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub version: i64,
    pub projects: Vec<RecoveryStorageProject>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryStorageProject {
    pub project_cwd: String,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MaintenanceReport {
    pub(super) reclaimed_bytes: u64,
}

/// Create the content-addressed schema and migrate legacy inline payloads.
///
/// SQLite DDL is transactional. The legacy table is dropped only after every
/// row can be reconstructed and compared inside the same immediate
/// transaction, so a failed or interrupted migration leaves the old archive
/// usable on the next launch.
pub(super) fn schema(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS worktree_environment_blobs (
            content_hash TEXT PRIMARY KEY,
            contents BLOB NOT NULL,
            byte_length INTEGER NOT NULL,
            CHECK(length(content_hash) = 64),
            CHECK(byte_length >= 0),
            CHECK(length(contents) = byte_length)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS worktree_environment_archive_files (
            archive_id TEXT NOT NULL,
            path TEXT NOT NULL,
            present INTEGER NOT NULL,
            content_hash TEXT,
            unix_mode INTEGER,
            PRIMARY KEY (archive_id, path),
            FOREIGN KEY (archive_id) REFERENCES worktree_environment_archives(archive_id)
              ON DELETE CASCADE,
            FOREIGN KEY (content_hash) REFERENCES worktree_environment_blobs(content_hash),
            CHECK(present IN (0, 1)),
            CHECK(
              (present = 0 AND content_hash IS NULL AND unix_mode IS NULL) OR
              (present = 1 AND content_hash IS NOT NULL)
            )
         );
         CREATE INDEX IF NOT EXISTS worktree_environment_archive_file_blobs
           ON worktree_environment_archive_files(content_hash);
         CREATE TABLE IF NOT EXISTS worktree_storage_settings (
            singleton_id INTEGER PRIMARY KEY CHECK(singleton_id = 1),
            limit_bytes INTEGER NOT NULL,
            version INTEGER NOT NULL,
            CHECK(limit_bytes >= 1048576 AND limit_bytes <= 4294967296),
            CHECK(limit_bytes % 1048576 = 0),
            CHECK(version >= 1)
         );",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO worktree_storage_settings
           (singleton_id, limit_bytes, version) VALUES (1, ?1, 1)",
        [DEFAULT_LIMIT_BYTES as i64],
    )?;
    ensure_archive_columns(&tx)?;
    tx.execute(
        "UPDATE worktree_environment_archives
            SET archive_order = rowid
          WHERE archive_order IS NULL",
        [],
    )?;
    tx.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS worktree_environment_archive_order
           ON worktree_environment_archives(archive_order)
          WHERE archive_order IS NOT NULL",
        [],
    )?;
    populate_legacy_project_cwds(&tx)?;
    migrate_legacy_files(&tx)?;
    tx.commit()
}

fn ensure_archive_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (column, declaration) in [("project_cwd", "TEXT"), ("archive_order", "INTEGER")] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM pragma_table_info('worktree_environment_archives')
                WHERE name = ?1
             )",
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute(
                &format!(
                    "ALTER TABLE worktree_environment_archives ADD COLUMN {column} {declaration}"
                ),
                [],
            )?;
        }
    }
    Ok(())
}

fn populate_legacy_project_cwds(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "worktree_retirement_items")? {
        return Ok(());
    }
    conn.execute(
        "UPDATE worktree_environment_archives AS archive
            SET project_cwd = (
              SELECT CASE
                       WHEN archive.project_path = '' THEN item.repo
                       ELSE rtrim(item.repo, '/\\') || '/' || archive.project_path
                     END
                FROM worktree_retirement_items item
               WHERE item.plan_id = archive.plan_id
                 AND item.worktree_id = archive.worktree_id
            )
          WHERE project_cwd IS NULL OR project_cwd = ''",
        [],
    )?;
    Ok(())
}

fn migrate_legacy_files(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "worktree_environment_files")? {
        return Ok(());
    }

    let legacy = read_legacy_files(conn)?;
    let mut archives = BTreeMap::<&str, Vec<&ArchiveFile>>::new();
    for row in &legacy {
        archives.entry(&row.archive_id).or_default().push(&row.file);
    }
    for files in archives.values() {
        validate_file_set(files.iter().copied(), |message| message).map_err(migration_error)?;
    }

    for row in &legacy {
        let archive_exists: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM worktree_environment_archives WHERE archive_id = ?1
             )",
            [&row.archive_id],
            |result| result.get(0),
        )?;
        if !archive_exists {
            return Err(migration_error(format!(
                "Legacy recovery file '{}' has no archive metadata",
                row.path()
            )));
        }
        insert_file(conn, &row.archive_id, &row.file).map_err(migration_error)?;
    }

    // Compare fully reconstructed values, including blob bytes rather than
    // trusting only the stored digest.
    let reconstructed = read_all_manifest_files(conn)?;
    for row in &legacy {
        let Some(saved) = reconstructed
            .iter()
            .find(|saved| saved.archive_id == row.archive_id && saved.file.path == row.file.path)
        else {
            return Err(migration_error(format!(
                "Migrated recovery file '{}' is missing",
                row.path()
            )));
        };
        if saved.file != row.file {
            return Err(migration_error(format!(
                "Migrated recovery file '{}' did not verify",
                row.path()
            )));
        }
    }

    #[cfg(test)]
    if table_exists_in_schema(conn, "temp", "worktree_storage_fail_migration")? {
        return Err(migration_error(
            "Injected recovery storage migration failure",
        ));
    }

    conn.execute("DROP TABLE worktree_environment_files", [])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveFileRow {
    archive_id: String,
    file: ArchiveFile,
}

impl ArchiveFileRow {
    fn path(&self) -> &str {
        &self.file.path
    }
}

fn read_legacy_files(conn: &Connection) -> rusqlite::Result<Vec<ArchiveFileRow>> {
    let mut statement = conn.prepare(
        "SELECT archive_id, path, present, contents, unix_mode
           FROM worktree_environment_files
          ORDER BY archive_id, path",
    )?;
    let rows = statement.query_map([], |row| {
        let archive_id: String = row.get(0)?;
        let path: String = row.get(1)?;
        let present: i64 = row.get(2)?;
        let contents: Option<Vec<u8>> = row.get(3)?;
        let unix_mode: Option<i64> = row.get(4)?;
        let file = decode_file(path, present, contents, unix_mode).map_err(migration_error)?;
        Ok(ArchiveFileRow { archive_id, file })
    })?;
    rows.collect()
}

fn read_all_manifest_files(conn: &Connection) -> rusqlite::Result<Vec<ArchiveFileRow>> {
    let mut statement = conn.prepare(
        "SELECT files.archive_id, files.path, files.present, blobs.contents,
                files.unix_mode, files.content_hash, blobs.content_hash,
                blobs.byte_length
           FROM worktree_environment_archive_files files
           LEFT JOIN worktree_environment_blobs blobs
             ON blobs.content_hash = files.content_hash
          ORDER BY files.archive_id, files.path",
    )?;
    let rows = statement.query_map([], decode_manifest_row)?;
    rows.collect()
}

fn decode_manifest_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveFileRow> {
    let archive_id: String = row.get(0)?;
    let path: String = row.get(1)?;
    let present: i64 = row.get(2)?;
    let contents: Option<Vec<u8>> = row.get(3)?;
    let unix_mode: Option<i64> = row.get(4)?;
    let manifest_hash: Option<String> = row.get(5)?;
    let blob_hash: Option<String> = row.get(6)?;
    let byte_length: Option<i64> = row.get(7)?;

    match present {
        0 if manifest_hash.is_none()
            && blob_hash.is_none()
            && contents.is_none()
            && unix_mode.is_none()
            && byte_length.is_none() => {}
        1 => {
            let expected = manifest_hash
                .as_deref()
                .ok_or_else(|| migration_error("Present recovery file has no blob hash"))?;
            let stored = blob_hash
                .as_deref()
                .ok_or_else(|| migration_error("Recovery archive refers to a missing blob"))?;
            let bytes = contents
                .as_deref()
                .ok_or_else(|| migration_error("Recovery archive blob has no contents"))?;
            let length = byte_length
                .ok_or_else(|| migration_error("Recovery archive blob has no length"))?;
            if expected != stored
                || length < 0
                || length as usize != bytes.len()
                || content_hash(bytes) != expected
            {
                return Err(migration_error("Recovery archive blob failed verification"));
            }
        }
        0 => {
            return Err(migration_error(
                "Recovery archive tombstone is inconsistent",
            ))
        }
        _ => {
            return Err(migration_error(
                "Recovery archive presence marker is invalid",
            ))
        }
    }

    let file = decode_file(path, present, contents, unix_mode).map_err(migration_error)?;
    Ok(ArchiveFileRow { archive_id, file })
}

fn decode_file(
    path: String,
    present: i64,
    contents: Option<Vec<u8>>,
    unix_mode: Option<i64>,
) -> Result<ArchiveFile, String> {
    if path.is_empty() || path.contains('\0') {
        return Err("Recovery archive contains an invalid path".into());
    }
    let unix_mode = match unix_mode {
        Some(mode) => Some(
            u32::try_from(mode)
                .map_err(|_| "Recovery archive contains an invalid file mode".to_string())?,
        ),
        None => None,
    };
    match (present, contents, unix_mode) {
        (1, Some(contents), unix_mode) => Ok(ArchiveFile {
            path,
            contents: Some(contents),
            unix_mode,
        }),
        (0, None, None) => Ok(ArchiveFile {
            path,
            contents: None,
            unix_mode: None,
        }),
        (0, _, _) => Err("Recovery archive tombstone is inconsistent".into()),
        _ => Err("Recovery archive presence marker is inconsistent".into()),
    }
}

fn migration_error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}

pub(super) fn store_archive(conn: &Connection, archive: &ArchiveManifest) -> Result<(), String> {
    validate_archive(archive)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;

    if let Some(existing) = existing_archive(&tx, &archive.archive_id)? {
        if existing.plan_id != archive.plan_id
            || existing.worktree_id != archive.worktree_id
            || existing.common_dir != archive.common_dir
            || existing.project_path != archive.project_path
            || existing.project_cwd != archive.project_cwd
            || existing.settings_json != archive.settings_json
            || load_archive_from(&tx, &archive.archive_id)? != archive.files
        {
            return Err("Saved worktree environment archive conflicts with this review".into());
        }
        tx.commit().map_err(|error| error.to_string())?;
        return Ok(());
    }

    let (limit_bytes, _) = read_limit(&tx)?;
    let used_bytes = blob_usage(&tx)?;
    let mut unique_contents = BTreeMap::<String, &[u8]>::new();
    for file in &archive.files {
        if let Some(contents) = file.contents.as_deref() {
            unique_contents
                .entry(content_hash(contents))
                .or_insert(contents);
        }
    }
    let mut new_bytes = 0_u64;
    for (hash, contents) in &unique_contents {
        match read_blob(&tx, hash)? {
            Some(existing) if existing == *contents => {}
            Some(_) => return Err("Recovery storage hash collision detected".into()),
            None => {
                new_bytes = new_bytes
                    .checked_add(contents.len() as u64)
                    .ok_or("Recovery storage size overflow")?;
            }
        }
    }
    if new_bytes > 0
        && used_bytes
            .checked_add(new_bytes)
            .is_none_or(|total| total > limit_bytes)
    {
        return Err(STORAGE_FULL.into());
    }

    tx.execute(
        "INSERT INTO worktree_environment_archives
           (archive_id, plan_id, worktree_id, common_dir, created_at,
            project_path, project_cwd, settings_json, archive_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                 (SELECT COALESCE(MAX(archive_order), 0) + 1
                    FROM worktree_environment_archives))",
        params![
            archive.archive_id,
            archive.plan_id,
            archive.worktree_id,
            archive.common_dir,
            archive.created_at,
            archive.project_path,
            archive.project_cwd,
            archive.settings_json,
        ],
    )
    .map_err(|error| error.to_string())?;
    for file in &archive.files {
        insert_file(&tx, &archive.archive_id, file)?;
    }
    tx.commit().map_err(|error| error.to_string())
}

#[derive(Debug)]
struct ExistingArchive {
    plan_id: String,
    worktree_id: String,
    common_dir: String,
    project_path: String,
    project_cwd: String,
    settings_json: String,
}

fn existing_archive(
    conn: &Connection,
    archive_id: &str,
) -> Result<Option<ExistingArchive>, String> {
    conn.query_row(
        "SELECT plan_id, worktree_id, common_dir, project_path,
                COALESCE(project_cwd, ''), COALESCE(settings_json, '')
           FROM worktree_environment_archives WHERE archive_id = ?1",
        [archive_id],
        |row| {
            Ok(ExistingArchive {
                plan_id: row.get(0)?,
                worktree_id: row.get(1)?,
                common_dir: row.get(2)?,
                project_path: row.get(3)?,
                project_cwd: row.get(4)?,
                settings_json: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(|error| error.to_string())
}

fn insert_file(conn: &Connection, archive_id: &str, file: &ArchiveFile) -> Result<(), String> {
    let hash = file.contents.as_deref().map(content_hash);
    if let (Some(hash), Some(contents)) = (hash.as_deref(), file.contents.as_deref()) {
        conn.execute(
            "INSERT OR IGNORE INTO worktree_environment_blobs
               (content_hash, contents, byte_length) VALUES (?1, ?2, ?3)",
            params![hash, contents, contents.len() as i64],
        )
        .map_err(|error| error.to_string())?;
        let stored = read_blob(conn, hash)?
            .ok_or("Recovery storage blob disappeared while writing the archive")?;
        if stored != contents {
            return Err("Recovery storage hash collision detected".into());
        }
    }
    conn.execute(
        "INSERT OR IGNORE INTO worktree_environment_archive_files
           (archive_id, path, present, content_hash, unix_mode)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            archive_id,
            file.path,
            file.contents.is_some(),
            hash,
            file.unix_mode.map(i64::from),
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(super) fn load_archive(
    conn: &Connection,
    archive_id: &str,
) -> Result<Vec<ArchiveFile>, String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
        .map_err(|error| error.to_string())?;
    let files = load_archive_from(&tx, archive_id)?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(files)
}

fn load_archive_from(conn: &Connection, archive_id: &str) -> Result<Vec<ArchiveFile>, String> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM worktree_environment_archives WHERE archive_id = ?1
             )",
            [archive_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !exists {
        return Err("Saved worktree environment archive is missing".into());
    }

    let mut statement = conn
        .prepare(
            "SELECT files.archive_id, files.path, files.present, blobs.contents,
                    files.unix_mode, files.content_hash, blobs.content_hash,
                    blobs.byte_length
               FROM worktree_environment_archive_files files
               LEFT JOIN worktree_environment_blobs blobs
                 ON blobs.content_hash = files.content_hash
              WHERE files.archive_id = ?1
              ORDER BY files.path",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([archive_id], decode_manifest_row)
        .map_err(|error| error.to_string())?;
    let files = rows
        .map(|row| row.map(|row| row.file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    validate_file_set(files.iter(), |message| message)?;
    Ok(files)
}

fn validate_archive(archive: &ArchiveManifest) -> Result<(), String> {
    if archive.archive_id.is_empty()
        || archive.plan_id.is_empty()
        || archive.worktree_id.is_empty()
        || archive.common_dir.is_empty()
        || archive.project_cwd.is_empty()
        || archive.archive_id.contains(['\0', '\n', '\r'])
    {
        return Err("Recovery archive identity is invalid".into());
    }
    validate_file_set(archive.files.iter(), |message| message)
}

fn validate_file_set<'a>(
    files: impl Iterator<Item = &'a ArchiveFile>,
    error: impl Fn(String) -> String,
) -> Result<(), String> {
    let files = files.collect::<Vec<_>>();
    if files.len() > MAX_ARCHIVE_FILES {
        return Err(error(
            "Saved worktree environment archive exceeds safety limits".into(),
        ));
    }
    let mut total = 0_u64;
    let mut paths = HashSet::new();
    for file in files {
        if file.path.is_empty() || file.path.contains('\0') || !paths.insert(file.path.as_str()) {
            return Err(error(
                "Recovery archive contains an invalid or duplicate path".into(),
            ));
        }
        match file.contents.as_deref() {
            Some(contents) => {
                let length = contents.len() as u64;
                if length > MAX_FILE_BYTES {
                    return Err(error(format!(
                        "Saved copy file exceeds safety limits: {}",
                        file.path
                    )));
                }
                total = total
                    .checked_add(length)
                    .ok_or_else(|| error("Recovery archive size overflow".into()))?;
            }
            None if file.unix_mode.is_some() => {
                return Err(error("Recovery archive tombstone is inconsistent".into()));
            }
            None => {}
        }
    }
    if total > MAX_ARCHIVE_BYTES {
        return Err(error(
            "Saved worktree environment archive exceeds safety limits".into(),
        ));
    }
    Ok(())
}

fn content_hash(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))
}

fn read_blob(conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>, String> {
    conn.query_row(
        "SELECT contents FROM worktree_environment_blobs
          WHERE content_hash = ?1 AND byte_length = length(contents)",
        [hash],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| error.to_string())
}

pub(super) fn usage(conn: &Connection) -> Result<RecoveryStorageUsage, String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
        .map_err(|error| error.to_string())?;
    let result = read_usage(&tx)?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(result)
}

fn read_usage(conn: &Connection) -> Result<RecoveryStorageUsage, String> {
    validate_storage_links(conn)?;
    let used_bytes = blob_usage(conn)?;
    let (limit_bytes, version) = read_limit(conn)?;
    let mut statement = conn
        .prepare(
            "SELECT project_cwd, COALESCE(SUM(blobs.byte_length), 0)
               FROM (
                 SELECT DISTINCT
                        CASE
                          WHEN archives.project_cwd IS NOT NULL
                           AND archives.project_cwd != '' THEN archives.project_cwd
                          WHEN archives.project_path = '' THEN archives.common_dir
                          ELSE rtrim(archives.common_dir, '/\\') || '/' || archives.project_path
                        END AS project_cwd,
                        files.content_hash AS content_hash
                   FROM worktree_environment_archive_files files
                   JOIN worktree_environment_archives archives
                     ON archives.archive_id = files.archive_id
                  WHERE files.present = 1
               ) projects
               JOIN worktree_environment_blobs blobs
                 ON blobs.content_hash = projects.content_hash
              GROUP BY project_cwd
              ORDER BY project_cwd",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let value: i64 = row.get(1)?;
            if value < 0 {
                return Err(rusqlite::Error::IntegralValueOutOfRange(1, value));
            }
            Ok(RecoveryStorageProject {
                project_cwd: row.get(0)?,
                used_bytes: value as u64,
            })
        })
        .map_err(|error| error.to_string())?;
    let projects = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(RecoveryStorageUsage {
        used_bytes,
        limit_bytes,
        version,
        projects,
    })
}

fn validate_storage_links(conn: &Connection) -> Result<(), String> {
    let invalid: bool = conn
        .query_row(
            "SELECT EXISTS(
               SELECT 1
                 FROM worktree_environment_archive_files files
                 LEFT JOIN worktree_environment_blobs blobs
                   ON blobs.content_hash = files.content_hash
                WHERE files.present NOT IN (0, 1)
                   OR (files.present = 0 AND
                       (files.content_hash IS NOT NULL OR files.unix_mode IS NOT NULL))
                   OR (files.present = 1 AND
                       (files.content_hash IS NULL OR blobs.content_hash IS NULL
                        OR blobs.byte_length != length(blobs.contents)))
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if invalid {
        Err("Recovery storage is corrupted".into())
    } else {
        Ok(())
    }
}

fn blob_usage(conn: &Connection) -> Result<u64, String> {
    let value: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(byte_length), 0) FROM worktree_environment_blobs",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    u64::try_from(value).map_err(|_| "Recovery storage size is invalid".into())
}

fn read_limit(conn: &Connection) -> Result<(u64, i64), String> {
    let (limit, version): (i64, i64) = conn
        .query_row(
            "SELECT limit_bytes, version FROM worktree_storage_settings
              WHERE singleton_id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    let limit = u64::try_from(limit).map_err(|_| "Recovery storage limit is invalid")?;
    Ok((limit, version))
}

pub(super) fn set_limit(
    conn: &Connection,
    limit_bytes: u64,
    expected_version: i64,
) -> Result<RecoveryStorageUsage, String> {
    if !(MIN_LIMIT_BYTES..=MAX_LIMIT_BYTES).contains(&limit_bytes)
        || !limit_bytes.is_multiple_of(MIB)
    {
        return Err("Recovery storage limit must be a whole number from 1 to 4096 MiB".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let (_, current_version) = read_limit(&tx)?;
    if current_version != expected_version {
        return Err(
            "WORKTREE_STORAGE_CONFLICT: Recovery storage settings changed in another window".into(),
        );
    }
    let used_bytes = blob_usage(&tx)?;
    if limit_bytes < used_bytes {
        return Err(format!(
            "Recovery storage already uses {used_bytes} bytes; choose a limit at least that large"
        ));
    }
    let changed = tx
        .execute(
            "UPDATE worktree_storage_settings
                SET limit_bytes = ?1, version = version + 1
              WHERE singleton_id = 1 AND version = ?2",
            params![limit_bytes as i64, expected_version],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err(
            "WORKTREE_STORAGE_CONFLICT: Recovery storage settings changed in another window".into(),
        );
    }
    let result = read_usage(&tx)?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(result)
}

/// Remove only archives that have no durable lifecycle reference, then remove
/// blobs no remaining manifest uses. Git recovery refs are outside this
/// database-only operation and are never touched.
pub(super) fn maintenance(conn: &Connection) -> Result<MaintenanceReport, String> {
    for table in [
        "worktree_retirement_items",
        "worktree_recoveries",
        "worktree_environment_setup",
        "managed_worktrees",
    ] {
        if !table_exists(conn, table).map_err(|error| error.to_string())? {
            return Err(format!(
                "Recovery storage maintenance cannot verify references: missing {table}"
            ));
        }
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM worktree_environment_archive_files
          WHERE archive_id IN (
            SELECT archive.archive_id
              FROM worktree_environment_archives archive
             WHERE NOT EXISTS (
                     SELECT 1 FROM worktree_retirement_items item
                      WHERE item.plan_id = archive.plan_id
                        AND item.worktree_id = archive.worktree_id
                   )
               AND NOT EXISTS (
                     SELECT 1 FROM worktree_recoveries recovery
                      WHERE recovery.plan_id = archive.plan_id
                        AND recovery.worktree_id = archive.worktree_id
                   )
               AND NOT EXISTS (
                     SELECT 1 FROM worktree_environment_setup setup
                      WHERE setup.archive_id = archive.archive_id
                   )
               AND NOT EXISTS (
                     SELECT 1 FROM managed_worktrees managed
                      WHERE managed.id = archive.worktree_id
                        AND (managed.active_retirement_plan_id = archive.plan_id
                             OR managed.pending_retirement_plan_id = archive.plan_id)
                   )
          )",
        [],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM worktree_environment_archives AS archive
         WHERE NOT EXISTS (
                 SELECT 1 FROM worktree_retirement_items item
                  WHERE item.plan_id = archive.plan_id
                    AND item.worktree_id = archive.worktree_id
               )
           AND NOT EXISTS (
                 SELECT 1 FROM worktree_recoveries recovery
                  WHERE recovery.plan_id = archive.plan_id
                    AND recovery.worktree_id = archive.worktree_id
               )
           AND NOT EXISTS (
                 SELECT 1 FROM worktree_environment_setup setup
                  WHERE setup.archive_id = archive.archive_id
               )
           AND NOT EXISTS (
                 SELECT 1 FROM managed_worktrees managed
                  WHERE managed.id = archive.worktree_id
                    AND (managed.active_retirement_plan_id = archive.plan_id
                         OR managed.pending_retirement_plan_id = archive.plan_id)
               )",
        [],
    )
    .map_err(|error| error.to_string())?;
    let reclaimed: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(byte_length), 0)
               FROM worktree_environment_blobs blob
              WHERE NOT EXISTS (
                    SELECT 1 FROM worktree_environment_archive_files files
                     WHERE files.content_hash = blob.content_hash
                  )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM worktree_environment_blobs AS blob
          WHERE NOT EXISTS (
                SELECT 1 FROM worktree_environment_archive_files files
                 WHERE files.content_hash = blob.content_hash
              )",
        [],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(MaintenanceReport {
        reclaimed_bytes: u64::try_from(reclaimed)
            .map_err(|_| "Recovery storage reclaimed byte count is invalid")?,
    })
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    table_exists_in_schema(conn, "main", table)
}

fn table_exists_in_schema(conn: &Connection, schema: &str, table: &str) -> rusqlite::Result<bool> {
    // `schema` is always one of the two constants supplied above.
    let query = format!(
        "SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_schema WHERE type = 'table' AND name = ?1)"
    );
    conn.query_row(&query, [table], |row| row.get(0))
}

#[cfg(test)]
pub(super) fn inject_migration_failure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TEMP TABLE worktree_storage_fail_migration (enabled INTEGER)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_schema(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE managed_worktrees (
               id TEXT PRIMARY KEY,
               active_retirement_plan_id TEXT,
               pending_retirement_plan_id TEXT
             );
             CREATE TABLE worktree_retirement_items (
               plan_id TEXT NOT NULL, worktree_id TEXT NOT NULL, repo TEXT NOT NULL,
               PRIMARY KEY(plan_id, worktree_id)
             );
             CREATE TABLE worktree_recoveries (
               plan_id TEXT NOT NULL, worktree_id TEXT NOT NULL, kind TEXT NOT NULL,
               PRIMARY KEY(plan_id, worktree_id, kind)
             );
             CREATE TABLE worktree_environment_archives (
               archive_id TEXT PRIMARY KEY, plan_id TEXT NOT NULL,
               worktree_id TEXT NOT NULL, common_dir TEXT NOT NULL,
               created_at INTEGER NOT NULL, project_path TEXT NOT NULL DEFAULT '',
               settings_json TEXT
             );
             CREATE TABLE worktree_environment_setup (
               worktree_id TEXT PRIMARY KEY, archive_id TEXT
             );",
        )
        .unwrap();
        schema(conn).unwrap();
    }

    fn archive(id: &str, project: &str, bytes: &[u8]) -> ArchiveManifest {
        ArchiveManifest {
            archive_id: id.into(),
            plan_id: format!("plan-{id}"),
            worktree_id: format!("worktree-{id}"),
            common_dir: "/repo/.git".into(),
            created_at: 1,
            project_path: String::new(),
            project_cwd: project.into(),
            settings_json: "{}".into(),
            files: vec![
                ArchiveFile {
                    path: ".env".into(),
                    contents: Some(bytes.to_vec()),
                    unix_mode: Some(0o600),
                },
                ArchiveFile {
                    path: ".env.missing".into(),
                    contents: None,
                    unix_mode: None,
                },
            ],
        }
    }

    #[test]
    fn identical_payloads_are_shared_while_manifests_remain_distinct() {
        let conn = Connection::open_in_memory().unwrap();
        base_schema(&conn);
        let bytes = b"the same secret";
        store_archive(&conn, &archive("one", "/repo", bytes)).unwrap();
        store_archive(&conn, &archive("two", "/repo", bytes)).unwrap();
        assert_eq!(blob_usage(&conn).unwrap(), bytes.len() as u64);
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM worktree_environment_blobs),
                   (SELECT COUNT(*) FROM worktree_environment_archive_files)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 4));
        assert_eq!(
            load_archive(&conn, "one").unwrap(),
            archive("one", "/repo", bytes).files
        );
    }

    #[test]
    fn quota_counts_only_new_unique_bytes_and_limit_uses_cas() {
        let conn = Connection::open_in_memory().unwrap();
        base_schema(&conn);
        let full = vec![7; MIB as usize];
        let initial = usage(&conn).unwrap();
        set_limit(&conn, MIB, initial.version).unwrap();
        store_archive(&conn, &archive("one", "/repo", &full)).unwrap();
        store_archive(&conn, &archive("two", "/repo", &full)).unwrap();
        let error = store_archive(&conn, &archive("three", "/repo", b"new")).unwrap_err();
        assert_eq!(error, STORAGE_FULL);
        assert!(set_limit(&conn, 2 * MIB, initial.version)
            .unwrap_err()
            .starts_with("WORKTREE_STORAGE_CONFLICT"));
        let current = usage(&conn).unwrap();
        assert!(set_limit(&conn, 2 * MIB, current.version).is_ok());
        let current = usage(&conn).unwrap();
        assert!(set_limit(&conn, MIB - 1, current.version).is_err());
    }

    #[test]
    fn legacy_migration_rolls_back_before_clearing_inline_bytes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE worktree_environment_archives (
               archive_id TEXT PRIMARY KEY, plan_id TEXT NOT NULL,
               worktree_id TEXT NOT NULL, common_dir TEXT NOT NULL,
               created_at INTEGER NOT NULL, project_path TEXT NOT NULL DEFAULT '',
               settings_json TEXT
             );
             INSERT INTO worktree_environment_archives VALUES
               ('archive', 'plan', 'worktree', '/repo/.git', 1, '', '{}');
             CREATE TABLE worktree_environment_files (
               archive_id TEXT NOT NULL, path TEXT NOT NULL, present INTEGER NOT NULL,
               contents BLOB, unix_mode INTEGER,
               PRIMARY KEY(archive_id, path)
             );
             INSERT INTO worktree_environment_files VALUES
               ('archive', '.env', 1, X'736563726574', 384);",
        )
        .unwrap();
        inject_migration_failure(&conn).unwrap();
        assert!(schema(&conn).is_err());
        let legacy: Vec<u8> = conn
            .query_row(
                "SELECT contents FROM worktree_environment_files",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy, b"secret");
        assert!(!table_exists(&conn, "worktree_environment_blobs").unwrap());
    }

    #[test]
    fn migration_validates_limits_per_archive_instead_of_across_the_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE worktree_environment_archives (
               archive_id TEXT PRIMARY KEY, plan_id TEXT NOT NULL,
               worktree_id TEXT NOT NULL, common_dir TEXT NOT NULL,
               created_at INTEGER NOT NULL, project_path TEXT NOT NULL DEFAULT '',
               settings_json TEXT
             );
             INSERT INTO worktree_environment_archives VALUES
               ('a', 'plan-a', 'worktree-a', '/repo/.git', 1, '', '{}'),
               ('b', 'plan-b', 'worktree-b', '/repo/.git', 2, '', '{}');
             CREATE TABLE worktree_environment_files (
               archive_id TEXT NOT NULL, path TEXT NOT NULL, present INTEGER NOT NULL,
               contents BLOB, unix_mode INTEGER,
               PRIMARY KEY(archive_id, path)
             );",
        )
        .unwrap();
        for archive_id in ["a", "b"] {
            for index in 0..40 {
                conn.execute(
                    "INSERT INTO worktree_environment_files VALUES (?1, ?2, 1, ?3, 384)",
                    params![archive_id, format!("file-{index}"), vec![index as u8]],
                )
                .unwrap();
            }
        }
        schema(&conn).unwrap();
        assert!(!table_exists(&conn, "worktree_environment_files").unwrap());
        assert_eq!(load_archive(&conn, "a").unwrap().len(), 40);
        assert_eq!(load_archive(&conn, "b").unwrap().len(), 40);
    }

    #[test]
    fn migrated_archives_can_exceed_budget_and_reuse_their_bytes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE worktree_environment_archives (
               archive_id TEXT PRIMARY KEY, plan_id TEXT NOT NULL,
               worktree_id TEXT NOT NULL, common_dir TEXT NOT NULL,
               created_at INTEGER NOT NULL, project_path TEXT NOT NULL DEFAULT '',
               settings_json TEXT
             );
             INSERT INTO worktree_environment_archives VALUES
               ('legacy', 'plan-legacy', 'worktree-legacy', '/repo/.git', 1, '', '{}');
             CREATE TABLE worktree_environment_files (
               archive_id TEXT NOT NULL, path TEXT NOT NULL, present INTEGER NOT NULL,
               contents BLOB, unix_mode INTEGER,
               PRIMARY KEY(archive_id, path)
             );
             CREATE TABLE worktree_storage_settings (
               singleton_id INTEGER PRIMARY KEY CHECK(singleton_id = 1),
               limit_bytes INTEGER NOT NULL,
               version INTEGER NOT NULL,
               CHECK(limit_bytes >= 1048576 AND limit_bytes <= 4294967296),
               CHECK(limit_bytes % 1048576 = 0),
               CHECK(version >= 1)
             );
             INSERT INTO worktree_storage_settings VALUES (1, 1048576, 1);",
        )
        .unwrap();
        let first = vec![1_u8; MIB as usize];
        let second = vec![2_u8; MIB as usize];
        conn.execute(
            "INSERT INTO worktree_environment_files VALUES
               ('legacy', 'one', 1, ?1, 384)",
            [&first],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO worktree_environment_files VALUES
               ('legacy', 'two', 1, ?1, 384)",
            [&second],
        )
        .unwrap();
        schema(&conn).unwrap();
        let migrated = usage(&conn).unwrap();
        assert_eq!(migrated.used_bytes, 2 * MIB);
        assert_eq!(migrated.limit_bytes, MIB);

        let duplicate = ArchiveManifest {
            archive_id: "duplicate".into(),
            plan_id: "plan-duplicate".into(),
            worktree_id: "worktree-duplicate".into(),
            common_dir: "/repo/.git".into(),
            created_at: 2,
            project_path: String::new(),
            project_cwd: "/repo".into(),
            settings_json: "{}".into(),
            files: vec![
                ArchiveFile {
                    path: "one".into(),
                    contents: Some(first),
                    unix_mode: Some(0o600),
                },
                ArchiveFile {
                    path: "two".into(),
                    contents: Some(second),
                    unix_mode: Some(0o600),
                },
            ],
        };
        store_archive(&conn, &duplicate).unwrap();
        assert_eq!(usage(&conn).unwrap().used_bytes, 2 * MIB);
        assert!(set_limit(&conn, MIB, migrated.version)
            .unwrap_err()
            .contains("already uses"));
    }

    #[test]
    fn corrupt_missing_blob_and_tombstone_are_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        base_schema(&conn);
        store_archive(&conn, &archive("one", "/repo", b"secret")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("DELETE FROM worktree_environment_blobs", [])
            .unwrap();
        assert!(load_archive(&conn, "one")
            .unwrap_err()
            .contains("missing blob"));

        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "UPDATE worktree_environment_archive_files
                SET present = 0, content_hash = NULL, unix_mode = 384
              WHERE archive_id = 'one' AND path = '.env'",
            [],
        )
        .unwrap();
        assert!(load_archive(&conn, "one")
            .unwrap_err()
            .contains("tombstone"));
    }

    #[test]
    fn maintenance_keeps_referenced_archives_and_reclaims_only_orphans() {
        let conn = Connection::open_in_memory().unwrap();
        base_schema(&conn);
        let kept = archive("kept", "/repo", b"kept");
        let orphan = archive("orphan", "/other", b"orphan");
        store_archive(&conn, &kept).unwrap();
        store_archive(&conn, &orphan).unwrap();
        conn.execute(
            "INSERT INTO worktree_retirement_items(plan_id, worktree_id, repo)
             VALUES (?1, ?2, '/repo')",
            params![kept.plan_id, kept.worktree_id],
        )
        .unwrap();
        let report = maintenance(&conn).unwrap();
        assert_eq!(report.reclaimed_bytes, 6);
        assert_eq!(load_archive(&conn, &kept.archive_id).unwrap(), kept.files);
        assert!(load_archive(&conn, &orphan.archive_id).is_err());
    }

    #[test]
    fn usage_counts_shared_content_once_globally_and_once_per_project() {
        let conn = Connection::open_in_memory().unwrap();
        base_schema(&conn);
        store_archive(&conn, &archive("one", "/repo/a", b"shared")).unwrap();
        store_archive(&conn, &archive("two", "/repo/b", b"shared")).unwrap();
        let usage = usage(&conn).unwrap();
        assert_eq!(usage.used_bytes, 6);
        assert_eq!(usage.projects.len(), 2);
        assert!(usage.projects.iter().all(|project| project.used_bytes == 6));
    }
}
