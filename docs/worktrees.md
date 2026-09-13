# Worktrees in this fork

Choose where a conversation works using the two controls above the message box:

- **New worktree · From main** — start an isolated task. Pick a different base if needed, then send your message. Monocode creates and names the branch and checkout before starting the agent. Selecting a base does not switch the source checkout.
- **Current checkout · main** — work directly in your project folder.
- **Existing worktree** — select a checkout from the workspace menu to continue work on that branch in another conversation.

New worktree is the default in this fork. Your explicit choice belongs to the draft and survives a restart; it does not change another conversation. Once the conversation starts, its checkout stays fixed. Its agent, files, changes and terminal dock use that directory. Conversations remain grouped under the original project.

A new worktree starts with committed files. Your source folder's uncommitted files, installed dependencies and `.env` are not copied. Choose a base already available locally; fetch and run project setup in your normal terminal workflow.

## Cleanup asks first

Open **Settings → Worktrees** to review the current project’s checkouts. Monocode suggests eligible worktrees unused for seven days. Suggestions refresh while the app is open. **Nothing is removed automatically**, including on installations that previously enabled automatic removal.

When a task is finished, archive its conversation and close its tabs and terminals. In the cleanup screen:

1. Select the checkouts you want to remove. Old eligible checkouts are suggested; you can also select a more recent eligible checkout.
2. Click **Review removal** and check the list.
3. Click **Remove checkouts** to confirm, or **Cancel** to keep them.

Branches and saved conversations stay. Expand **Kept worktrees** to see why a checkout is protected, pin ongoing work, or open/restore a checkout. Archiving preserves conversation history; explicitly deleting a conversation removes that history separately.

Cleanup preserves primary and external checkouts, pinned worktrees, checkouts referenced by unarchived or pinned conversations, open editors/diffs/terminals, live Monocode agents and terminals, dirty/untracked/ignored files, and commits not merged into the recorded base's current local tip. Unknown Git state, locks, changed branches and detached HEADs also prevent removal. The same checks run again at confirmation before non-forced `git worktree remove`.

## Returning to a task

Opening an archived conversation restores a cleaned checkout from its retained branch. **Restore** in the kept list also recreates it. If the branch was deleted externally or the path is occupied, Monocode reports the problem. Restoring a checkout does not reinstall dependencies or recreate local configuration.

## Current limits

Ignored `.env`, dependency and build folders prevent cleanup until you remove or move them. Merge detection uses local Git ancestry, so a push or squash merge alone may leave a checkout protected. Setup/teardown scripts, shared dependencies and automatic fetching are not included yet. Pin or Git-lock checkouts used by processes outside Monocode.

The implementation uses native Git worktrees and SQLite ownership records. The T3 composer comparison and Orca cleanup inspiration are recorded in [the research notes](worktree-research.md).
