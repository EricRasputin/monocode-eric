// @vitest-environment happy-dom
import { act, createElement, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { WorktreeAutomaticRetirement } from "./WorktreeAutomaticRetirement";
import {
  listWorktrees,
  retryAutomaticRetirement,
  saveRetirementPolicy,
} from "../lib/worktrees";

vi.mock("../lib/worktrees", async (original) => ({
  ...(await original<typeof import("../lib/worktrees")>()),
  listWorktrees: vi.fn(),
  retryAutomaticRetirement: vi.fn(),
  saveRetirementPolicy: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
const onSaved = vi.fn();
beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  vi.mocked(saveRetirementPolicy).mockImplementation(async (_cwd, policy) => ({
    ...policy,
    version: policy.version + 1,
  }));
  vi.mocked(retryAutomaticRetirement).mockResolvedValue(undefined);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});
async function render(
  props: Partial<ComponentProps<typeof WorktreeAutomaticRetirement>> = {},
) {
  await act(async () =>
    root.render(
      createElement(WorktreeAutomaticRetirement, {
        cwd: "/repo",
        onSaved,
        ...props,
      }),
    ),
  );
}
const select = () => container.querySelector("select")!;
const button = (text: string) =>
  [...container.querySelectorAll("button")].find(
    (b) => b.textContent === text,
  )!;
async function choose(mode: string) {
  await act(async () => {
    select().value = mode;
    select().dispatchEvent(new Event("change", { bubbles: true }));
  });
}
async function click(text: string) {
  await act(async () => button(text).click());
}

it("defaults to manual and enables automatic mode only through an explicit versioned save", async () => {
  await render();
  expect(select().value).toBe("manual");
  expect(button("Save retirement preference").disabled).toBe(true);
  await choose("automatic");
  expect(saveRetirementPolicy).not.toHaveBeenCalled();
  await click("Save retirement preference");
  expect(saveRetirementPolicy).toHaveBeenCalledExactlyOnceWith("/repo", {
    schemaVersion: 1,
    version: 0,
    mode: "automatic",
  });
  expect(container.textContent).toContain("Retirement preference saved");
  expect(onSaved).toHaveBeenCalledTimes(1);
});

it("keeps a dirty draft when another window saves and requires reload before overwriting", async () => {
  await render();
  await choose("automatic");
  await render({ policy: { schemaVersion: 1, version: 1, mode: "manual" } });
  expect(select().value).toBe("automatic");
  expect(container.textContent).toContain("another window");
  expect(button("Save retirement preference").disabled).toBe(true);
  vi.mocked(listWorktrees).mockResolvedValue({
    repo: "/repo",
    entries: [],
    settings: { isolateByDefault: true },
    retirementPolicy: { schemaVersion: 1, version: 2, mode: "manual" },
  });
  await click("Reload preference");
  await choose("automatic");
  await click("Save retirement preference");
  expect(saveRetirementPolicy).toHaveBeenLastCalledWith("/repo", {
    schemaVersion: 1,
    version: 2,
    mode: "automatic",
  });
});

it("keeps generic save failures retryable without claiming another window changed the preference", async () => {
  await render();
  await choose("automatic");
  vi.mocked(saveRetirementPolicy).mockRejectedValueOnce(
    new Error("Database unavailable"),
  );
  await click("Save retirement preference");
  expect(select().value).toBe("automatic");
  expect(container.textContent).toContain("Database unavailable");
  expect(container.textContent).not.toContain("another window");
  expect(button("Save retirement preference").disabled).toBe(false);
  await click("Save retirement preference");
  expect(saveRetirementPolicy).toHaveBeenCalledTimes(2);
  expect(container.textContent).toContain("Retirement preference saved");
});

it("recognizes a native version conflict even before the inventory event arrives", async () => {
  await render();
  await choose("automatic");
  vi.mocked(saveRetirementPolicy).mockRejectedValueOnce(
    "WORKTREE_RETIREMENT_CONFLICT: Retirement preference changed in another window.",
  );
  await click("Save retirement preference");
  expect(select().value).toBe("automatic");
  expect(button("Save retirement preference").disabled).toBe(true);
  expect(button("Reload preference")).toBeTruthy();
});

it("shows durable blocked and partial failure explanations and retries through native maintenance", async () => {
  await render({
    policy: { schemaVersion: 1, version: 3, mode: "automatic" },
    items: [
      {
        id: "pinned",
        path: "/managed/pinned",
        planId: null,
        status: "blocked",
        reason: "Conversation is pinned",
        updatedAt: 100,
      },
      {
        id: "partial",
        path: "/managed/partial",
        planId: "existing-plan",
        status: "failed",
        reason: "Git removed the folder; completion could not be recorded",
        updatedAt: 100,
      },
    ],
  });
  expect(container.textContent).toContain("Pending archive cleanup (2)");
  expect(container.textContent).toContain("Conversation is pinned");
  expect(container.textContent).toContain("completion could not be recorded");
  await click("Retry automatic cleanup");
  expect(retryAutomaticRetirement).toHaveBeenCalledTimes(1);
  expect(saveRetirementPolicy).not.toHaveBeenCalled();
  await choose("manual");
  await click("Save retirement preference");
  expect(saveRetirementPolicy).toHaveBeenCalledWith("/repo", {
    schemaVersion: 1,
    version: 3,
    mode: "manual",
  });
  expect(container.textContent).not.toContain("Retry automatic cleanup");
  expect(container.textContent).toContain("completion could not be recorded");
});
