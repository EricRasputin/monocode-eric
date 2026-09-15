// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { listen } from "@tauri-apps/api/event";
import { WorktreeOutputCleanup } from "./WorktreeOutputCleanup";
import {
  executeWorktreeOutputCleanup,
  getWorktreeOutputHistory,
  reviewWorktreeOutputs,
  type OutputCleanupReport,
  type OutputReview,
} from "../lib/worktreeOutputCleanup";
import type { WorktreeEntry } from "../lib/worktrees";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../lib/worktreeOutputCleanup", () => ({
  executeWorktreeOutputCleanup: vi.fn(),
  getWorktreeOutputHistory: vi.fn(),
  reviewWorktreeOutputs: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
let events: Map<string, () => void>;
const onChanged = vi.fn();
const onReviewingChange = vi.fn();
const entry: WorktreeEntry = {
  id: "unfinished",
  branch: "monocode/unfinished",
  path: "/managed/unfinished",
  baseRef: "main",
  pinned: false,
  main: false,
  missing: false,
  lastUsed: 1,
  blockedReason: "Local files need attention: source.ts (modified)",
};
const review: OutputReview = {
  planId: "review-17",
  id: entry.id!,
  branch: entry.branch!,
  path: entry.path,
  blockedReason: null,
  candidates: [
    {
      path: "node_modules",
      estimatedBytes: 2 * 1024 ** 3,
      preservedPaths: [],
      blockedReason: null,
    },
    {
      path: "dist",
      estimatedBytes: 1024 ** 3,
      preservedPaths: ["dist/.env"],
      blockedReason: null,
    },
    {
      path: "nested/dist",
      estimatedBytes: 0,
      preservedPaths: [],
      blockedReason: "Candidate contains tracked files: nested/dist/source.ts",
    },
  ],
};
const report: OutputCleanupReport = {
  planId: review.planId,
  id: entry.id!,
  path: entry.path,
  selectedPaths: ["node_modules", "dist"],
  status: "partial",
  results: [
    { path: "node_modules", estimatedRemovedBytes: 2 * 1024 ** 3, error: null },
    {
      path: "dist",
      estimatedRemovedBytes: 0,
      error: "Output was replaced; review again",
    },
  ],
  estimatedRemovedBytes: 2 * 1024 ** 3,
  observedFreeSpaceChange: -(1024 ** 3),
  measurementError: null,
  preparationNeeded: true,
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  events = new Map();
  vi.mocked(listen).mockImplementation(async (event, callback) => {
    events.set(event, callback as () => void);
    return () => {
      events.delete(event);
    };
  });
  vi.mocked(getWorktreeOutputHistory).mockResolvedValue([]);
  vi.mocked(reviewWorktreeOutputs).mockResolvedValue(review);
  vi.mocked(executeWorktreeOutputCleanup).mockResolvedValue(report);
  onChanged.mockResolvedValue(undefined);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});
async function render(entries: WorktreeEntry[] = [entry]) {
  await act(async () =>
    root.render(
      createElement(WorktreeOutputCleanup, {
        cwd: "/repo",
        entries,
        onChanged,
        onReviewingChange,
      }),
    ),
  );
}
const button = (text: string) =>
  [...container.querySelectorAll("button")].find(
    (b) => b.textContent === text,
  )!;
async function click(text: string) {
  await act(async () => button(text).click());
}
const checkbox = (path: string) =>
  container.querySelector<HTMLInputElement>(
    `input[aria-label="Clear ${path}"]`,
  )!;

it("offers unfinished checkouts independently of retirement and requires explicit review then execution", async () => {
  await render();
  expect(reviewWorktreeOutputs).not.toHaveBeenCalled();
  expect(executeWorktreeOutputCleanup).not.toHaveBeenCalled();
  await click("Review generated files");
  expect(reviewWorktreeOutputs).toHaveBeenCalledExactlyOnceWith(
    "/repo",
    "unfinished",
  );
  expect(container.textContent).toContain("/managed/unfinished");
  expect(container.textContent).toContain("Preserved in place: dist/.env");
  expect(checkbox("nested/dist").disabled).toBe(true);
  expect(container.textContent).toContain("tracked files");
  expect(executeWorktreeOutputCleanup).not.toHaveBeenCalled();
  await act(async () => checkbox("dist").click());
  await click("Clear selected outputs (1)");
  expect(executeWorktreeOutputCleanup).toHaveBeenCalledExactlyOnceWith(
    "review-17",
    ["node_modules"],
  );
  expect(onChanged).toHaveBeenCalledOnce();
});

it("shows partial outcomes and distinguishes removed estimates from negative observed free-space change", async () => {
  await render();
  await click("Review generated files");
  await click("Clear selected outputs (2)");
  expect(container.textContent).toContain("Some outputs were kept");
  expect(container.textContent).toContain("Estimated removed bytes: 2 GiB");
  expect(container.textContent).toContain(
    "Observed filesystem free-space change: −1 GiB",
  );
  expect(container.textContent).toContain("Output was replaced; review again");
  expect(container.textContent).toContain("Workspace preparation needed");
  expect(onReviewingChange).toHaveBeenLastCalledWith(false);
});

it("never enables execution for an active checkout and can cancel without deleting", async () => {
  vi.mocked(reviewWorktreeOutputs).mockResolvedValue({
    ...review,
    blockedReason: "Open in a window (session, editor or terminal)",
  });
  await render();
  await click("Review generated files");
  expect(button("Clear selected outputs (0)").disabled).toBe(true);
  expect(checkbox("dist").disabled).toBe(true);
  await click("Cancel review");
  expect(executeWorktreeOutputCleanup).not.toHaveBeenCalled();
  expect(button("Review generated files").disabled).toBe(false);
});

it("recovers interrupted reports after restart without retrying deletion", async () => {
  vi.mocked(getWorktreeOutputHistory).mockResolvedValue([
    {
      ...report,
      status: "interrupted",
      results: [],
      estimatedRemovedBytes: 0,
      observedFreeSpaceChange: null,
    },
  ]);
  await render();
  expect(container.textContent).toContain("Cleanup was interrupted");
  expect(container.textContent).toContain("saved results only");
  expect(container.textContent).toContain(
    "Selected directories: node_modules, dist",
  );
  expect(container.textContent).toContain(
    "Observed filesystem free-space change: unavailable",
  );
  expect(executeWorktreeOutputCleanup).not.toHaveBeenCalled();
});

it("handles a lost execute response by reading durable results and requiring a fresh review", async () => {
  await render();
  await click("Review generated files");
  vi.mocked(executeWorktreeOutputCleanup).mockRejectedValue(
    new Error("Connection interrupted"),
  );
  vi.mocked(getWorktreeOutputHistory).mockResolvedValue([
    { ...report, status: "interrupted" },
  ]);
  await click("Clear selected outputs (2)");
  expect(container.textContent).toContain("Connection interrupted");
  expect(container.textContent).toContain("Cleanup was interrupted");
  expect(
    container.querySelector('[aria-label="Output cleanup review"]'),
  ).toBeNull();
  expect(onChanged).toHaveBeenCalledOnce();
  await click("Review generated files");
  expect(reviewWorktreeOutputs).toHaveBeenCalledTimes(2);
  expect(executeWorktreeOutputCleanup).toHaveBeenCalledTimes(1);
});

it("refreshes preparation state on setup events and preserves results when usage refresh fails", async () => {
  onChanged.mockRejectedValue(new Error("Disk scan unavailable"));
  await render();
  await click("Review generated files");
  await click("Clear selected outputs (2)");
  expect(container.textContent).toContain("Some outputs were kept");
  expect(container.textContent).toContain("Disk scan unavailable");
  vi.mocked(getWorktreeOutputHistory).mockResolvedValue([
    { ...report, preparationNeeded: false },
  ]);
  await act(async () => events.get("worktree-setup-progress")!());
  expect(container.textContent).not.toContain("Workspace preparation needed");
});

it("excludes main, external and missing checkouts from output selection", async () => {
  await render([
    { ...entry, main: true },
    { ...entry, id: null },
    { ...entry, missing: true },
  ]);
  expect(
    container.querySelector(
      '[aria-haspopup="listbox"][aria-label^="Worktree to clear:"]',
    ),
  ).toBeNull();
  expect(container.textContent).toContain("No managed checkouts");
});

it("selects a worktree with the shared settings picker before reviewing", async () => {
  await render([
    entry,
    { ...entry, id: "other", branch: "monocode/other", path: "/managed/other" },
  ]);
  const picker = container.querySelector<HTMLButtonElement>(
    '[aria-haspopup="listbox"][aria-label^="Worktree to clear:"]',
  )!;
  await act(async () => picker.click());
  const option = [
    ...document.querySelectorAll<HTMLButtonElement>('[role="option"]'),
  ].find((item) => item.textContent === "monocode/other")!;
  await act(async () => option.click());
  await click("Review generated files");
  expect(reviewWorktreeOutputs).toHaveBeenCalledExactlyOnceWith(
    "/repo",
    "other",
  );
  expect(picker.disabled).toBe(true);
});

it("keeps past cleanup details collapsed and reveals the result of a new cleanup", async () => {
  vi.mocked(getWorktreeOutputHistory).mockResolvedValue([report]);
  await render();
  const history = [...container.querySelectorAll("details")].find((details) =>
    details
      .querySelector("summary")
      ?.textContent?.includes("Recent output cleanup"),
  )!;
  expect(history.open).toBe(false);
  await click("Review generated files");
  await click("Clear selected outputs (2)");
  expect(history.open).toBe(true);
});
