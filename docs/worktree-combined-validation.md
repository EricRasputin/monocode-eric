# Combined worktree lifecycle validation (#13)

Validation date: 2026-09-15. Implementation baseline: `bd813b7` on `main`, seven
commits ahead of `origin/main`. Review used the live GitHub requirements for
[#13](https://github.com/EricRasputin/monocode-eric/issues/13) and
[#14](https://github.com/EricRasputin/monocode-eric/issues/14)–
[#17](https://github.com/EricRasputin/monocode-eric/issues/17).

**The combined desktop lifecycle passed on macOS.** No product integration defect
was found in the exercised paths. This checkpoint extends the existing development
preview with a native concurrent-capacity assertion and explicit event-log export;
it does not change production lifecycle behavior. The separate
[#18 investigation](worktree-dependency-sharing.md) recommends retaining isolation.

## Issue-to-commit mapping

| Issue | Implementation commits |
| --- | --- |
| #14, archived history without preparation | `98dddea` |
| #15, native disk admission and settings | `f82db51`, `cfe5d54` |
| #16, automatic retirement and settings/retry recovery | `479b7cc`, `086ea62` |
| #17, durable output cleanup and review settings | `31a82df`, `bd813b7` |
| #18 | `aeb0688`, `docs: measure dependency sharing tradeoffs (#18)` |
| #13 | This validation checkpoint titled `test: validate combined desktop worktree lifecycle (#13)` |

The #13 hash is supplied in the delivery summary; its title avoids a
self-referential hash in this file. Earlier commits are retained without amendment
or squashing. No release or push is part of this validation.

## Combined review

### Standards

Reviewed the implementation diff `git diff 45dcac5...bd813b7`, concentrating on
shared preparation, archive callbacks, native coordination and cleanup boundaries,
against `CONTRIBUTING.md`. Production mutations remain in the native lifecycle;
the UI uses the shared preparation helper and the existing settings/components.
No additional actionable standards defect was identified. Automated standards
checks are reported separately below. This was a local review with no workers.

### Spec and interaction checks

| Interaction | Review and verification |
| --- | --- |
| #14 → #16 | Stored sessions are transcript-only; heartbeat protection excludes history-only sessions. Explicit preparation activates only the resumed conversation. Automatic retirement can remove the last archived checkout without making its history unreadable. Desktop archive/read/restart/restore sequence passed. |
| #14 → #15 | Preparation funnels through native create/restore/setup admission; capacity failure retains the requested path. The preview invokes two native creates concurrently, keeps the successful setup handoff reserved until both settle, then runs setup. Exactly one rejection and reservation were observed. |
| #15 → #16/#17 | Retirement and cleanup share native repository/lifecycle coordination and honor reservations. Cleanup invalidates usage and leaves no reservation behind. Settings restored to 30 GiB budget, 10 GiB reserve and 5 GiB initial allowance after the test. |
| #16 → #17 | An unarchived conversation and unfinished source keep a checkout from retirement, while an idle checkout can still clear reviewed outputs. Automatic queue explains the active conversation; output review succeeds after its workspace view closes. |
| #17 → #14/#15 | Output intent records `origin=cleaned`, `status=pending` before deletion. Reading another archived conversation does not run setup. Opening Explorer reruns capacity-controlled preparation, preserves selected local files and activates the requesting conversation. |

The existing regression suite covers additional boundaries: failures/retries,
provider and terminal preparation, shared preparation requests, threshold changes,
cross-repository admission, abandoned reservations, pins/window activity, locks,
unknown data, symlink replacement, partial cleanup and restart recovery. These
automated cases are not represented as additional manual desktop scenarios.

## Actual desktop execution

Used the real Tauri development app and its SQLite store through native IPC,
first at `/dev/worktree-native.html`, then at the full `/` application. The mocked
preview was not used as lifecycle evidence. Computer Use drove the native UI;
read-only SQLite and filesystem observations checked results independently.

Isolation:

- Disposable primary repository:
  `/private/tmp/monocode-retirement-native-final-cidzjft7/repo`.
- App identifier: `com.monocode.validation.issue13.final20260915`.
- Database:
  `~/Library/Application Support/com.monocode.validation.issue13.final20260915/monocode.db`.
- All app-created fixture worktrees were under that identifier's `worktrees/`.
  Initial database counts were zero sessions and zero managed worktrees.
- Launch config: `/private/tmp/monocode-retirement-native-final-cidzjft7/tauri.validation.json`;
  Vite served only `127.0.0.1:1438`. Its window URL initially selected the native
  fixture page and was changed to `/` for the restart check.
- Command: `npm run tauri -- dev --no-watch --config <isolated-config>`.
  This is the Tauri development launcher, not the MonoCode control CLI.
- The fixture has a committed source file, an empty npm lockfile and a small
  build script producing one 65,536-byte file in each of `dist` and `node_modules`.
  A counter outside the checkout records actual build executions. Selected `.env`
  contains only synthetic fixture values.

No real user worktree or live app database was used. No provider conversation was
started, no workers were spawned, and the control CLI was not invoked. The fixture
desktop and its Vite process were closed at the end. The original primary fixture
source and `.env` remained unchanged.

### Observed sequence

| Step | Desktop action | Independent result |
| --- | --- | --- |
| 1 | Save automatic retirement; prepare one checkout for two saved conversations | One build; both conversations unarchived; generated files and copied `.env` present. `01-prepared.json`. |
| 2 | Archive conversation 1 | Checkout survives because conversation 2 remains active. `02-first-archived.json`. |
| 3 | Archive conversation 2 | Native UI reports automatic completion. Checkout disappears; both records stay archived; local branch and Git recovery ref remain. `03-auto-retired.json`. |
| 4 | Read saved conversation in native preview | Transcript returned; checkout remains absent; build counter unchanged. `04-history-only.json`. |
| 5 | Two concurrent native creates with one preparation allowance | One `WORKTREE_CAPACITY` rejection, one success, one durable handoff reservation. Rejected path never appears. Successful setup releases reservation; saved disk policy is restored. Full native snapshots in `native-events.json`. |
| 6 | Open archived history in full desktop app | Actual transcript and “workspace not in use” banner visible. Checkout remains absent and both archive bits stay set. `06-full-app-history.json`. |
| 7 | Quit and relaunch with transcript open | Saved transcript returns; checkout remains absent; build counter unchanged; both conversations archived. `07-restarted-history.json`. |
| 8 | Click Restore workspace | Checkout and selected `.env` recovered, build counter increases exactly once, conversation 1 becomes active and conversation 2 stays archived. `08-resumed.json`. |
| 9 | Modify fixture source, create untracked notes and change local `.env`; close workspace; select `dist/.env` for preservation in Worktrees settings | Actual review offers `dist` and `node_modules`, explicitly lists `dist/.env` as preserved, and accepts unfinished source. `09-output-review.json`. |
| 10 | Clear both reviewed outputs | Source change, notes and both configuration files survive. Generated files disappear; empty root directories may remain. Durable report is complete with `preparationNeeded=true`. `10-cleared.json`, `11-before-reprepare.json`. |
| 11 | Read archived conversation 2 while setup is pending | History remains readable; setup stays pending; build counter stays unchanged; archive bit stays set. `12-pending-history.json`. |
| 12 | Click Explorer, then open `source.txt` | One setup rebuild before Explorer opens; preparation becomes ready; conversation 2 becomes active. Editor displays the unchanged unfinished source; both `.env` values and notes survive. `13-filesystem-reprepared.json`, `14-editor-open.json`. |

Capacity was exercised twice: the first run passed, and the second retained full
events after the preview's new log-export control caused a development reload.
The retained second run used a 16,932,864-byte budget and a 16,777,216-byte
allowance. The rejected request saw one reservation with 16,756,736 remaining
bytes; successful setup ended with zero pending bytes. Two small capacity fixture
checkouts account for two of the final five build-counter entries. None belongs
to a user project.

Output cleanup reported **131,072 estimated removed bytes** and **−8,192 bytes
observed free-space change**. The UI displayed the negative measurement separately,
with its explanation of other filesystem activity. `node_modules` remained an
empty directory; an ad hoc observation initially required the directory itself to
disappear. The corrected check verified generated-file absence and unchanged
source/configuration, which is the required behavior, rather than root removal.
There was no product failure or timeout relaxation.

## Evidence and checks

Native evidence directory: `/private/tmp/monocode-retirement-native-final-cidzjft7/`.
It contains numbered read-only observation snapshots and `native-events.json`.
The observation script is `/tmp/monocode-13-observe.py`. Development logs are
`/tmp/monocode-13-native-desktop.log` and `/tmp/monocode-13-native-restart.log`.
The final evidence archive is `/tmp/monocode-13-native-evidence.tar.gz`.

The reviewed baseline's full `npm run check` passed **2,386 web tests, 15 release
tests and 441 Rust tests**; exact log:
`/tmp/monocode-17-checkpoint-2-final-check.log`. This is inherited evidence, not a
newly executed check. The earlier failed baseline log
`/tmp/monocode-17-checkpoint-2-check.log` remains retained: its four failures
coincided with a prolonged host execution stall (remote-operation and setup
timeouts), then passed individually and in the full run without changing timeout
protections.

New checkpoint `npm run check`: **passed**, with **214 web test files / 2,386 web
tests, 15 release tests and 441 Rust tests**; TypeScript, `cargo fmt --check` and
Clippy with `-D warnings` also passed. Exact log:
`/tmp/monocode-13-combined-checkpoint-check.log`. The benchmark's 110 subprocess
records also all exited zero; its separate output/digest/source-survival assertions
passed. `git diff --check` passed.

Final verification uses the same full `npm run check` after the final edits,
with exact output retained at `/tmp/monocode-13-final-check.log`. That log is the
authoritative result for the delivered tree; the delivery summary records its
exit status and totals. No individual test replaces the full-check requirement.

## Platform and validation limits

- New runtime validation is **macOS arm64 only**, using macOS 27.0, Node 24.13.1
  and stable Rust 1.98.1. No Linux or Windows desktop run is claimed.
- Prior #17 Windows output-filesystem module type-check passed on stable 1.98.1
  for `x86_64-pc-windows-msvc`. The full Windows check failed in `ring` because
  this Mac lacks MSVC SDK/C headers, including `assert.h`. Linking and Windows
  runtime behavior remain unverified. Logs:
  `/tmp/monocode-17-windows-fs-check.log`, `/tmp/monocode-17-windows-check.log`.
- This desktop sequence used one window and two concurrent IPC requests. The
  multi-window/process and interruption races are covered by native regression
  tests, not additional manual desktop instances in this run.
- Paid/authenticated provider turns, terminal failure/retry, forced-crash partial
  removal and long-running disk growth were not exercised manually here.
  Provider/terminal gating, failure retry and lifecycle protections rely on the
  reviewed code and automated checks for those cases.
- Dependency fixtures are deliberately small, with one final sample per phase.
  sccache, native addon compilation, cross-volume linking and physical APFS clone
  savings remain unavailable/unmeasured as detailed in the #18 report.
