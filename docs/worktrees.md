# Worktrees in this fork

Choose where a conversation works using the two controls above the message box:

- **New worktree · From main** — start an isolated task. Pick a different base if needed, then send your message. Monocode creates the branch and checkout and completes setup before starting the agent. Selecting a base does not switch the source checkout.
- **Current checkout · main** — work directly in your project folder.
- **Existing worktree** — select a checkout from the workspace menu to continue work on that branch in another conversation.

New worktree is the default in this fork. Your explicit choice belongs to the draft and survives a restart; it does not change another conversation. Once the conversation starts, its checkout stays fixed. Its agent, files, changes and terminal dock use that directory. Conversations remain grouped under the original project.

New worktrees receive an AI-generated branch name based on your initial request, such as `monocode/add-history-search`. A short temporary name appears immediately; naming runs in the background alongside the conversation-title request and never delays the agent. Duplicate names receive a numeric suffix. If the provider cannot generate a name, the temporary name stays usable. A custom conversation title is preserved.

Naming accepts structured JSON only, so provider quota or sign-in messages cannot become titles or branches. If naming fails or exceeds its 45-second deadline, a dismissible **AI naming unavailable** notice explaining that the default branch name was kept offers **Retry** when the provider supports naming. Retry uses the same provider and initial request; it does not create another worktree or automatically switch accounts/models. Repeated clicks share one attempt. The notice remains until dismissed or the retry completes; it is not restored after restarting the app.

Retries remain subject to the original naming request's ownership and branch checks. A saved suggestion can be applied after setup succeeds. Native naming errors are shown separately from generation failures, and a branch that is no longer eligible is kept without reporting a successful rename. See the [T3 failure-handling comparison](worktree-naming-failures-research.md) for the source behavior behind this design.

Naming keeps the checkout directory fixed. It only applies once to a newly created worktree, after successful setup; a setup retry can use an already saved suggestion. Existing worktrees and branches you switch or publish through Monocode are left as chosen. Observed external branch changes and upstream configuration also prevent a delayed automatic rename.

A new worktree starts with committed files. In **Settings → Worktrees**, choose a project and configure its environment once:

- **Setup command** runs in the selected project's directory inside a newly created or restored worktree before the agent starts, for example `npm ci`. Setup failures keep the checkout and can be retried. Newly added copy paths are applied on retry without overwriting previously copied files that you edited. An existing checkout that has completed setup is not set up again on every message.
- **Copy local files** lists ignored local files to copy from the primary checkout, for example `.env` and `.env.local`. Each worktree owns its copy. Source-code changes and arbitrary ignored files are not copied.
- **Disposable folders** lists additional ignored generated directories to remove with the worktree, for example `coverage` or `.next`. Standard outputs need no configuration: `node_modules` and `dist` beside a tracked `package.json`, `target` beside a tracked `Cargo.toml`, and `gen/schemas` beside tracked Tauri and Cargo configuration are recognized automatically, including in nested projects. Only ignored directories without tracked files qualify; arbitrary ignored files are still protected.

All three settings are empty by default; automatic recognition also applies to existing worktrees and does not rewrite saved settings. Configured paths are relative to the selected project; Git internals, traversal, overlapping rules and unsafe symlinks are rejected. `repo/apps/web` and `repo/apps/api` have independent settings and run setup in their respective directories. Each newly created worktree retains its originating project's environment when another conversation shares it. Retirement still checks the entire checkout, so unknown data in a sibling folder remains protected.

Legacy repository-wide settings apply to the repository root, and pre-existing worktrees retain that environment. A nested project starts with independent defaults. If another window saves settings while you are editing, your draft remains visible and you must reload the saved settings before saving again. Dependencies are installed separately in each worktree rather than shared through a writable directory. Choose a creation base already available locally.

## Finish a task by archiving

Archive a conversation as usual. If another unarchived conversation uses the same worktree, archiving finishes without a cleanup prompt. Monocode checks stored conversations too, including those without an open tab.

In **Settings → Worktrees → After archiving**, each project has a versioned retirement preference. **Manual review** is the initial and migration default, including for older databases with an obsolete automatic-cleanup setting. Choosing **Automatically retire eligible worktrees** only takes effect after **Save retirement preference**. It also applies to already archived conversations. A stale settings window cannot overwrite a newer saved choice; generic save failures keep the draft available to retry.

When the last conversation is archived, Monocode checks whether the worktree can be retired. In manual mode, one review offers **Keep for now** or **Retire worktree**. Bulk archiving produces one combined manual review, with automatic cleanup following each originating project's saved preference. Archiving is already complete: cancelling cleanup or a cleanup error does not undo the archive.

In automatic mode, eligible managed working folders are retired using the same validated native removal operation. Automatic retirement never requests local or remote branch deletion, performs no remote checks and needs no merged-PR evidence. Exact committed code, selected local configuration and conversation history remain recoverable. A shared checkout stays until its final attached conversation is archived. Pins, live agents, setup, preparation reservations, sessions, files and terminals in any window protect it. Reading an archived transcript alone does not retain the checkout.

Automatic cleanup problems remain visible in a dismissible archive notice. **Pending archive cleanup** in Worktrees settings persists each checkout's pending, protected, paused or failed state and explanation across restarts. **Retry automatic cleanup** uses the existing pending record. The app also runs one coordinated native maintenance pass after startup lifecycle registration, every five minutes, and after relevant archive, preference, activity and pin changes. Open windows must register their workspace activity before automatic removal; this is checked again at execution and after configuration capture.

Saving manual review prevents automatic removals that have not started. Pending work remains listed, and **Review retirement** can resume an interrupted automatic attempt even with automatic mode disabled. That review reconciles an already removed checkout only after verifying its recovery and ownership, or reviews a still-present checkout afresh. Branch deletion remains a separate explicit selection. Unchanged retries reuse the saved retirement plan and immutable configuration archive. New committed work or changed preservation inputs receive one new review while retaining earlier recovery data.

Retirement removes the working folder. Two separate, initially unchecked options can also delete its local branch and its configured remote branch. The review names each branch and shows the remote destination. Unsafe or unverifiable branch options are disabled. Other windows, live agents, terminal panes and files still protect a checkout. If cleanup is blocked, a **Worktree kept** dialog stays open with the reasons and next steps until you dismiss it. Recognized dependencies and build outputs are removed with the checkout when you confirm retirement. Unknown local data or uncommitted changes still need attention, even after a PR is merged.

A clean checkout can be retired even when its committed work is not merged: its branch and recovery reference preserve the work. Branch deletion requires separate evidence that the exact reviewed work has been integrated. A pending or failed network check does not require keeping a safely recoverable folder forever.

Cleanup does not expire work by age. **Settings → Worktrees** is the inventory for unfinished cleanup and worktrees you kept. The project picker includes remembered and archived projects, with paths to distinguish projects with the same name. Selecting a project here does not switch the active workspace. Nested projects have independent retirement preferences; a shared checkout keeps its originating project's policy. Switching projects clears an unconfirmed review.

Select ready checkouts and choose **Review retirement** to open the same review. Expand **Kept worktrees** to inspect protection reasons, pin ongoing work, or open/restore a checkout. If only some retirement steps succeed, the result distinguishes the removed checkout from branches that were kept and offers a retry. You can also open a new review to delete remaining branches after the working folder was retired. That review preserves the original code and local-configuration recovery; restoring and retiring the checkout again invalidates older reviews.

## Returning to a task

Before any removal, Monocode saves the exact final commit under a dedicated Git recovery reference and records the checkout identity in SQLite. The completed retirement identifies both the code and configuration to restore; confirming an older review cannot replace it. This protects the saved commit from normal Git garbage collection even when both visible branches are deleted. A differing remote tip must also be preserved before remote deletion. Recovery references are retained; this feature does not expire them.

Selected local configuration is preserved separately from Git before removal, including edits made inside the worktree. It stays on this machine and is not committed or pushed. Unknown local data and uncommitted code still block retirement with the affected paths. Recognized and explicitly configured generated folders are discarded and recreated by setup. Explicitly copied files are preserved even when they live inside a recognized generated folder.

Opening or reloading an archived conversation displays its saved messages only. It keeps its saved worktree identity and archive flag, starts no provider, and does not restore a checkout or run setup. Reading a transcript or saved plan alone does not protect the worktree from retirement.

Saved conversations show **Viewing saved conversation** until their workspace is activated. This does not mean the project folder is missing. Sending a message, opening files or changes, browsing file mentions, starting a terminal, or choosing **Resume workspace** prepares the saved workspace before use. **Restore** in Settings uses the same preparation path. Preparation restores saved configuration and runs the originating project's current setup command. Only successful preparation makes the conversation active again; opening files or terminals does not start a coding provider. Concurrent requests for the same conversation share preparation. Conversations and standalone surfaces sharing a checkout also wait for the same native setup result, even from different subdirectories or windows; each conversation activates separately. Archiving waits for preparation to settle before persisting the archive. Every later workspace action checks native setup again, including when setup has been marked pending after earlier use.

A failed preparation leaves the messages readable and the archive flag intact. Retry the action or use **Resume workspace** after fixing the problem. Files, terminals and coding providers never start in the primary checkout as a fallback. Checkout preparation and real file/editor/terminal surfaces retain their own protection leases, including when restored from a saved layout. Ordinary files and terminals outside Git repositories remain supported.

During recovery, archived file contents and intentionally absent files take priority over today's primary checkout. New copy paths explicitly added in Settings can still be applied, including on a failed setup retry, without overwriting restored files. If the original branch name has been reused, recovery creates another branch rather than overwriting it. If Git is unavailable, the path is occupied, recovery cannot be verified, or setup fails, the workspace action reports the error and can be retried. Restoration does not recreate a remote branch automatically.

Retirement checks repository identity, ownership, current policy, active and pinned conversations, worktree pins, live Monocode windows/agents/terminals/setup, preparation and awaiting-setup reservations, Git locks, branch identity, tracked changes, untracked files and ignored files. Main and external checkouts are protected. These checks run immediately before removal, including after preserving configuration; removing a checkout never uses force. Maintenance, manual retirement and preparation share native repository/lifecycle coordination. Disk estimates are invalidated and refreshed after complete or partial cleanup, with scans outside lifecycle, repository and database writer locks.

Branch review refreshes the relevant remote target without checking out or pulling the primary branch. It checks ordinary ancestry and conservative content equivalence for squash or rebase merges. New work added after a merge remains protected. Uncertain integration evidence disables branch deletion while leaving checkout-only retirement available.

Remote deletion uses the exact reviewed remote reference and commit, so changes pushed after review prevent deletion. Local branch deletion also checks the reviewed commit and other Git checkouts. Git server protections still apply. These operations can fail independently; Monocode records their progress so retries preserve successful steps and recovery data.

## Clear generated files while keeping a checkout

**Settings → Worktrees → Clear generated files** is a manual action, independent of the archive-retirement preference. Select an idle managed checkout, choose **Review generated files**, inspect the exact directories and preserved configuration, then choose **Clear selected outputs**. You can deselect individual directories or cancel. The checkout, dirty/staged/untracked source outside disposable directories, local and remote branches, and conversation history remain intact.

This uses the same disposal policy as retirement: ignored Node `node_modules`/`dist`, Rust `target`, and Tauri `gen/schemas` are recognized beside tracked manifests, including nested projects. Explicit **Disposable folders** add literal ignored directories. Other ignored files are never treated as disposable merely because Git ignores them. A candidate containing a file tracked in the index or HEAD is kept in its entirety with an explanation. Review again if ownership, repository/checkout identity, Git state, directory identity or policy changes.

**Copy local files** selections stay in place, including inside recognized or configured output directories. Their ancestor directories and the top-level output directory remain; other emptied subdirectories can be removed. Cleanup does not copy primary-checkout configuration, replay old archives, or consume recovery storage. Subsequent retirement still uses the existing per-file, per-archive and app-wide recovery limits, and keeps the checkout if preservation cannot fit.

An unarchived conversation in the database alone does not block this action. Live workspace/editor/file leases, pins, agents, terminals, setup flights or running setup, capacity reservations, Git locks and pending retirement do. The repository reservation coordinates with retirement, naming, setup and new workspace use; the global lifecycle lock is released during the output walk so other repositories remain usable. Filesystem operations reject replaced paths and symlink ancestors; links inside output trees are unlinked without following their targets. Git identity/index/ref/policy changes during the walk stop remaining deletion using cheap metadata/lock checks rather than launching Git per file.

The exact selected directories and preparation-needed state are committed before deleting any output. Partial or interrupted cleanup remains preparation-needed across restart, and deletion never resumes automatically. **Recent output cleanup** shows saved results; after interruption those byte totals include only results already recorded, so review the remaining outputs again. Successfully removed files are not reported as rolled back if another directory fails.

The next coding, file/editor or terminal operation goes through the same workspace preparation interface as history restoration and reruns the originating project's current setup command. Selected configuration and unfinished source are kept in place on retry; setup errors remain retryable. Setup commands themselves run with normal project permissions. Archiving a clean, idle, recoverable checkout may retire it directly without reinstalling dependencies first. Dirty source, unknown data, unfinished configuration restoration, active setup and reservations retain their usual retirement protections.

Results distinguish **estimated removed bytes** from **observed filesystem free-space change**. Estimates exclude shared hard-linked file bytes on Unix; clones, snapshots and compression can make physical reclamation different. The observed change can be negative because it includes unrelated filesystem activity. Missing measurements are shown as unavailable. Complete or partial attempts invalidate checkout accounting and schedule refreshed usage outside mutation locks; the capacity panel shows the scan/probe timestamps and any measurement error.

## Recovery storage

**Settings → Worktrees → Recovery storage** shows saved configuration usage across the app and for the selected project. Identical file contents are stored once, even across projects and repeated retirements. Each retirement keeps its own immutable record of file paths, permissions and deliberately absent files, so sharing bytes does not change what a conversation restores. Project totals can overlap when their backups share contents.

The app-wide budget defaults to 64 MiB and can be changed from 1 to 4096 MiB in whole-MiB steps (labelled MB in Settings). A warning appears at 80% usage. The budget measures unique saved configuration bytes, not repository size, dependencies, Git recovery references, or the SQLite database's total size. You cannot lower it below current usage. Another window's newer setting cannot be silently overwritten.

If a retirement needs more storage than the budget allows, Monocode keeps the checkout and explains how to increase the limit and retry. A backup that adds no new bytes still fits, including when migrated backups already exceed the budget. Recovery data is never silently discarded to complete retirement.

Existing backups are converted in one transaction and their contents, presence and permissions are verified before the old storage is removed. A failed or interrupted conversion leaves the original backups intact. Background maintenance removes only archives without any durable retirement, recovery or setup reference, and blobs no archive uses. Unique historical recovery data and Git recovery references do not expire. Freed database pages are reused; quiet, bounded compaction also reclaims disk space when worthwhile without undertaking a large conversation-database rewrite.

## Current limits

Configure local file copying and any additional disposable folders explicitly; standard generated folders are recognized from tracked project manifests. Local databases and unknown ignored content require manual attention. Configuration backups support up to 64 selected files, 1 MiB per file and 4 MiB per archive, within the adjustable app-wide budget. Setup commands must finish within 20 minutes; long-running development servers belong in terminals. Teardown scripts and shared dependency directories are not included. Pin or Git-lock checkouts used by processes outside Monocode. Integration checks use Git evidence and may keep a branch when history has been substantially rewritten; a hosting provider's merged PR status alone does not authorize deletion.

The agreed scope and optional ideas outside this feature are recorded in [the follow-up notes](worktree-followups.md).

The implementation uses native Git worktrees and SQLite ownership records. The T3 composer comparison and Orca cleanup inspiration are recorded in [the research notes](worktree-research.md).

## Iterating on the UI

Use `npm run tauri dev` to review and verify the feature in the actual desktop app. Vite updates the UI during development, and native Git operations run through the real backend. Use disposable repositories and an isolated app identifier/database for destructive lifecycle tests.

An optional layout preview is available with `npm run dev:worktrees` at <http://127.0.0.1:1422/dev/worktrees.html>. It uses production components with mocked native commands and cannot verify setup, retirement or recovery. This separate development entry is not imported into the desktop release bundle. UI iterations do not require a packaged release; create one once the native development version has been reviewed.

The native verification page at `/dev/worktree-native.html` accepts only temporary fixture repositories matching `/tmp/monocode-retirement-native-*/repo` (or `/private/tmp/...`). **Read saved conversation 1** displays history without preparing it; **Restore conversation 1** uses the production preparation interface. Never point lifecycle tests at live user worktrees. Automated native tests create their own temporary Git repositories and SQLite databases.

For a mocked history/retry preview, open `/dev/worktrees.html?history=1`. Toggle the setup failure control, read the saved conversation, then restore it to inspect the archived/active transition. Reloading this preview returns to the saved transcript fixture.

Use `/dev/worktrees.html?automatic=1` to preview a saved automatic preference, a pinned checkout and a partial-cleanup failure. The default preview still starts in manual mode; select automatic mode and save to exercise final/shared/bulk archive scenarios. **Retry automatic cleanup** resolves the simulated partial failure while retaining the pin blocker. The native fixture page uses the production preference and output-cleanup panels and logs automatic outcomes; it never enables automatic mode without a saved choice.

Use `/dev/worktrees.html?outputs=partial` for output review with selected configuration, a tracked-file blocker, and an execution failure in one directory. `?outputs=interrupted` shows the persisted-intent recovery presentation after executing the mocked action. These previews use sample data only. The native fixture page also exposes the production **Clear generated files** action; release its workspace leases before reviewing an idle fixture.

Native output-cleanup fixtures cover dirty source, nested manifests, configuration inside outputs, pins/activity/setup/reservations, index and path replacement, partial accounting, failed intent persistence, interruption/restart/setup retry, and clear-then-archive retirement without reinstalling. A 2,048-file fixture asserts bounded Git subprocess counts and that traversal releases the global lifecycle lock. UI tests cover selection, blockers, lost responses, durable results and preparation before subsequent filesystem/provider use.

## Checkout disk capacity

See [Managed checkout disk capacity](disk-management.md) for the 30 GiB managed
checkout budget, 10 GiB per-volume reserve, 5 GiB initial allowance, accounting
limits, reservations, and pressure warnings. These are separate from configuration
recovery storage. Ready workspaces remain usable when capacity is tight.
