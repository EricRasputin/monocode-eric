import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  createWorktree,
  prepareSessionWorktree,
  protectedWorktreePaths,
  shouldIsolateSession,
  worktreeProjectPath,
} from "./worktrees";
import { newSession, sessionWorkCwd } from "./session";
import { newTab } from "./layout";
import { restoreSessionCheckout } from "./fs";
import {
  collectWorkspaceSnapshot,
  hydrateWorkspaceSnapshot,
} from "./workspaceSnapshot";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

beforeEach(() => vi.clearAllMocks());

describe("worktree session lifecycle", () => {
  it("projects a selected nested project into another checkout", () => {
    expect(
      worktreeProjectPath(
        { repo: "/repo", projectCwd: "/repo/apps/web" },
        "/managed/worktree",
      ),
    ).toBe("/managed/worktree/apps/web");
    expect(
      worktreeProjectPath(
        { repo: "/repo", projectCwd: "/repo" },
        "/managed/worktree",
      ),
    ).toBe("/managed/worktree");
    expect(
      worktreeProjectPath(
        { repo: "/repo", projectCwd: "/repo/apps/web " },
        "/managed/worktree",
      ),
    ).toBe("/managed/worktree/apps/web ");
  });

  it("isolates new conversations and retries failed setup without moving bound providers", () => {
    const session = newSession("claude", "/repo");
    expect(shouldIsolateSession(session)).toBe(true);
    expect(
      shouldIsolateSession({
        ...session,
        blocks: [{ id: "1", role: "user", text: "try" }],
      }),
    ).toBe(true);
    expect(
      shouldIsolateSession({ ...session, providerSessionId: "bound" }),
    ).toBe(false);
    expect(
      shouldIsolateSession({
        ...session,
        blocks: [{ id: "2", role: "assistant", text: "done" }],
      }),
    ).toBe(false);
    expect(shouldIsolateSession({ ...session, worktreeCwd: "/isolated" })).toBe(
      false,
    );
  });

  it("prepares the existing path before agent startup and surfaces failures", async () => {
    const session = {
      ...newSession("claude", "/repo"),
      worktreeCwd: "/isolated",
    };
    vi.mocked(invoke).mockRejectedValueOnce("Preserved branch missing");
    await expect(prepareSessionWorktree(session)).rejects.toBe(
      "Preserved branch missing",
    );
    expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
      request: expect.objectContaining({
        cwd: "/repo",
        path: "/isolated",
        createNew: false,
      }),
    });
  });

  it("runs setup after prepare and retries setup failures on the next send", async () => {
    const session = newSession("claude", "/repo");
    vi.mocked(invoke)
      .mockResolvedValueOnce("/repo/.worktrees/new")
      .mockRejectedValueOnce(new Error("Setup command failed"));

    await expect(prepareSessionWorktree(session, "Add search")).rejects.toThrow(
      "Setup command failed",
    );
    expect(vi.mocked(invoke).mock.calls).toEqual([
      ["worktree_prepare", { request: expect.any(Object) }],
      ["worktree_setup", { path: "/repo/.worktrees/new" }],
    ]);

    vi.mocked(invoke)
      .mockResolvedValueOnce("/repo/.worktrees/new")
      .mockResolvedValueOnce(undefined);
    await expect(prepareSessionWorktree(session, "Add search")).resolves.toBe(
      "/repo/.worktrees/new",
    );
    expect(invoke).toHaveBeenNthCalledWith(3, "worktree_prepare", {
      request: expect.any(Object),
    });
    expect(invoke).toHaveBeenNthCalledWith(4, "worktree_setup", {
      path: "/repo/.worktrees/new",
    });
  });

  it("sets up worktrees created through the direct API", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce("/repo/.worktrees/manual")
      .mockResolvedValueOnce(undefined);

    await expect(
      createWorktree("/repo", "session", "Manual", " main "),
    ).resolves.toBe("/repo/.worktrees/manual");
    expect(vi.mocked(invoke).mock.calls).toEqual([
      [
        "worktree_create",
        {
          cwd: "/repo",
          sessionId: "session",
          name: "Manual",
          baseRef: "main",
        },
      ],
      ["worktree_setup", { path: "/repo/.worktrees/manual" }],
    ]);
  });

  it("keeps checkout and provider identity through workspace snapshots and restore", () => {
    const session = {
      ...newSession("claude", "/repo"),
      worktreeCwd: "/isolated",
      providerSessionId: "provider",
      branch: "feature",
    };
    const tab = newTab(session.id);
    const snapshot = collectWorkspaceSnapshot(
      [tab],
      [session],
      tab.id,
      "/repo",
      new Map(),
    );
    const restored = hydrateWorkspaceSnapshot(
      snapshot,
      new Map([[session.id, session]]),
    )!;
    expect(restoreSessionCheckout(restored.sessions[0])).toMatchObject({
      cwd: "/repo",
      worktreeCwd: "/isolated",
      providerSessionId: "provider",
    });
    expect(sessionWorkCwd(restored.sessions[0])).toBe("/isolated");
  });

  it("protects editor and terminal paths after their conversation is closed", () => {
    const tab = {
      ...newTab("session"),
      editorPanes: [
        {
          id: "editor",
          activeFileId: "file",
          files: [
            { id: "file", cwd: "/isolated", path: "/isolated/edited.ts" },
          ],
        },
      ],
      terminalPanes: [
        {
          id: "terminal",
          activeFileId: "shell",
          files: [
            { id: "shell", cwd: "/another", path: "Terminal", terminal: true },
          ],
        },
      ],
    };
    const paths = protectedWorktreePaths([], [tab], []);
    expect(paths).toContain("/isolated");
    expect(paths).toContain("/isolated/edited.ts");
    expect(paths).toContain("/another");
  });
  it("preserves a draft's explicit workspace and base across restart", () => {
    const session = {
      ...newSession("claude", "/repo"),
      workspaceChoice: { mode: "worktree" as const, baseRef: "develop" },
    };
    const tab = newTab(session.id);
    const snapshot = collectWorkspaceSnapshot(
      [tab],
      [session],
      tab.id,
      "/repo",
      new Map(),
    );
    for (const saved of [
      new Map(),
      new Map([[session.id, { ...session, workspaceChoice: undefined }]]),
    ]) {
      expect(
        hydrateWorkspaceSnapshot(snapshot, saved)!.sessions[0].workspaceChoice,
      ).toEqual(session.workspaceChoice);
    }
  });
});
