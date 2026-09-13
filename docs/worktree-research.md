# Worktree lifecycle research

Reviewed 2026-09-13 for the Monocode fork. This is an implementation design input, not a claim that every recommendation below is already implemented. Findings come from official documentation and source; links to code are pinned to the reviewed commits.

## Worktree and branch icons

Reviewed on 13 September 2026. T3 Code uses `FolderGit2Icon` for a new worktree and `FolderGitIcon` for an existing checkout, while its adjacent branch control uses a branch glyph. [Workspace selector](https://github.com/pingdotgg/t3code/blob/20363c32c9bfdbf49c2716ef11d1f18483fcc01b/apps/web/src/components/BranchToolbarEnvModeSelector.tsx)

Superset's workspace sidebar also distinguishes worktrees with `LuFolderGit2`; its newer dashboard has a different status/host-based icon treatment. [Workspace sidebar icon](https://github.com/superset-sh/superset/blob/c3c6717bb7de1ce91dbf82505e9dce12c2c0324e/apps/desktop/src/renderer/screens/main/components/WorkspaceSidebar/WorkspaceListItem/WorkspaceIcon.tsx), [dashboard icon](https://github.com/superset-sh/superset/blob/c3c6717bb7de1ce91dbf82505e9dce12c2c0324e/apps/desktop/src/renderer/routes/_authenticated/_dashboard/components/DashboardSidebar/components/DashboardSidebarWorkspaceItem/components/DashboardSidebarWorkspaceIcon/DashboardSidebarWorkspaceIcon.tsx)

Monocode adopts the folder-with-Git convention using `FolderGitTwoIcon` from its existing Hugeicons set, exported as `Worktree`. Use it for worktree selection, Settings navigation, inventory rows and retirement reviews. Keep the plain branch glyph for branch names and base-ref selection, and the ordinary folder for the current checkout option.

## Findings

### T3 Code

T3 creates real Git worktrees from either a new branch or an existing ref. Its Git driver derives a default path from the repository name and branch, initializes submodules when present, and saves an explicit base for later PR creation. Removal uses `git worktree remove`, supports a separately supplied force flag, and treats an already-missing checkout as an idempotent success after pruning its registration. [GitVcsDriverCore](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/server/src/vcs/GitVcsDriverCore.ts)

Threads can share a checkout. The client only treats a thread's worktree as orphaned if no other thread references the same path. This is a useful distinction between closing a conversation and retiring its workspace. [worktreeCleanup](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/worktreeCleanup.ts)

T3 also has automatic **thread settlement**, based on inactivity or completed pull requests. It excludes running/starting sessions, pending approvals or input, background liveness, recently queued turns, and explicit user overrides. Settlement is conversation organization; this policy does not itself delete the checkout. [ThreadSettlementPolicy](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/server/src/orchestration/ThreadSettlementPolicy.ts)

### Orca

Orca scopes agents, terminals, editors, browser tabs, and review state to a workspace. Its creation flow offers a start ref, branch naming, background progress, cancellation, and retry. The sidebar groups workspaces by project and supports pinning. External Git worktrees can be discovered and selectively shown. Resource Manager provides a searchable cleanup review; branches Git refuses to delete can be preserved for a separate review. [Worktrees documentation](https://www.onorca.dev/docs/model/worktrees)

Orca's classifier considers archived workspaces after seven idle days and other workspaces after thirty. It labels candidates ready, review, or protected. Protections include main/folder checkouts, pinning, the active workspace, running or unverifiable terminals, dirty editor buffers, live agents, disconnected hosts, unreadable Git state, dirty files, unpublished commits, and unknown base state. The evidence model includes timestamps and a fingerprint so a dismissal can expire when the workspace changes. These are confirmed **cleanup candidate rules**; the inspected documentation describes user-reviewed removal, not a verified unattended timer that deletes every old workspace. [Cleanup policy](https://github.com/stablyai/orca/blob/9a12ccd19d0a8377c255e4acec6ae89a3af6c0ce/src/shared/workspace-cleanup.ts)

Its Git inspection checks ahead-of-upstream commits. Without an upstream, it counts commits not reachable from remote refs; an inability to establish this becomes a blocker. [Git evidence](https://github.com/stablyai/orca/blob/9a12ccd19d0a8377c255e4acec6ae89a3af6c0ce/src/main/ipc/workspace-cleanup-git-evidence.ts)

Branch cleanup goes beyond ancestry: it checks merge results and patch equivalence, including bounded squash detection. This demonstrates why checkout removal and branch deletion need independent semantics; a clean checkout does not establish that its branch is expendable. [Branch cleanup](https://github.com/stablyai/orca/blob/9a12ccd19d0a8377c255e4acec6ae89a3af6c0ce/src/shared/git-branch-cleanup.ts)

Orca also supports local setup material: configured shared paths, repository shared directories, and `.worktreeinclude` copies. The latter only accepts existing, ignored literal paths, and copies instead of sharing them. This avoids requiring users to recreate every local configuration file by hand. [Worktrees documentation](https://www.onorca.dev/docs/model/worktrees)

### Superset

Superset separates the user's choice to delete a branch from checkout removal: `deleteBranch` defaults to false. Its deletion workflow persists intent, performs preflight, runs teardown, cleans up terminals and the checkout, then optionally deletes the branch. Failures restore the visible workspace where appropriate; startup reconciliation completes interrupted deletions. Unknown Git state caused by worker timeout fails closed. Its explicit branch-deletion option currently uses force deletion, which is stronger than appropriate for an unattended cleanup policy. [Deletion workflow](https://github.com/superset-sh/superset/blob/c3c6717bb7de1ce91dbf82505e9dce12c2c0324e/packages/host-service/src/trpc/router/workspace-cleanup/workspace-cleanup.ts)

Setup, teardown, and restartable run commands have project settings, workspace configuration, and personal overrides. Teardown failures expose logs and an explicit retry choice. Slow development servers belong in the run phase rather than blocking checkout setup. [Project lifecycle scripts](https://docs.superset.sh/setup-teardown-scripts)

### Vibe Kanban

Vibe Kanban distinguishes hiding an archived workspace from deleting it, retains conversation history separately, and documents reconstruction of missing checkouts from the branch's last commit. It also cleans orphan workspace directories on startup. Reconstruction cannot recover uncommitted files. [Workspace management documentation](https://www.vibekanban.com/docs/workspaces/managing-workspaces)

The reviewed implementation additionally runs expiration cleanup every thirty minutes; `DISABLE_WORKTREE_CLEANUP` disables expiration. Its database query uses one hour for archived workspaces and seventy-two hours otherwise, excluding sessions with unfinished execution processes. The query does not filter pinned rows or inspect dirty files. These source details are more aggressive than the documentation's archive explanation and should not be copied as Monocode's data-preservation contract. [Cleanup loop](https://github.com/BloopAI/vibe-kanban/blob/4deb7eca8f381f7cbc1f9d15515a9ab8f8009053/crates/local-deployment/src/container.rs), [expiration query](https://github.com/BloopAI/vibe-kanban/blob/4deb7eca8f381f7cbc1f9d15515a9ab8f8009053/crates/db/src/models/workspace.rs)

## Current UX direction: T3 composer flow, reviewed cleanup

The follow-up product direction supersedes the initial broad workspace-manager proposal: keep worktree selection in the composer, following T3, and take Orca's cleanup review as inspiration. Automatically find cleanup candidates and ask the user; do not silently delete them on a timer.

At the same T3 commit reviewed above, the concrete flow is:

| Moment | Confirmed T3 behavior | Monocode control to mirror |
| --- | --- | --- |
| Before the first message | A compact context strip beside the composer contains a Workspace dropdown. Current labels are **Current checkout** and **New worktree**; an existing selected checkout can read **Current worktree**. | One small **Local / Worktree** control in the composer, without a worktree-management dialog in the normal send flow. |
| Selecting a base | The adjacent searchable ref picker reads **From &lt;ref&gt;** for a new worktree. Selecting a base only updates draft context. An optional **Start from origin** switch chooses the latest matching remote branch. | A compact **From main** branch picker shown for a new worktree. Picking a base must not switch the main checkout. |
| Sending | First send prepares the checkout with a generated temporary branch, runs configured setup, then starts the turn. The send flow reports preparation; absent base selection is an error. | Create on first Send; show **Preparing worktree…** and launch in that checkout. No mandatory name, branch, path, or archive form. |
| Following up | Selecting a branch already checked out elsewhere reuses its worktree in the current-checkout flow. A **Previous worktree (&lt;branch&gt;)** item also offers the most recently touched non-archived worktree. | Existing worktrees can appear as compact menu choices. A new conversation may share one intentionally. |
| After starting | The workspace mode becomes a static label for an established worktree, retaining its identity. | Keep the current branch visible and bind subsequent messages to the same checkout. |

Control labels and draft reuse are defined in [BranchToolbar.logic](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/BranchToolbar.logic.ts) and [Workspace selector](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/BranchToolbarEnvModeSelector.tsx). Ref selection and its origin switch are in [Branch selector](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/BranchToolbarBranchSelector.tsx). The context strip placement, draft-mode mutation, and first-send preparation are in [ChatView](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/ChatView.tsx). Locking and the previous-worktree shortcut are in [BranchToolbar](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/BranchToolbar.tsx).

**Default remembering:** T3 persists the selection within a draft. New drafts resolve an explicit composer pick before a project setting, repository `t3.json`, and global setting, in that order. Changing the composer mode does not rewrite the global default. Its settings expose a separate default Workspace choice. Monocode can offer a simple remembered preference, but that is our adaptation rather than a verified T3 “last choice wins” behavior. [Default resolver](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/packages/shared/src/threadEnvMode.ts), [draft creation](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/hooks/useHandleNewThread.ts), [default Workspace setting](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/components/settings/ProjectDefaultsSettings.tsx).

**Deletion and cleanup:** deleting the last thread referencing a checkout triggers a second, contextual question naming that worktree. Declining still deletes the thread and preserves the checkout; shared worktrees are not offered. T3 stops its session and terminal, deletes the thread, then attempts approved worktree removal, with cleanup failures reported separately. The reviewed caller passes `force: true`; Monocode should copy the question and shared-reference protection while retaining its stricter dirty-data and branch preservation guards. [Thread actions](https://github.com/pingdotgg/t3code/blob/db6e0531e4faac0fc5ac0e12088b01bed6e7b851/apps/web/src/hooks/useThreadActions.ts).

For Orca-inspired housekeeping, prefer a discreet **Review cleanup** notice when old unused worktrees exist. Open a small review listing branch, last activity, and protection reason; ask before removing eligible selected checkouts. This is a Monocode recommendation, separate from the confirmed T3 composer controls above.

## Recommended Monocode lifecycle design

These are recommendations derived from the comparison, with deliberate emphasis on predictable preservation.

1. **Make the workspace the execution boundary.** A repository can own many workspaces; each conversation points to one workspace. Resolve filesystem operations, terminal cwd, harness launch cwd, search, and diffs from the selected workspace. A new-task flow should prefer a fresh worktree, with working in the main checkout an explicit option.
2. **Own what we create, discover what others create.** Persist a stable workspace ID, canonical repository identity, checkout path, branch, base ref, creation time, last activity, archive time, pin state, and whether Monocode created it. External checkouts stay usable but are excluded from automatic deletion. Repository identity should use the common Git directory rather than only the folder basename.
3. **Keep creation reliable.** Validate branch names with Git, resolve the start ref before mutation, allocate collision-resistant paths outside the source checkout, and serialize Git mutations per repository. Show progress and actionable errors. Failure must not remove pre-existing paths or branches.
4. **Separate archive, checkout cleanup, and branch deletion.** Archive hides the workspace while preserving it. Checkout cleanup releases disk space and retains history plus the branch. Branch deletion remains a separate explicit action. Avoid automatic commits or stashes that silently change the user's repository history.
5. **Find cleanup candidates automatically, remove after review.** Scan old unused Monocode worktrees on startup and periodically while the application is open. Surface a discreet review prompt, let the user select eligible checkouts, and ask before removal. A scan or retention threshold does not authorize deletion. Use the same classification for the prompt and manual cleanup so users can see why each workspace is kept.
6. **Fail closed.** Require a managed, existing, unlocked linked worktree; no active selection, pin, shared active conversation, running agent/terminal, or dirty editor buffer; successful fresh Git inspection; and no staged, unstaged, or untracked files. Do not treat unreadable or disconnected paths as empty. Protect locally unique commits during automatic cleanup even when branches are retained. Retain external worktrees, the primary checkout, and detached HEADs unless recovery is explicitly implemented.
7. **Treat ignored files as a separate preservation concern.** Git cleanliness alone does not protect `.env`, local databases, or other ignored files. Initially preserve checkouts containing ignored content unless a specific path is known to be safely reproducible or an explicit user policy allows its removal. Local configuration copying should use opt-in literal paths with canonical containment checks; never broadly copy secrets or share writable directories silently.
8. **Recheck at the mutation boundary.** A preview can become stale. Acquire the operation guard, re-evaluate activity and Git status, then call non-forced `git worktree remove`. Do not fall back to recursive deletion on refusal. Keep a retryable record until removal succeeds and report skipped reasons. External processes remain a race, so avoid a “guaranteed safe” claim.
9. **Preserve resumability.** Retain archived conversation records and branch metadata after checkout cleanup. Reopening may recreate the checkout from its retained branch; report when a branch was deleted externally instead of silently creating unrelated history.

Git's native worktree commands provide stable porcelain listings, locks, non-forced removal, and stale registration pruning. Pruning removes stale administrative metadata; it is not equivalent to deleting a living checkout or its branch. [Git worktree manual](https://git-scm.com/docs/git-worktree)

## Acceptance checks

- Create from a local ref; start a harness and terminal in the new checkout; switch workspaces without changing another checkout's files or conversation.
- Discover externally created worktrees and leave them out of automatic cleanup.
- Preserve pinned, active, dirty, locked, detached, shared, unreadable, and unpublished workspaces with visible reasons.
- Protect ignored local data independently of ordinary Git status.
- After user review, remove an eligible old checkout without deleting its branch or conversation; reopen it successfully. Merely reaching the retention threshold must not remove files.
- Handle branch/path collisions, missing checkouts, interrupted operations, concurrent actions, and Git refusal without forced filesystem deletion.
