// @vitest-environment happy-dom
import { act, createElement, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorktreeManager } from "./WorktreeManager";
import { WorktreeRecoveryStorage } from "./WorktreeRecoveryStorage";
import { refreshWorktrees, useWorktrees } from "../hooks/useWorktrees";
import {
  listWorktrees,
  planWorktreeRetirement,
  retireWorktrees,
  saveWorktreeSettings,
  type WorktreeOverview,
  type WorktreeRetirementPlan,
} from "../lib/worktrees";
import {
  reviewWorktreeOutputs,
  executeWorktreeOutputCleanup,
} from "../lib/worktreeOutputCleanup";
import { archiveProject, rememberProject } from "../lib/recents";

vi.mock("../hooks/useWorktrees", () => ({
  useWorktrees: vi.fn(),
  refreshWorktrees: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("../lib/worktrees", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/worktrees")>()),
  listWorktrees: vi.fn(),
  planWorktreeRetirement: vi.fn(),
  retireWorktrees: vi.fn(),
  saveWorktreeSettings: vi.fn(),
  pinWorktree: vi.fn(),
}));
vi.mock("../lib/worktreeOutputCleanup", () => ({
  getWorktreeOutputHistory: vi.fn().mockResolvedValue([]),
  reviewWorktreeOutputs: vi.fn(),
  executeWorktreeOutputCleanup: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));
vi.mock("./WorktreeDiskSettings", () => ({
  WorktreeDiskSettings: () =>
    createElement("div", {}, "Checkout disk capacity"),
}));
vi.mock("./WorktreeRecoveryStorage", () => ({
  WorktreeRecoveryStorage: vi.fn(({ projectCwd }: { projectCwd: string }) =>
    createElement(
      "div",
      { "data-recovery-project": projectCwd },
      "Recovery storage",
    ),
  ),
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
  settings: { isolateByDefault: true },
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

function planFor(cwd = "/repo", ids = ["ready"]): WorktreeRetirementPlan {
  return {
    planId: `plan:${cwd}`,
    entries: ids.map((id) => ({
      id,
      repo: cwd,
      path: `${cwd}/.worktrees/${id}`,
      branch: `monocode/${id}`,
      blockedReason: null,
      localBranch: {
        name: `monocode/${id}`,
        allowed: true,
        reason: null,
      },
      remoteBranch: {
        name: `monocode/${id}`,
        remote: "origin",
        destination: `github.com/acme/project/${id}`,
        allowed: true,
        reason: null,
      },
    })),
    kept: [],
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  Element.prototype.scrollIntoView = vi.fn();
  vi.mocked(listWorktrees).mockResolvedValue(overview);
  vi.mocked(saveWorktreeSettings).mockImplementation(
    async (_cwd, settings) => ({
      ...settings,
      environmentVersion: (settings.environmentVersion ?? 0) + 1,
    }),
  );
  vi.mocked(planWorktreeRetirement).mockResolvedValue(planFor());
  vi.mocked(retireWorktrees).mockResolvedValue({ results: [] });
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

async function render(
  props: Partial<ComponentProps<typeof WorktreeManager>> = {},
) {
  await act(async () =>
    root.render(
      createElement(WorktreeManager, {
        cwd: "/repo",
        recents: [],
        onOpen: vi.fn(),
        ...props,
      }),
    ),
  );
}

function projectPicker() {
  return container.querySelector<HTMLButtonElement>(
    'button[aria-label^="Project:"]',
  );
}

async function openProjectPicker() {
  if (projectPicker()?.getAttribute("aria-expanded") !== "true") {
    await act(async () => projectPicker()!.click());
  }
}

async function selectProject(path: string) {
  await openProjectPicker();
  const option = [
    ...document.querySelectorAll<HTMLButtonElement>('[role="option"]'),
  ].find((candidate) => candidate.textContent?.includes(path))!;
  await act(async () => option.click());
}

function button(text: string): HTMLButtonElement {
  return [...document.querySelectorAll<HTMLButtonElement>("button")].find(
    (candidate) => candidate.textContent?.includes(text),
  )!;
}

function selection(name = "monocode/ready"): HTMLInputElement {
  return document.querySelector<HTMLInputElement>(
    `input[aria-label="Select ${name}"]`,
  )!;
}

async function openEnvironmentSettings() {
  const summary = [...container.querySelectorAll("summary")].find((candidate) =>
    candidate.textContent?.includes("Worktree environment"),
  );
  expect(summary).toBeDefined();
  await act(async () => summary!.click());
}

function environmentField(label: string): HTMLTextAreaElement {
  const input = container.querySelector<HTMLTextAreaElement>(
    `textarea[aria-label="${label}"]`,
  );
  expect(input, label).toBeDefined();
  return input!;
}

async function enterEnvironmentValue(label: string, value: string) {
  const input = environmentField(label);
  await act(async () => {
    Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      "value",
    )?.set?.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("worktree environment settings", () => {
  it("keeps edits when another window saves and requires loading the current settings", async () => {
    const first = {
      ...overview.settings,
      environmentVersion: 1,
      setupCommand: "npm ci",
    };
    vi.mocked(useWorktrees).mockReturnValue({
      overview: { ...overview, settings: first },
      error: null,
      pending: false,
    });
    await render();
    await openEnvironmentSettings();
    await enterEnvironmentValue("Setup command", "npm ci && npm run build");

    const second = {
      ...first,
      environmentVersion: 2,
      setupCommand: "pnpm install",
      copyPaths: [".env.local"],
    };
    vi.mocked(useWorktrees).mockReturnValue({
      overview: { ...overview, settings: second },
      error: null,
      pending: false,
    });
    vi.mocked(listWorktrees).mockResolvedValue({
      ...overview,
      settings: second,
    });
    await render();

    expect(environmentField("Setup command").value).toBe(
      "npm ci && npm run build",
    );
    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "another window",
    );
    expect(button("Save environment").disabled).toBe(true);
    expect(saveWorktreeSettings).not.toHaveBeenCalled();

    await act(async () => button("Reload saved settings").click());
    expect(environmentField("Setup command").value).toBe("pnpm install");
    expect(environmentField("Copy local files").value).toBe(".env.local");
    expect(button("Save environment").disabled).toBe(false);
  });

  it("updates an untouched form after another window saves", async () => {
    const first = {
      ...overview.settings,
      environmentVersion: 1,
      setupCommand: "npm ci",
    };
    vi.mocked(useWorktrees).mockReturnValue({
      overview: { ...overview, settings: first },
      error: null,
      pending: false,
    });
    await render();
    await openEnvironmentSettings();
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        settings: {
          ...first,
          environmentVersion: 2,
          setupCommand: "pnpm install",
        },
      },
      error: null,
      pending: false,
    });
    await render();
    expect(environmentField("Setup command").value).toBe("pnpm install");
  });

  it("keeps the draft revision when the backend reports a save conflict", async () => {
    const settings = {
      ...overview.settings,
      environmentVersion: 3,
      setupCommand: "npm ci",
    };
    vi.mocked(useWorktrees).mockReturnValue({
      overview: { ...overview, settings },
      error: null,
      pending: false,
    });
    vi.mocked(saveWorktreeSettings).mockRejectedValueOnce(
      "WORKTREE_SETTINGS_CONFLICT: settings changed",
    );
    await render();
    await openEnvironmentSettings();
    await enterEnvironmentValue("Setup command", "npm install");
    await act(async () => button("Save environment").click());
    expect(saveWorktreeSettings).toHaveBeenCalledWith(
      "/repo",
      expect.objectContaining({ environmentVersion: 3 }),
    );
    expect(environmentField("Setup command").value).toBe("npm install");
    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "another window",
    );
    expect(button("Save environment").disabled).toBe(true);
  });

  it("saves project-relative setup fields explicitly", async () => {
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        settings: {
          ...overview.settings,
          setupCommand: "pnpm install",
          copyPaths: [".env.local"],
          disposablePaths: ["node_modules"],
        },
      },
      error: null,
      pending: false,
    });
    await render();
    await openEnvironmentSettings();

    expect(environmentField("Setup command").value).toBe("pnpm install");
    await enterEnvironmentValue(
      "Copy local files",
      " .env.local \n\n.config/project.json ",
    );
    await enterEnvironmentValue("Disposable folders", "node_modules\n.next");
    await act(async () => button("Save environment").click());

    expect(saveWorktreeSettings).toHaveBeenCalledWith("/repo", {
      isolateByDefault: true,
      environmentVersion: 0,
      setupCommand: "pnpm install",
      copyPaths: [".env.local", ".config/project.json"],
      disposablePaths: ["node_modules", ".next"],
    });
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "Saved",
    );
  });

  it("resets an unsaved environment draft when the selected project changes", async () => {
    vi.mocked(useWorktrees).mockImplementation((cwd) => ({
      overview: {
        ...overview,
        repo: cwd,
        settings: {
          ...overview.settings,
          setupCommand: cwd === "/repo" ? "pnpm install" : "npm ci",
          copyPaths: cwd === "/repo" ? [".env.local"] : [".env.test"],
        },
      },
      error: null,
      pending: false,
    }));
    await render({ recents: [{ path: "/second", openedAt: 1 }] });
    await openEnvironmentSettings();
    await enterEnvironmentValue("Setup command", "unsaved change");

    await selectProject("/second");
    await openEnvironmentSettings();

    expect(environmentField("Setup command").value).toBe("npm ci");
    expect(environmentField("Copy local files").value).toBe(".env.test");
    expect(saveWorktreeSettings).not.toHaveBeenCalled();
  });

  it("keeps a failed save editable and reports the error inline", async () => {
    vi.mocked(saveWorktreeSettings).mockRejectedValueOnce(
      new Error("Could not save project environment"),
    );
    await render();
    await openEnvironmentSettings();
    await enterEnvironmentValue("Setup command", "pnpm install");
    await act(async () => button("Save environment").click());

    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "Could not save project environment",
    );
    expect(environmentField("Setup command").value).toBe("pnpm install");
    expect(environmentField("Setup command").disabled).toBe(false);
    expect(button("Save environment").disabled).toBe(false);
  });
});

describe("worktree retirement inventory", () => {
  it("opens and restores a nested project inside the selected checkout", async () => {
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        projectCwd: "/repo/apps/web",
        entries: [
          {
            ...entry,
            id: "open",
            path: "/managed/open",
            branch: "monocode/open",
            blockedReason: "Pinned",
          },
          {
            ...entry,
            id: "restore",
            path: "/managed/restore",
            branch: "monocode/restore",
            missing: true,
            blockedReason: "Retired; durable recovery ref preserved",
          },
        ],
      },
      error: null,
      pending: false,
    });
    const onOpen = vi.fn().mockResolvedValue(undefined);
    await render({ cwd: "/repo/apps/web", onOpen });

    await act(async () => button("Open").click());
    expect(onOpen).toHaveBeenNthCalledWith(
      1,
      "/repo/apps/web",
      "/managed/open/apps/web",
    );
    await act(async () => button("Restore").click());
    expect(onOpen).toHaveBeenNthCalledWith(
      2,
      "/repo/apps/web",
      "/managed/restore/apps/web",
    );
  });

  it("shows recovery storage for the canonical selected project", async () => {
    vi.mocked(useWorktrees).mockReturnValue({
      overview: { ...overview, projectCwd: "/repo/apps/web" },
      error: null,
      pending: false,
    });
    await render({ cwd: "/repo" });

    expect(WorktreeRecoveryStorage).toHaveBeenCalled();
    expect(
      container
        .querySelector("[data-recovery-project]")
        ?.getAttribute("data-recovery-project"),
    ).toBe("/repo/apps/web");
  });

  it("shows only the selected nested project's owned worktrees", async () => {
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        projectCwd: "/repo/apps/web",
        entries: [
          {
            ...entry,
            id: "web",
            branch: "monocode/web-task",
            projectCwd: "/repo/apps/web",
          },
          {
            ...entry,
            id: "api",
            branch: "monocode/api-task",
            projectCwd: "/repo/apps/api",
          },
        ],
      },
      error: null,
      pending: false,
    });
    await render({ cwd: "/repo/apps/web" });
    expect(container.textContent).toContain("monocode/web-task");
    expect(container.textContent).not.toContain("monocode/api-task");
  });

  it("keeps the full manual inventory unselected without age suggestions", async () => {
    await render();

    expect(container.textContent).toContain("Ready to retire");
    expect(container.textContent).not.toContain("7 days");
    expect(container.textContent).not.toContain("Suggested");
    expect(selection().checked).toBe(false);
    expect(button("Review retirement (0)").disabled).toBe(true);
    expect(planWorktreeRetirement).not.toHaveBeenCalled();
  });

  it("asks for one plan for the selected IDs and opens the shared review", async () => {
    await render();
    await act(async () => selection().click());
    await act(async () => button("Review retirement (1)").click());

    expect(planWorktreeRetirement).toHaveBeenCalledOnce();
    expect(planWorktreeRetirement).toHaveBeenCalledWith({
      cwd: "/repo",
      ids: ["ready"],
    });
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(
      "Retire worktree?",
    );
    expect(retireWorktrees).not.toHaveBeenCalled();

    await act(async () => button("Keep for now").click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(selection().checked).toBe(true);
  });

  it("uses one review for a bulk settings selection", async () => {
    const bulkOverview = {
      ...overview,
      entries: [
        ...overview.entries,
        {
          ...entry,
          id: "second",
          branch: "monocode/second",
          path: "/managed/second",
        },
      ],
    };
    vi.mocked(useWorktrees).mockReturnValue({
      overview: bulkOverview,
      error: null,
      pending: false,
    });
    vi.mocked(planWorktreeRetirement).mockResolvedValue(
      planFor("/repo", ["ready", "second"]),
    );
    await render();

    await act(async () => selection().click());
    await act(async () => selection("monocode/second").click());
    await act(async () => button("Review retirement (2)").click());

    expect(planWorktreeRetirement).toHaveBeenCalledOnce();
    expect(planWorktreeRetirement).toHaveBeenCalledWith({
      cwd: "/repo",
      ids: ["ready", "second"],
    });
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(
      "Retire 2 worktrees?",
    );
  });

  it("keeps project selection fixed while a plan is loading", async () => {
    let finish!: (plan: WorktreeRetirementPlan) => void;
    vi.mocked(planWorktreeRetirement).mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    await render({ recents: [{ path: "/second", openedAt: 1 }] });
    await act(async () => selection().click());
    await act(async () => button("Review retirement (1)").click());
    expect(projectPicker()?.disabled).toBe(true);
    await act(async () => finish(planFor()));
    expect(document.querySelector('[role="dialog"]')).not.toBeNull();
  });

  it("surfaces planning failures and leaves review available to retry", async () => {
    vi.mocked(planWorktreeRetirement).mockRejectedValue(
      new Error("Could not inspect this working folder"),
    );
    await render();
    await act(async () => selection().click());
    await act(async () => button("Review retirement (1)").click());

    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "Could not inspect",
    );
    expect(button("Review retirement (1)").disabled).toBe(false);
  });

  it("shows a missing journaled worktree as ready for a safe retry", async () => {
    vi.mocked(useWorktrees).mockReturnValue({
      overview: {
        ...overview,
        entries: [
          {
            ...entry,
            id: "retry",
            path: "/managed/removed",
            branch: "monocode/retry",
            missing: true,
            retirementPending: true,
          },
        ],
      },
      error: null,
      pending: false,
    });
    await render();

    expect(container.textContent).toContain("monocode/retry");
    expect(container.textContent).toContain("Ready to retire");
    expect(selection("monocode/retry").disabled).toBe(false);
  });

  it("supports keyboard project selection and Escape without planning", async () => {
    await render({ recents: [{ path: "/second", openedAt: 1 }] });
    await openProjectPicker();
    const input = document.querySelector<HTMLInputElement>(
      '[aria-label="Find project"]',
    )!;
    expect(document.activeElement).toBe(input);
    await act(async () =>
      input.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
      ),
    );
    await act(async () =>
      input.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      ),
    );
    expect(useWorktrees).toHaveBeenLastCalledWith("/second");
    expect(document.activeElement).toBe(projectPicker());
    await openProjectPicker();
    await act(async () =>
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      ),
    );
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(projectPicker());
    expect(planWorktreeRetirement).not.toHaveBeenCalled();
  });

  it("includes remembered and archived projects and scopes a plan to the chosen project", async () => {
    rememberProject("/work/api");
    archiveProject("/archived/api");
    vi.mocked(useWorktrees).mockImplementation((cwd) => ({
      overview: {
        ...overview,
        repo: cwd,
        entries: overview.entries.map((candidate) => ({
          ...candidate,
          path: `${cwd}${candidate.path}`,
        })),
      },
      pending: false,
      error: null,
    }));
    vi.mocked(planWorktreeRetirement).mockResolvedValue(
      planFor("/archived/api"),
    );
    await render({ recents: [{ path: "/work/api/", openedAt: 1 }] });
    await openProjectPicker();
    const options = [...document.querySelectorAll('[role="option"]')];
    expect(options).toHaveLength(3);
    expect(options[0].textContent).toContain("Archived");
    await selectProject("/archived/api");
    await act(async () => selection().click());
    await act(async () => button("Review retirement (1)").click());

    expect(planWorktreeRetirement).toHaveBeenCalledWith({
      cwd: "/archived/api",
      ids: ["ready"],
    });
  });

  it("can manage remembered projects with no active workspace", async () => {
    await render({ cwd: "~" });
    expect(projectPicker()).toBeNull();
    expect(container.textContent).toContain("Open a Git project");
    expect(useWorktrees).not.toHaveBeenCalled();

    await render({ cwd: "~", recents: [{ path: "/second", openedAt: 1 }] });
    expect(projectPicker()?.getAttribute("aria-label")).toBe("Project: second");
    expect(useWorktrees).toHaveBeenLastCalledWith("/second");
  });

  it("refreshes the inventory after retirement while leaving results open", async () => {
    await render();
    await act(async () => selection().click());
    await act(async () => button("Review retirement (1)").click());
    vi.mocked(retireWorktrees).mockResolvedValue({
      results: [
        {
          id: "ready",
          path: "/managed/ready",
          worktreeRemoved: true,
          localBranchDeleted: false,
          remoteBranchDeleted: false,
          recoveryRef: "recovery/ready",
          error: null,
        },
      ],
    });
    await act(async () => button("Retire worktree").click());

    expect(refreshWorktrees).toHaveBeenCalledWith("/repo");
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(
      "Working folder removed",
    );
  });
});

it("reviews outputs of a kept unfinished checkout and locks conflicting settings actions", async () => {
  vi.mocked(useWorktrees).mockReturnValue({
    overview: {
      ...overview,
      entries: [
        { ...entry, id: "dirty", blockedReason: "Local source is modified" },
      ],
    },
    error: null,
    pending: false,
  });
  vi.mocked(reviewWorktreeOutputs).mockResolvedValue({
    planId: "outputs",
    id: "dirty",
    path: entry.path,
    branch: entry.branch,
    blockedReason: null,
    candidates: [
      {
        path: "dist",
        estimatedBytes: 10,
        preservedPaths: [],
        blockedReason: null,
      },
    ],
  });
  await render();
  expect(container.textContent).toContain("Nothing ready to retire");
  await act(async () => button("Review generated files").click());
  expect(reviewWorktreeOutputs).toHaveBeenCalledExactlyOnceWith(
    "/repo",
    "dirty",
  );
  expect(button("Clear selected outputs").disabled).toBe(false);
  expect(button("Refresh").disabled).toBe(true);
  expect(executeWorktreeOutputCleanup).not.toHaveBeenCalled();
  await act(async () => button("Cancel review").click());
  expect(button("Refresh").disabled).toBe(false);
});
