import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  prepareSessionWorktree,
  protectedWorktreePaths,
  shouldIsolateSession,
  suggestedWorktrees,
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

  it("suggests only old, eligible owned checkouts", () => {
    const now = 20 * 86400_000;
    const entry = {
      id: "old",
      path: "/old",
      branch: "old",
      baseRef: "main",
      main: false,
      pinned: false,
      missing: false,
      lastUsed: now - 7 * 86400_000,
      blockedReason: null,
    };
    const overview = {
      repo: "/repo",
      settings: {
        isolateByDefault: true,
        autoCleanup: false,
        retentionDays: 7,
      },
      entries: [
        entry,
        { ...entry, id: "recent", lastUsed: now },
        { ...entry, id: "dirty", blockedReason: "Contains changes" },
        { ...entry, id: null },
        { ...entry, id: "missing", missing: true },
      ],
    };
    expect(suggestedWorktrees(overview, now).map((item) => item.id)).toEqual([
      "old",
    ]);
  });
});
