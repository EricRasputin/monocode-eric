import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { bindHarnessSession, isLiveHarness } from "./harness";
import { bindResumedSessions } from "./appLifecycle";
import { getSession, setSessionArchived } from "./sessionStore";
import { newSession, sessionWorkCwd, type Session } from "./session";
import { newFileTab, newPlanTab, newTab, openEditorTab } from "./layout";
import {
  collectWorkspaceSnapshot,
  hydrateWorkspaceSnapshot,
} from "./workspaceSnapshot";
import { heartbeatWorktrees, protectedWorktreePaths } from "./worktrees";
import {
  createSessionWorkspacePreparation,
  prepareStandaloneWorkspacePath,
  prepareWorkspacePath,
  prepareProviderWorkspace,
} from "./sessionWorkspace";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./harness", () => ({
  bindHarnessSession: vi.fn(),
  isLiveHarness: vi.fn(() => true),
}));

function deferred() {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

function fixture() {
  let session: Session = {
    ...newSession("claude", "/repo"),
    id: "saved",
    providerSessionId: "provider",
    branch: "old-branch",
    worktreeCwd: "/checkout",
    transcriptOnly: true,
    blocks: [{ id: "message", role: "user", text: "Saved messages" }],
  };
  let archived = true;
  let preparedPath = "/canonical/checkout";
  const activated = vi.fn();
  const setup = vi.fn(async () => {});
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "session_get") return { ...session, archived } as never;
    if (command === "worktree_prepare") return preparedPath as never;
    if (command === "worktree_setup") return (await setup()) as never;
    if (command === "session_set_archived")
      archived = (args as { archived: boolean }).archived;
    return undefined as never;
  });
  const prepare = createSessionWorkspacePreparation({
    current: (id) => (id === session.id ? session : undefined),
    update: (input, patch) => {
      if (input.id !== session.id) return { ...input, ...patch };
      session = { ...session, ...patch };
      return session;
    },
    activated,
  });
  return {
    prepare,
    activated,
    setup,
    get session() {
      return session;
    },
    get archived() {
      return archived;
    },
    setPath(path: string) {
      preparedPath = path;
    },
    editTitle(title: string) {
      session = { ...session, title };
    },
  };
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe("transcript workspace boundary", () => {
  it("opens saved history without preparation, setup, activation or provider binding", async () => {
    const f = fixture();
    const saved = (await getSession(f.session.id))!;
    expect(saved.transcriptOnly).toBe(true);
    expect(saved.blocks).toEqual(f.session.blocks);
    expect(sessionWorkCwd(saved)).toBe("/checkout");
    expect(f.archived).toBe(true);
    bindResumedSessions([saved]);
    expect(bindHarnessSession).not.toHaveBeenCalled();
    expect(invoke).toHaveBeenCalledExactlyOnceWith("session_get", {
      sessionId: "saved",
    });
    expect(protectedWorktreePaths([saved], [newTab(saved.id)], [])).toEqual([]);
  });

  it("reloads history without a lease or provider, including a snapshot-only transcript", async () => {
    const f = fixture();
    const saved = (await getSession(f.session.id))!;
    const tab = openEditorTab(
      newTab(saved.id),
      newPlanTab(saved.id, "plan", "Saved plan", "/checkout"),
    );
    const snapshot = collectWorkspaceSnapshot(
      [tab],
      [saved],
      tab.id,
      "/repo",
      new Map(),
    );
    for (const loaded of [
      new Map([[saved.id, saved]]),
      new Map<string, Session>(),
    ]) {
      const restored = hydrateWorkspaceSnapshot(
        JSON.parse(JSON.stringify(snapshot)),
        loaded,
      )!;
      expect(restored.sessions[0].transcriptOnly).toBe(true);
      expect(sessionWorkCwd(restored.sessions[0])).toBe("/checkout");
      expect(
        protectedWorktreePaths(restored.sessions, restored.tabs, []),
      ).toEqual([]);
      bindResumedSessions(restored.sessions);
    }
    expect(bindHarnessSession).not.toHaveBeenCalled();
    expect(f.archived).toBe(true);
    expect(f.setup).not.toHaveBeenCalled();
  });

  it("uses the actual prepared path, invalidates the old branch and activates only after setup", async () => {
    const f = fixture();
    const setup = deferred();
    f.setup.mockImplementation(() => setup.promise);
    const task = f.prepare(f.session);
    await vi.waitFor(() => expect(f.setup).toHaveBeenCalledOnce());
    expect(f.session.transcriptOnly).toBe(true);
    expect(f.archived).toBe(true);
    f.editTitle("Edited during setup");
    setup.resolve();
    const prepared = await task;
    expect(prepared.cwd).toBe("/canonical/checkout");
    expect(prepared.session).toMatchObject({
      title: "Edited during setup",
      worktreeCwd: prepared.cwd,
      transcriptOnly: false,
    });
    expect(prepared.session.branch).toBeUndefined();
    expect(f.archived).toBe(false);
    expect(f.activated).toHaveBeenCalledOnce();
    expect(protectedWorktreePaths([f.session], [], [])).toEqual([prepared.cwd]);
    const commands = vi.mocked(invoke).mock.calls.map(([command]) => command);
    expect(commands.indexOf("worktree_setup")).toBeLessThan(
      commands.indexOf("session_upsert"),
    );
    expect(commands.indexOf("session_upsert")).toBeLessThan(
      commands.indexOf("session_set_archived"),
    );
    expect(bindHarnessSession).not.toHaveBeenCalled();
  });

  it.each(["worktree_prepare", "worktree_setup", "session_set_archived"])(
    "keeps history readable and retryable after %s failure",
    async (failingCommand) => {
      const f = fixture();
      const native = vi.mocked(invoke).getMockImplementation()!;
      let fail = true;
      vi.mocked(invoke).mockImplementation(async (command, args) => {
        if (fail && command === failingCommand) throw new Error("Unavailable");
        return native(command, args);
      });
      await expect(f.prepare(f.session)).rejects.toThrow("Unavailable");
      expect(f.session.transcriptOnly).toBe(true);
      expect(f.session.blocks[0].text).toBe("Saved messages");
      expect(f.archived).toBe(true);
      expect(f.activated).not.toHaveBeenCalled();
      expect(bindHarnessSession).not.toHaveBeenCalled();
      fail = false;
      await expect(f.prepare(f.session)).resolves.toMatchObject({
        cwd: "/canonical/checkout",
      });
      expect(f.archived).toBe(false);
    },
  );

  it("rejects an empty native location for a saved checkout without falling back to main", async () => {
    const f = fixture();
    vi.mocked(invoke).mockResolvedValue(null);
    await expect(f.prepare(f.session)).rejects.toThrow("saved checkout");
    expect(sessionWorkCwd(f.session)).toBe("/checkout");
    expect(f.archived).toBe(true);
    expect(f.setup).not.toHaveBeenCalled();
  });

  it("shares concurrent coding, file and terminal preparation, with leases during setup", async () => {
    const f = fixture();
    const setup = deferred();
    f.setup.mockImplementation(() => setup.promise);
    const original = f.session;
    const readFile = vi.fn();
    const spawnTerminal = vi.fn();
    const coding = f.prepare(original);
    expect(f.prepare(original)).toBe(coding);
    const file = prepareWorkspacePath(
      original,
      "/checkout/src/file.ts",
      f.prepare,
    ).then(readFile);
    const terminal = prepareWorkspacePath(
      original,
      "/checkout/src",
      f.prepare,
    ).then(spawnTerminal);
    await vi.waitFor(() => expect(f.setup).toHaveBeenCalledOnce());
    await heartbeatWorktrees([]);
    expect(invoke).toHaveBeenLastCalledWith("worktree_heartbeat", {
      paths: ["/checkout", "/canonical/checkout"],
    });
    expect(readFile).not.toHaveBeenCalled();
    expect(spawnTerminal).not.toHaveBeenCalled();
    setup.resolve();
    await Promise.all([coding, file, terminal]);
    expect(readFile).toHaveBeenCalledWith("/canonical/checkout/src/file.ts");
    expect(spawnTerminal).toHaveBeenCalledWith("/canonical/checkout/src");
    expect(
      vi
        .mocked(invoke)
        .mock.calls.filter(([command]) => command === "worktree_prepare"),
    ).toHaveLength(1);
    expect(f.activated).toHaveBeenCalledOnce();
    await heartbeatWorktrees([]);
    expect(invoke).toHaveBeenLastCalledWith("worktree_heartbeat", {
      paths: [],
    });
  });

  it("does not run file or terminal callbacks after failed setup, and retries both", async () => {
    const f = fixture();
    f.setup.mockRejectedValueOnce(new Error("Setup failed"));
    const file = vi.fn();
    const terminal = vi.fn();
    const original = f.session;
    const results = await Promise.allSettled([
      prepareWorkspacePath(original, "/checkout/file.ts", f.prepare).then(file),
      prepareWorkspacePath(original, "/checkout", f.prepare).then(terminal),
    ]);
    expect(results.map((result) => result.status)).toEqual([
      "rejected",
      "rejected",
    ]);
    expect(file).not.toHaveBeenCalled();
    expect(terminal).not.toHaveBeenCalled();
    expect(f.archived).toBe(true);
    await prepareWorkspacePath(
      f.session,
      "/canonical/checkout/file.ts",
      f.prepare,
    ).then(file);
    await prepareWorkspacePath(
      f.session,
      "/canonical/checkout",
      f.prepare,
    ).then(terminal);
    expect(file).toHaveBeenCalledWith("/canonical/checkout/file.ts");
    expect(terminal).toHaveBeenCalledWith("/canonical/checkout");
  });

  it("checks native setup again after success so pending generated-output cleanup is honored", async () => {
    const f = fixture();
    await f.prepare(f.session);
    f.setup.mockRejectedValueOnce(new Error("Setup pending after cleanup"));
    await expect(f.prepare(f.session)).rejects.toThrow(
      "Setup pending after cleanup",
    );
    await f.prepare(f.session);
    expect(f.setup).toHaveBeenCalledTimes(3);
  });

  it.each(["/plain folder", "/repo", "/managed/retired"])(
    "prepares a standalone directory, never its requested file: %s",
    async (cwd) => {
      const f = fixture();
      f.setPath(cwd);
      expect(
        await prepareStandaloneWorkspacePath(cwd, `${cwd}/file.txt`, f.prepare),
      ).toBe(`${cwd}/file.txt`);
      expect(invoke).toHaveBeenCalledWith("worktree_prepare", {
        request: expect.objectContaining({
          cwd,
          path: cwd,
          createNew: false,
          useWorktree: false,
        }),
      });
      expect(
        vi
          .mocked(invoke)
          .mock.calls.some(([command]) => command === "session_set_archived"),
      ).toBe(false);
      expect(await prepareStandaloneWorkspacePath(cwd, cwd, f.prepare)).toBe(
        cwd,
      );
    },
  );

  it("binds and starts the coding provider only after setup succeeds, including retry", async () => {
    const f = fixture();
    const send = vi.fn();
    const start = () =>
      prepareProviderWorkspace(f.session, f.prepare, () => true).then(
        (prepared) => send(prepared.cwd),
      );
    f.setup.mockRejectedValueOnce(new Error("Setup failed"));
    await expect(start()).rejects.toThrow("Setup failed");
    expect(bindHarnessSession).not.toHaveBeenCalled();
    expect(send).not.toHaveBeenCalled();
    const setup = deferred();
    f.setup.mockImplementationOnce(() => setup.promise);
    const retry = start();
    await vi.waitFor(() => expect(f.setup).toHaveBeenCalledTimes(2));
    expect(send).not.toHaveBeenCalled();
    expect(bindHarnessSession).not.toHaveBeenCalled();
    setup.resolve();
    await retry;
    expect(bindHarnessSession).toHaveBeenCalledExactlyOnceWith(
      "claude",
      "saved",
      "provider",
      "/canonical/checkout",
    );
    expect(send).toHaveBeenCalledExactlyOnceWith("/canonical/checkout");
  });

  it("does not bind a provider after a pending turn was cancelled", async () => {
    const f = fixture();
    await prepareProviderWorkspace(f.session, f.prepare, () => false);
    expect(bindHarnessSession).not.toHaveBeenCalled();
  });

  it("activates each conversation separately when they share a checkout setup", async () => {
    const f = fixture();
    const second = { ...f.session, id: "second" };
    const sessions = new Map([
      [f.session.id, f.session],
      [second.id, second],
    ]);
    const activated = vi.fn();
    const prepare = createSessionWorkspacePreparation({
      current: (id) => sessions.get(id),
      update: (session, patch) => {
        const next = { ...sessions.get(session.id)!, ...patch };
        sessions.set(session.id, next);
        return next;
      },
      activated,
    });
    const setup = deferred();
    // Native setup shares this completion per checkout, even for distinct callers.
    f.setup.mockImplementation(() => setup.promise);
    const tasks = [prepare(f.session), prepare(second)];
    await vi.waitFor(() => expect(f.setup).toHaveBeenCalledTimes(2));
    expect(activated).not.toHaveBeenCalled();
    setup.resolve();
    await Promise.all(tasks);
    expect(activated.mock.calls.map(([session]) => session.id).sort()).toEqual([
      "saved",
      "second",
    ]);
    for (const sessionId of ["saved", "second"])
      expect(invoke).toHaveBeenCalledWith("session_set_archived", {
        sessionId,
        archived: false,
      });
  });

  it.each([false, true])(
    "archive waits for preparation to settle, with setup failure=%s",
    async (fail) => {
      const f = fixture();
      const setup = deferred();
      f.setup.mockImplementation(() => setup.promise);
      const preparing = f.prepare(f.session);
      const outcome = preparing.catch(() => undefined);
      await vi.waitFor(() => expect(f.setup).toHaveBeenCalledOnce());
      let archived = false;
      const archive = (async () => {
        await f.prepare.settled(f.session.id);
        await setSessionArchived(f.session.id, true);
        archived = true;
      })();
      await Promise.resolve();
      expect(archived).toBe(false);
      if (fail) setup.reject(new Error("Setup failed"));
      else setup.resolve();
      await Promise.all([outcome, archive]);
      expect(f.archived).toBe(true);
      expect(invoke).toHaveBeenLastCalledWith("session_set_archived", {
        sessionId: "saved",
        archived: true,
      });
      await f.prepare.settled(f.session.id);
      await heartbeatWorktrees([]);
      expect(invoke).toHaveBeenLastCalledWith("worktree_heartbeat", {
        paths: [],
      });
    },
  );

  it("retains independent file leases alongside transcript-only conversations", () => {
    const f = fixture();
    const tab = openEditorTab(
      newTab(f.session.id),
      newFileTab("/checkout/file.ts", "/checkout"),
    );
    expect(protectedWorktreePaths([f.session], [tab], [])).toEqual([
      "/checkout",
      "/checkout/file.ts",
    ]);
    expect(isLiveHarness("claude")).toBe(true);
  });
});
