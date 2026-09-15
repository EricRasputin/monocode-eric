// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { archiveSessionsWithRetirement } from "../lib/worktreeRetirement";
import type { WorktreeRetirementPlan } from "../lib/worktrees";
import { WorktreeRetirementDialog } from "../chrome/WorktreeRetirementDialog";
import { useArchiveRetirementReview } from "./useArchiveRetirementReview";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("sonner", () => ({ toast: vi.fn() }));

let root: Root;
let container: HTMLDivElement;
let archive: () => Promise<boolean>;
const commitArchive = vi.fn(async () => true);

function ArchiveHarness() {
  const { plans, review, close } = useArchiveRetirementReview();
  archive = () =>
    archiveSessionsWithRetirement({
      sessionIds: ["last-conversation"],
      archive: commitArchive,
      protectedPaths: () => [],
      onReview: review,
      onReviewError: (error) => {
        throw error;
      },
    });
  return plans[0]
    ? createElement(WorktreeRetirementDialog, {
        key: plans[0].planId,
        plan: plans[0],
        source: "archive",
        onClose: close,
        onRetired: vi.fn(),
      })
    : null;
}

beforeEach(async () => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => root.render(createElement(ArchiveHarness)));
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

function returnPlan(plan: WorktreeRetirementPlan) {
  vi.mocked(invoke).mockImplementation(
    async (command) =>
      (command === "worktree_retirement_plan" ? plan : undefined) as never,
  );
}

it("shows why the last archived conversation's worktree was kept until acknowledged", async () => {
  const reason =
    "Local files need attention: node_modules (ignored), target (ignored).";
  returnPlan({
    planId: "blocked-review",
    entries: [],
    kept: [{ id: "checkout", path: "/repo/worktrees/feature", reason }],
  });
  await act(async () => expect(await archive()).toBe(true));
  expect(commitArchive).toHaveBeenCalledExactlyOnceWith("last-conversation");
  const dialog = document.querySelector('[role="dialog"]');
  expect(dialog?.textContent).toContain("Worktree kept");
  expect(dialog?.textContent).toContain(reason);
  expect(dialog?.textContent).not.toContain("will be removed");
  expect(invoke).not.toHaveBeenCalledWith("worktree_retire", expect.anything());
  const done = [...document.querySelectorAll("button")].find(
    (b) => b.textContent === "Done",
  );
  expect(done).toBeTruthy();
  await act(async () => done!.click());
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

it("keeps archive reviews in order when another archive finishes before dismissal", async () => {
  for (const id of ["first", "second"]) {
    returnPlan({
      planId: id,
      entries: [],
      kept: [{ id, path: `/repo/${id}`, reason: `${id} is protected` }],
    });
    await act(async () => {
      await archive();
    });
  }
  expect(document.body.textContent).toContain("first is protected");
  expect(document.body.textContent).not.toContain("second is protected");
  const done = [...document.querySelectorAll("button")].find(
    (b) => b.textContent === "Done",
  );
  await act(async () => done!.click());
  expect(document.body.textContent).toContain("second is protected");
});

it("does not show a cleanup dialog for a primary checkout or a shared active worktree", async () => {
  returnPlan({ planId: "no-cleanup", entries: [], kept: [] });
  await act(async () => expect(await archive()).toBe(true));
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});
