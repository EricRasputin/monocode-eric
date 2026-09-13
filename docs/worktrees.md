# Worktrees in this fork

Choose where a conversation works using the two controls above the message box:

- **New worktree · From main** — start an isolated task. Pick a different base if needed, then send your message. Monocode creates and names the branch and checkout before starting the agent. Selecting a base does not switch the source checkout.
- **Current checkout · main** — work directly in your project folder.
- **Existing worktree** — select a checkout from the workspace menu to continue work on that branch in another conversation.

New worktree is the default in this fork. Your explicit choice belongs to the draft and survives a restart; it does not change another conversation. Once the conversation starts, its checkout stays fixed. Its agent, files, changes and terminal dock use that directory. Conversations remain grouped under the original project.

A new worktree starts with committed files. In **Settings → Worktrees**, choose a project and configure its environment once:

- **Setup command** runs in the selected project's directory inside a newly created or restored worktree before the agent starts, for example `npm ci`. Setup failures keep the checkout and can be retried. Newly added copy paths are applied on retry without overwriting previously copied files that you edited. An existing checkout that has completed setup is not set up again on every message.
- **Copy local files** lists ignored local files to copy from the primary checkout, for example `.env` and `.env.local`. Each worktree owns its copy. Source-code changes and arbitrary ignored files are not copied.
- **Disposable folders** lists ignored generated directories that may be removed with the worktree, for example `node_modules` and `dist`. These entries are an explicit project policy; an ignored path is not automatically disposable.

All three are empty by default. Paths are relative to the selected project; Git internals, traversal, overlapping rules and unsafe symlinks are rejected. `repo/apps/web` and `repo/apps/api` have independent settings and run setup in their respective directories. Each newly created worktree retains its originating project's environment when another conversation shares it. Retirement still checks the entire checkout, so unknown data in a sibling folder remains protected.

Legacy repository-wide settings apply to the repository root, and pre-existing worktrees retain that environment. A nested project starts with independent defaults. If another window saves settings while you are editing, your draft remains visible and you must reload the saved settings before saving again. Dependencies are installed separately in each worktree rather than shared through a writable directory. Choose a creation base already available locally.

## Finish a task by archiving

Archive a conversation as usual. If another unarchived conversation uses the same worktree, archiving finishes without a cleanup prompt. Monocode checks stored conversations too, including those without an open tab.

When the last conversation is archived, Monocode checks whether the worktree can be retired. If it is ready, one review offers **Keep for now** or **Retire worktree**. Bulk archiving produces one combined review. Archiving is already complete: cancelling cleanup or a cleanup error does not undo the archive.

Retirement removes the working folder. Two separate, initially unchecked options can also delete its local branch and its configured remote branch. The review names each branch and shows the remote destination. Unsafe or unverifiable branch options are disabled. Other windows, live agents, terminal panes and files still protect a checkout; a blocked checkout is kept with an explanation.

A clean checkout can be retired even when its committed work is not merged: its branch and recovery reference preserve the work. Branch deletion requires separate evidence that the exact reviewed work has been integrated. A pending or failed network check does not require keeping a safely recoverable folder forever.

There are no age-based reminders or automatic removal. **Settings → Worktrees** is the manual inventory for unfinished cleanup and worktrees you kept. The project picker includes remembered and archived projects, with paths to distinguish projects with the same name. Selecting a project here does not switch the active workspace. Each project has its own list and pins, and switching projects clears an unconfirmed review.

Select ready checkouts and choose **Review retirement** to open the same review. Expand **Kept worktrees** to inspect protection reasons, pin ongoing work, or open/restore a checkout. If only some retirement steps succeed, the result distinguishes the removed checkout from branches that were kept and offers a retry.

## Returning to a task

Before any removal, Monocode saves the exact final commit under a dedicated Git recovery reference and records the checkout identity in SQLite. The completed retirement identifies both the code and configuration to restore; confirming an older review cannot replace it. This protects the saved commit from normal Git garbage collection even when both visible branches are deleted. A differing remote tip must also be preserved before remote deletion. Recovery references are retained; this feature does not expire them.

Selected local configuration is preserved separately from Git before removal, including edits made inside the worktree. It stays on this machine and is not committed or pushed. Unknown local data and uncommitted code still block retirement with the affected paths. Approved generated folders are discarded and recreated by setup.

Opening an archived conversation restores its saved checkout and configuration, runs the originating project's current setup command, and makes the conversation active again. Archived file contents and intentionally absent files take priority over today's primary checkout. New copy paths explicitly added in Settings can still be applied, including on a failed setup retry, without overwriting restored files. If the original branch name has been reused, recovery creates another branch rather than overwriting it. If the path is occupied, recovery cannot be verified, or setup fails, opening reports the problem and leaves the conversation archived. **Restore** in Settings also recreates the checkout and runs setup. Restoration does not recreate a remote branch automatically.

Retirement checks repository identity, ownership, active and pinned conversations, worktree pins, live Monocode windows/agents/terminals/setup, Git locks, branch identity, tracked changes, untracked files and ignored files. Main and external checkouts are protected. The checks run again when you confirm; removing a checkout never uses force.

Branch review refreshes the relevant remote target without checking out or pulling the primary branch. It checks ordinary ancestry and conservative content equivalence for squash or rebase merges. New work added after a merge remains protected. Uncertain integration evidence disables branch deletion while leaving checkout-only retirement available.

Remote deletion uses the exact reviewed remote reference and commit, so changes pushed after review prevent deletion. Local branch deletion also checks the reviewed commit and other Git checkouts. Git server protections still apply. These operations can fail independently; Monocode records their progress so retries preserve successful steps and recovery data.

## Recovery storage

**Settings → Worktrees → Recovery storage** shows saved configuration usage across the app and for the selected project. Identical file contents are stored once, even across projects and repeated retirements. Each retirement keeps its own immutable record of file paths, permissions and deliberately absent files, so sharing bytes does not change what a conversation restores. Project totals can overlap when their backups share contents.

The app-wide budget defaults to 64 MiB and can be changed from 1 to 4096 MiB in whole-MiB steps (labelled MB in Settings). A warning appears at 80% usage. The budget measures unique saved configuration bytes, not repository size, dependencies, Git recovery references, or the SQLite database's total size. You cannot lower it below current usage. Another window's newer setting cannot be silently overwritten.

If a retirement needs more storage than the budget allows, Monocode keeps the checkout and explains how to increase the limit and retry. A backup that adds no new bytes still fits, including when migrated backups already exceed the budget. Recovery data is never silently discarded to complete retirement.

Existing backups are converted in one transaction and their contents, presence and permissions are verified before the old storage is removed. A failed or interrupted conversion leaves the original backups intact. Background maintenance removes only archives without any durable retirement, recovery or setup reference, and blobs no archive uses. Unique historical recovery data and Git recovery references do not expire. Freed database pages are reused; quiet, bounded compaction also reclaims disk space when worthwhile without undertaking a large conversation-database rewrite.

## Current limits

Configure local file copying and disposable folders explicitly; existing projects are not given a deletion allowlist automatically. Local databases and unknown ignored content require manual attention. Configuration backups support up to 64 selected files, 1 MiB per file and 4 MiB per archive, within the adjustable app-wide budget. Setup commands must finish within 20 minutes; long-running development servers belong in terminals. Teardown scripts and shared dependency directories are not included. Pin or Git-lock checkouts used by processes outside Monocode. Integration checks use Git evidence and may keep a branch when history has been substantially rewritten; a hosting provider's merged PR status alone does not authorize deletion.

The agreed scope and optional ideas outside this feature are recorded in [the follow-up notes](worktree-followups.md).

The implementation uses native Git worktrees and SQLite ownership records. The T3 composer comparison and Orca cleanup inspiration are recorded in [the research notes](worktree-research.md).

## Iterating on the UI

Use `npm run tauri dev` to review and verify the feature in the actual desktop app. Vite updates the UI during development, and native Git operations run through the real backend. Use disposable repositories and an isolated app identifier/database for destructive lifecycle tests.

An optional layout preview is available with `npm run dev:worktrees` at <http://127.0.0.1:1422/dev/worktrees.html>. It uses production components with mocked native commands and cannot verify setup, retirement or recovery. This separate development entry is not imported into the desktop release bundle. UI iterations do not require a packaged release; create one once the native development version has been reviewed.
