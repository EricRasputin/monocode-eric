import { invoke } from "@tauri-apps/api/core";

export type OutputCandidate = {
  path: string;
  estimatedBytes: number;
  preservedPaths: string[];
  blockedReason: string | null;
};
export type OutputReview = {
  planId: string;
  id: string;
  path: string;
  branch: string;
  candidates: OutputCandidate[];
  blockedReason: string | null;
};
export type OutputCleanupReport = {
  planId: string;
  id: string;
  path: string;
  status: "executing" | "interrupted" | "partial" | "complete";
  selectedPaths: string[];
  results: {
    path: string;
    estimatedRemovedBytes: number;
    error: string | null;
  }[];
  estimatedRemovedBytes: number;
  observedFreeSpaceChange: number | null;
  measurementError: string | null;
  preparationNeeded: boolean;
};

export const reviewWorktreeOutputs = (cwd: string, id: string) =>
  invoke<OutputReview>("worktree_output_review", { cwd, id });
export const executeWorktreeOutputCleanup = (planId: string, paths: string[]) =>
  invoke<OutputCleanupReport>("worktree_output_execute", { planId, paths });
export const getWorktreeOutputHistory = (cwd: string) =>
  invoke<OutputCleanupReport[]>("worktree_output_history", { cwd });
