import { useCallback, useState } from "react";
import type { WorktreeRetirementPlan } from "../lib/worktrees";

/** Keep archive outcomes visible until acknowledged, in archive order. */
export function useArchiveRetirementReview() {
  const [plans, setPlans] = useState<WorktreeRetirementPlan[]>([]);
  const review = useCallback((plan: WorktreeRetirementPlan) => {
    if (plan.entries.length > 0 || plan.kept.length > 0) {
      setPlans((current) => [...current, plan]);
    }
  }, []);
  const close = useCallback(() => setPlans((current) => current.slice(1)), []);
  return { plans, review, close };
}
