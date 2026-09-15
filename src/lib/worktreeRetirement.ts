import {
  heartbeatWorktrees,
  planWorktreeRetirement,
  type WorktreeRetirementPlan,
} from "./worktrees";

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
