import { invoke } from "@tauri-apps/api/core";
import type { Session } from "./session";
import type { WorkspaceTab } from "./layout";
import type { ProjectTerminalDock } from "./projectTerminal";

export type WorktreeSettings = {
  isolateByDefault: boolean;
  autoCleanup: boolean;
  retentionDays: number;
};

export type WorktreeEntry = {
  id: string | null;
  path: string;
  branch: string | null;
  baseRef: string | null;
  main: boolean;
  pinned: boolean;
  missing: boolean;
  lastUsed: number | null;
  blockedReason: string | null;
};

export type WorktreeOverview = {
  repo: string;
  settings: WorktreeSettings;
  entries: WorktreeEntry[];
};

export type CleanupReport = { removed: string[]; skipped: string[] };

export const listWorktrees = (cwd: string) =>
  invoke<WorktreeOverview>("worktree_list", { cwd });
export const saveWorktreeSettings = (cwd: string, settings: WorktreeSettings) =>
  invoke<void>("worktree_settings_set", { cwd, settings });
export const createWorktree = (
  cwd: string,
  sessionId: string,
  name: string,
  baseRef: string,
) =>
  invoke<string>("worktree_create", {
    cwd,
    sessionId,
    name,
    baseRef: baseRef.trim() || null,
  });
export const pinWorktree = (id: string, pinned: boolean) =>
  invoke<void>("worktree_pin", { id, pinned });
export const cleanupWorktrees = (cwd: string, ids: string[]) =>
  invoke<CleanupReport>("worktree_cleanup", { cwd, ids });
export const heartbeatWorktrees = (paths: string[]) =>
  invoke<void>("worktree_heartbeat", { paths });

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

export function suggestedWorktrees(
  overview: WorktreeOverview,
  now = Date.now(),
): WorktreeEntry[] {
  return overview.entries.filter(
    (entry) =>
      entry.id &&
      !entry.blockedReason &&
      !entry.missing &&
      entry.lastUsed != null &&
      now - entry.lastUsed >= 7 * 24 * 60 * 60 * 1000,
  );
}

export async function prepareSessionWorktree(
  session: Session,
  name = session.title,
): Promise<string | null> {
  if (session.inboxAsk || session.cwd === "~") return null;
  return invoke<string | null>("worktree_prepare", {
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
