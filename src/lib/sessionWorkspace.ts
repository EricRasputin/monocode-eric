import { bindHarnessSession, isLiveHarness } from "./harness";
import { newSession, sessionWorkCwd, type Session } from "./session";
import {
  setSessionArchived,
  shouldPersistSession,
  upsertSession,
} from "./sessionStore";
import { leasePreparingWorkspace, prepareSessionWorktree } from "./worktrees";
import type { WorktreeNaming } from "./worktreeNaming";
import { rebasePath } from "./paths";

export type PreparedWorkspace = { session: Session; cwd: string };
type WorkspaceUpdate = Pick<
  Session,
  "worktreeCwd" | "workspaceChoice" | "branch" | "transcriptOnly"
>;

/** One boundary for coding, files, terminals and explicit restoration. Only
 * in-flight requests are shared: native setup remains authoritative on every use.
 * update must merge into current state so asynchronous preparation cannot erase
 * messages, title edits, or provider changes made while setup was running. */
export function createSessionWorkspacePreparation(options: {
  current: (id: string) => Session | undefined;
  update: (session: Session, patch: WorkspaceUpdate) => Session;
  activated?: (session: Session) => void;
}) {
  const pending = new Map<string, Promise<PreparedWorkspace>>();
  function prepareWorkspace(
    input: Session,
    name?: string,
    naming?: WorktreeNaming,
  ): Promise<PreparedWorkspace> {
    const existing = pending.get(input.id);
    if (existing) return existing;
    const session = options.current(input.id) ?? input;
    const lease = leasePreparingWorkspace(sessionWorkCwd(session));
    const task = (async () => {
      let located = session;
      const path = await prepareSessionWorktree(
        session,
        name,
        naming,
        (cwd) => {
          lease.add(cwd);
          located = options.update(session, {
            worktreeCwd: cwd,
            workspaceChoice: undefined,
            // Recovery may have selected a different branch. Read it from Git.
            branch: undefined,
          });
        },
      );
      const latest = options.current(session.id) ?? located;
      if (shouldPersistSession(latest)) {
        // Save the actual checkout before allowing anything to use its files.
        await upsertSession(latest);
        await setSessionArchived(session.id, false);
      }
      const prepared = options.update(latest, { transcriptOnly: false });
      options.activated?.(prepared);
      return { session: prepared, cwd: path ?? sessionWorkCwd(prepared) };
    })().finally(() => {
      pending.delete(session.id);
      lease.release();
    });
    pending.set(session.id, task);
    return task;
  }
  return Object.assign(prepareWorkspace, {
    settled: async (id: string) => {
      await pending.get(id)?.catch(() => undefined);
    },
  });
}

/** Rebase file/subdirectory requests onto the location native recovery selected. */
export async function prepareWorkspacePath(
  session: Session,
  path: string,
  prepare: (session: Session) => Promise<PreparedWorkspace>,
): Promise<string> {
  const previous = sessionWorkCwd(session);
  const prepared = await prepare(session);
  return rebasePath(path, previous, prepared.cwd);
}

/** A filesystem surface without an open conversation still checks native setup.
 * cwd is the containing project/checkout directory, never the requested file.
 * Native ownership lookup handles retired managed folders and plain directories. */
export async function prepareStandaloneWorkspacePath(
  cwd: string,
  path: string,
  prepare: (session: Session) => Promise<PreparedWorkspace>,
): Promise<string> {
  const session: Session = {
    ...newSession("claude", cwd),
    id: `filesystem:${cwd}`,
    workspaceChoice: { mode: "local" },
  };
  return prepareWorkspacePath(session, path, prepare);
}

/** Provider continuity is bound only for coding/compaction after preparation.
 * Viewing history, files, or terminals never calls this continuation. */
export async function prepareProviderWorkspace(
  session: Session,
  prepare: (
    session: Session,
    name?: string,
    naming?: WorktreeNaming,
  ) => Promise<PreparedWorkspace>,
  isCurrent: () => boolean,
  name?: string,
  naming?: WorktreeNaming,
): Promise<PreparedWorkspace> {
  const prepared = await prepare(session, name, naming);
  if (
    isCurrent() &&
    prepared.session.providerSessionId &&
    isLiveHarness(prepared.session.harness)
  ) {
    bindHarnessSession(
      prepared.session.harness,
      session.id,
      prepared.session.providerSessionId,
      prepared.cwd,
    );
  }
  return prepared;
}
