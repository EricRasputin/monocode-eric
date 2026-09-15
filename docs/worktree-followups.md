# Worktree feature scope

Recovery storage, checkout capacity (#15), opt-in automatic retirement after archiving (#16), and manual generated-output cleanup (#17) are implemented for this fork. The user-facing workflow is documented in [Worktrees in this fork](worktrees.md).

## Implemented storage behavior

Git recovery references preserve committed code. Monocode separately saves selected local configuration, such as `.env`, in the private application database before removing a checkout. Recognized dependencies and build outputs are removed with a confirmed retirement and recreated by setup instead of being backed up. Standard Node, Rust and Tauri outputs are recognized from tracked project manifests without requiring a disposable-folder setting; unknown ignored data still needs explicit handling.

- Identical configuration bytes are stored once. Immutable retirement manifests preserve paths, permissions and deliberately absent files independently of shared contents.
- Legacy backups migrate transactionally and are verified against reconstructed files before their old storage is removed. Existing recoveries are retained even if their unique contents exceed the default budget.
- Settings → Worktrees → Recovery storage shows app-wide and selected-project usage, warns at 80%, and allows a whole-MiB budget from 1 to 4096 MiB. The initial budget is 64 MiB. Concurrent settings windows cannot silently overwrite a newer saved limit.
- The budget measures unique file contents. Per-file and per-archive safeguards remain 1 MiB and 4 MiB. If new configuration will exceed the budget, retirement keeps the checkout and explains how to raise the limit and retry. Repeated snapshots that add no bytes remain possible.
- Background maintenance reclaims only archives with no durable lifecycle reference, then blobs without any remaining manifest. Failed retryable retirement records remain protected. Unique historical snapshots and Git recovery references never expire by age.
- Freed SQLite pages are reused. Quiet maintenance attempts bounded physical compaction when the savings justify rewriting a reasonably sized database. A busy or interrupted compaction can retry without turning a completed retirement into a failure.

The storage limit is a configurable resource budget, not a reason to discard recovery data. It excludes Git objects, checked-out files, dependencies and unrelated conversation data in SQLite. Automatic retirement preserves the same immutable recovery data and leaves the checkout intact when configuration storage cannot admit the snapshot. Pending work and explanations survive restarts; raising the budget schedules another coordinated attempt without creating a duplicate recovery plan.

## Archive retirement

Project policy schema 1 defaults to manual review, with an independent save version for concurrent settings windows. Older automatic-cleanup preferences never opt users in. Saved automatic mode removes only eligible managed checkouts after their last conversation is archived, without requesting branch deletion, merge evidence or remote access. Native queue records survive failures and restart; manual review remains available after automatic mode is disabled, including interrupted removals. Pins, activity, Git locks, local files and capacity reservations are checked at the final removal boundary. New windows must register their leases before automatic cleanup can proceed.

Disposable native tests cover shared/bulk archives, multiple windows, changing activity and pins, setup overlap and preparation handoffs, recovery quota failures, partial removal, saved policy changes, manual takeover, exact recovery, restart retry and idempotence. UI tests cover explicit saving, stale versions, generic save errors, durable explanations and archive-success isolation. No lifecycle verification uses live user worktrees.

## Manual output cleanup

An explicit review/execution flow clears only recognized or configured disposable directories from idle managed checkouts, including checkouts with unfinished source. It retains selected configuration in place and requires setup before subsequent workspace use. Durable intent and preparation state precede deletion; interruptions require another manual review. It uses the retirement/preparation lifecycle guards, honors capacity reservations, reports partial results, and refreshes disk accounting without equating removed-file estimates with filesystem free-space change. Clean recoverable checkouts can still retire after archiving without reinstalling dependencies.

## Deliberate boundaries

Export and explicitly forgetting historical configuration snapshots are optional future features, outside this completed scope. They would need a separate review naming affected conversations and explaining the loss of configuration recovery. No such deletion is performed automatically.

Monocode tracks its own active sessions, agents, terminals and setup operations. Independently launched or deliberately detached processes cannot be fully accounted for. Pin or Git-lock a checkout used outside Monocode. An external-process scan would still be a point-in-time check and would not prevent another application from opening the folder immediately afterward; it is not required for this fork's approved workflow.

Git integration checks remain conservative. Unknown merge evidence keeps branches, while a clean, recoverable checkout can still be retired. Hosting-provider PR integration is outside scope and would need to verify the exact repository, branch and commit before authorizing deletion.

Native verification for this personal fork targets macOS. Additional Windows and Linux runtime verification remains a platform limitation. For #17, the Windows filesystem boundary was type-checked with stable Rust for `x86_64-pc-windows-msvc` using stable handle APIs. A full cross-target check was attempted but the local Mac lacks Windows C/SDK headers (`ring` could not find `assert.h`); no Windows runtime result is claimed.

## Dependency sharing investigation (#18)

[Measured Node and Rust fixtures](worktree-dependency-sharing.md) compare isolated
outputs with supported stores, include shared-cache bytes and cleanup behavior,
and distinguish disk accounting from rebuild speed. Retain this project's npm
workflow and private build outputs. Compiler-cache results are unavailable without
`sccache`; an opt-in follow-up needs its own measurements and cache accounting.

The [combined #13 validation](worktree-combined-validation.md) records the isolated
desktop lifecycle, issue-to-commit mapping, full checks and platform limits.
