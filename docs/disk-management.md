# Managed checkout disk capacity (#15)

MonoCode uses **admission control and monitoring**, not an operating-system quota.
Existing agents, builds and terminals can continue growing beyond estimates. Disk
pressure never kills processes, deletes active work, or changes the requested
workspace. Ready existing workspaces remain usable under pressure; create,
restore and pending setup require admission.

## Defaults and settings

The app-wide native disk policy is separate from configuration **Recovery
storage** and its quota:

| Setting | Default | Adjustment |
| --- | --- | --- |
| Managed checkout budget | Enabled, 30 GiB | Positive byte count, or disabled |
| Minimum available space | Enabled, 10 GiB per affected volume | Positive byte count, or disabled |
| Initial preparation allowance | 5 GiB | Positive byte count |

One GiB is 1,073,741,824 bytes. Native settings accept 1 byte through 1 PiB. The
settings schema is version 1; a separate optimistic-concurrency version prevents
one settings window overwriting another window's changes. Disabling the reserve
still requires enough actual free space for estimated pending growth.

## Estimation and accounting

Preparation targets the larger of the initial allowance and the largest observed
completed-checkout footprint for that project (including its subdirectory origin).
The remaining reservation is `max(0, target - already-accounted checkout bytes)`.
A retry therefore accounts for files written by earlier partial attempts. Current
usage plus pending reservations plus proposed growth may equal the budget; an
amount above it is rejected. Actual available volume space must cover its reserve
plus pending and proposed growth. A pending setup that already meets its estimate
still checks the reserve. Ready-workspace access requires no growth admission.

Usage snapshots include measurement timestamps, total managed checkout usage,
per-checkout estimates, safely reclaimable estimates, reservations, and native
volume identities with actual available bytes. Missing folders count as zero.
Managed directory aliases and nested roots are traversed once. Primary checkouts
and `.git` metadata are excluded, and symlinks are never followed. Unix estimates
use allocated blocks; Windows estimates use file lengths. File identities count
hard links once app-wide; individual checkout estimates can overlap. Shared hard
links are excluded from reclaimable estimates. Filesystem clones, compression,
snapshots and shared storage pools prevent exact attribution or a guarantee that
removal will return the displayed bytes. Unmanaged files, shared Git metadata and
external writes still affect the native free-space measurement.

Reclaimable estimates use existing retirement safety checks (including windows,
native processes, setup, conversation references, pins and local configuration).
They are advisory: explicit cleanup review and execution recheck those protections.
No quota policy bypasses retirement or recovery protections.

## Coordination and failure handling

Expensive scans run on native workers, outside the conversation database writer,
repository and lifecycle/window locks. Snapshots are cached for up to 30 seconds.
A growing operation measures before mutation, then validates ownership and root
identities, rereads settings and reservations, and probes free space in a short
admission transaction. Changed identities trigger a new scan outside those locks.
SQLite serializes admission across repositories/windows and native instances using
the same database. Pending reservations reduce as checkout bytes become accounted.

Create/restore retain a durable `awaitingSetup` reservation across the IPC gap.
Only the owner of `setup::coordinate_setup` takes it over; requests joined from
other windows or project subdirectories share its result and reservation. Success,
failure and unwinding release the reservation while retaining/accounting partial
files. An unverified surviving setup process keeps its reservation and protection.
At startup, dead-owner reservations are reconciled before new admission; surviving
setup processes retain theirs. During runtime monitoring, abandoned handoffs from
this native process expire after 60 seconds only if no window/native process
protects the checkout and setup is not running. A later preparation re-admits.
Explicit retirement also clears obsolete handoffs. No reconciliation deletes files.

Capacity errors cross IPC as structured `WORKTREE_CAPACITY` failures with reason,
requested path/operation, required growth, affected volume, a timestamped snapshot
and cleanup/settings/reuse guidance. Other workspace errors remain strings.

A separate native timer probes available space every 30 seconds. A coalesced worker
refreshes usage after lifecycle changes and during monitoring, so a slow scan does
not delay free-space probes. Warnings persist until pressure clears; measurement
errors must not be presented as zero usage or successful capacity checks.
