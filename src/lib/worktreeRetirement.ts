import {
  heartbeatWorktrees,
  planWorktreeRetirement,
  prepareSessionWorktree,
  type WorktreeRetirementPlan,
} from "./worktrees";
import { setSessionArchived } from "./sessionStore";
import type { Session } from "./session";

/** Reopening is a resume: keep it archived if restoration fails, and make the
 * stored conversation active before its temporary window lease can disappear. */
export async function resumeArchivedWorktreeSession(
  session: Session,
): Promise<Session> {
  if (session.worktreeCwd) await prepareSessionWorktree(session);
  await setSessionArchived(session.id, false);
  // Recovery may have chosen another branch when the old name was reused.
  // Let Git supply the current branch instead of persisting an obsolete label.
  return session.worktreeCwd ? { ...session, branch: undefined } : session;
}

/** Archive is committed first. Review failures cannot undo it or turn a
 * successful archive into an archive error. A batch gets exactly one review. */
export async function archiveSessionsWithRetirement(options: {
  sessionIds: readonly string[];
  archive: (sessionId: string) => Promise<boolean>;
  protectedPaths: () => string[];
  onReview: (plan: WorktreeRetirementPlan) => void;
  onReviewError: (error: unknown) => void;
}): Promise<boolean> {
  const archived: string[] = [];
  const ids = [...new Set(options.sessionIds)];
  try {
    for (const id of ids) {
      if (!(await options.archive(id))) break;
      archived.push(id);
    }
  } finally {
    if (archived.length > 0) {
      try {
        await heartbeatWorktrees(options.protectedPaths());
        options.onReview(
          await planWorktreeRetirement({ sessionIds: archived }),
        );
      } catch (error) {
        options.onReviewError(error);
      }
    }
  }
  return archived.length === ids.length;
}
