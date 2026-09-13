# Worktree feature scope

Recovery storage is the final approved implementation item for this fork's worktree feature. The user-facing workflow is documented in [Worktrees in this fork](worktrees.md).

## Implemented storage behavior

Git recovery references preserve committed code. Monocode separately saves selected local configuration, such as `.env`, in the private application database before removing a checkout. Dependencies and build outputs are recreated by setup instead of being backed up.

- Identical configuration bytes are stored once. Immutable retirement manifests preserve paths, permissions and deliberately absent files independently of shared contents.
- Legacy backups migrate transactionally and are verified against reconstructed files before their old storage is removed. Existing recoveries are retained even if their unique contents exceed the default budget.
- Settings → Worktrees → Recovery storage shows app-wide and selected-project usage, warns at 80%, and allows a whole-MiB budget from 1 to 4096 MiB. The initial budget is 64 MiB. Concurrent settings windows cannot silently overwrite a newer saved limit.
- The budget measures unique file contents. Per-file and per-archive safeguards remain 1 MiB and 4 MiB. If new configuration will exceed the budget, retirement keeps the checkout and explains how to raise the limit and retry. Repeated snapshots that add no bytes remain possible.
- Background maintenance reclaims only archives with no durable lifecycle reference, then blobs without any remaining manifest. Failed retryable retirement records remain protected. Unique historical snapshots and Git recovery references never expire by age.
- Freed SQLite pages are reused. Quiet maintenance attempts bounded physical compaction when the savings justify rewriting a reasonably sized database. A busy or interrupted compaction can retry without turning a completed retirement into a failure.

The storage limit is a configurable resource budget, not a reason to discard recovery data. It excludes Git objects, checked-out files, dependencies and unrelated conversation data in SQLite.

## Deliberate boundaries

Export and explicitly forgetting historical configuration snapshots are optional future features, outside this completed scope. They would need a separate review naming affected conversations and explaining the loss of configuration recovery. No such deletion is performed automatically.

Monocode tracks its own active sessions, agents, terminals and setup operations. Independently launched or deliberately detached processes cannot be fully accounted for. Pin or Git-lock a checkout used outside Monocode. An external-process scan would still be a point-in-time check and would not prevent another application from opening the folder immediately afterward; it is not required for this fork's approved workflow.

Git integration checks remain conservative. Unknown merge evidence keeps branches, while a clean, recoverable checkout can still be retired. Hosting-provider PR integration is outside scope and would need to verify the exact repository, branch and commit before authorizing deletion.

Native verification for this personal fork targets macOS. Additional Windows and Linux runtime verification is left to upstream and is not a pending requirement for this feature.
