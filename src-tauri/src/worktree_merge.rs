//! Conservative evidence that a reviewed branch tip is integrated into its
//! recorded base. The target is refreshed from the base branch's configured
//! upstream when one exists; fetching never updates a checkout or tracking ref.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{git, git_output, git_remote_output, is_ancestor, resolve_commit};

const MAX_SOURCE_COMMITS: usize = 100;
const MAX_TARGET_COMMITS: usize = 200;

#[derive(Debug, Clone)]
pub(super) struct MergeEvidence {
    pub target_oid: String,
    pub integrated: bool,
}

/// Resolve the recorded base to the exact target used for deletion review.
/// A local branch with an upstream, or a remote-tracking ref, is refreshed
/// directly from that configured remote ref. No default remote is guessed.
pub(super) fn review(
    root: &Path,
    base_ref: &str,
    source_oid: &str,
) -> Result<MergeEvidence, String> {
    let target_oid = reviewed_target(root, base_ref)?;
    let integrated = is_integrated(root, source_oid, &target_oid)?;
    Ok(MergeEvidence {
        target_oid,
        integrated,
    })
}

/// Repeat target resolution and content evidence so a saved plan cannot use a
/// moved base or evidence that no longer describes the selected branch tip.
pub(super) fn recheck(
    root: &Path,
    base_ref: &str,
    source_oid: &str,
    expected_target_oid: &str,
) -> Result<(), String> {
    if expected_target_oid.is_empty() {
        return Err("No reviewed integration target is available".into());
    }
    let current_target = reviewed_target(root, base_ref)?;
    if current_target != expected_target_oid {
        return Err("Recorded base moved after review; review the worktree again".into());
    }
    if !is_integrated(root, source_oid, expected_target_oid)? {
        return Err("The exact reviewed commit is no longer confirmed integrated".into());
    }
    Ok(())
}

/// Fetch one exact remote branch without updating FETCH_HEAD, a local branch,
/// or a remote-tracking ref. The returned OID is checked against the object DB.
pub(super) fn fetch_remote_ref(
    root: &Path,
    target: &str,
    source_ref: &str,
) -> Result<String, String> {
    if target.is_empty()
        || target.starts_with('-')
        || target.contains(['\0', '\n', '\r'])
        || !source_ref.starts_with("refs/heads/")
        || source_ref.contains(['\0', '\n', '\r'])
    {
        return Err("Configured merge target is invalid".into());
    }
    let output = git_remote_output(root, &["ls-remote", "--refs", target, source_ref])?;
    if !output.status.success() {
        return Err(redacted_remote_error(&output.stderr, target));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned remote data that is not valid UTF-8")?;
    let mut matches = stdout.lines().filter_map(|line| {
        let (oid, reference) = line.split_once('\t')?;
        (reference == source_ref && valid_oid(oid)).then(|| oid.to_string())
    });
    let oid = matches
        .next()
        .ok_or_else(|| format!("Configured merge target {source_ref} is unavailable"))?;
    if matches.next().is_some() {
        return Err("Remote returned an ambiguous merge target".into());
    }

    let fetch = git_remote_output(
        root,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            target,
            source_ref,
        ],
    )?;
    if !fetch.status.success() {
        return Err(redacted_remote_error(&fetch.stderr, target));
    }
    let object = format!("{oid}^{{commit}}");
    if !git_output(root, &["cat-file", "-e", &object])?
        .status
        .success()
    {
        return Err("Git did not retain the fetched merge target commit".into());
    }
    Ok(oid)
}

pub(super) fn is_integrated(
    root: &Path,
    source_oid: &str,
    target_oid: &str,
) -> Result<bool, String> {
    if source_oid == target_oid || is_ancestor(root, source_oid, target_oid)? {
        return Ok(true);
    }

    let merge_base = unique_merge_base(root, source_oid, target_oid)?;
    if merge_tree_has_no_content_change(root, &merge_base, source_oid, target_oid)? == Some(true) {
        return Ok(true);
    }
    let source_only = bounded_commits(
        root,
        &[
            "rev-list",
            "--topo-order",
            source_oid,
            &format!("^{target_oid}"),
        ],
        MAX_SOURCE_COMMITS,
    )?;
    if source_only.is_empty() {
        return Ok(false);
    }
    let target_commits = bounded_commits(
        root,
        &[
            "rev-list",
            "--topo-order",
            target_oid,
            &format!("^{merge_base}"),
        ],
        MAX_TARGET_COMMITS,
    )?;

    if rebased_commits_are_present(root, &source_only, &target_commits)? {
        return Ok(true);
    }
    squash_patch_is_present(root, &merge_base, source_oid, &target_commits)
}

fn reviewed_target(root: &Path, base_ref: &str) -> Result<String, String> {
    let local_oid = resolve_commit(root, base_ref)
        .map_err(|_| format!("Recorded base ref {base_ref} is unavailable"))?;
    let canonical = git(
        root,
        &[
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--end-of-options",
            base_ref,
        ],
    )?;
    if let Some(branch) = canonical.strip_prefix("refs/heads/") {
        if let Some((remote, remote_ref)) = local_branch_upstream(root, branch)? {
            let target = remote_fetch_url(root, &remote)?;
            return fetch_remote_ref(root, &target, &remote_ref);
        }
    } else if canonical.starts_with("refs/remotes/") {
        if let Some((remote, remote_ref)) = remote_tracking_source(root, &canonical)? {
            let target = remote_fetch_url(root, &remote)?;
            return fetch_remote_ref(root, &target, &remote_ref);
        }
        return Err("Recorded remote-tracking base has no unique configured source".into());
    }
    Ok(local_oid)
}

fn local_branch_upstream(root: &Path, branch: &str) -> Result<Option<(String, String)>, String> {
    let reference = format!("refs/heads/{branch}");
    let format = "%(upstream:remotename)%00%(upstream:remoteref)";
    let value = git(
        root,
        &["for-each-ref", &format!("--format={format}"), &reference],
    )?;
    if value.is_empty() {
        return Ok(None);
    }
    let mut fields = value.split('\0');
    let remote = fields.next().unwrap_or_default();
    let remote_ref = fields.next().unwrap_or_default();
    if fields.next().is_some() || remote.is_empty() || remote_ref.is_empty() || remote == "." {
        return Ok(None);
    }
    if !remote_ref.starts_with("refs/heads/") {
        return Err("Recorded base upstream is not a branch".into());
    }
    Ok(Some((remote.into(), remote_ref.into())))
}

fn remote_fetch_url(root: &Path, remote: &str) -> Result<String, String> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains(['\0', '\n', '\r']) {
        return Err("Configured base remote is invalid".into());
    }
    let output = git_output(root, &["remote", "get-url", "--all", remote])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned remote data that is not valid UTF-8")?;
    let urls: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    if urls.len() != 1 {
        return Err("Base remote must have exactly one fetch destination".into());
    }
    Ok(urls[0].into())
}

fn remote_tracking_source(
    root: &Path,
    tracking_ref: &str,
) -> Result<Option<(String, String)>, String> {
    let remotes = git(root, &["remote"])?;
    let mut matches = HashSet::new();
    for remote in remotes.lines().filter(|remote| !remote.is_empty()) {
        let key = format!("remote.{remote}.fetch");
        let output = git_output(root, &["config", "--get-all", &key])?;
        if !output.status.success() {
            if output.status.code() == Some(1) {
                continue;
            }
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let raw = String::from_utf8(output.stdout)
            .map_err(|_| "Git returned configuration that is not valid UTF-8")?;
        let specs: Vec<&str> = raw.lines().filter(|line| !line.is_empty()).collect();
        for spec in specs.iter().filter(|spec| !spec.starts_with('^')) {
            if let Some(source) = invert_refspec(spec, tracking_ref) {
                if source.starts_with("refs/heads/")
                    && !specs
                        .iter()
                        .filter_map(|negative| negative.strip_prefix('^'))
                        .any(|negative| refspec_matches(negative, &source))
                {
                    matches.insert((remote.to_string(), source));
                }
            }
        }
    }
    if matches.len() == 1 {
        Ok(matches.into_iter().next())
    } else {
        Ok(None)
    }
}

fn invert_refspec(spec: &str, destination: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (source, target) = spec.split_once(':')?;
    if target == destination {
        return Some(source.into());
    }
    let (target_prefix, target_suffix) = single_wildcard(target)?;
    let middle = destination
        .strip_prefix(target_prefix)?
        .strip_suffix(target_suffix)?;
    let (source_prefix, source_suffix) = single_wildcard(source)?;
    Some(format!("{source_prefix}{middle}{source_suffix}"))
}

fn refspec_matches(spec: &str, reference: &str) -> bool {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let source = spec
        .split_once(':')
        .map(|(source, _)| source)
        .unwrap_or(spec);
    if source == reference {
        return true;
    }
    single_wildcard(source).is_some_and(|(prefix, suffix)| {
        reference.starts_with(prefix) && reference.ends_with(suffix)
    })
}

fn single_wildcard(value: &str) -> Option<(&str, &str)> {
    let (prefix, suffix) = value.split_once('*')?;
    (!suffix.contains('*')).then_some((prefix, suffix))
}

fn unique_merge_base(root: &Path, source_oid: &str, target_oid: &str) -> Result<String, String> {
    let output = git(root, &["merge-base", "--all", source_oid, target_oid])?;
    let bases: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
    if bases.len() != 1 {
        return Err("The reviewed commits do not have one unambiguous merge base".into());
    }
    Ok(bases[0].into())
}

fn bounded_commits(root: &Path, args: &[&str], maximum: usize) -> Result<Vec<String>, String> {
    let mut owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    owned.insert(1, format!("--max-count={}", maximum + 1));
    let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
    let output = git(root, &borrowed)?;
    let commits: Vec<String> = output
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if commits.len() > maximum {
        return Err(format!(
            "Merge evidence exceeds the conservative {maximum}-commit inspection limit"
        ));
    }
    Ok(commits)
}

fn rebased_commits_are_present(
    root: &Path,
    source_commits: &[String],
    target_commits: &[String],
) -> Result<bool, String> {
    let mut target_diffs: HashMap<Vec<u8>, usize> = HashMap::new();
    for commit in target_commits {
        let Some(parent) = single_parent(root, commit)? else {
            continue;
        };
        if let Some(diff) = exact_diff_between(root, &parent, commit)? {
            *target_diffs.entry(diff).or_default() += 1;
        }
    }
    if target_diffs.is_empty() {
        return Ok(false);
    }
    for commit in source_commits {
        let Some(parent) = single_parent(root, commit)? else {
            return Ok(false);
        };
        let Some(diff) = exact_diff_between(root, &parent, commit)? else {
            return Ok(false);
        };
        let Some(remaining) = target_diffs.get_mut(&diff) else {
            return Ok(false);
        };
        if *remaining == 0 {
            return Ok(false);
        }
        *remaining -= 1;
    }
    Ok(true)
}

fn squash_patch_is_present(
    root: &Path,
    merge_base: &str,
    source_oid: &str,
    target_commits: &[String],
) -> Result<bool, String> {
    let Some(source_diff) = exact_diff_between(root, merge_base, source_oid)? else {
        return Ok(false);
    };
    for commit in target_commits {
        let Some(parent) = single_parent(root, commit)? else {
            continue;
        };
        if exact_diff_between(root, &parent, commit)?.as_ref() == Some(&source_diff) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn single_parent(root: &Path, commit: &str) -> Result<Option<String>, String> {
    let value = git(root, &["rev-list", "--parents", "-n", "1", commit])?;
    let fields: Vec<&str> = value.split_whitespace().collect();
    Ok((fields.len() == 2).then(|| fields[1].to_string()))
}

fn merge_tree_has_no_content_change(
    root: &Path,
    merge_base: &str,
    source_oid: &str,
    target_oid: &str,
) -> Result<Option<bool>, String> {
    let output = git_output(
        root,
        &[
            "merge-tree",
            "--write-tree",
            "--no-messages",
            "--merge-base",
            merge_base,
            target_oid,
            source_oid,
        ],
    )?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Some(false));
        }
        // `merge-tree --write-tree` is unavailable on older Git versions.
        // Exact bounded diff comparison below remains a safe fallback.
        if output.status.code() == Some(129) {
            return Ok(None);
        }
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "Git returned merge content evidence that is not valid UTF-8")?;
    let mut fields = stdout.split_whitespace();
    let Some(merged_tree) = fields.next().filter(|value| valid_oid(value)) else {
        return Err("Git did not return a merge result tree".into());
    };
    let target_tree = git(root, &["rev-parse", &format!("{target_oid}^{{tree}}")])?;
    Ok(Some(merged_tree == target_tree))
}

fn exact_diff_between(root: &Path, from: &str, to: &str) -> Result<Option<Vec<u8>>, String> {
    let output = git_output(
        root,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            from,
            to,
            "--",
        ],
    )?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.is_empty() {
        Ok(None)
    } else {
        Ok(Some(output.stdout))
    }
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn redacted_remote_error(stderr: &[u8], target: &str) -> String {
    let safe = super::safe_remote_destination(target);
    let error = String::from_utf8_lossy(stderr)
        .trim()
        .replace(target, &safe);
    if error.is_empty() {
        format!("Could not contact remote {safe}")
    } else {
        error
    }
}
