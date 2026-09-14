# AI worktree naming investigation

Reviewed 2026-09-13 against Monocode `e080298ca5fe4b356ed4b02b1bf476d2de945547` and T3 Code `77bca8b2d76a1f42552e5eee7d277fcb1160347a`. The findings below describe the pre-implementation source. Implementation followed this investigation; current user-facing behavior is documented in [Worktrees](worktrees.md).

## Finding

Monocode already has the AI generation infrastructure. Worktree creation bypasses it and uses the beginning of the raw message plus the full session ID as the Git branch name. The recommended change is to generate a semantic branch name from the first-message context in the background, keeping creation and agent startup independent of AI availability. Preserve the checkout's stable directory and update the branch through the native worktree lifecycle, including its ownership metadata.

For this request, the current algorithm produces `monocode/right-now-the-name-of-the-worktree-is-aut-<session-id>`. The desired result could be `monocode/ai-worktree-naming`. The latter is an illustrative target, not a measured model response.

## What Monocode does today

| Area | Confirmed implementation |
| --- | --- |
| First send | `onSubmit` calls `prepareSessionWorktree(current, submittedText)`, waits for checkout preparation and setup, persists `worktreeCwd`, then starts the provider turn. [Send flow](../src/App.tsx#L4536) |
| Branch name | Native `create` calls `slug(name)`: ASCII lowercase, punctuation replaced with hyphens, truncate to 42 characters before collapsing hyphens, fallback `task`. It creates `monocode/{slug}-{full session ID}`. [Allocator](../src-tauri/src/worktrees.rs#L3401) |
| Directory | The physical directory is `<app data>/worktrees/<repo slug>-<common-dir hash>/<session ID>`. It is already independent of the human-readable branch fragment. [Creation](../src-tauri/src/worktrees.rs#L3452) |
| Visible name | The worktree menu and Settings inventory display the Git branch. There is no separate worktree display-name field in `WorktreeEntry`. [Workspace picker](../src/chrome/WorkspacePicker.tsx#L181), [inventory](../src/chrome/WorktreeManager.tsx#L202), [type](../src/lib/worktrees.ts#L20) |
| AI session title | First send independently launches `generateHarnessTitle`. Its callback updates the conversation title and optional linked work item, but never the worktree branch. It preserves a title that the user has changed. [Title dispatch](../src/App.tsx#L4388), [metadata prompt/parser](../src/lib/sessionTitle.ts) |
| Existing AI branch API | `generateHarnessBranchName` and the optional adapter hook already exist. Codex, Claude, Cursor, OpenCode and Grok implement them. The symbol has no application call site and is not exported from the harness barrel. [Registry](../src/lib/harness/registry.ts#L298), [exports](../src/lib/harness/index.ts#L130) |
| Existing branch prompt | Requests a short, specific 2–6 word description of the work as JSON. Input is capped at 8,000 characters; output is sanitized to a branch fragment. [Prompt and parser](../src/lib/gitText.ts#L75) |

The reusable provider runners use installed harnesses. Current utility-model choices include Codex's `gpt-5.6-luna` with low effort, Claude's discovered Haiku model (fallback `claude-haiku-4-5`), Cursor's `composer-2.5`, Grok's `grok-4.6`, and OpenCode's first usable catalog model (fallback `opencode/glm-5`). These are observed code defaults, not recommendations about current model availability. [Codex](../src/lib/harness/codexText.ts), [Claude](../src/lib/harness/claudeText.ts), [Cursor](../src/lib/harness/cursorText.ts), [Grok](../src/lib/harness/grokProtocol.ts#L23), [OpenCode](../src/lib/harness/opencodeText.ts#L165)

Pi and OMP have title generation and text runners, but no branch-generation adapter hook. FX exposes neither title nor branch generation. Extending the existing title metadata would cover Pi/OMP without adding another provider transport. [Pi adapter](../src/lib/harness/piAdapter.ts), [OMP adapter](../src/lib/harness/ompAdapter.ts), [Pi titles](../src/lib/harness/piTitle.ts), [FX adapter](../src/lib/harness/fxAdapter.ts)

## What T3 Code actually does

T3's implementation names the **Git branch**, while retaining the original checkout directory:

1. First send requests a worktree with a temporary `t3code/<8 hex characters>` branch. Server bootstrap creates the checkout and records its path/branch before dispatching the turn. [Composer bootstrap](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/web/src/components/ChatView.tsx#L7405)
2. On the first user turn, excluding its compact command, the provider reactor forks branch generation and separately forks title generation. The branch task requires an existing worktree path and a branch matching T3's temporary pattern. Provider startup proceeds independently. [First-turn dispatch](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L1402)
3. Branch generation uses the configured source-control writer model when available, otherwise the configured text-generation model. This selection is independent of the conversation's model. The text service routes through that provider instance. [Model selection](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L1009), [writer resolver](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/packages/shared/src/serverSettings.ts#L84), [provider routing](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/TextGeneration.ts#L149)
4. Context comes from the first message with citation markup converted to plain text, plus attachments. The prompt asks for a short semantic description; it is not a random word-pair generator. [Generation input](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L1414), [branch prompt](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/TextGenerationPrompts.ts#L185)
5. The result is normalized to a maximum 64-character fragment, prefixed with `t3code/`, and renamed using Git. If a local name is taken, the driver tries suffixes `-1` through `-100`. It uses non-forced `git branch -m` with the explicit old name. [Normalization](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L299), [collision handling](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/vcs/GitVcsDriverCore.ts#L973), [Git rename](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/vcs/GitVcsDriverCore.ts#L3312)
6. On success, the reactor updates thread metadata and refreshes Git status. Generation or rename failure is logged and does not fail the main turn. A generation failure leaves the temporary name usable. [Completion/failure handling](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L1033)

T3 launches its configured setup program before dispatching the first turn; that interface reports that setup has started, so naming is not guaranteed to wait for setup completion. Monocode currently waits for setup completion before its main agent starts. This difference matters when choosing when to apply the name. [T3 bootstrap order](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/ws.ts#L1189), [setup launch](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/ws.ts#L1086), [Monocode prepare/setup](../src/lib/worktrees.ts#L164)

T3 caps prompt message text at 8,000 characters and attachment metadata at 4,000. Attachment handling differs by provider: its Codex helper also supplies image files to an ephemeral, read-only `codex exec` request with a JSON output schema; the Claude branch helper includes attachment metadata in its text prompt. These utility requests are separate from the visible conversation. [Prompt construction](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/TextGenerationPrompts.ts#L158), [Codex execution](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/CodexTextGeneration.ts#L186), [Codex branch/images](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/CodexTextGeneration.ts#L367), [Claude branch helper](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/ClaudeTextGeneration.ts#L370)

The inspected AI helper checks the temporary name before inference. It does not explicitly re-read the current branch or check upstream/publication state after inference. The explicit old-name, non-forced rename supplies some protection, but this should not be described as a comprehensive user-rename or publish-race guarantee. Monocode should enforce its own eligibility checks when applying a delayed result. [AI helper](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.ts#L991), [rename implementation](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/vcs/GitVcsDriverCore.ts#L3312)

Inspected tests cover first-turn generation and citation conversion, branch prompt attachments, temporary-name eligibility and Git rename/no-op behavior. They were not run, and this investigation did not establish coverage for a publish race or a crash between Git rename and metadata persistence. [Reactor tests](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/orchestration/Layers/ProviderCommandReactor.test.ts#L2422), [prompt tests](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/textGeneration/TextGenerationPrompts.test.ts#L116), [temporary-name tests](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/packages/shared/src/git.test.ts#L135), [Git rename tests](https://github.com/pingdotgg/t3code/blob/77bca8b2d76a1f42552e5eee7d277fcb1160347a/apps/server/src/vcs/GitVcsDriverCore.test.ts#L1511)

## Recommended Monocode implementation

These are proposed choices, not existing behavior.

### One first-message metadata request

Extend the existing title request to optionally return a `branch` fragment alongside `title` and `workItem`, reusing the branch prompt's semantic rules and sanitizer. Validate fields independently so a malformed branch cannot discard a usable title or work-item hint.

This avoids two queued calls: Monocode's utility runners serialize requests through a per-provider promise chain, and the current send flow starts title generation first. Simply adding and awaiting `generateHarnessBranchName` would wait behind title generation. Existing branch-generation timeouts are 60 seconds for Cursor and 90 seconds for the other branch adapters, before accounting for queue and initialization time. [Codex queue](../src/lib/harness/codexText.ts#L84), [Claude queue](../src/lib/harness/claudeText.ts#L69), [Cursor branch timeout](../src/lib/harness/cursorGit.ts#L15), [Codex branch timeout](../src/lib/harness/codexGit.ts#L15)

Trigger metadata generation when a new worktree needs a name even if the conversation already has a custom title. Keep title replacement eligibility separate from worktree naming eligibility. Use the selected harness's existing metadata capability; retain a deterministic fallback when unsupported or unavailable.

Build context from the actual initial request and relevant note, linked-item or handoff context, with attachment filenames when no meaningful text is available. Do not name a plan-based task from the generic `Build approved plan` string. The current worktree path receives `submittedText`, whereas title generation receives `harnessText` or attachment names. Neither existing branch hook nor `TitleInput` accepts image payloads; visual understanding from attachments would be an explicit extension. [Submission context](../src/App.tsx#L4165), [title input](../src/App.tsx#L4388), [adapter input](../src/lib/harness/registry.ts#L14)

### Create immediately, finalize the name independently

Keep the current UUID directory and stable ownership ID. Create with a concise temporary/fallback branch such as `monocode/task-<short ID>`, using native collision checks. Start inference independently; creation and setup continue without waiting for it. Once setup finishes, apply an available result, or let the bounded background task apply it later if the checkout is still eligible. A failed or timed-out naming task must not fail the user's turn.

Final names should be `monocode/<semantic-fragment>` with a short collision suffix only when necessary. Keep `monocode/`: the existing lifecycle uses that prefix as part of its branch-ownership checks. The full session UUID should remain an internal identity rather than a permanent suffix on every visible branch. [Ownership checks](../src-tauri/src/worktrees.rs#L781)

Generating before creation is a smaller alternative because it avoids synchronizing a later rename, but it adds model latency to the critical path. A short deadline limits that delay at the cost of abandoning valid late names. A separate display-name field would also avoid Git renames, but leave the underlying Git branch verbose. Background branch naming most closely matches the requested T3 experience.

### Add a metadata-aware native rename operation

This is the main integration work. A frontend `git branch -m` alone is insufficient: setup, retirement and reopening compare the checkout's branch against `managed_worktrees.branch`. A mismatch currently blocks them. [Setup validation](../src-tauri/src/worktree_setup.rs#L79), [retirement validation](../src-tauri/src/worktrees.rs#L1294), [reopen validation](../src-tauri/src/worktrees.rs#L3777)

The native operation should:

- Accept managed identity, expected temporary branch and proposed fragment. Re-read repository, checkout and naming eligibility under the existing repository reservation. Only worktrees explicitly marked as awaiting an automatic name qualify; a `monocode/` prefix alone is insufficient.
- Validate and allocate the final name in Rust using Git. Never overwrite an existing branch. Preserve the directory, base ref, commit, setup state and recovery identity.
- Decline a stale result if the branch was manually changed, the checkout was retired/replaced, retirement is underway, or publication makes a rename inappropriate. Do not rename during active setup. Intentional reuse of an existing worktree must never schedule a new name from the new conversation.
- Journal rename intent before mutating Git, then update the owned branch and affected session metadata. Git and SQLite cannot commit atomically; reconcile an interrupted, explicitly journaled operation without loosening the existing checks to accept arbitrary branch changes.
- Update every session referencing the checkout, including nested project paths. Protect against a stale frontend session save restoring the old branch: current session persistence prefers a supplied branch over fresh Git metadata. [Session persistence](../src-tauri/src/session_store.rs#L717)
- Notify all relevant windows, refresh branch/worktree caches and update visible session state. Current `notifyGitChanged()` is window-local. [Refresh event](../src/lib/fs.ts#L302), [worktree cache](../src/hooks/useWorktrees.ts), [branch cache](../src/hooks/useProjectBranches.ts)

Persist naming status per worktree, so setup retry, app restart or a second conversation cannot repeatedly rename it. Generation must run outside lifecycle/database locks. Any coordination with Monocode's own publish actions should share the naming eligibility rule; external Git operations remain a race to handle conservatively.

## Implementation scope and validation

| Change | Main location |
| --- | --- |
| Combined title/work-item/branch response and parsing | `src/lib/sessionTitle.ts`, `src/lib/harness/registry.ts`, provider title adapters |
| First-message context and independent naming orchestration | `src/App.tsx`, preferably with the naming lifecycle extracted into a small helper |
| Prepare result identifies created versus reused worktree and pending naming | `src/lib/worktrees.ts`, native `PrepareWorktree`/prepare result |
| Collision allocation, rename journal, native rename and recovery reconciliation | `src-tauri/src/worktrees.rs` or a focused worktree naming module, command registration in `src-tauri/src/lib.rs` |
| Branch metadata freshness and window updates | `src-tauri/src/session_store.rs`, Git/worktree event subscribers |

The work is a contained feature, but more than wiring one AI function: the existing provider plumbing is reusable; native lifecycle consistency is the substantive part.

Acceptance checks should cover:

- A conversational, long initial prompt yields a semantic name rather than its opening words; a manually set conversation title remains intact.
- AI failure, invalid/empty output, unsupported provider and timeout leave creation and the agent working. Exercise the total queue/startup deadline, not only the provider response timeout.
- Identical tasks in concurrent windows get distinct valid branches without overwriting refs. Names preserve the `monocode/` prefix and do not expose the full session UUID.
- Existing/local/external/shared worktrees are not renamed by subsequent conversations; setup retry resumes the same identity without another naming cycle.
- Late results after manual branch change, setup, publication, cancellation or retirement obey the chosen eligibility policy.
- A process interruption between Git rename and SQLite completion is recoverable; stale session saves cannot undo the displayed branch metadata.
- The named worktree can still run setup, archive, retire and restore, including nested projects. Terminal/editor/provider paths remain unchanged.

Validation performed for this investigation: traced both source flows, searched branch-generation callers and adapter coverage, inspected lifecycle invariants, and reproduced the current slug calculation for the user's message. No application code changed; no live model calls, branch mutations or test suites were run. This is source-level evidence, not a latency or model-quality benchmark.

## Implementation notes

The implementation combines branch generation with the existing title/work-item request and bounds acceptance of that request to 45 seconds, including queue/startup time. Unsupported providers keep a short fallback name. A unique request token registers one naming operation during native creation; the prepare API continues returning the stable path. Naming errors remain separate from the main turn.

Suggestions are saved after setup settles. A failed setup retains its suggestion for a later successful retry. Native naming validates ownership, branch identity, setup, review/shared/archive state and known publication state, allocates a non-conflicting `monocode/` name, and journals the Git-to-SQLite transition. Startup and reopening reconcile an interrupted rename. Events refresh every window, and session saves cannot reintroduce the temporary branch. Monocode push/sync/PR and checkout actions freeze pending naming before proceeding. External Git operations are still outside Monocode's coordination.

Code and executable coverage: [native naming](../src-tauri/src/worktree_naming.rs), [Git lifecycle tests](../src-tauri/src/worktree_naming_tests.rs), [metadata deadline/context tests](../src/lib/initialSessionMetadata.test.ts), [preparation tests](../src/lib/worktrees.test.ts), [metadata parsing tests](../src/lib/sessionTitle.test.ts).
