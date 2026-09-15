import {
  heartbeatWorktrees,
  planWorktreeRetirement,
  prepareSessionWorktree,
  type WorktreeRetirementPlan,
} from "./worktrees";
import { setSessionArchived } from "./sessionStore";
import type { Session } from "./session";

export type SessionResumeResult =
  | { session: Session; resumed: true }
  | { session: Session; resumed: false; worktreeError: string };

/** Saved history stays readable when checkout preparation fails. Keep its
 * archive state in that case; preparing the next turn will retry restoration.
 * Successful resumes stay active after the temporary window lease ends. */
export async function resumeArchivedWorktreeSession(
  session: Session,
): Promise<SessionResumeResult> {
  if (session.worktreeCwd) {
    try {
      await prepareSessionWorktree(session);
    } catch (error) {
      return { session, resumed: false, worktreeError: String(error) };
    }
  }
  await setSessionArchived(session.id, false);
  // Recovery may have chosen another branch when the old name was reused.
  // Let Git supply the current branch instead of persisting an obsolete label.
  return {
    session: session.worktreeCwd ? { ...session, branch: undefined } : session,
    resumed: true,
  };
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
