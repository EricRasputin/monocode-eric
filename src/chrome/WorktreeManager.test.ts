// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorktreeManager } from "./WorktreeManager";
import { useWorktrees, refreshWorktrees } from "../hooks/useWorktrees";
import {
  cleanupWorktrees,
  listWorktrees,
  type WorktreeOverview,
} from "../lib/worktrees";

vi.mock("../hooks/useWorktrees", () => ({
  useWorktrees: vi.fn(),
  refreshWorktrees: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("../lib/worktrees", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/worktrees")>()),
  listWorktrees: vi.fn(),
  cleanupWorktrees: vi.fn(),
  saveWorktreeSettings: vi.fn(),
  pinWorktree: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
const entry = {
  path: "/managed/ready",
  branch: "monocode/ready",
  baseRef: "main",
  main: false,
  pinned: false,
  missing: false,
  lastUsed: 123,
  blockedReason: null,
};
const overview: WorktreeOverview = {
  repo: "/repo",
  settings: { isolateByDefault: true, autoCleanup: true, retentionDays: 7 },
  entries: [
    { ...entry, id: "ready" },
    {
      ...entry,
      id: "protected",
      path: "/managed/pinned",
      branch: "monocode/pinned",
      pinned: true,
      blockedReason: "Pinned",
    },
    {
      ...entry,
      id: null,
      path: "/external",
      branch: "external",
      blockedReason: "External worktree",
    },
  ],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.mocked(listWorktrees).mockResolvedValue(overview);
  vi.mocked(useWorktrees).mockReturnValue({
    overview,
    error: null,
    pending: false,
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function render() {
  await act(async () =>
    root.render(
      createElement(WorktreeManager, {
        cwd: "/repo",
        onOpen: vi.fn(),
      }),
    ),
  );
}
function button(text: string): HTMLButtonElement {
  return [...document.querySelectorAll<HTMLButtonElement>("button")].find(
    (button) => button.textContent?.includes(text),
  )!;
}

describe("worktree cleanup review", () => {
  it("reviews only eligible IDs, preserves the preview selection and reports skipped changes", async () => {
    await render();
    await act(async () => button("Review removal (1)").click());
    expect(cleanupWorktrees).not.toHaveBeenCalled();
    const list = document.querySelector("ul")!;
    expect(list.textContent).toContain("monocode/ready");
    expect(list.textContent).not.toContain("monocode/pinned");
    vi.mocked(cleanupWorktrees).mockResolvedValue({
      removed: [],
      skipped: ["monocode/ready: Contains changes"],
    });
    await act(async () => button("Remove checkouts").click());
    expect(cleanupWorktrees).toHaveBeenCalledWith("/repo", ["ready"]);
    expect(document.querySelector('[role="status"]')?.textContent).toContain(
      "Contains changes",
    );
  });

  it("surfaces failures and leaves cleanup available to retry", async () => {
    await render();
    await act(async () => button("Review removal").click());
    vi.mocked(cleanupWorktrees).mockRejectedValue(
      "Git could not inspect the checkout",
    );
    await act(async () => button("Remove checkouts").click());
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(
      "Git could not inspect",
    );
    expect(button("Remove checkouts").disabled).toBe(false);
  });
  it("never removes a suggested worktree until the review is confirmed", async () => {
    await render();
    expect(cleanupWorktrees).not.toHaveBeenCalled();
    expect(document.body.textContent).not.toContain("Automatically clean up");
    await act(async () => button("Review removal").click());
    await act(async () => button("Cancel").click());
    expect(cleanupWorktrees).not.toHaveBeenCalled();
    expect(refreshWorktrees).not.toHaveBeenCalled();
  });

  it("keeps the reviewed IDs when the eligible list changes before confirmation", async () => {
    await render();
    await act(async () => button("Review removal").click());
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        entries: [
          ...overview.entries,
          {
            ...entry,
            id: "arrived-later",
            path: "/managed/later",
            branch: "later",
          },
        ],
      },
      error: null,
      pending: false,
    });
    await render();
    vi.mocked(cleanupWorktrees).mockResolvedValue({
      removed: [entry.path],
      skipped: [],
    });
    await act(async () => button("Remove checkouts").click());
    expect(cleanupWorktrees).toHaveBeenCalledWith("/repo", ["ready"]);
  });
});
