// @vitest-environment happy-dom
import { act, createElement, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorkspacePicker } from "./WorkspacePicker";
import { newSession, type Session } from "../lib/session";
import { prepareSessionWorktree } from "../lib/worktrees";
import { invoke } from "@tauri-apps/api/core";
import { gitCheckout, gitCreateBranch } from "../lib/fs";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(null),
}));
vi.mock("../hooks/useWorktrees", () => ({
  useWorktrees: (cwd: string) => ({
    overview: {
      repo: "/repo",
      projectCwd: cwd,
      settings: { isolateByDefault: true },
      entries: [
        { path: "/repo", branch: "main", main: true },
        { path: "/existing", branch: "feature/existing", main: false },
      ],
    },
    pending: false,
    error: null,
  }),
}));
vi.mock("../hooks/useProjectBranches", () => ({
  useProjectBranchesState: () => ({
    settled: true,
    branches: {
      current: "main",
      branches: [
        { name: "main", remote: null, current: true },
        { name: "develop", remote: null, current: false },
      ],
    },
  }),
}));
vi.mock("../lib/fs", () => ({
  gitCheckout: vi.fn(),
  gitCreateBranch: vi.fn(),
  gitCommit: vi.fn(),
  gitStageAll: vi.fn(),
  gitStash: vi.fn(),
  isCheckoutBlockedByChanges: vi.fn(),
  notifyGitChanged: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
let latest: Session;
function Harness({
  initial = newSession("claude", "/repo"),
}: {
  initial?: Session;
}) {
  const [session, setSession] = useState(initial);
  latest = session;
  return createElement(WorkspacePicker, {
    session,
    enabled: true,
    onChange: (workspaceChoice) => setSession({ ...session, workspaceChoice }),
  });
}
function button(label: string) {
  const found = [
    ...document.querySelectorAll<HTMLButtonElement>("button"),
  ].find(
    (item) =>
      item.getAttribute("aria-label") === label ||
      item.textContent?.includes(label),
  );
  expect(found, label).toBeDefined();
  return found!;
}
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(invoke).mockImplementation(async (command, args) =>
    command === "worktree_prepare"
      ? (args as { request: { path: string | null } }).request.path
      : null,
  );
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  Element.prototype.scrollIntoView = vi.fn();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});
describe("composer workspace choice", () => {
  it("shows the saved worktree identity without mounting Git controls", async () => {
    const initial = {
      ...newSession("claude", "/repo"),
      transcriptOnly: true,
      worktreeCwd: "/retired",
      branch: "saved-branch",
    };
    await act(async () => root.render(createElement(Harness, { initial })));
    expect(container.textContent).toContain("Saved workspace · saved-branch");
    expect(container.querySelector("[title='/retired']")).not.toBeNull();
    expect(container.querySelectorAll("button")).toHaveLength(0);
    expect(invoke).not.toHaveBeenCalled();
    expect(gitCheckout).not.toHaveBeenCalled();
  });

  it("chooses a base without checking out the source, then uses it on first send", async () => {
    await act(async () => root.render(createElement(Harness)));
    expect(button("Workspace: New worktree")).toBeDefined();
    await act(async () => button("Worktree base main").click());
    await act(async () => button("develop").click());
    expect(latest.workspaceChoice).toEqual({
      mode: "worktree",
      baseRef: "develop",
    });
    expect(gitCheckout).not.toHaveBeenCalled();
    expect(gitCreateBranch).not.toHaveBeenCalled();
    expect(invoke).not.toHaveBeenCalled();
    await prepareSessionWorktree(latest, "Fix validation");
    expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
      request: expect.objectContaining({
        createNew: true,
        useWorktree: true,
        baseRef: "develop",
        path: null,
      }),
    });
  });
  it("selects the current checkout or an existing worktree without creating a new one", async () => {
    await act(async () => root.render(createElement(Harness)));
    await act(async () => button("Workspace: New worktree").click());
    await act(async () => button("Current checkout").click());
    await prepareSessionWorktree(latest);
    expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
      request: expect.objectContaining({ createNew: false, path: "/repo" }),
    });
    await act(async () => button("Workspace: Current checkout").click());
    await act(async () => button("feature/existing").click());
    await prepareSessionWorktree(latest);
    expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
      request: expect.objectContaining({ createNew: false, path: "/existing" }),
    });
  });
  it("keeps a nested project cwd when choosing an existing worktree", async () => {
    await act(async () =>
      root.render(
        createElement(Harness, {
          initial: newSession("claude", "/repo/apps/web"),
        }),
      ),
    );
    await act(async () => button("Workspace: New worktree").click());
    await act(async () => button("feature/existing").click());

    expect(latest.workspaceChoice).toEqual({
      mode: "local",
      path: "/existing/apps/web",
    });
    await prepareSessionWorktree(latest);
    expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
      request: expect.objectContaining({
        cwd: "/repo/apps/web",
        createNew: false,
        path: "/existing/apps/web",
      }),
    });
  });
  it("keeps an established conversation bound to its checkout", async () => {
    await act(async () =>
      root.render(
        createElement(Harness, {
          initial: {
            ...newSession("claude", "/repo"),
            worktreeCwd: "/existing",
            providerSessionId: "bound",
          },
        }),
      ),
    );
    expect(button("Workspace: Worktree").disabled).toBe(true);
    expect(document.body.textContent).not.toContain("From main");
  });
});
