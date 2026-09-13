import { useCallback, useSyncExternalStore } from "react";
import { listWorktrees, type WorktreeOverview } from "../lib/worktrees";
import { subscribeGitChanged } from "../lib/fs";

type Snapshot = {
  overview: WorktreeOverview | null;
  error: string | null;
  pending: boolean;
};
type Entry = {
  cwd: string;
  snapshot: Snapshot;
  listeners: Set<() => void>;
  stop?: () => void;
  loading: boolean;
  again: boolean;
};
const EMPTY: Snapshot = { overview: null, error: null, pending: false };
const entries = new Map<string, Entry>();
function entryFor(cwd: string): Entry {
  let entry = entries.get(cwd);
  if (!entry) {
    entry = {
      cwd,
      snapshot: { ...EMPTY, pending: true },
      listeners: new Set(),
      loading: false,
      again: false,
    };
    entries.set(cwd, entry);
  }
  return entry;
}
async function refresh(entry: Entry) {
  if (entry.loading) {
    entry.again = true;
    return;
  }
  entry.loading = true;
  try {
    entry.snapshot = {
      overview: await listWorktrees(entry.cwd),
      error: null,
      pending: false,
    };
  } catch (error) {
    entry.snapshot = { overview: null, error: String(error), pending: false };
  } finally {
    entry.loading = false;
    for (const listener of entry.listeners) listener();
    if (entry.again) {
      entry.again = false;
      void refresh(entry);
    }
  }
}
export function refreshWorktrees(cwd: string) {
  return refresh(entryFor(cwd));
}

/** Shared read-only inspection; this timer can suggest cleanup but never removes anything. */
export function useWorktrees(cwd: string, enabled = true): Snapshot {
  const active = enabled && !!cwd && cwd !== "~";
  const subscribe = useCallback(
    (listener: () => void) => {
      if (!active) return () => undefined;
      const entry = entryFor(cwd);
      entry.listeners.add(listener);
      if (entry.listeners.size === 1) {
        const update = () => {
          if (!document.hidden) void refresh(entry);
        };
        void refresh(entry);
        const interval = window.setInterval(update, 60_000);
        const unsubscribe = subscribeGitChanged(update);
        window.addEventListener("focus", update);
        entry.stop = () => {
          window.clearInterval(interval);
          unsubscribe();
          window.removeEventListener("focus", update);
        };
      }
      return () => {
        entry.listeners.delete(listener);
        if (!entry.listeners.size) {
          entry.stop?.();
          entry.stop = undefined;
        }
      };
    },
    [active, cwd],
  );
  const getSnapshot = useCallback(
    () => (active ? entryFor(cwd).snapshot : EMPTY),
    [active, cwd],
  );
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}
