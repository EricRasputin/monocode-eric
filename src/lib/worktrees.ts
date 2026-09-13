import { invoke } from "@tauri-apps/api/core";
import type { Session } from "./session";
import type { WorkspaceTab } from "./layout";
import type { ProjectTerminalDock } from "./projectTerminal";

export type WorktreeSettings = {
  isolateByDefault: boolean;
  autoCleanup: boolean;
  retentionDays: number;
  setupCommand?: string;
  copyPaths?: string[];
  disposablePaths?: string[];
  environmentVersion?: number;
};

export type WorktreeSetupProgress = {
  path: string;
  phase: "copying" | "running" | "ready" | "failed";
  message?: string;
};

export type WorktreeEntry = {
  id: string | null;
  path: string;
  projectCwd?: string | null;
  branch: string | null;
  baseRef: string | null;
  main: boolean;
  pinned: boolean;
  missing: boolean;
  lastUsed: number | null;
  blockedReason: string | null;
  retirementPending?: boolean;
};

export type WorktreeOverview = {
  repo: string;
  projectCwd?: string;
  settings: WorktreeSettings;
  entries: WorktreeEntry[];
};

export type CleanupReport = { removed: string[]; skipped: string[] };

export type WorktreeRetirementEntry = {
  id: string;
  repo: string;
  path: string;
  branch: string;
  worktreeRemoved?: boolean;
  blockedReason: string | null;
  localBranch: { name: string; allowed: boolean; reason: string | null };
  remoteBranch: {
    name: string;
    remote: string;
    destination: string;
    allowed: boolean;
    reason: string | null;
  } | null;
};

export type WorktreeRetirementPlan = {
  planId: string;
  entries: WorktreeRetirementEntry[];
  kept: { id: string; path: string; reason: string }[];
};

export type WorktreeRetirementSelection = {
  id: string;
  deleteLocalBranch: boolean;
  deleteRemoteBranch: boolean;
};

export type WorktreeRetirementReport = {
  results: {
    id: string;
    path: string;
    worktreeRemoved: boolean;
    localBranchDeleted: boolean;
    remoteBranchDeleted: boolean;
    recoveryRef: string | null;
    error: string | null;
  }[];
};

export const planWorktreeRetirement = (request: {
  sessionIds?: readonly string[];
  cwd?: string;
  ids?: readonly string[];
}) =>
  invoke<WorktreeRetirementPlan>("worktree_retirement_plan", {
    sessionIds: request.sessionIds ?? [],
    cwd: request.cwd ?? null,
    ids: request.ids ?? [],
  });

export const retireWorktrees = (
  planId: string,
  selections: WorktreeRetirementSelection[],
) =>
  invoke<WorktreeRetirementReport>("worktree_retire", { planId, selections });

export const listWorktrees = (cwd: string) =>
  invoke<WorktreeOverview>("worktree_list", { cwd });
export const saveWorktreeSettings = (cwd: string, settings: WorktreeSettings) =>
  invoke<WorktreeSettings>("worktree_settings_set", { cwd, settings });
export const setupWorktree = (path: string) =>
  invoke<void>("worktree_setup", { path });
export const createWorktree = async (
  cwd: string,
  sessionId: string,
  name: string,
  baseRef: string,
) => {
  const path = await invoke<string>("worktree_create", {
    cwd,
    sessionId,
    name,
    baseRef: baseRef.trim() || null,
  });
  await setupWorktree(path);
  return path;
};
export const pinWorktree = (id: string, pinned: boolean) =>
  invoke<void>("worktree_pin", { id, pinned });
export const cleanupWorktrees = (cwd: string, ids: string[]) =>
  invoke<CleanupReport>("worktree_cleanup", { cwd, ids });
// A delayed heartbeat must not reinstate paths released by a later archive.
let heartbeatQueue: Promise<void> = Promise.resolve();
export const heartbeatWorktrees = (paths: string[]): Promise<void> => {
  const pending = heartbeatQueue.then(() =>
    invoke<void>("worktree_heartbeat", { paths }),
  );
  heartbeatQueue = pending.catch(() => undefined);
  return pending;
};

/** Existing conversations stay in their checkout. A failed first preparation
 * can retry, but a provider that has already bound must never move underneath it. */
export function canChooseWorkspace(session: Session): boolean {
  return (
    !session.inboxAsk &&
    !session.worktreeCwd &&
    !session.providerSessionId &&
    !session.blocks.some(
      (block) => block.role === "assistant" || block.role === "tool",
    )
  );
}

export function shouldIsolateSession(session: Session): boolean {
  return (
    canChooseWorkspace(session) && session.workspaceChoice?.mode !== "local"
  );
}

export async function prepareSessionWorktree(
  session: Session,
  name = session.title,
): Promise<string | null> {
  if (session.inboxAsk || session.cwd === "~") return null;
  const path = await invoke<string | null>("worktree_prepare", {
    request: {
      cwd: session.cwd,
      sessionId: session.id,
      path:
        session.worktreeCwd ??
        session.workspaceChoice?.path ??
        (session.workspaceChoice?.mode === "local" ? session.cwd : null),
      name,
      createNew: shouldIsolateSession(session),
      useWorktree: session.workspaceChoice
        ? session.workspaceChoice.mode === "worktree"
        : null,
      baseRef: session.workspaceChoice?.baseRef ?? null,
    },
  });
  if (path) await setupWorktree(path);
  return path;
}

/** Every open surface protects its checkout, even when its conversation closed. */
export function protectedWorktreePaths(
  sessions: readonly Session[],
  tabs: readonly WorkspaceTab[],
  docks: readonly ProjectTerminalDock[],
): string[] {
  return [
    ...new Set(
      [
        ...sessions.map(
          (session) =>
            session.worktreeCwd || session.workspaceChoice?.path || session.cwd,
        ),
        ...tabs.flatMap((tab) =>
          [...tab.editorPanes, ...tab.terminalPanes].flatMap((pane) =>
            pane.files.flatMap((file) => [file.cwd, file.path]),
          ),
        ),
        ...docks.flatMap((dock) =>
          dock.pane.files.flatMap((file) => [file.cwd, file.path]),
        ),
      ].filter((path) => path && path !== "~"),
    ),
  ];
}
