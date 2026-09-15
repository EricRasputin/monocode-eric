import { expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  executeWorktreeOutputCleanup,
  getWorktreeOutputHistory,
  reviewWorktreeOutputs,
} from "./worktreeOutputCleanup";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
it("keeps review, exact selection execution, and durable history as separate native requests", async () => {
  await reviewWorktreeOutputs("/repo/subproject", "owned");
  await executeWorktreeOutputCleanup("review", ["packages/web/dist"]);
  await getWorktreeOutputHistory("/repo/subproject");
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["worktree_output_review", { cwd: "/repo/subproject", id: "owned" }],
    [
      "worktree_output_execute",
      { planId: "review", paths: ["packages/web/dist"] },
    ],
    ["worktree_output_history", { cwd: "/repo/subproject" }],
  ]);
});
